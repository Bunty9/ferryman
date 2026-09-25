//! Active health checker. Probes every upstream's `/health` concurrently on
//! a fixed interval, so one slow upstream doesn't delay the others, and
//! records the result through the circuit breaker.

use crate::route::SharedTable;
use crate::route::Upstream;
use std::time::Duration;
use tokio::task::JoinSet;

/// Runs forever. Cancel by aborting the spawned task.
pub async fn health_loop(table: SharedTable, interval: Duration) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
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
    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => up.record_success(),
        _ => up.record_failure(),
    }
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
