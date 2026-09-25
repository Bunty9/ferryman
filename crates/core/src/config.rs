//! TOML config schema + reload-time builder.
//!
//! `config.toml` is the authoritative source for routing rules. The reload
//! task in `crates/server/src/reload.rs` re-reads this file on filesystem
//! events, calls [`build_table`], and atomically swaps the result into the
//! `SharedTable`.

use crate::route::{Route, RouteTable, Upstream};
use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Top-level config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigToml {
    /// Active health-check interval in seconds.
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// Default circuit-breaker cooldown if a route doesn't override.
    #[serde(default = "default_cooldown")]
    pub default_cooldown_secs: u64,
    /// Consecutive failures before a closed circuit opens. Must be >= 1.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Timeout for a forwarded request to an upstream, in seconds.
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    pub routes: Vec<RouteToml>,
}

fn default_health_interval() -> u64 {
    5
}
fn default_cooldown() -> u64 {
    30
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_upstream_timeout() -> u64 {
    30
}

/// One routing rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteToml {
    /// Path prefix to match (e.g. `/svc-a`).
    pub prefix: String,
    /// Upstream URI, e.g. `http://localhost:8001`.
    pub upstream: String,
    /// Per-route circuit-breaker cooldown override.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

/// Read and parse the TOML config at `path`. Shared by the server's initial
/// boot and its hot-reload watcher so both get the same error context.
pub fn load_config(path: &Path) -> anyhow::Result<ConfigToml> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing config file {}", path.display()))
}

/// Build a [`RouteTable`] from a parsed [`ConfigToml`], validating routes
/// and upstream URIs. If `prev` is given, upstreams that also existed in it
/// (matched by `host:port` name) share its circuit breaker, so a hot reload
/// neither resets an open circuit nor strands results from in-flight
/// requests on an orphaned breaker. Fails without side effects.
pub fn build_table(cfg: ConfigToml, prev: Option<&RouteTable>) -> anyhow::Result<RouteTable> {
    if cfg.failure_threshold < 1 {
        bail!(
            "failure_threshold must be >= 1, got {}",
            cfg.failure_threshold
        );
    }
    for (key, v) in [
        ("health_interval_secs", cfg.health_interval_secs),
        ("upstream_timeout_secs", cfg.upstream_timeout_secs),
        ("default_cooldown_secs", cfg.default_cooldown_secs),
    ] {
        if v == 0 {
            bail!("{key} must be >= 1");
        }
    }

    let default_cooldown = Duration::from_secs(cfg.default_cooldown_secs);
    let upstream_timeout = Duration::from_secs(cfg.upstream_timeout_secs);

    let prev_by_name: HashMap<&str, &Upstream> = prev
        .map(|t| t.upstreams().map(|u| (u.name.as_str(), u)).collect())
        .unwrap_or_default();

    // Validate everything first so a rejected reload never touches the
    // breakers the live table is still using.
    let mut seen_prefixes = HashSet::new();
    let mut cooldown_by_name: HashMap<String, Duration> = HashMap::new();
    let mut validated = Vec::with_capacity(cfg.routes.len());

    for r in cfg.routes {
        if !r.prefix.starts_with('/') {
            bail!("route prefix {:?} must start with '/'", r.prefix);
        }
        if !seen_prefixes.insert(r.prefix.clone()) {
            bail!("duplicate route prefix {:?}", r.prefix);
        }

        let uri: http::Uri = r.upstream.parse().with_context(|| {
            format!(
                "route {:?}: invalid upstream URI {:?}",
                r.prefix, r.upstream
            )
        })?;

        match uri.scheme_str() {
            Some("http") => {}
            Some(other) => bail!(
                "route {:?}: upstream {:?} has scheme {:?} — https upstreams are not supported",
                r.prefix,
                r.upstream,
                other
            ),
            None => bail!(
                "route {:?}: upstream {:?} has no scheme (expected e.g. http://host:port)",
                r.prefix,
                r.upstream
            ),
        }
        if uri.authority().is_none() {
            bail!(
                "route {:?}: upstream {:?} has no host",
                r.prefix,
                r.upstream
            );
        }
        if let Some(pq) = uri.path_and_query() {
            if pq.query().is_some() {
                bail!(
                    "route {:?}: upstream {:?} must not include a query string",
                    r.prefix,
                    r.upstream
                );
            }
            if !pq.path().is_empty() && pq.path() != "/" {
                bail!(
                    "route {:?}: upstream {:?} must not include a path (got {:?})",
                    r.prefix,
                    r.upstream,
                    pq.path()
                );
            }
        }

        let name = crate::route::upstream_name(&uri);
        let cooldown = r
            .cooldown_secs
            .map(Duration::from_secs)
            .unwrap_or(default_cooldown);
        if cooldown.is_zero() {
            bail!("route {:?}: cooldown_secs must be >= 1", r.prefix);
        }
        let first = *cooldown_by_name.entry(name.clone()).or_insert(cooldown);
        if first != cooldown {
            bail!(
                "route {:?}: upstream {name} is shared with another route but has a \
                 different cooldown ({}s vs {}s); routes to one upstream share one breaker",
                r.prefix,
                cooldown.as_secs(),
                first.as_secs()
            );
        }
        validated.push((r.prefix, uri, name, cooldown));
    }

    let mut upstreams_by_name: HashMap<String, Upstream> = HashMap::new();
    let routes = validated
        .into_iter()
        .map(|(prefix, uri, name, cooldown)| {
            let upstream = upstreams_by_name
                .entry(name)
                .or_insert_with_key(|name| match prev_by_name.get(name.as_str()) {
                    Some(prev_u) => prev_u.reuse(cooldown, cfg.failure_threshold),
                    None => Upstream::new(uri, cooldown, cfg.failure_threshold),
                })
                .clone();
            Route { prefix, upstream }
        })
        .collect();

    Ok(RouteTable::new(routes, upstream_timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{Admission, CircuitState};

    fn cfg(routes: Vec<RouteToml>) -> ConfigToml {
        ConfigToml {
            health_interval_secs: 5,
            default_cooldown_secs: 30,
            failure_threshold: 3,
            upstream_timeout_secs: 30,
            routes,
        }
    }

    fn route(prefix: &str, upstream: &str) -> RouteToml {
        RouteToml {
            prefix: prefix.to_string(),
            upstream: upstream.to_string(),
            cooldown_secs: None,
        }
    }

    #[test]
    fn rejects_prefix_without_leading_slash() {
        let c = cfg(vec![route("svc-a", "http://localhost:8001")]);
        assert!(build_table(c, None).is_err());
    }

    #[test]
    fn rejects_https_upstream() {
        let c = cfg(vec![route("/svc-a", "https://localhost:8001")]);
        let err = build_table(c, None).err().expect("should reject https");
        assert!(err.to_string().contains("https"), "{err}");
    }

    #[test]
    fn rejects_duplicate_prefix() {
        let c = cfg(vec![
            route("/svc-a", "http://localhost:8001"),
            route("/svc-a", "http://localhost:8002"),
        ]);
        assert!(build_table(c, None).is_err());
    }

    #[test]
    fn rejects_upstream_with_path() {
        let c = cfg(vec![route("/svc-a", "http://localhost:8001/api")]);
        assert!(build_table(c, None).is_err());
    }

    #[test]
    fn rejects_upstream_with_query() {
        let c = cfg(vec![route("/svc-a", "http://localhost:8001/?x=1")]);
        assert!(build_table(c, None).is_err());
    }

    #[test]
    fn rejects_zero_failure_threshold() {
        let mut c = cfg(vec![route("/svc-a", "http://localhost:8001")]);
        c.failure_threshold = 0;
        assert!(build_table(c, None).is_err());
    }

    #[test]
    fn accepts_valid_config() {
        let c = cfg(vec![
            route("/svc-a", "http://localhost:8001"),
            route("/svc-b", "http://localhost:8002"),
        ]);
        let t = build_table(c, None).unwrap();
        assert!(t.lookup("/svc-a").is_some());
        assert_eq!(t.upstreams().count(), 2);
    }

    #[test]
    fn reload_preserves_open_circuit() {
        let c = cfg(vec![route("/svc-a", "http://localhost:8001")]);
        let old = build_table(c.clone(), None).unwrap();
        let up = old.upstreams().next().unwrap();
        for _ in 0..3 {
            up.record_failure(Admission::Normal);
        }
        assert_eq!(up.state(), crate::route::CircuitState::Open);

        let new = build_table(c, Some(&old)).unwrap();
        let new_up = new.upstreams().next().unwrap();
        assert_eq!(new_up.state(), CircuitState::Open);

        // Same breaker, not a copy: a probe result landing on the old
        // table's upstream is visible through the new table.
        up.record_success(Admission::Probe);
        assert_eq!(new_up.state(), CircuitState::Closed);
    }

    #[test]
    fn failed_reload_leaves_prev_breakers_untouched() {
        let mut c = cfg(vec![route("/svc-a", "http://localhost:8001")]);
        c.default_cooldown_secs = 1000;
        let old = build_table(c.clone(), None).unwrap();
        let up = old.upstreams().next().unwrap();
        for _ in 0..3 {
            up.record_failure(Admission::Normal);
        }

        // Would shrink the cooldown, but the second route is invalid.
        c.default_cooldown_secs = 1;
        c.routes.push(route("nope", "http://localhost:8002"));
        assert!(build_table(c, Some(&old)).is_err());
        assert_eq!(up.try_acquire(), None, "cooldown must still be 1000s");
    }

    #[test]
    fn rejects_zero_durations() {
        for field in [
            "health_interval_secs",
            "upstream_timeout_secs",
            "default_cooldown_secs",
        ] {
            let raw =
                format!("{field} = 0\n[[routes]]\nprefix = \"/a\"\nupstream = \"http://h:1\"\n");
            let c: ConfigToml = toml::from_str(&raw).unwrap();
            let err = build_table(c, None).err().expect(field);
            assert!(format!("{err}").contains(field), "{err}");
        }
        let mut r = route("/a", "http://h:1");
        r.cooldown_secs = Some(0);
        assert!(build_table(cfg(vec![r]), None).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = "failure_treshold = 1\nroutes = []\n";
        assert!(toml::from_str::<ConfigToml>(raw).is_err());
        let raw = "[[routes]]\nprefix = \"/a\"\nupstream = \"http://h:1\"\ncooldown_sec = 5\n";
        assert!(toml::from_str::<ConfigToml>(raw).is_err());
    }

    #[test]
    fn rejects_conflicting_cooldowns_for_shared_upstream() {
        let mut b = route("/b", "http://localhost:8001");
        b.cooldown_secs = Some(5);
        let c = cfg(vec![route("/a", "http://localhost:8001"), b]);
        let err = build_table(c, None).err().expect("conflict");
        assert!(format!("{err}").contains("different cooldown"), "{err}");
    }

    #[test]
    fn reload_shares_state_across_routes_with_same_upstream() {
        let c = cfg(vec![
            route("/svc-a", "http://localhost:8001"),
            route("/svc-a-v2", "http://localhost:8001"),
        ]);
        let t = build_table(c, None).unwrap();
        assert_eq!(t.upstreams().count(), 1);
    }
}
