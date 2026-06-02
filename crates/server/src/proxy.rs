//! Per-request handler. Looks up the routing table, rebuilds the URI for
//! the chosen upstream, forwards via the shared hyper client, records
//! metrics, and turns transport errors into circuit-breaker trips.

use ferryman_core::SharedTable;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use std::sync::atomic::Ordering;

pub type Body = Full<Bytes>;

/// Handle a single inbound request.
///
/// On no matching route: 404. On a routable match but transport error
/// (upstream down, DNS failure, etc.): 502 plus a circuit-breaker trip
/// (`Upstream::mark_failed`). On success: forwards the upstream status +
/// body unchanged.
pub async fn handle(
    table: SharedTable,
    client: Client<HttpConnector, Body>,
    req: Request<Incoming>,
) -> Result<Response<Body>, anyhow::Error> {
    let started = std::time::Instant::now();
    let snapshot = table.load();
    let path = req.uri().path().to_string();

    let upstream = match snapshot.lookup(&path) {
        Some(u) => u.clone(),
        None => {
            metrics::counter!(
                "ferryman_requests_total",
                "status" => "404",
                "route" => "none"
            )
            .increment(1);
            return Ok(Response::builder()
                .status(404)
                .body(Body::new(Bytes::from_static(b"no route")))?);
        }
    };

    // Rebuild URI: upstream scheme+authority + original path+query.
    let (mut parts, body) = req.into_parts();
    let mut up_parts = upstream.uri.clone().into_parts();
    up_parts.path_and_query = parts.uri.path_and_query().cloned();
    parts.uri = http::Uri::from_parts(up_parts)?;
    let bytes = body.collect().await?.to_bytes();
    let fwd = Request::from_parts(parts, Body::new(bytes));

    let host = upstream.uri.host().unwrap_or("").to_string();
    match client.request(fwd).await {
        Ok(resp) => {
            upstream.alive.store(true, Ordering::Relaxed); // recovery
            let status = resp.status().as_u16();
            let body = resp.into_body().collect().await?.to_bytes();
            metrics::histogram!(
                "ferryman_request_duration_seconds",
                "upstream" => host
            )
            .record(started.elapsed().as_secs_f64());
            metrics::counter!(
                "ferryman_requests_total",
                "status" => status.to_string()
            )
            .increment(1);
            Ok(Response::builder().status(status).body(Body::new(body))?)
        }
        Err(e) => {
            upstream.mark_failed();
            metrics::counter!(
                "ferryman_requests_total",
                "status" => "502"
            )
            .increment(1);
            let msg = format!("upstream: {e}");
            Ok(Response::builder()
                .status(502)
                .body(Body::new(Bytes::from(msg)))?)
        }
    }
}
