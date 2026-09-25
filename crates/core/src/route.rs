//! Routing table + per-upstream circuit breaker.
//!
//! Lookup is O(N) over a `Vec` sorted DESC by prefix length so the most
//! specific match wins (e.g. `/api/v1/users` beats `/api`). For N in the
//! tens of routes typical of a self-hosted edge, a linear scan beats a trie
//! on cache behaviour and is trivial to reason about.

use crate::breaker::Breaker;
pub use crate::breaker::{Admission, CircuitState};
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Duration;

/// A backend the proxy may forward to. Cheap to clone (all shared state is
/// behind `Arc`), so a matched `Upstream` can be pulled out of the routing
/// table and carried across an `await` point.
#[derive(Clone)]
pub struct Upstream {
    pub uri: http::Uri,
    /// `host:port` — used as the `upstream` metric label. Port defaults from
    /// the scheme (80 for http) when the URI doesn't specify one.
    pub name: String,
    breaker: Arc<Breaker>,
}

impl Upstream {
    pub fn new(uri: http::Uri, cooldown: Duration, failure_threshold: u32) -> Self {
        let name = upstream_name(&uri);
        let breaker = Arc::new(Breaker::new(name.clone(), cooldown, failure_threshold));
        Self { uri, name, breaker }
    }

    /// May this request be sent to the upstream right now? Returns the
    /// ticket to pass back to `record_*`; see [`Admission`].
    pub fn try_acquire(&self) -> Option<Admission> {
        self.breaker.try_acquire()
    }

    pub fn record_success(&self, admission: Admission) {
        self.breaker.record_success(admission)
    }

    pub fn record_failure(&self, admission: Admission) {
        self.breaker.record_failure(admission)
    }

    pub fn state(&self) -> CircuitState {
        self.breaker.state()
    }

    /// Same upstream, same breaker, new config. Used by hot reload so
    /// in-flight requests and health probes holding the old table keep
    /// reporting to the breaker the new table uses.
    pub(crate) fn reuse(&self, cooldown: Duration, failure_threshold: u32) -> Self {
        self.breaker.reconfigure(cooldown, failure_threshold);
        self.clone()
    }
}

pub(crate) fn upstream_name(uri: &http::Uri) -> String {
    // Hostnames are case-insensitive; one name per backend, one breaker.
    let host = uri.host().unwrap_or("").to_ascii_lowercase();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });
    format!("{host}:{port}")
}

/// One routing rule: a path prefix and the upstream it forwards to.
pub struct Route {
    pub prefix: String,
    pub upstream: Upstream,
}

/// Routing decisions table. `routes` is sorted DESC by prefix length at
/// construction time so iteration order is the lookup order.
///
/// Lookup deliberately ignores upstream health — falling back to a shorter,
/// less specific prefix would route the request to the wrong service. It's
/// up to the caller to check [`Upstream::try_acquire`] and return 503 when
/// the breaker refuses.
pub struct RouteTable {
    routes: Vec<Route>,
    pub upstream_timeout: Duration,
}

impl RouteTable {
    pub fn new(mut routes: Vec<Route>, upstream_timeout: Duration) -> Self {
        routes.sort_by_key(|r| std::cmp::Reverse(r.prefix.len()));
        Self {
            routes,
            upstream_timeout,
        }
    }

    /// Longest prefix that matches `path` on a path-segment boundary: prefix
    /// `/svc-a` matches `/svc-a`, `/svc-a/`, `/svc-a/x`, but not `/svc-ab`.
    /// A prefix ending in `/` matches anything starting with it (including
    /// `/` itself, which acts as a catch-all).
    pub fn lookup(&self, path: &str) -> Option<&Route> {
        self.routes.iter().find(|r| prefix_matches(&r.prefix, path))
    }

    /// Upstreams referenced by this table, deduplicated by name — routes
    /// sharing an upstream URI share one `Upstream`/breaker.
    pub fn upstreams(&self) -> impl Iterator<Item = &Upstream> {
        let mut seen = std::collections::HashSet::new();
        self.routes
            .iter()
            .filter_map(move |r| seen.insert(r.upstream.name.clone()).then_some(&r.upstream))
    }

    /// Set `ferryman_circuit_state` / `ferryman_upstream_alive` for every
    /// upstream. Call after installing a table (and after the metrics
    /// recorder is installed) so healthy upstreams are exported too.
    pub fn publish_gauges(&self) {
        for u in self.upstreams() {
            u.breaker.set_gauges();
        }
    }
}

fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix.ends_with('/') {
        return path.starts_with(prefix);
    }
    if !path.starts_with(prefix) {
        return false;
    }
    matches!(path.as_bytes().get(prefix.len()), None | Some(b'/'))
}

/// Hot-swappable handle. Cloning is cheap (Arc bump); `load()` is a single
/// atomic read.
pub type SharedTable = Arc<ArcSwap<RouteTable>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream() -> Upstream {
        Upstream::new(
            "http://localhost:8001".parse().unwrap(),
            Duration::from_secs(30),
            3,
        )
    }

    fn table(prefixes: &[&str]) -> RouteTable {
        let routes = prefixes
            .iter()
            .map(|p| Route {
                prefix: p.to_string(),
                upstream: upstream(),
            })
            .collect();
        RouteTable::new(routes, Duration::from_secs(30))
    }

    #[test]
    fn segment_boundary_matches() {
        let t = table(&["/svc-a"]);
        assert!(t.lookup("/svc-a").is_some());
        assert!(t.lookup("/svc-a/").is_some());
        assert!(t.lookup("/svc-a/x").is_some());
        assert!(t.lookup("/svc-ab").is_none());
        assert!(t.lookup("/other").is_none());
    }

    #[test]
    fn longest_prefix_wins() {
        let t = table(&["/svc-a", "/svc-a/v2"]);
        let r = t.lookup("/svc-a/v2/users").unwrap();
        assert_eq!(r.prefix, "/svc-a/v2");
        let r = t.lookup("/svc-a/v1/users").unwrap();
        assert_eq!(r.prefix, "/svc-a");
    }

    #[test]
    fn root_catch_all() {
        let t = table(&["/svc-a", "/"]);
        assert_eq!(t.lookup("/svc-a").unwrap().prefix, "/svc-a");
        assert_eq!(t.lookup("/anything/else").unwrap().prefix, "/");
    }

    #[test]
    fn trailing_slash_prefix_is_plain_starts_with() {
        let t = table(&["/svc-a/"]);
        assert!(t.lookup("/svc-a/x").is_some());
        assert!(t.lookup("/svc-a").is_none());
    }

    #[test]
    fn upstreams_deduped_by_name() {
        let shared = upstream();
        let routes = vec![
            Route {
                prefix: "/a".to_string(),
                upstream: shared.clone(),
            },
            Route {
                prefix: "/b".to_string(),
                upstream: shared.clone(),
            },
        ];
        let t = RouteTable::new(routes, Duration::from_secs(30));
        assert_eq!(t.upstreams().count(), 1);
    }
}
