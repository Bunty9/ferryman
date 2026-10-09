//! Routing table + per-upstream circuit breaker.
//!
//! Lookup is O(N) over a `Vec` sorted DESC by prefix length so the most
//! specific match wins (e.g. `/api/v1/users` beats `/api`). For N in the
//! tens of routes typical of a self-hosted edge, a linear scan beats a trie
//! on cache behaviour and is trivial to reason about.

pub use crate::breaker::{Admission, CircuitState};
use crate::breaker::{Breaker, BreakerConfig};
use crate::proxies::TrustedProxies;
use crate::Error;
use arc_swap::ArcSwap;
use std::borrow::Cow;
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
    health_path: Option<String>,
    health_disabled: bool,
}

impl Upstream {
    /// Create an upstream with its own closed breaker. Fails with
    /// [`Error::InvalidBreakerConfig`] if `breaker` has a cooldown under
    /// 1 ms or a zero threshold. The breaker is labelled with the upstream's
    /// `host:port` name (`breaker.name` is ignored) and writes the
    /// `ferryman_*` gauges.
    pub fn new(uri: http::Uri, breaker: BreakerConfig) -> Result<Self, Error> {
        let name = upstream_name(&uri);
        let breaker = Arc::new(Breaker::for_upstream(name.clone(), &breaker)?);
        Ok(Self {
            uri,
            name,
            breaker,
            health_path: None,
            health_disabled: false,
        })
    }

    /// Set the health-probe path (`None` = `/health`) and whether active
    /// probing is disabled. Does not touch the breaker.
    pub fn with_health(mut self, path: Option<String>, disabled: bool) -> Self {
        self.health_path = path;
        self.health_disabled = disabled;
        self
    }

    /// Health-probe path; `/health` unless configured.
    pub fn health_path(&self) -> &str {
        self.health_path.as_deref().unwrap_or("/health")
    }

    /// Whether the health loop skips this upstream (only requests drive its
    /// breaker).
    pub fn health_disabled(&self) -> bool {
        self.health_disabled
    }

    /// May this request be sent to the upstream right now? Returns the
    /// ticket to pass back to `record_*`; see [`Admission`].
    pub fn try_acquire(&self) -> Option<Admission> {
        self.breaker.try_acquire()
    }

    /// See [`Breaker::release`].
    pub fn release(&self, admission: Admission) {
        self.breaker.release(admission)
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

    /// Same upstream, same breaker (state kept), new config and health
    /// settings. Used by hot reload so in-flight requests and health probes
    /// holding the old table keep reporting to the breaker the new table
    /// uses. Health settings are plain fields on the returned value, so the
    /// old table keeps its own and nothing is shared mutably.
    pub(crate) fn reuse(
        &self,
        cooldown: Duration,
        failure_threshold: u32,
        health_path: Option<String>,
        health_disabled: bool,
    ) -> Self {
        self.breaker.reconfigure(cooldown, failure_threshold);
        self.clone().with_health(health_path, health_disabled)
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
#[non_exhaustive]
pub struct Route {
    pub prefix: String,
    pub upstream: Upstream,
    /// `true`: send the upstream's own authority as `Host`. `false` (default):
    /// keep the client's `Host` (or the request authority, see the proxy docs).
    pub rewrite_host: bool,
}

impl Route {
    /// A route with `rewrite_host` off.
    pub fn new(prefix: impl Into<String>, upstream: Upstream) -> Self {
        Self {
            prefix: prefix.into(),
            upstream,
            rewrite_host: false,
        }
    }

    /// Set [`Route::rewrite_host`].
    pub fn with_rewrite_host(mut self, rewrite: bool) -> Self {
        self.rewrite_host = rewrite;
        self
    }
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
    keepalive_timeout: Duration,
    request_body_idle_timeout: Duration,
    request_body_timeout: Duration,
    drain_timeout: Duration,
    shutdown_delay: Duration,
    trusted_proxies: TrustedProxies,
    local_health_path: Option<String>,
}

impl RouteTable {
    /// Does not validate prefixes; `build_table` does (they must already be in
    /// normalised form, or they can never match).
    pub fn new(mut routes: Vec<Route>, upstream_timeout: Duration) -> Self {
        routes.sort_by_key(|r| std::cmp::Reverse(r.prefix.len()));
        Self {
            routes,
            upstream_timeout,
            keepalive_timeout: Duration::from_secs(crate::config::DEFAULT_KEEPALIVE_SECS),
            request_body_idle_timeout: Duration::from_secs(crate::config::DEFAULT_BODY_IDLE_SECS),
            request_body_timeout: Duration::from_secs(crate::config::DEFAULT_BODY_TOTAL_SECS),
            drain_timeout: Duration::from_secs(crate::config::DEFAULT_DRAIN_SECS),
            shutdown_delay: Duration::ZERO,
            trusted_proxies: TrustedProxies::default(),
            local_health_path: None,
        }
    }

    /// Set the path the proxy answers itself (config `local_health_path`).
    pub fn with_local_health_path(mut self, p: Option<String>) -> Self {
        self.local_health_path = p;
        self
    }

    /// Path answered locally with `200 ok` instead of being routed.
    pub fn local_health_path(&self) -> Option<&str> {
        self.local_health_path.as_deref()
    }

    /// Set the HTTP/1 keep-alive idle timeout (config `keepalive_timeout_secs`;
    /// default 10 s).
    pub fn with_keepalive_timeout(mut self, d: Duration) -> Self {
        self.keepalive_timeout = d;
        self
    }

    /// Set the longest gap between request-body frames (config
    /// `request_body_idle_timeout_secs`; default 30 s).
    pub fn with_request_body_idle_timeout(mut self, d: Duration) -> Self {
        self.request_body_idle_timeout = d;
        self
    }

    /// Set the total time allowed to receive a request body (config
    /// `request_body_timeout_secs`; default 300 s).
    pub fn with_request_body_timeout(mut self, d: Duration) -> Self {
        self.request_body_timeout = d;
        self
    }

    /// Set the shutdown drain budget (config `drain_timeout_secs`; default 25 s).
    pub fn with_drain_timeout(mut self, d: Duration) -> Self {
        self.drain_timeout = d;
        self
    }

    /// Set how long to keep serving after shutdown starts, before the
    /// listener closes (config `shutdown_delay_secs`; default 0).
    pub fn with_shutdown_delay(mut self, d: Duration) -> Self {
        self.shutdown_delay = d;
        self
    }

    /// Longest wait for in-flight connections once the listener has closed.
    pub fn drain_timeout(&self) -> Duration {
        self.drain_timeout
    }

    /// Time to keep accepting (with readiness failing) after shutdown starts.
    ///
    /// `ferryman::serve` does NOT apply this: an embedder does it inside its
    /// shutdown future (flip readiness, then
    /// `tokio::time::sleep(table.load().shutdown_delay())`, then resolve).
    pub fn shutdown_delay(&self) -> Duration {
        self.shutdown_delay
    }

    /// Set the trusted proxy ranges (config `trusted_proxies`; default empty).
    pub fn with_trusted_proxies(mut self, t: TrustedProxies) -> Self {
        self.trusted_proxies = t;
        self
    }

    /// HTTP/1 keep-alive idle timeout.
    pub fn keepalive_timeout(&self) -> Duration {
        self.keepalive_timeout
    }

    /// Longest gap between request-body frames before a stalled upload is dropped.
    pub fn request_body_idle_timeout(&self) -> Duration {
        self.request_body_idle_timeout
    }

    /// Total time a client may take to send its request body.
    pub fn request_body_timeout(&self) -> Duration {
        self.request_body_timeout
    }

    /// Peers whose forwarding headers may be trusted.
    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.trusted_proxies
    }

    /// Longest prefix that matches `path` on a path-segment boundary: prefix
    /// `/svc-a` matches `/svc-a`, `/svc-a/`, `/svc-a/x`, but not `/svc-ab`.
    /// A prefix ending in `/` matches anything starting with it (including
    /// `/` itself, which acts as a catch-all).
    ///
    /// Pass the RAW request path: matching runs on a normalised copy
    /// (`%XX` of unreserved chars decoded, other escapes' hex uppercased,
    /// repeated `/` merged; case-sensitive, `%2f` stays encoded), so
    /// `/%61pi/x` and `//api/x` match `/api`. The caller still forwards the
    /// raw path. Normalisation CAN produce dot segments (`%2e%2e` becomes
    /// `..`); it is safe only because the caller (the proxy's `bad_path`)
    /// rejects those on the raw path before calling `lookup`. Normalisation
    /// runs exactly once and is not idempotent for malformed escapes
    /// (`/%2%61` becomes `/%2a`).
    pub fn lookup(&self, path: &str) -> Option<&Route> {
        let path = normalize(path);
        self.routes
            .iter()
            .find(|r| prefix_matches(&r.prefix, &path))
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

fn hex_val(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

fn is_unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

/// What to do at `b[i]`: `Some(n)` = consume `n` bytes and emit the
/// normalised form, `None` = already normal.
fn rewrite_at(b: &[u8], i: usize) -> Option<(usize, [u8; 3], usize)> {
    match b[i] {
        b'/' if b.get(i + 1) == Some(&b'/') => Some((1, [0; 3], 0)),
        b'%' => {
            let (h, l) = (hex_val(*b.get(i + 1)?)?, hex_val(*b.get(i + 2)?)?);
            let c = h << 4 | l;
            if is_unreserved(c) {
                Some((3, [c, 0, 0], 1))
            } else if b[i + 1].is_ascii_lowercase() || b[i + 2].is_ascii_lowercase() {
                Some((
                    3,
                    [
                        b'%',
                        b[i + 1].to_ascii_uppercase(),
                        b[i + 2].to_ascii_uppercase(),
                    ],
                    3,
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Matching-only normalisation; borrows when the path is already normal.
pub(crate) fn normalize(path: &str) -> Cow<'_, str> {
    if !path.contains('%') && !path.contains("//") {
        return Cow::Borrowed(path);
    }
    let b = path.as_bytes();
    let Some(first) = (0..b.len()).find(|&i| rewrite_at(b, i).is_some()) else {
        return Cow::Borrowed(path);
    };
    let mut out = Vec::with_capacity(b.len());
    out.extend_from_slice(&b[..first]);
    let mut i = first;
    while i < b.len() {
        match rewrite_at(b, i) {
            Some((n, bytes, len)) => {
                out.extend_from_slice(&bytes[..len]);
                i += n;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    // Only ASCII bytes were substituted, so this is always valid UTF-8.
    Cow::Owned(String::from_utf8(out).expect("only ASCII substituted"))
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
        Upstream::new("http://localhost:8001".parse().unwrap(), Default::default()).unwrap()
    }

    fn table(prefixes: &[&str]) -> RouteTable {
        let routes = prefixes
            .iter()
            .map(|p| Route::new(*p, upstream()))
            .collect();
        RouteTable::new(routes, Duration::from_secs(30))
    }

    fn route_of<'a>(t: &'a RouteTable, p: &str) -> Option<&'a str> {
        t.lookup(p).map(|r| r.prefix.as_str())
    }

    #[test]
    fn matches_on_normalised_path() {
        let t = table(&["/", "/api", "/~x"]);
        assert_eq!(route_of(&t, "/%61pi/x"), Some("/api"));
        assert_eq!(route_of(&t, "//api/x"), Some("/api"));
        assert_eq!(route_of(&t, "/api//x"), Some("/api"));
        assert_eq!(route_of(&t, "///api"), Some("/api"));
        assert_eq!(route_of(&t, "/%7ex"), Some("/~x"));
        assert_eq!(route_of(&t, "/%7Ex/y"), Some("/~x"));
        assert_eq!(route_of(&t, "/API/x"), Some("/"));
        assert_eq!(route_of(&t, "/%41pi/x"), Some("/"));
        assert_eq!(route_of(&t, "/api%2fx"), Some("/"));
        assert_eq!(route_of(&t, "/api%2Fx"), Some("/"));
        assert_eq!(route_of(&t, "/apix"), Some("/"));
    }

    #[test]
    fn normalize_cases() {
        for (raw, want) in [
            ("/a%2fb", "/a%2Fb"),
            ("/a%2Fb", "/a%2Fb"),
            ("/%61%2f//b", "/a%2F/b"),
            ("/a%zz", "/a%zz"),
            ("/a%6", "/a%6"),
            ("/a%e4%b8%ad", "/a%E4%B8%AD"),
        ] {
            assert_eq!(normalize(raw), want, "{raw}");
        }
        for p in ["/", "/api/x", "/a%2Fb", "/a%zz"] {
            assert!(matches!(normalize(p), Cow::Borrowed(_)), "{p}");
        }
    }

    /// Normalisation must not turn a path `bad_path` accepted into one with a
    /// dot segment: `%2e` decodes to `.`, but `bad_path` rejects `%2e`-only
    /// segments first, and a segment like `%2e%2e` never reaches lookup.
    #[test]
    fn decoding_dots_only_happens_inside_longer_segments() {
        // `%2e` inside a longer segment stays a harmless filename.
        assert_eq!(normalize("/api/a%2eb"), "/api/a.b");
        // A pure-dot segment would be rejected by the proxy before lookup;
        // lookup alone does not create a *new* one beyond what was there.
        assert_eq!(normalize("/api/%2e%2e/x"), "/api/../x");
    }

    #[test]
    fn escaped_prefix_and_trailing_slash_prefix() {
        let t = table(&["/", "/a%2Fb", "/v/"]);
        assert_eq!(route_of(&t, "/a%2fb"), Some("/a%2Fb"));
        assert_eq!(route_of(&t, "/a%2Fb/x"), Some("/a%2Fb"));
        assert_eq!(route_of(&t, "/v//x"), Some("/v/"));
        assert_eq!(route_of(&t, "/v/a//x"), Some("/v/"));
        let t = table(&["/api/", "/api/v1/"]);
        assert_eq!(route_of(&t, "/api//"), Some("/api/"));
        assert_eq!(route_of(&t, "/api/v1//x"), Some("/api/v1/"));
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
            Route::new("/a", shared.clone()),
            Route::new("/b", shared.clone()),
        ];
        let t = RouteTable::new(routes, Duration::from_secs(30));
        assert_eq!(t.upstreams().count(), 1);
    }
}
