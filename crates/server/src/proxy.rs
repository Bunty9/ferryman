//! Per-request handler. Looks up the routing table, rebuilds the URI for
//! the chosen upstream, streams the request straight through via the shared
//! hyper client, records metrics, and turns transport failures into
//! circuit-breaker trips.

use ferryman_core::{SharedTable, TrustedProxies};
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Incoming};
use hyper::{Request, Response};
use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::Sleep;

#[allow(deprecated)]
use crate::ProxyClient;
use crate::StreamingClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;

/// One response body type for both streamed upstream responses and locally
/// generated ones (404/502/503/504), so `handle`'s return type doesn't leak
/// the streaming vs. buffered distinction to callers.
pub type ResponseBody = BoxBody<Bytes, hyper::Error>;

/// The client's request body, streamed to the upstream with two additions:
/// it enforces `request_body_idle_timeout_secs` between frames and
/// `request_body_timeout_secs` overall (an over-slow upload errors, which
/// `handle_streaming` turns into 408), and it reports end-of-stream so `handle_streaming` can
/// start the upstream timeout only once the whole body is sent. Never buffers.
pub struct RequestBody {
    inner: Incoming,
    idle: Duration,
    deadline: tokio::time::Instant,
    // Armed only while waiting on the client, so a slow upstream connect or
    // an upstream that is not reading is never blamed on the client.
    idle_sleep: Option<Pin<Box<Sleep>>>,
    total_sleep: Option<Pin<Box<Sleep>>>,
    eos: Option<oneshot::Sender<()>>,
    state: Arc<BodyState>,
}

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestBody").finish_non_exhaustive()
    }
}

/// What `handle`'s timer can see of the body: whether hyper polled it, and
/// whether the last poll was left waiting on the client (as opposed to hyper
/// having stopped reading, e.g. an upstream whose receive buffer is full).
#[derive(Default)]
struct BodyState {
    polled: AtomicBool,
    waiting_on_client: AtomicBool,
}

struct Eos {
    rx: oneshot::Receiver<()>,
    state: Arc<BodyState>,
}

/// The client stalled its upload for longer than `request_body_idle_timeout_secs`.
#[derive(Debug)]
pub(crate) struct BodyIdleTimeout;

impl std::fmt::Display for BodyIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body idle timeout")
    }
}
impl std::error::Error for BodyIdleTimeout {}

/// The client took longer than `request_body_timeout_secs` to send its body.
#[derive(Debug)]
pub(crate) struct BodyTotalTimeout;

impl std::fmt::Display for BodyTotalTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body total timeout")
    }
}
impl std::error::Error for BodyTotalTimeout {}

impl RequestBody {
    fn new(inner: Incoming, idle: Duration, total: Duration) -> (Self, Eos) {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(BodyState::default());
        let mut b = Self {
            inner,
            idle,
            deadline: tokio::time::Instant::now() + total,
            idle_sleep: None,
            total_sleep: None,
            eos: Some(tx),
            state: state.clone(),
        };
        b.signal_if_done();
        (b, Eos { rx, state })
    }

    fn signal_if_done(&mut self) {
        if self.inner.is_end_stream() {
            if let Some(tx) = self.eos.take() {
                let _ = tx.send(());
            }
        }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        this.state.polled.store(true, Ordering::Relaxed);
        if tokio::time::Instant::now() >= this.deadline {
            return Poll::Ready(Some(Err(BodyTotalTimeout.into())));
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.idle_sleep = None;
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                // The last frame of a content-length body may not be followed
                // by another poll, so check here as well as on `None`.
                this.signal_if_done();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(None) => {
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                if let Some(tx) = this.eos.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(None)
            }
            Poll::Pending => {
                this.state.waiting_on_client.store(true, Ordering::Relaxed);
                let idle = this.idle;
                let idle_sleep = this
                    .idle_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(idle)));
                if idle_sleep.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err(BodyIdleTimeout.into())));
                }
                let deadline = this.deadline;
                let total_sleep = this
                    .total_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
                if total_sleep.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err(BodyTotalTimeout.into())));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Headers that are meaningful only for one hop and must never be forwarded,
/// per RFC 7230 §6.1 (plus `keep-alive`, which predates the RFC but is
/// conventionally hop-by-hop too).
const HOP_BY_HOP_HEADERS: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Handle a single inbound request. Never returns `Err` — every expected
/// failure (no route, breaker open, timeout, transport error) becomes an
/// error response instead, per hyper service conventions.
///
/// `proto` is `"http"` or `"https"`, reflecting whether this connection was
/// TLS-terminated, and is forwarded as `x-forwarded-proto`.
///
/// This is the streaming handler: `upstream_timeout_secs` starts once the
/// request body is complete, `request_body_idle_timeout_secs` applies between
/// body frames, and `request_body_timeout_secs` caps the whole upload.
pub async fn handle_streaming(
    table: SharedTable,
    client: StreamingClient,
    peer: SocketAddr,
    proto: &'static str,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    let wrap = |body, idle, total| {
        let (b, eos) = RequestBody::new(body, idle, total);
        (b, Some(eos))
    };
    handle_inner(table, client, peer, proto, req, wrap).await
}

/// The 0.2.2 handler, kept for compatibility: `upstream_timeout_secs` covers
/// the whole request including the body upload, no body idle timeout, and a
/// timeout counts against the breaker only for bodyless requests. It shares
/// the core with `handle_streaming`, so it still gets the forwarded-header and
/// bad-path fixes.
#[deprecated(
    since = "0.2.3",
    note = "long uploads can 504; use `handle_streaming` with a `StreamingClient`"
)]
#[allow(deprecated)]
pub async fn handle(
    table: SharedTable,
    client: ProxyClient,
    peer: SocketAddr,
    proto: &'static str,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    handle_inner(table, client, peer, proto, req, |b, _, _| (b, None)).await
}

/// Shared core. `wrap` turns the inbound body into the client's body type and
/// optionally returns an end-of-body signal; without one (legacy), the
/// upstream timeout starts immediately and only counts for bodyless requests.
async fn handle_inner<B>(
    table: SharedTable,
    client: Client<HttpConnector, B>,
    peer: SocketAddr,
    proto: &'static str,
    req: Request<Incoming>,
    wrap: impl FnOnce(Incoming, Duration, Duration) -> (B, Option<Eos>),
) -> Result<Response<ResponseBody>, Infallible>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let started = Instant::now();
    // `load_full` (not `load`) because the returned `Arc` is held across
    // `.await` points below; `load`'s guard is meant for short critical
    // sections only.
    let table = table.load_full();

    if ferryman_core::path::bad_path(req.uri().path())
        || ferryman_core::path::ambiguous_route(&table, req.uri().path())
    {
        record(started, "none", "none", 400);
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
    }

    let Some(route) = table.lookup(req.uri().path()) else {
        record(started, "none", "none", 404);
        return Ok(error_response(StatusCode::NOT_FOUND, "no route"));
    };
    let route_label = route.prefix.clone();
    let upstream = route.upstream.clone();
    let rewrite_host = route.rewrite_host;

    // Upgrades (WebSocket etc.) need both hops spliced together, which this
    // proxy doesn't do; say so instead of forwarding a mangled plain GET.
    // `h2c` is exempt: servers may ignore it (RFC 9110 §7.8), and clients
    // like curl --http2 or Java's HttpClient send it on every plain request.
    let wants_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"h2c"));
    if wants_upgrade || req.method() == http::Method::CONNECT {
        record(started, &route_label, &upstream.name, 501);
        return Ok(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "protocol upgrades are not supported",
        ));
    }

    let Some(admission) = upstream.try_acquire() else {
        record(started, &route_label, &upstream.name, 503);
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream unavailable",
        ));
    };

    let (mut parts, body) = req.into_parts();
    // `upstream_timeout` starts at end-of-request-body (immediately when
    // bodyless), so a slow upload can't be mistaken for a slow upstream.
    let bodyless = body.is_end_stream();
    let (body, body_done) = wrap(
        body,
        table.request_body_idle_timeout(),
        table.request_body_timeout(),
    );
    let counts_timeout = bodyless || body_done.is_some();
    strip_hop_by_hop(&mut parts.headers);
    if parts.version == http::Version::HTTP_2 {
        join_cookies(&mut parts.headers);
    }
    // Default: an authority in the request URI (HTTP/2 `:authority`, or an
    // HTTP/1 absolute-form target) overrides `Host` (RFC 9112 §3.2.2). Pin it
    // before the URI is rewritten so upstreams see the client's host either
    // way. With `rewrite_host`, the upstream's own authority wins over both.
    let authority = if rewrite_host {
        upstream.uri.authority()
    } else {
        parts.uri.authority()
    };
    if let Some(v) = authority.and_then(|a| {
        let host = match a.port() {
            Some(port) => format!("{}:{port}", a.host()),
            None => a.host().to_string(),
        };
        HeaderValue::from_str(&host).ok()
    }) {
        parts.headers.insert(header::HOST, v);
    }

    let mut up_parts = upstream.uri.clone().into_parts();
    up_parts.path_and_query = parts.uri.path_and_query().cloned();
    parts.uri = match http::Uri::from_parts(up_parts) {
        Ok(uri) => uri,
        Err(e) => {
            upstream.release(admission);
            record(started, &route_label, &upstream.name, 502);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("rebuilding upstream URI: {e}"),
            ));
        }
    };
    // The legacy hyper client speaks HTTP/1.1 to upstreams regardless of
    // what the inbound connection negotiated; an inbound HTTP/2 request
    // otherwise fails the client outright.
    parts.version = http::Version::HTTP_11;
    apply_forwarded_headers(
        &mut parts.headers,
        peer.ip(),
        proto,
        table.trusted_proxies(),
    );

    let fwd = Request::from_parts(parts, body);

    let upstream_timeout = table.upstream_timeout;
    // Completes when the request must be failed as an upstream timeout (504).
    let timer = async move {
        let Some(Eos { mut rx, state }) = body_done else {
            // Legacy: the whole request, body included, is under one timeout.
            return tokio::time::sleep(upstream_timeout).await;
        };
        loop {
            tokio::select! {
                r = &mut rx => {
                    // Sender dropped without EOS (body failed): the request
                    // future reports that; never time out the upstream for it.
                    if r.is_err() {
                        std::future::pending::<()>().await;
                    }
                    break;
                }
                // hyper stopped reading the body (upstream not draining it)
                // for a whole window while not waiting on the client: that is
                // the upstream's doing. A client that is merely slow is left
                // to the body's own idle/total deadlines (408).
                () = tokio::time::sleep(upstream_timeout) => {
                    if !state.polled.swap(false, Ordering::Relaxed)
                        && !state.waiting_on_client.load(Ordering::Relaxed)
                    {
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(upstream_timeout).await;
    };
    let result = tokio::select! {
        r = client.request(fwd) => Some(r),
        () = timer => None,
    };
    match result {
        None => {
            // With EOS tracking the body is fully sent, so this is the
            // upstream's fault; the legacy path can't tell for bodies.
            if counts_timeout {
                upstream.record_failure(admission);
            }
            record(started, &route_label, &upstream.name, 504);
            Ok(error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream timeout",
            ))
        }
        Some(Err(e)) if is_body_idle_timeout(&e) => {
            // The client stalled or over-ran its upload: 408, no breaker effect.
            upstream.release(admission);
            tracing::debug!("client request body idle timeout");
            record(started, &route_label, &upstream.name, 408);
            Ok(error_response(
                StatusCode::REQUEST_TIMEOUT,
                "request body timeout",
            ))
        }
        Some(Err(e)) if is_client_body_error(&e) => {
            // The client's request body failed (e.g. it hung up mid-upload).
            // Not the upstream's fault, so the breaker stays out of it.
            upstream.release(admission);
            tracing::debug!(error = %e, "client request body failed");
            record(started, &route_label, &upstream.name, 400);
            Ok(error_response(
                StatusCode::BAD_REQUEST,
                "request body error",
            ))
        }
        Some(Err(e)) => {
            upstream.record_failure(admission);
            tracing::warn!(upstream = %upstream.name, error = %e, "upstream request failed");
            record(started, &route_label, &upstream.name, 502);
            Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"))
        }
        Some(Ok(resp)) => {
            let status = resp.status();
            // Health is judged on the response head; the body is streamed
            // afterwards with no timeout of its own.
            if matches!(status.as_u16(), 502..=504) {
                upstream.record_failure(admission);
            } else {
                upstream.record_success(admission);
            }
            record(started, &route_label, &upstream.name, status.as_u16());

            let (mut rparts, rbody) = resp.into_parts();
            strip_hop_by_hop(&mut rparts.headers);
            Ok(Response::from_parts(rparts, rbody.boxed()))
        }
    }
}

/// Remove hop-by-hop headers: the fixed RFC 7230 set, plus any header named
/// in the `Connection` header (which may list additional per-message
/// hop-by-hop headers on top of the fixed set).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| HeaderName::from_bytes(s.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in &HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// Did `client.request` fail because the client stalled its upload?
fn is_body_idle_timeout(e: &hyper_util::client::legacy::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        if err.is::<BodyIdleTimeout>() || err.is::<BodyTotalTimeout>() {
            return true;
        }
        source = err.source();
    }
    false
}

/// Did `client.request` fail because reading the *client's* body failed?
/// hyper reports errors from the outgoing body as user errors.
fn is_client_body_error(e: &hyper_util::client::legacy::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        if err
            .downcast_ref::<hyper::Error>()
            .is_some_and(|h| h.is_user())
        {
            return true;
        }
        source = err.source();
    }
    false
}

/// HTTP/2 lets clients split `cookie` into several fields; HTTP/1.1
/// upstreams expect one (RFC 9113 §8.2.3), so join them with `"; "`.
fn join_cookies(headers: &mut HeaderMap) {
    if headers.get_all(header::COOKIE).iter().count() < 2 {
        return;
    }
    let joined = headers
        .get_all(header::COOKIE)
        .iter()
        .map(|v| v.as_bytes())
        .collect::<Vec<_>>()
        .join(&b"; "[..]);
    if let Ok(v) = HeaderValue::from_bytes(&joined) {
        headers.insert(header::COOKIE, v);
    }
}

/// Decide the forwarding headers the upstream sees (pure; unit-tested).
///
/// Untrusted peer (every peer when `trusted_proxies` is empty): `x-forwarded-proto`
/// is set from the connection, `x-forwarded-for` gets the peer appended,
/// `x-real-ip` is overwritten with the peer, and client `forwarded` /
/// `x-forwarded-host` are stripped.
///
/// Trusted peer: incoming `x-forwarded-proto` (rightmost value), `x-forwarded-host`
/// and `forwarded` are kept; missing `x-forwarded-proto` comes from the
/// connection. `x-real-ip` is always overwritten with the rightmost
/// `x-forwarded-for` entry that is not a trusted proxy (`:port` suffixes
/// tolerated; an unparsable entry stops the walk), else the peer.
/// `x-forwarded-for` always gets the peer appended.
fn apply_forwarded_headers(
    headers: &mut HeaderMap,
    peer: IpAddr,
    proto: &'static str,
    trusted: &TrustedProxies,
) {
    let xfp = HeaderName::from_static("x-forwarded-proto");
    let peer_trusted = trusted.contains(peer);
    if peer_trusted {
        // Rightmost non-empty value across all lines (the hop closest to us
        // wrote it); anything that isn't a scheme token falls back to the
        // connection's proto.
        let scheme = headers
            .get_all(&xfp)
            .iter()
            .flat_map(|v| v.to_str().unwrap_or("!").split(','))
            .map(str::trim)
            .rfind(|v| !v.is_empty())
            .filter(|v| {
                v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            })
            .and_then(|v| HeaderValue::from_str(v).ok());
        headers.insert(&xfp, scheme.unwrap_or(HeaderValue::from_static(proto)));
        // Always derived, never taken from the client: cloud LBs pass a
        // client's X-Real-IP through untouched.
        // A header line that isn't visible ASCII counts as one garbage entry.
        let entries: Vec<Option<IpAddr>> = headers
            .get_all("x-forwarded-for")
            .iter()
            .flat_map(|v| match v.to_str() {
                Ok(v) => v.split(',').map(|e| parse_xff_entry(e.trim())).collect(),
                Err(_) => vec![None],
            })
            .collect();
        let client = entries
            .into_iter()
            .rev()
            .find(|e| !matches!(e, Some(ip) if trusted.contains(*ip)))
            .flatten()
            .unwrap_or(peer);
        set_real_ip(headers, client);
    } else {
        headers.insert(&xfp, HeaderValue::from_static(proto));
        headers.remove(header::FORWARDED);
        headers.remove("x-forwarded-host");
        set_real_ip(headers, peer);
    }
    append_forwarded_for(headers, peer);
}

/// Overwrite `x-real-ip` (all instances) with `ip`, v4-mapped shown as v4.
fn set_real_ip(headers: &mut HeaderMap, ip: IpAddr) {
    if let Ok(v) = HeaderValue::from_str(&ip.to_canonical().to_string()) {
        headers.insert("x-real-ip", v);
    }
}

/// One `X-Forwarded-For` entry: `ip`, `ip:port` (v4) or `[v6]` / `[v6]:port`.
fn parse_xff_entry(e: &str) -> Option<IpAddr> {
    e.parse::<IpAddr>()
        .ok()
        .or_else(|| e.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| e.strip_prefix('[')?.strip_suffix(']')?.parse().ok())
}

fn append_forwarded_for(headers: &mut HeaderMap, ip: IpAddr) {
    let name = HeaderName::from_static("x-forwarded-for");
    let existing: Vec<&str> = headers
        .get_all(&name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let value = if existing.is_empty() {
        ip.to_string()
    } else {
        format!("{}, {ip}", existing.join(", "))
    };
    if let Ok(v) = HeaderValue::from_str(&value) {
        headers.insert(name, v);
    }
}

fn record(started: Instant, route: &str, upstream: &str, status: u16) {
    metrics::counter!(
        "ferryman_requests_total",
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
        "status" => status.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "ferryman_request_duration_seconds",
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
    )
    .record(started.elapsed().as_secs_f64());
}

fn error_response(status: StatusCode, body: impl Into<Bytes>) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(full_body(body))
        .expect("error response with fixed headers is always valid")
}

fn full_body(body: impl Into<Bytes>) -> ResponseBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(c: &[&str]) -> TrustedProxies {
        TrustedProxies::parse(c).unwrap()
    }

    fn run(trusted: &[&str], peer: &str, proto: &'static str, h: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in h {
            m.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        apply_forwarded_headers(&mut m, peer.parse().unwrap(), proto, &tp(trusted));
        m
    }

    fn get<'a>(m: &'a HeaderMap, k: &str) -> Option<&'a str> {
        m.get(k).map(|v| v.to_str().unwrap())
    }

    const FORGED: [(&str, &str); 4] = [
        ("x-real-ip", "6.6.6.6"),
        ("forwarded", "for=6.6.6.6"),
        ("x-forwarded-host", "evil.example"),
        ("x-forwarded-proto", "https"),
    ];

    #[test]
    fn empty_list_is_untrusted_for_everyone() {
        let m = run(&[], "127.0.0.1", "http", &FORGED);
        assert_eq!(get(&m, "x-real-ip"), Some("127.0.0.1"));
        assert_eq!(get(&m, "forwarded"), None);
        assert_eq!(get(&m, "x-forwarded-host"), None);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("http"));
        assert_eq!(get(&m, "x-forwarded-for"), Some("127.0.0.1"));
    }

    #[test]
    fn untrusted_peer_appends_xff_and_overwrites() {
        let m = run(
            &["10.0.0.0/8"],
            "1.2.3.4",
            "https",
            &[("x-forwarded-for", "9.9.9.9")],
        );
        assert_eq!(get(&m, "x-forwarded-for"), Some("9.9.9.9, 1.2.3.4"));
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        assert_eq!(get(&m, "x-real-ip"), Some("1.2.3.4"));
    }

    #[test]
    fn trusted_v4_keeps_incoming_headers_but_derives_real_ip() {
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &FORGED);
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        assert_eq!(get(&m, "forwarded"), Some("for=6.6.6.6"));
        assert_eq!(get(&m, "x-forwarded-host"), Some("evil.example"));
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
    }

    #[test]
    fn trusted_v6_keeps_incoming() {
        let m = run(&["fd00::/8"], "fd00::1", "http", &FORGED);
        assert_eq!(get(&m, "x-real-ip"), Some("fd00::1"));
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        let m = run(&["fd00::/8"], "fe80::1", "http", &FORGED);
        assert_eq!(get(&m, "x-real-ip"), Some("fe80::1"));
    }

    #[test]
    fn v4_mapped_peer_matches_v4_cidr() {
        let h = [("x-forwarded-for", "6.6.6.6")];
        let m = run(&["10.0.0.0/8"], "::ffff:10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("6.6.6.6"));
        // untrusted v4-mapped peer: canonical dotted form
        let m = run(&["10.0.0.0/8"], "::ffff:11.1.2.3", "http", &FORGED);
        assert_eq!(get(&m, "x-real-ip"), Some("11.1.2.3"));
    }

    #[test]
    fn trusted_missing_headers_fall_back_to_connection() {
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "https", &[]);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        assert_eq!(get(&m, "x-forwarded-host"), None);
    }

    #[test]
    fn trusted_xfp_takes_rightmost_value() {
        let t = ["10.0.0.0/8"];
        let x = |h: &[(&str, &str)]| run(&t, "10.1.2.3", "http", h);
        let m = x(&[("x-forwarded-proto", "https, http")]);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("http"));
        let m = x(&[
            ("x-forwarded-proto", "http"),
            ("x-forwarded-proto", "https"),
        ]);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        let m = x(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-proto", "http"),
        ]);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("http"));
        let m = x(&[("x-forwarded-proto", "http, https, ")]);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        for bad in ["", " , ", "ht tp", "https, ja:va", "https, <x>"] {
            let m = run(&t, "10.1.2.3", "https", &[("x-forwarded-proto", bad)]);
            assert_eq!(get(&m, "x-forwarded-proto"), Some("https"), "{bad:?}");
        }
    }

    #[test]
    fn trusted_forged_real_ip_is_ignored() {
        let h = [
            ("x-real-ip", "7.7.7.7"),
            ("x-forwarded-for", "1.1.1.1, 6.6.6.6"),
        ];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("6.6.6.6"));
        assert_eq!(m.get_all("x-real-ip").iter().count(), 1);
        let h = [
            ("x-real-ip", "7.7.7.7"),
            ("x-forwarded-for", "6.6.6.6, 10.0.0.5"),
        ];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("6.6.6.6"));
    }

    #[test]
    fn non_ascii_xff_line_stops_the_walk() {
        let mut m = HeaderMap::new();
        m.append("x-forwarded-for", "5.5.5.5".parse().unwrap());
        m.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"6.6.6.\x80").unwrap(),
        );
        apply_forwarded_headers(
            &mut m,
            "10.1.2.3".parse().unwrap(),
            "http",
            &tp(&["10.0.0.0/8"]),
        );
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        // a bad line left of a good untrusted entry is never reached
        let mut m = HeaderMap::new();
        m.append("x-forwarded-for", HeaderValue::from_bytes(b"\x80").unwrap());
        m.append("x-forwarded-for", "5.5.5.5".parse().unwrap());
        apply_forwarded_headers(
            &mut m,
            "10.1.2.3".parse().unwrap(),
            "http",
            &tp(&["10.0.0.0/8"]),
        );
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
    }

    #[test]
    fn xff_ports_and_brackets_are_parsed() {
        let h = [("x-forwarded-for", "9.9.9.9, 5.5.5.5:4711")];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
        let h = [("x-forwarded-for", "9.9.9.9, [2001:db8::1]:443, 10.0.0.7:80")];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("2001:db8::1"));
        let h = [("x-forwarded-for", "9.9.9.9, [2001:db8::2]")];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("2001:db8::2"));
    }

    #[test]
    fn trusted_real_ip_derived_from_rightmost_untrusted_xff() {
        let h = [("x-forwarded-for", "6.6.6.6, 5.5.5.5, 10.0.0.9")];
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
        assert_eq!(
            get(&m, "x-forwarded-for"),
            Some("6.6.6.6, 5.5.5.5, 10.0.0.9, 10.1.2.3")
        );
        let m = run(
            &["10.0.0.0/8"],
            "10.1.2.3",
            "http",
            &[("x-forwarded-for", "10.0.0.9")],
        );
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
    }

    #[test]
    fn malformed_xff_entries_are_tolerated() {
        let m = run(
            &["10.0.0.0/8"],
            "10.1.2.3",
            "http",
            &[("x-forwarded-for", "5.5.5.5, garbage")],
        );
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        let m = run(
            &["10.0.0.0/8"],
            "10.1.2.3",
            "http",
            &[("x-forwarded-for", "garbage, 5.5.5.5, ")],
        );
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        let m = run(
            &["10.0.0.0/8"],
            "10.1.2.3",
            "http",
            &[("x-forwarded-for", "garbage, 5.5.5.5")],
        );
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
    }
}
