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
/// (matched by `host:port` name) reuse their circuit breaker's runtime
/// state, so a hot reload doesn't reset an open circuit.
pub fn build_table(cfg: ConfigToml, prev: Option<&RouteTable>) -> anyhow::Result<RouteTable> {
    if cfg.failure_threshold < 1 {
        bail!(
            "failure_threshold must be >= 1, got {}",
            cfg.failure_threshold
        );
    }

    let default_cooldown = Duration::from_secs(cfg.default_cooldown_secs);
    let upstream_timeout = Duration::from_secs(cfg.upstream_timeout_secs);

    let prev_by_name: HashMap<&str, &Upstream> = prev
        .map(|t| t.upstreams().map(|u| (u.name.as_str(), u)).collect())
        .unwrap_or_default();

    let mut seen_prefixes = HashSet::new();
    let mut upstreams_by_name: HashMap<String, Upstream> = HashMap::new();
    let mut routes = Vec::with_capacity(cfg.routes.len());

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

        let upstream = upstreams_by_name
            .entry(name.clone())
            .or_insert_with(|| {
                let u = Upstream::new(uri.clone(), cooldown, cfg.failure_threshold);
                if let Some(prev_u) = prev_by_name.get(name.as_str()) {
                    u.adopt_state(prev_u);
                }
                u
            })
            .clone();

        routes.push(Route {
            prefix: r.prefix,
            upstream,
        });
    }

    Ok(RouteTable::new(routes, upstream_timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        up.record_failure();
        up.record_failure();
        up.record_failure();
        assert_eq!(up.state(), crate::route::CircuitState::Open);

        let new = build_table(c, Some(&old)).unwrap();
        let new_up = new.upstreams().next().unwrap();
        assert_eq!(new_up.state(), crate::route::CircuitState::Open);
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
