//! TOML config schema + reload-time builder.
//!
//! `config.toml` is the authoritative source for routing rules. The reload
//! task in `crates/server/src/reload.rs` re-reads this file on filesystem
//! events, calls [`build_table`], and atomically swaps the result into the
//! `SharedTable`.

use crate::error::Error;
use crate::proxies::TrustedProxies;
use crate::route::{Route, RouteTable, Upstream};
use crate::BreakerConfig;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Top-level config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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
    /// HTTP/1 keep-alive idle timeout in seconds (1..=86400).
    #[serde(default = "default_keepalive_timeout")]
    pub keepalive_timeout_secs: u64,
    /// CIDR ranges (or bare IPs) of proxies whose forwarding headers are trusted.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Longest gap between request-body frames, in seconds (1..=86400).
    #[serde(default = "default_body_idle_timeout")]
    pub request_body_idle_timeout_secs: u64,
    /// Total time allowed to receive a request body, in seconds (1..=86400).
    /// Not required to be >= the idle timeout; whichever fires first wins.
    #[serde(default = "default_body_timeout")]
    pub request_body_timeout_secs: u64,
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
pub(crate) const DEFAULT_KEEPALIVE_SECS: u64 = 10;
pub(crate) const DEFAULT_BODY_IDLE_SECS: u64 = 30;
pub(crate) const DEFAULT_BODY_TOTAL_SECS: u64 = 300;
fn default_body_timeout() -> u64 {
    DEFAULT_BODY_TOTAL_SECS
}
fn default_keepalive_timeout() -> u64 {
    DEFAULT_KEEPALIVE_SECS
}
fn default_body_idle_timeout() -> u64 {
    DEFAULT_BODY_IDLE_SECS
}

/// Upper bound for every duration key, so `Duration` arithmetic can't overflow.
const MAX_SECS: u64 = 86_400;

/// One routing rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RouteToml {
    /// Path prefix to match (e.g. `/svc-a`).
    pub prefix: String,
    /// Upstream URI, e.g. `http://localhost:8001`.
    pub upstream: String,
    /// Per-route circuit-breaker cooldown override.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    /// Active health-probe path; absolute, no query or fragment. Default
    /// `/health`. Routes sharing an upstream must agree.
    #[serde(default)]
    pub health_path: Option<String>,
    /// Skip active health probing for this route's upstream (default
    /// `false`); only request traffic then drives its breaker. Routes
    /// sharing an upstream must agree.
    #[serde(default)]
    pub health_disabled: bool,
}

/// Read and parse the TOML config at `path`. Shared by the server's initial
/// boot and its hot-reload watcher so both get the same error context.
///
/// # Errors
///
/// [`Error::ReadConfig`] if the file can't be read, [`Error::Toml`] if it
/// doesn't parse (both carry the cause as `source()`).
pub fn load_config(path: &Path) -> Result<ConfigToml, Error> {
    let raw = std::fs::read_to_string(path).map_err(|source| Error::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| Error::Toml {
        path: path.to_path_buf(),
        source,
    })
}

/// Build a [`RouteTable`] from a parsed [`ConfigToml`], validating routes
/// and upstream URIs. If `prev` is given, upstreams that also existed in it
/// (matched by `host:port` name) share its circuit breaker, so a hot reload
/// neither resets an open circuit nor strands results from in-flight
/// requests on an orphaned breaker. Fails without side effects.
///
/// # Errors
///
/// [`Error::InvalidConfig`] (out-of-range top-level key),
/// [`Error::InvalidCidr`] (`trusted_proxies`), [`Error::InvalidPrefix`],
/// [`Error::DuplicatePrefix`], [`Error::InvalidUpstream`],
/// [`Error::InvalidCooldown`], [`Error::ConflictingCooldown`],
/// [`Error::InvalidHealthPath`], [`Error::ConflictingHealth`] and
/// [`Error::InvalidBreakerConfig`].
pub fn build_table(cfg: ConfigToml, prev: Option<&RouteTable>) -> Result<RouteTable, Error> {
    if cfg.failure_threshold < 1 {
        return Err(Error::InvalidConfig {
            key: "failure_threshold",
            reason: format!("must be >= 1, got {}", cfg.failure_threshold),
        });
    }
    for (key, v) in [
        ("health_interval_secs", cfg.health_interval_secs),
        ("upstream_timeout_secs", cfg.upstream_timeout_secs),
        ("default_cooldown_secs", cfg.default_cooldown_secs),
        ("keepalive_timeout_secs", cfg.keepalive_timeout_secs),
        (
            "request_body_idle_timeout_secs",
            cfg.request_body_idle_timeout_secs,
        ),
        ("request_body_timeout_secs", cfg.request_body_timeout_secs),
    ] {
        if v == 0 {
            return Err(Error::InvalidConfig {
                key,
                reason: "must be >= 1".into(),
            });
        }
        if v > MAX_SECS {
            return Err(Error::InvalidConfig {
                key,
                reason: format!("must be <= {MAX_SECS}, got {v}"),
            });
        }
    }
    let trusted_proxies = TrustedProxies::parse(&cfg.trusted_proxies)?;

    let default_cooldown = Duration::from_secs(cfg.default_cooldown_secs);
    let upstream_timeout = Duration::from_secs(cfg.upstream_timeout_secs);

    let prev_by_name: HashMap<&str, &Upstream> = prev
        .map(|t| t.upstreams().map(|u| (u.name.as_str(), u)).collect())
        .unwrap_or_default();

    // Validate everything first so a rejected reload never touches the
    // breakers the live table is still using.
    let mut seen_prefixes = HashSet::new();
    let mut cooldown_by_name: HashMap<String, Duration> = HashMap::new();
    let mut health_by_name: HashMap<String, (String, bool)> = HashMap::new();
    let mut validated = Vec::with_capacity(cfg.routes.len());

    for r in cfg.routes {
        if !r.prefix.starts_with('/') {
            return Err(Error::InvalidPrefix { prefix: r.prefix });
        }
        if !seen_prefixes.insert(r.prefix.clone()) {
            return Err(Error::DuplicatePrefix { prefix: r.prefix });
        }

        let bad = |reason: String, source| Error::InvalidUpstream {
            route: r.prefix.clone(),
            upstream: r.upstream.clone(),
            reason,
            source,
        };
        let uri: http::Uri = r
            .upstream
            .parse()
            .map_err(|e| bad(format!("invalid upstream URI {:?}", r.upstream), Some(e)))?;

        match uri.scheme_str() {
            Some("http") => {}
            Some(other) => {
                return Err(bad(
                    format!(
                        "upstream {:?} has scheme {:?} — https upstreams are not supported",
                        r.upstream, other
                    ),
                    None,
                ))
            }
            None => {
                return Err(bad(
                    format!(
                        "upstream {:?} has no scheme (expected e.g. http://host:port)",
                        r.upstream
                    ),
                    None,
                ))
            }
        }
        // Also catches an empty IPv6 literal (`http://[]:80`).
        if uri
            .host()
            .is_none_or(|h| h.trim_matches(['[', ']']).is_empty())
        {
            return Err(bad(format!("upstream {:?} has no host", r.upstream), None));
        }
        if let Some(pq) = uri.path_and_query() {
            if pq.query().is_some() {
                return Err(bad(
                    format!("upstream {:?} must not include a query string", r.upstream),
                    None,
                ));
            }
            if !pq.path().is_empty() && pq.path() != "/" {
                return Err(bad(
                    format!(
                        "upstream {:?} must not include a path (got {:?})",
                        r.upstream,
                        pq.path()
                    ),
                    None,
                ));
            }
        }

        let name = crate::route::upstream_name(&uri);
        let cooldown = r
            .cooldown_secs
            .map(Duration::from_secs)
            .unwrap_or(default_cooldown);
        if cooldown.is_zero() {
            return Err(Error::InvalidCooldown {
                route: r.prefix,
                reason: "must be >= 1".into(),
            });
        }
        if cooldown.as_secs() > MAX_SECS {
            return Err(Error::InvalidCooldown {
                route: r.prefix,
                reason: format!("must be <= {MAX_SECS}, got {}", cooldown.as_secs()),
            });
        }
        // Validate the breaker config here, before `prev` is touched, so the
        // `Upstream::new` below cannot fail.
        let breaker = BreakerConfig::default()
            .with_cooldown(cooldown)
            .with_failure_threshold(cfg.failure_threshold);
        breaker.validate().map_err(|e| match e {
            Error::InvalidBreakerConfig { reason, .. } => Error::InvalidBreakerConfig {
                reason,
                upstream: Some(name.clone()),
            },
            e => e,
        })?;
        let first = *cooldown_by_name.entry(name.clone()).or_insert(cooldown);
        if first != cooldown {
            return Err(Error::ConflictingCooldown {
                route: r.prefix,
                upstream: name,
                cooldown_secs: cooldown.as_secs(),
                other_secs: first.as_secs(),
            });
        }
        if let Some(p) = &r.health_path {
            if !(p.starts_with('/')
                && !p.contains(['?', '#'])
                && p.parse::<http::uri::PathAndQuery>().is_ok())
            {
                return Err(Error::InvalidHealthPath {
                    route: r.prefix,
                    path: p.clone(),
                });
            }
        }
        let health = (
            r.health_path.unwrap_or_else(|| "/health".to_string()),
            r.health_disabled,
        );
        let first = health_by_name
            .entry(name.clone())
            .or_insert_with(|| health.clone());
        let conflict = if first.0 != health.0 {
            Some(("health_path", health.0.clone(), first.0.clone()))
        } else if first.1 != health.1 {
            Some(("health_disabled", health.1.to_string(), first.1.to_string()))
        } else {
            None
        };
        if let Some((setting, value, other)) = conflict {
            return Err(Error::ConflictingHealth {
                route: r.prefix,
                upstream: name,
                setting,
                value,
                other,
            });
        }
        validated.push((r.prefix, uri, name, cooldown, breaker, health));
    }

    let mut upstreams_by_name: HashMap<String, Upstream> = HashMap::new();
    let mut routes = Vec::with_capacity(validated.len());
    for (prefix, uri, name, cooldown, breaker, (health_path, health_disabled)) in validated {
        let upstream = match upstreams_by_name.get(&name) {
            Some(u) => u.clone(),
            None => {
                let health_path = Some(health_path);
                let u = match prev_by_name.get(name.as_str()) {
                    // Breaker (and its state) is kept; health settings are
                    // simply replaced on the new table's Upstream.
                    Some(prev_u) => prev_u.reuse(
                        cooldown,
                        cfg.failure_threshold,
                        health_path,
                        health_disabled,
                    ),
                    None => Upstream::new(uri, breaker)?.with_health(health_path, health_disabled),
                };
                upstreams_by_name.insert(name, u.clone());
                u
            }
        };
        routes.push(Route { prefix, upstream });
    }

    Ok(RouteTable::new(routes, upstream_timeout)
        .with_keepalive_timeout(Duration::from_secs(cfg.keepalive_timeout_secs))
        .with_request_body_idle_timeout(Duration::from_secs(cfg.request_body_idle_timeout_secs))
        .with_request_body_timeout(Duration::from_secs(cfg.request_body_timeout_secs))
        .with_trusted_proxies(trusted_proxies))
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
            keepalive_timeout_secs: 10,
            trusted_proxies: Vec::new(),
            request_body_idle_timeout_secs: 30,
            request_body_timeout_secs: 300,
            routes,
        }
    }

    fn route(prefix: &str, upstream: &str) -> RouteToml {
        RouteToml {
            prefix: prefix.to_string(),
            upstream: upstream.to_string(),
            cooldown_secs: None,
            health_path: None,
            health_disabled: false,
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

    fn parse(extra: &str) -> Result<RouteTable, Error> {
        let raw = format!("{extra}\n[[routes]]\nprefix = \"/a\"\nupstream = \"http://h:1\"\n");
        build_table(toml::from_str(&raw).unwrap(), None)
    }

    #[test]
    fn rejects_empty_upstream_host() {
        for up in ["http://:80", "http://", "http://[]:80", "http://@:80"] {
            let err = build_table(cfg(vec![route("/a", up)]), None)
                .err()
                .expect(up);
            assert!(
                err.to_string().contains("no host") || err.to_string().contains("invalid"),
                "{err}"
            );
        }
        let err = build_table(cfg(vec![route("/a", "http://:80")]), None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("has no host"), "{err}");
    }

    #[test]
    fn rejects_durations_above_one_day() {
        for key in [
            "health_interval_secs",
            "upstream_timeout_secs",
            "default_cooldown_secs",
            "keepalive_timeout_secs",
            "request_body_idle_timeout_secs",
            "request_body_timeout_secs",
        ] {
            assert!(parse(&format!("{key} = 86400")).is_ok(), "{key} at bound");
            let err = parse(&format!("{key} = 86401")).err().expect(key);
            assert!(err.to_string().contains(key), "{err}");
            assert!(parse(&format!("{key} = 9223372036854775807")).is_err());
        }
        let mut r = route("/a", "http://h:1");
        r.cooldown_secs = Some(86_401);
        let err = build_table(cfg(vec![r]), None).err().unwrap();
        assert!(err.to_string().contains("cooldown_secs"), "{err}");
    }

    #[test]
    fn new_keys_default_and_parse() {
        let t = parse("").unwrap();
        assert_eq!(t.keepalive_timeout(), Duration::from_secs(10));
        assert_eq!(t.request_body_idle_timeout(), Duration::from_secs(30));
        assert_eq!(t.request_body_timeout(), Duration::from_secs(300));
        assert!(t.trusted_proxies().is_empty());

        let t = parse(
            "keepalive_timeout_secs = 75\nrequest_body_idle_timeout_secs = 5\n\
             trusted_proxies = [\"10.0.0.0/8\", \"fd00::/8\"]",
        )
        .unwrap();
        assert_eq!(t.keepalive_timeout(), Duration::from_secs(75));
        assert_eq!(t.request_body_idle_timeout(), Duration::from_secs(5));
        assert!(t
            .trusted_proxies()
            .contains("::ffff:10.1.1.1".parse().unwrap()));
    }

    #[test]
    fn rejects_zero_new_keys_and_bad_cidr() {
        for key in [
            "keepalive_timeout_secs",
            "request_body_idle_timeout_secs",
            "request_body_timeout_secs",
        ] {
            assert!(parse(&format!("{key} = 0")).is_err());
        }
        let err = parse("trusted_proxies = [\"10.0.0.0/33\"]").err().unwrap();
        assert!(err.to_string().contains("10.0.0.0/33"), "{err}");
    }

    #[test]
    fn health_keys_default_and_parse() {
        let t = parse("").unwrap();
        let u = t.lookup("/a").unwrap().upstream.clone();
        assert_eq!((u.health_path(), u.health_disabled()), ("/health", false));

        let raw = "[[routes]]\nprefix = \"/a\"\nupstream = \"http://h:1\"\n\
                   health_path = \"/ready\"\nhealth_disabled = true\n";
        let t = build_table(toml::from_str(raw).unwrap(), None).unwrap();
        let u = &t.lookup("/a").unwrap().upstream;
        assert_eq!((u.health_path(), u.health_disabled()), ("/ready", true));
    }

    #[test]
    fn rejects_bad_health_path() {
        for bad in ["healthz", "", "/h?x=1", "/h#f", "/a b"] {
            let mut r = route("/a", "http://h:1");
            r.health_path = Some(bad.to_string());
            let r = build_table(cfg(vec![r]), None);
            assert!(
                matches!(r, Err(Error::InvalidHealthPath { .. })),
                "{bad:?}: {:?}",
                r.err()
            );
        }
    }

    #[test]
    fn rejects_conflicting_health_for_shared_upstream() {
        let mut b = route("/b", "http://localhost:8001");
        b.health_path = Some("/ready".into());
        let c = cfg(vec![route("/a", "http://localhost:8001"), b]);
        let err = build_table(c, None).err().expect("conflict");
        assert!(
            matches!(&err, Error::ConflictingHealth { route, setting: "health_path", .. } if route == "/b"),
            "{err}"
        );

        let mut b = route("/b", "http://localhost:8001");
        b.health_disabled = true;
        let c = cfg(vec![route("/a", "http://localhost:8001"), b]);
        assert!(matches!(
            build_table(c, None).err(),
            Some(Error::ConflictingHealth {
                setting: "health_disabled",
                ..
            })
        ));

        // Explicit default equals implicit default: no conflict.
        let mut b = route("/b", "http://localhost:8001");
        b.health_path = Some("/health".into());
        let c = cfg(vec![route("/a", "http://localhost:8001"), b]);
        assert!(build_table(c, None).is_ok());
    }

    #[test]
    fn reload_changing_health_keeps_breaker_state() {
        let c = cfg(vec![route("/a", "http://localhost:8001")]);
        let old = build_table(c.clone(), None).unwrap();
        let up = old.upstreams().next().unwrap();
        for _ in 0..3 {
            up.record_failure(Admission::Normal);
        }
        assert_eq!(up.state(), CircuitState::Open);

        let mut r = route("/a", "http://localhost:8001");
        r.health_path = Some("/ready".into());
        r.health_disabled = true;
        let new = build_table(cfg(vec![r]), Some(&old)).unwrap();
        let new_up = new.upstreams().next().unwrap();
        assert_eq!(new_up.state(), CircuitState::Open, "state must survive");
        assert_eq!(
            (new_up.health_path(), new_up.health_disabled()),
            ("/ready", true)
        );
        // Old table keeps its own settings; breaker is still shared.
        assert_eq!((up.health_path(), up.health_disabled()), ("/health", false));
        up.record_success(Admission::Probe);
        assert_eq!(new_up.state(), CircuitState::Closed);
    }
}
