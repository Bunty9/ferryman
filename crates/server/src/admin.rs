//! Admin server on the metrics bind: `GET /metrics`, `/healthz`, `/readyz`.
//!
//! Readiness only reflects whether this process is accepting traffic (it
//! flips to 503 once shutdown begins); it is deliberately not tied to
//! upstream health, so a dead upstream never takes the proxy out of rotation.

use http::{header, Method, Response, StatusCode};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use metrics_exporter_prometheus::PrometheusHandle;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn respond(status: StatusCode, ctype: &str, body: String) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, ctype.parse().unwrap());
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
        "/healthz" => respond(StatusCode::OK, TEXT, "ok".into()),
        "/readyz" if draining.load(Ordering::Relaxed) => {
            respond(StatusCode::SERVICE_UNAVAILABLE, TEXT, "draining".into())
        }
        "/readyz" => respond(StatusCode::OK, TEXT, "ok".into()),
        _ => respond(StatusCode::NOT_FOUND, TEXT, "not found\n".into()),
    }
}

/// Serve the admin endpoints on `listener` until the future is dropped.
/// `draining` is set by the caller when shutdown begins (`/readyz` then
/// returns 503). Also runs the recorder's histogram upkeep every 5 s, as the
/// exporter's own listener used to.
pub async fn serve_admin(
    listener: TcpListener,
    metrics: PrometheusHandle,
    draining: Arc<AtomicBool>,
) {
    let upkeep = metrics.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let (metrics, draining, builder) = (metrics.clone(), draining.clone(), builder.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let r = route(&req, &metrics, &draining);
                async move { Ok::<_, Infallible>(r) }
            });
            let _ = builder.serve_connection(TokioIo::new(stream), svc).await;
        });
    }
}
