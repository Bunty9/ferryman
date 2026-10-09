//! Active health checker. Probes every upstream's health path (default
//! `/health`; upstreams with `health_disabled` are skipped entirely: no
//! probe, no breaker effect) concurrently on
//! a fixed interval, so one slow upstream doesn't delay the others, and
//! records the result through the circuit breaker.

use crate::route::SharedTable;
use crate::route::{Admission, Upstream};
use std::time::Duration;
use tokio::task::JoinSet;

/// Runs forever. Cancel by aborting the spawned task.
pub async fn health_loop(table: SharedTable, interval: Duration) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        // A 3xx (e.g. an http->https redirect) already proves the upstream
        // is up; following it would fail for lack of TLS here.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(?e, "failed to build health-check reqwest client");
            return;
        }
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let current = table.load_full();
        // Refresh every tick: gauges for upstreams dropped by a reload stop
        // being written and expire from the exporter (see main.rs).
        current.publish_gauges();
        let mut probes = JoinSet::new();
        for up in current.upstreams().filter(|u| !u.health_disabled()) {
            let client = client.clone();
            let up = up.clone();
            probes.spawn(async move { probe_one(&client, &up).await });
        }
        while probes.join_next().await.is_some() {}
    }
}

async fn probe_one(client: &reqwest::Client, up: &Upstream) {
    let url = health_url(&up.uri, up.health_path());
    // Health checks are authoritative, like the half-open probe: a healthy
    // answer closes an open circuit at once (fast failover recovery).
    let status = client.get(&url).send().await.map(|r| r.status());
    if is_healthy(status.as_ref().ok()) {
        up.record_success(Admission::Probe);
    } else {
        up.record_failure(Admission::Probe);
    }
}

/// Any answer below 500 means the process is up. Many upstreams have no
/// `/health` route and answer 404; only transport errors, timeouts and 5xx
/// count as failures.
fn is_healthy(status: Option<&reqwest::StatusCode>) -> bool {
    status.is_some_and(|s| !s.is_server_error())
}

/// Build the probe URL (`path` is e.g. `/health`) from an upstream URI. `http::Uri`'s
/// `Display` impl adds a trailing `/` even for a bare `http://host:port`
/// (e.g. it prints `http://host:1/`), so naively appending `/health` would
/// produce a double slash. Reassembling from scheme+authority avoids that.
fn health_url(uri: &http::Uri, path: &str) -> String {
    let scheme = uri.scheme_str().unwrap_or("http");
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or("");
    format!("{scheme}://{authority}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_status_mapping() {
        use reqwest::StatusCode as S;
        assert!(is_healthy(Some(&S::OK)));
        assert!(is_healthy(Some(&S::NOT_FOUND)));
        assert!(!is_healthy(Some(&S::SERVICE_UNAVAILABLE)));
        assert!(!is_healthy(None));
    }

    #[test]
    fn health_url_has_no_double_slash() {
        let uri: http::Uri = "http://localhost:8001".parse().unwrap();
        assert_eq!(health_url(&uri, "/health"), "http://localhost:8001/health");
    }

    #[test]
    fn health_url_uses_custom_path() {
        let uri: http::Uri = "http://localhost:8001".parse().unwrap();
        assert_eq!(health_url(&uri, "/ready"), "http://localhost:8001/ready");
    }

    /// Accepts connections, records the request line, answers `status`.
    async fn stub(
        status: u16,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut c, _) = l.accept().await.unwrap();
                let s2 = s2.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = c.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    s2.lock()
                        .unwrap()
                        .push(req.lines().next().unwrap_or("").to_string());
                    let _ = c
                        .write_all(
                            format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .await;
                });
            }
        });
        (addr, seen)
    }

    fn one_route_table(
        addr: std::net::SocketAddr,
        path: Option<&str>,
        disabled: bool,
    ) -> SharedTable {
        let up = Upstream::new(
            format!("http://{addr}").parse().unwrap(),
            crate::BreakerConfig::default().with_failure_threshold(1),
        )
        .unwrap()
        .with_health(path.map(String::from), disabled);
        let t = crate::route::RouteTable::new(
            vec![crate::route::Route {
                prefix: "/".into(),
                upstream: up,
            }],
            Duration::from_secs(5),
        );
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(t))
    }

    #[tokio::test]
    async fn disabled_upstream_is_not_probed_and_enabled_uses_custom_path() {
        // 500 stub: a probe would open the (threshold 1) circuit.
        let (addr, seen) = stub(500).await;
        let off = one_route_table(addr, None, true);
        let h = tokio::spawn(health_loop(off.clone(), Duration::from_millis(30)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.abort();
        assert!(
            seen.lock().unwrap().is_empty(),
            "disabled must not be probed"
        );
        assert_eq!(
            off.load().lookup("/").unwrap().upstream.state(),
            crate::CircuitState::Closed
        );

        let on = one_route_table(addr, Some("/ready"), false);
        let h = tokio::spawn(health_loop(on.clone(), Duration::from_millis(30)));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while on.load().lookup("/").unwrap().upstream.state() != crate::CircuitState::Open
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        h.abort();
        let seen = seen.lock().unwrap();
        assert!(
            !seen.is_empty() && seen.iter().all(|l| l.starts_with("GET /ready ")),
            "{seen:?}"
        );
        assert_eq!(
            on.load().lookup("/").unwrap().upstream.state(),
            crate::CircuitState::Open
        );
    }

    #[test]
    fn health_url_ignores_original_path() {
        // build_table rejects a non-"/" path on config load, but health_url
        // itself should still never double up regardless of input shape.
        let uri: http::Uri = "http://localhost:8001/".parse().unwrap();
        assert_eq!(health_url(&uri, "/health"), "http://localhost:8001/health");
    }
}
