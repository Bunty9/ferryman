//! Admin server on the metrics bind: `GET /metrics`, `/healthz` (alias
//! `/health`), `/readyz`. HTTP/1 only, with a header-read deadline so idle
//! sockets cannot pin file descriptors.
//!
//! Readiness only reflects whether this process is accepting traffic (it
//! flips to 503 once shutdown begins); it is deliberately not tied to
//! upstream health, so a dead upstream never takes the proxy out of rotation.

use http::{header, HeaderValue, Method, Response, StatusCode};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Time a client has to send a request head (also bounds idle keep-alive).
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

fn respond(status: StatusCode, ctype: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    r
}

fn route(
    req: &Request<Incoming>,
    metrics: &PrometheusHandle,
    draining: &AtomicBool,
) -> Response<Full<Bytes>> {
    const TEXT: &str = "text/plain; charset=utf-8";
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return respond(
            StatusCode::METHOD_NOT_ALLOWED,
            TEXT,
            "method not allowed\n".into(),
        );
    }
    match req.uri().path() {
        "/metrics" => respond(
            StatusCode::OK,
            "text/plain; version=0.0.4",
            metrics.render(),
        ),
        "/healthz" | "/health" => respond(StatusCode::OK, TEXT, "ok".into()),
        "/readyz" if draining.load(Ordering::Relaxed) => {
            respond(StatusCode::SERVICE_UNAVAILABLE, TEXT, "draining".into())
        }
        "/readyz" => respond(StatusCode::OK, TEXT, "ok".into()),
        _ => respond(StatusCode::NOT_FOUND, TEXT, "not found\n".into()),
    }
}

/// Serve the admin endpoints on `listener`; runs until the returned future
/// is dropped (which also stops the histogram upkeep it drives every 5 s).
/// `draining` is set by the caller when shutdown begins (`/readyz` then
/// returns 503).
///
/// Note: `PrometheusHandle` is a `metrics-exporter-prometheus` 0.16 type, so
/// this signature is coupled to that crate's version.
pub async fn serve_admin(
    listener: TcpListener,
    metrics: PrometheusHandle,
    draining: Arc<AtomicBool>,
) {
    let upkeep = {
        let metrics = metrics.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                metrics.run_upkeep();
            }
        }
    };
    let accept = async {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(?e, "admin accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (metrics, draining) = (metrics.clone(), draining.clone());
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let r = route(&req, &metrics, &draining);
                    async move { Ok::<_, Infallible>(r) }
                });
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    };
    tokio::select! {
        _ = upkeep => {}
        _ = accept => {}
    }
}
