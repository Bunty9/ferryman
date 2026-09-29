//! This crate's own tiny admin HTTP server: `/healthz`, `/status`,
//! `/metrics`. Hand-rolled with raw hyper (the same building blocks
//! `ferryman::serve` itself uses — see crates/server/src/lib.rs) rather than
//! pulling in a web framework: a reference example should stick to
//! dependencies the rest of this repo already needs, and an admin surface
//! this small doesn't need one anyway.
//!
//! Ferryman's own binary doesn't offer any of these three routes as a
//! single family — `/metrics` is a separate exporter-owned listener, and
//! there's no `/healthz` or `/status` at all. An app embedding ferryman
//! gets to decide that; this is one reasonable answer.

use ferryman_core::{CircuitState, SharedTable};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Accept connections on `listener` and serve the admin routes until
/// `shutdown` carries `true`. Mirrors the shape of `ferryman::serve`'s
/// accept loop (crates/server/src/lib.rs), minus the TLS/graceful-drain
/// machinery that loop needs and this one doesn't: admin traffic is
/// operator/scraper-driven and short-lived, not internet-facing, so
/// dropping an in-flight `/metrics` scrape on shutdown is an acceptable
/// simplification for a reference example (a production admin surface
/// might want the same graceful-drain treatment as the proxy).
pub(crate) async fn run(
    listener: TcpListener,
    table: SharedTable,
    metrics: PrometheusHandle,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.wait_for(|&v| v) => return,
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(?e, "admin accept failed");
                        continue;
                    }
                };
                let table = table.clone();
                let metrics = metrics.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let table = table.clone();
                        let metrics = metrics.clone();
                        async move { Ok::<_, std::convert::Infallible>(handle(&table, &metrics, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        }
    }
}

async fn handle(
    table: &SharedTable,
    metrics: &PrometheusHandle,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => text(StatusCode::OK, "ok"),
        (&Method::GET, "/status") => status_json(table),
        (&Method::GET, "/metrics") => metrics_response(metrics),
        _ => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// `{"upstreams":[{"name":"host:port","uri":"http://host:port/","state":"closed"|"open"|"half_open"}]}`
fn status_json(table: &SharedTable) -> Response<Full<Bytes>> {
    let current = table.load();
    let upstreams: Vec<_> = current
        .upstreams()
        .map(|u| {
            json!({
                "name": u.name,
                // http::Uri's Display always prints a trailing `/`, even for
                // a bare `http://host:port` with no path — see the same note
                // in crates/core/src/health.rs::health_url.
                "uri": u.uri.to_string(),
                "state": state_str(u.state()),
            })
        })
        .collect();
    let body =
        serde_json::to_vec(&json!({ "upstreams": upstreams })).expect("json! output serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("status response is well-formed")
}

fn state_str(state: CircuitState) -> &'static str {
    match state {
        CircuitState::Closed => "closed",
        CircuitState::Open => "open",
        CircuitState::HalfOpen => "half_open",
    }
}

fn metrics_response(metrics: &PrometheusHandle) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        // The Prometheus text exposition format's registered content type;
        // scrapers (and `promtool check metrics`) key off this.
        .header("content-type", "text/plain; version=0.0.4")
        .body(Full::new(Bytes::from(metrics.render())))
        .expect("metrics response is well-formed")
}

fn text(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("text response is well-formed")
}
