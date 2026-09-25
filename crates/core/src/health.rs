//! Active health checker. Probes every upstream's `/health` concurrently on
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
        for up in current.upstreams() {
            let client = client.clone();
            let up = up.clone();
            probes.spawn(async move { probe_one(&client, &up).await });
        }
        while probes.join_next().await.is_some() {}
    }
}

async fn probe_one(client: &reqwest::Client, up: &Upstream) {
    let url = health_url(&up.uri);
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

/// Build the `/health` probe URL from an upstream URI. `http::Uri`'s
/// `Display` impl adds a trailing `/` even for a bare `http://host:port`
/// (e.g. it prints `http://host:1/`), so naively appending `/health` would
/// produce a double slash. Reassembling from scheme+authority avoids that.
fn health_url(uri: &http::Uri) -> String {
    let scheme = uri.scheme_str().unwrap_or("http");
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or("");
    format!("{scheme}://{authority}/health")
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
        assert_eq!(health_url(&uri), "http://localhost:8001/health");
    }

    #[test]
    fn health_url_ignores_original_path() {
        // build_table rejects a non-"/" path on config load, but health_url
        // itself should still never double up regardless of input shape.
        let uri: http::Uri = "http://localhost:8001/".parse().unwrap();
        assert_eq!(health_url(&uri), "http://localhost:8001/health");
    }
}
