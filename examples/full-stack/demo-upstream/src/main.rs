//! `demo-upstream` — a fake backend used by the full-stack reference example
//! (three instances: users, orders, orders-v2 sit behind the ferryman proxy).
//!
//! Binds `0.0.0.0:$PORT` (default `8080`), reads its name from
//! `$SERVICE_NAME` (default `demo`), and stamps every response with
//! `x-served-by: $SERVICE_NAME`. ferryman forwards the full request path
//! unchanged (no prefix stripping), so dispatch below matches on the path's
//! *final* segment(s) rather than a fixed prefix:
//!
//! | Request (path suffix)                  | Response                                            |
//! |-----------------------------------------|------------------------------------------------------|
//! | exactly `/health`                        | 200 `ok`, or 503 `failing` while fail mode is on      |
//! | `…/admin/fail?on=1` / `?on=0`             | 200, turns fail mode on/off (always reachable)        |
//! | anything else while fail mode is on       | 503 `failing`                                          |
//! | `…/slow?ms=N`                             | sleeps N ms (capped at 60000), then 200 `slow done`    |
//! | `…/stream?chunks=N&interval_ms=M`         | 200 chunked, N lines `chunk i\n`, M ms apart (N ≤ 100) |
//! | `…/status?code=NNN`                       | responds with NNN (100–599, else 400)                  |
//! | anything else (`/echo`, `/`, …)           | 200 JSON echo of the request; body drained, not buffered |

use http::{HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

/// What a request maps to, decided purely from the path/query/fail-mode —
/// no I/O, so it's cheap to test exhaustively.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Fail(bool),
    Failing,
    Health,
    Slow(u64),
    Stream { chunks: u32, interval_ms: u64 },
    Status(u16),
    Echo,
    BadRequest(&'static str),
}

/// Pure dispatch: decides what a request means before anything touches I/O.
///
/// Precedence: `…/admin/fail` always wins (reachable even in fail mode);
/// then, while `failing` is set, everything else becomes `Failing`
/// (including `/health`); only then does the contract table apply.
fn route(path: &str, query: Option<&str>, failing: bool) -> Action {
    if path.ends_with("/admin/fail") {
        return Action::Fail(query_param(query, "on") == Some("1"));
    }

    if failing {
        return Action::Failing;
    }

    if path == "/health" {
        return Action::Health;
    }

    if path.ends_with("/slow") {
        return match query_param(query, "ms").and_then(|v| v.parse::<u64>().ok()) {
            Some(ms) => Action::Slow(ms.min(60_000)),
            None => Action::BadRequest("ms must be a non-negative integer"),
        };
    }

    if path.ends_with("/stream") {
        let chunks = query_param(query, "chunks").and_then(|v| v.parse::<u32>().ok());
        let interval_ms = query_param(query, "interval_ms")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        return match chunks {
            Some(chunks) if chunks <= 100 => Action::Stream {
                chunks,
                interval_ms,
            },
            _ => Action::BadRequest("chunks must be 0..=100"),
        };
    }

    if path.ends_with("/status") {
        return match query_param(query, "code").and_then(|v| v.parse::<u16>().ok()) {
            Some(code) if (100..=599).contains(&code) => Action::Status(code),
            _ => Action::BadRequest("code must be 100..=599"),
        };
    }

    Action::Echo
}

/// Looks up `key` in a raw (unescaped) `a=1&b=2` query string.
fn query_param<'q>(query: Option<&'q str>, key: &str) -> Option<&'q str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// A response body backed by an mpsc channel, so `/stream` can send chunks
/// as they're produced instead of buffering the whole response.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(Frame::data(b))))
    }
}

/// Wraps a byte body as the one response body type every branch returns.
fn full(body: impl Into<Bytes>) -> BoxBody<Bytes, Infallible> {
    Full::new(body.into()).boxed()
}

fn text_response(status: StatusCode, body: &'static str) -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(status)
        .body(full(body))
        .expect("static status + body is always a valid response")
}

fn stream_response(chunks: u32, interval_ms: u64) -> Response<BoxBody<Bytes, Infallible>> {
    // Small buffer: the sender backpressures on a slow/stalled client
    // instead of piling chunks up in memory.
    let (tx, rx) = mpsc::channel::<Bytes>(4);
    tokio::spawn(async move {
        for i in 0..chunks {
            if tx.send(Bytes::from(format!("chunk {i}\n"))).await.is_err() {
                break; // receiver (client) gone
            }
            if interval_ms > 0 {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        }
    });
    Response::new(BoxBody::new(ChannelBody(rx)))
}

/// Drains and counts the request body without buffering it, then echoes the
/// request back as JSON.
async fn echo_response(
    service_name: &str,
    method: &Method,
    path: &str,
    query: Option<&str>,
    req: Request<Incoming>,
) -> Response<BoxBody<Bytes, Infallible>> {
    let headers: serde_json::Map<String, serde_json::Value> = req
        .headers()
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().unwrap_or("").to_string();
            (name.to_string(), serde_json::Value::String(value))
        })
        .collect();

    let mut body_bytes: u64 = 0;
    let mut body = req.into_body();
    while let Some(frame) = body.frame().await {
        if let Ok(frame) = frame {
            if let Some(data) = frame.data_ref() {
                body_bytes += data.len() as u64;
            }
        }
    }

    let json = serde_json::json!({
        "service": service_name,
        "method": method.as_str(),
        "path": path,
        "query": query,
        "headers": headers,
        "body_bytes": body_bytes,
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(full(json.to_string()))
        .expect("json body is always a valid response")
}

async fn handle(
    req: Request<Incoming>,
    service_name: Arc<str>,
    failing: Arc<AtomicBool>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    let action = route(&path, query.as_deref(), failing.load(Ordering::Relaxed));

    let mut response = match action {
        Action::Fail(on) => {
            failing.store(on, Ordering::Relaxed);
            text_response(StatusCode::OK, "ok")
        }
        Action::Failing => text_response(StatusCode::SERVICE_UNAVAILABLE, "failing"),
        Action::Health => text_response(StatusCode::OK, "ok"),
        Action::Slow(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            text_response(StatusCode::OK, "slow done")
        }
        Action::Stream {
            chunks,
            interval_ms,
        } => stream_response(chunks, interval_ms),
        Action::Status(code) => Response::builder()
            .status(StatusCode::from_u16(code).expect("route validated 100..=599"))
            .body(full(Bytes::new()))
            .expect("valid status + empty body is always a valid response"),
        Action::Echo => echo_response(&service_name, &method, &path, query.as_deref(), req).await,
        Action::BadRequest(msg) => text_response(StatusCode::BAD_REQUEST, msg),
    };

    response.headers_mut().insert(
        HeaderName::from_static("x-served-by"),
        HeaderValue::from_str(&service_name).expect("service name is a valid header value"),
    );
    Ok(response)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let service_name: Arc<str> = std::env::var("SERVICE_NAME")
        .unwrap_or_else(|_| "demo".to_string())
        .into();
    let failing = Arc::new(AtomicBool::new(false));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    eprintln!("{service_name}: listening on {addr}");

    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                eprintln!("{service_name}: received SIGTERM, shutting down");
                break;
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("{service_name}: accept failed: {e}");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let service_name = service_name.clone();
                let failing = failing.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| handle(req, service_name.clone(), failing.clone()));
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        eprintln!("connection error: {e}");
                    }
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_parses_ms() {
        assert_eq!(
            route("/api/users/slow", Some("ms=10"), false),
            Action::Slow(10)
        );
    }

    #[test]
    fn slow_caps_at_60s() {
        assert_eq!(
            route("/api/users/slow", Some("ms=999999"), false),
            Action::Slow(60_000)
        );
    }

    #[test]
    fn health_ok() {
        assert_eq!(route("/health", None, false), Action::Health);
    }

    #[test]
    fn health_fails_in_fail_mode() {
        assert_eq!(route("/health", None, true), Action::Failing);
    }

    #[test]
    fn admin_fail_always_reachable_even_while_failing() {
        assert_eq!(
            route("/x/admin/fail", Some("on=1"), true),
            Action::Fail(true)
        );
    }

    #[test]
    fn admin_fail_off() {
        assert_eq!(
            route("/x/admin/fail", Some("on=0"), false),
            Action::Fail(false)
        );
    }

    #[test]
    fn status_out_of_range_is_bad_request() {
        assert!(matches!(
            route("/x/status", Some("code=42"), false),
            Action::BadRequest(_)
        ));
    }

    #[test]
    fn status_in_range() {
        assert_eq!(
            route("/x/status", Some("code=503"), false),
            Action::Status(503)
        );
    }

    #[test]
    fn stream_too_many_chunks_is_bad_request() {
        assert!(matches!(
            route("/x/stream", Some("chunks=500"), false),
            Action::BadRequest(_)
        ));
    }

    #[test]
    fn stream_within_limit() {
        assert_eq!(
            route("/x/stream", Some("chunks=3&interval_ms=200"), false),
            Action::Stream {
                chunks: 3,
                interval_ms: 200
            }
        );
    }

    #[test]
    fn unmatched_path_is_echo() {
        assert_eq!(route("/api/users/x", None, false), Action::Echo);
    }

    #[test]
    fn failing_mode_blocks_everything_but_admin() {
        assert_eq!(route("/api/orders/x", None, true), Action::Failing);
    }
}
