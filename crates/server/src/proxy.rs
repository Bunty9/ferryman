//! Per-request handler. Looks up the routing table, rebuilds the URI for
//! the chosen upstream, streams the request straight through via the shared
//! hyper client, records metrics, and turns transport failures into
//! circuit-breaker trips.

use ferryman_core::{SharedTable, TrustedProxies, XffMode};
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
struct BodyState {
    polled: AtomicBool,
    waiting_on_client: AtomicBool,
    origin: Instant,
    /// Start of the current client stall, ms since `origin` + 1 (0 = none).
    stall_since: AtomicU64,
    /// Longest finished client stall so far, ms.
    longest_stall: AtomicU64,
}

impl BodyState {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
    fn stall_ended(&self) {
        let since = self.stall_since.swap(0, Ordering::Relaxed);
        if since != 0 {
            let d = (self.now_ms() + 1).saturating_sub(since);
            self.longest_stall.fetch_max(d, Ordering::Relaxed);
        }
    }
    /// Did the client leave its upload idle for >= `theta` at any point (incl. now)?
    fn client_stalled(&self, theta: Duration) -> bool {
        let t = theta.as_millis() as u64;
        let since = self.stall_since.load(Ordering::Relaxed);
        self.longest_stall.load(Ordering::Relaxed) >= t
            || (since != 0 && (self.now_ms() + 1).saturating_sub(since) >= t)
    }
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
        let state = Arc::new(BodyState {
            polled: AtomicBool::new(false),
            waiting_on_client: AtomicBool::new(false),
            origin: Instant::now(),
            stall_since: AtomicU64::new(0),
            longest_stall: AtomicU64::new(0),
        });
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
                this.state.stall_ended();
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                // The last frame of a content-length body may not be followed
                // by another poll, so check here as well as on `None`.
                this.signal_if_done();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.state.stall_ended();
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(None) => {
                this.state.stall_ended();
                this.state.waiting_on_client.store(false, Ordering::Relaxed);
                if let Some(tx) = this.eos.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(None)
            }
            Poll::Pending => {
                this.state.waiting_on_client.store(true, Ordering::Relaxed);
                let now = this.state.now_ms() + 1;
                let _ = this.state.stall_since.compare_exchange(
                    0,
                    now,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
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
    handle_streaming_at(table, client, peer, None, proto, req).await
}

/// [`handle_streaming`] plus the listener's local port, used for
/// `X-Forwarded-Port`. `serve` calls this; without a port
/// (`handle_streaming`) `X-Forwarded-Port` is omitted for untrusted peers
/// (never derived from the client's `Host`).
pub async fn handle_streaming_at(
    table: SharedTable,
    client: StreamingClient,
    peer: SocketAddr,
    local_port: Option<u16>,
    proto: &'static str,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    let wrap = |body, idle, total| {
        let (b, eos) = RequestBody::new(body, idle, total);
        (b, Some(eos))
    };
    handle_inner(table, client, peer, local_port, proto, req, wrap).await
}

/// The 0.2.2 handler, kept for compatibility: `upstream_timeout_secs` covers
/// the whole request including the body upload, no body idle timeout, and a
/// timeout counts against the breaker only for bodyless requests. It shares
/// the core with `handle_streaming`, so it still gets the forwarded-header and
/// bad-path fixes. It does not get the client-stall protection (`handle_streaming` only).
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
    handle_inner(table, client, peer, None, proto, req, |b, _, _| (b, None)).await
}

/// Shared core. `wrap` turns the inbound body into the client's body type and
/// optionally returns an end-of-body signal; without one (legacy), the
/// upstream timeout starts immediately and only counts for bodyless requests.
async fn handle_inner<B>(
    table: SharedTable,
    client: Client<HttpConnector, B>,
    peer: SocketAddr,
    local_port: Option<u16>,
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

    // Answered before bad_path/lookup: no upstream, no breaker. Exact match
    // on the raw path. Metric label is the fixed "local_health".
    if table.local_health_path() == Some(req.uri().path())
        && matches!(*req.method(), http::Method::GET | http::Method::HEAD)
    {
        record(started, "local_health", "none", 200);
        let body = if req.method() == http::Method::HEAD {
            ""
        } else {
            "ok"
        };
        return Ok(error_response(StatusCode::OK, body));
    }

    if ferryman_core::path::bad_path(req.uri().path())
        || ferryman_core::path::ambiguous_route(&table, req.uri().path())
    {
        record(started, "none", "none", 400);
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
    }

    if bad_host(req.headers(), req.uri(), req.version()) {
        record(started, "none", "none", 400);
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad host\n"));
    }

    let Some(route) = table.lookup(req.uri().path()) else {
        record(started, "none", "none", 404);
        return Ok(error_response(StatusCode::NOT_FOUND, "no route"));
    };
    let route_label = route.prefix.clone();
    let upstream = route.upstream.clone();
    let rewrite_host = route.rewrite_host;
    // bad_path/ambiguous_route already accepted the raw path; strip only
    // after them, on the normalised path, and re-check the result.
    let strip = if route.strip_prefix {
        match route.stripped_path(req.uri().path()) {
            Some(p) => Some((p, route.stripped_prefix().to_string())),
            None => {
                record(started, "none", "none", 400);
                return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
            }
        }
    } else {
        None
    };

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
    // The client's effective host, for `Forwarded host=`; captured before
    // `rewrite_host` can overwrite it.
    let client_host: Option<String> = table
        .forwarded_header()
        .then(|| match parts.uri.authority() {
            Some(a) => a.as_str().rsplit('@').next().unwrap_or("").to_string(),
            None => parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string(),
        })
        .filter(|h| !h.is_empty());
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
    up_parts.path_and_query = match &strip {
        Some((path, _)) => {
            let pq = match parts.uri.query() {
                Some(q) => format!("{path}?{q}"),
                None => path.clone(),
            };
            match pq.parse() {
                Ok(pq) => Some(pq),
                Err(_) => {
                    // Request-derived, so the client's fault.
                    upstream.release(admission);
                    record(started, &route_label, &upstream.name, 400);
                    return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
                }
            }
        }
        None => parts.uri.path_and_query().cloned(),
    };
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
    if table.trusted_proxies().is_empty() && is_private_peer(peer.ip()) {
        warn_private_peer_once();
    }
    apply_forwarded_headers(
        &mut parts.headers,
        peer.ip(),
        local_port,
        proto,
        client_host.as_deref(),
        &FwdCfg {
            trusted: table.trusted_proxies(),
            forwarded_header: table.forwarded_header(),
            xff: table.xff_mode(),
        },
    );

    // After apply_forwarded_headers (it strips untrusted values).
    if let Some((_, prefix)) = strip.as_ref().filter(|(_, p)| !p.is_empty()) {
        set_forwarded_prefix(&mut parts.headers, prefix);
    }

    let fwd = Request::from_parts(parts, body);

    let upstream_timeout = table.upstream_timeout;
    // A client that left its upload idle for >= theta may have tripped the
    // upstream's own body-read timeout; its 502/5xx then isn't evidence.
    let theta = (table.request_body_idle_timeout() / 2).min(Duration::from_secs(1));
    let stall = body_done.as_ref().map(|e| e.state.clone());
    let client_stalled = || stall.as_ref().is_some_and(|s| s.client_stalled(theta));
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
            if client_stalled() {
                upstream.release(admission);
            } else {
                upstream.record_failure(admission);
            }
            tracing::warn!(upstream = %upstream.name, error = %e, "upstream request failed");
            record(started, &route_label, &upstream.name, 502);
            Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"))
        }
        Some(Ok(resp)) => {
            let status = resp.status();
            // Health is judged on the response head; the body is streamed
            // afterwards with no timeout of its own.
            if matches!(status.as_u16(), 502..=504) {
                if client_stalled() {
                    upstream.release(admission);
                } else {
                    upstream.record_failure(admission);
                }
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
        // The proxy relies on `Host`; a client must not delete it by naming
        // it in `Connection` (applies to the response strip too).
        if name == header::HOST {
            continue;
        }
        headers.remove(name);
    }
    for name in &HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// More than one `Host`, or (when the request target has no authority, so the
/// header is what gets forwarded) a missing one on HTTP/1.1 or one that is
/// empty, `*`, or not a bare `host[:port]` (userinfo, list, path, obs-text,
/// port not 1-5 digits <= 65535). h2 `:authority` / absolute form still win
/// over `Host`, which is then not judged beyond the count. HTTP/1.0 without
/// `Host` stays allowed.
fn bad_host(headers: &HeaderMap, uri: &http::Uri, version: http::Version) -> bool {
    let mut it = headers.get_all(header::HOST).iter();
    match (it.next(), it.next()) {
        (None, _) => uri.authority().is_none() && version == http::Version::HTTP_11,
        (Some(v), None) => uri.authority().is_none() && !plain_authority(v.as_bytes()),
        _ => true,
    }
}

fn plain_authority(b: &[u8]) -> bool {
    if b == b"*"
        || b.iter()
            .any(|c| matches!(c, b'@' | b',' | b'/') || *c >= 0x80)
    {
        return false;
    }
    if http::uri::Authority::try_from(b).is_err() {
        return false;
    }
    // A port part after the host (not inside `[v6]`) must be 1-5 digits <= 65535.
    match b.iter().rposition(|c| *c == b':') {
        Some(i) if !b[i..].contains(&b']') => {
            let p = &b[i + 1..];
            !p.is_empty()
                && p.iter().all(u8::is_ascii_digit)
                && std::str::from_utf8(p).is_ok_and(|p| p.parse::<u16>().is_ok())
        }
        _ => true,
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

/// Per-table forwarding policy for [`apply_forwarded_headers`].
struct FwdCfg<'a> {
    trusted: &'a TrustedProxies,
    forwarded_header: bool,
    xff: XffMode,
}

/// RFC 1918, 100.64/10, fc00::/7, fe80::/10, loopback.
fn is_private_peer(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.segments()[0] & 0xfe00 == 0xfc00
                || v6.segments()[0] & 0xffc0 == 0xfe80
        }
    }
}

/// One warning per process: with no `trusted_proxies`, a private-range peer
/// is probably a proxy/LB whose forwarding headers are being discarded.
fn warn_private_peer_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "request from a private/loopback peer while trusted_proxies is empty: if ferryman \
             sits behind a proxy or load balancer, add its address range to trusted_proxies, \
             otherwise client IP and scheme headers it sends are ignored"
        );
    }
}

/// `X-Forwarded-Prefix` for a stripped route: a trusted hop's single plain
/// path value (untrusted ones are already gone) is kept in front of ours
/// (`/outer` + `/api` gives `/outer/api`); anything else is replaced.
fn set_forwarded_prefix(headers: &mut HeaderMap, prefix: &str) {
    let name = HeaderName::from_static("x-forwarded-prefix");
    let mut all = headers.get_all(&name).iter();
    let outer = match (all.next(), all.next()) {
        (Some(v), None) => v.to_str().ok().map(|v| v.trim().trim_end_matches('/')),
        _ => None,
    }
    .filter(|v| {
        v.starts_with('/')
            && v.bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b',' | b'?' | b'#'))
            && !v.contains("//")
            && !ferryman_core::path::bad_path(v)
    })
    .unwrap_or("");
    if let Ok(v) = HeaderValue::from_str(&format!("{outer}{prefix}")) {
        headers.insert(name, v);
    }
}

/// Client-supplied hop headers an untrusted peer must not set.
const UNTRUSTED_STRIPPED: [&str; 6] = [
    "forwarded",
    "x-forwarded-host",
    "x-forwarded-ssl",
    "x-forwarded-scheme",
    "x-forwarded-port",
    "x-forwarded-prefix",
];

/// Canonical trust-governed names. A header whose name maps onto one of these
/// (every non-alphanumeric byte read as `-`, as CGI-style backends do: WSGI,
/// Rack, PHP-FPM fold `_`, `.` and space) without being spelled exactly so is
/// dropped.
const TRUST_GOVERNED: [&str; 9] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-ssl",
    "x-forwarded-scheme",
    "x-forwarded-prefix",
    "x-real-ip",
];

/// Is `name` (already lowercase) a non-canonical spelling of a trust-governed
/// header? Allocation-free for names made only of `[a-z0-9-]`, which are
/// either canonical or unrelated. Runs of separators are collapsed and
/// the ends trimmed (`x__real_ip`, `-x-real-ip` -> `x-real-ip`).
fn is_noncanonical_spelling(name: &str) -> bool {
    if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return false;
    }
    // Fold every non-alphanumeric byte to `-`, collapse runs, trim the ends.
    let mut folded = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            folded.push(c);
        } else if !folded.is_empty() && !folded.ends_with('-') {
            folded.push('-');
        }
    }
    let folded = folded.trim_end_matches('-');
    TRUST_GOVERNED.contains(&folded)
}

/// Remove, for every peer, non-canonical spellings (`X_Real_IP`, `x.forwarded.for`)
/// of trust-governed headers.
fn strip_noncanonical_spellings(headers: &mut HeaderMap) {
    let bad: Vec<HeaderName> = headers
        .keys()
        .filter(|k| is_noncanonical_spelling(k.as_str()))
        .cloned()
        .collect();
    for k in bad {
        headers.remove(k);
    }
}

/// Rightmost comma-separated token across all lines of `name`.
fn rightmost<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers
        .get_all(name)
        .iter()
        .flat_map(|v| v.to_str().unwrap_or("!").split(','))
        .map(str::trim)
        .rfind(|v| !v.is_empty())
}

/// RFC 7239 §6 node: bare v4, quoted+bracketed v6.
fn forwarded_node(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("\"[{v6}]\""),
    }
}

/// Decide the forwarding headers the upstream sees (pure; unit-tested).
///
/// Untrusted peer (every peer when `trusted_proxies` is empty): `x-forwarded-proto`
/// is set from the connection, `x-forwarded-port` from the listener port,
/// `x-forwarded-for` gets the peer appended, `x-real-ip` is overwritten with
/// the peer, and client `forwarded`, `x-forwarded-host`, `-ssl`, `-scheme`
/// and `-prefix` are stripped.
///
/// Trusted peer: incoming `x-forwarded-proto` / `x-forwarded-port`
/// (rightmost value, port must be valid), `x-forwarded-host`, `-ssl`,
/// `-scheme`, `-prefix` and `forwarded` are kept; a missing or invalid proto
/// or port comes from the connection / listener. `x-real-ip` is always
/// overwritten with the rightmost `x-forwarded-for` entry that is not a
/// trusted proxy (`:port` suffixes tolerated; an unparsable entry stops the
/// walk), else the peer. `x-forwarded-for` gets the peer appended
/// (`xff = "append"`) or is set to that client address (`"replace"`).
///
/// With `forwarded_header`, a `for=<peer>;proto=;host=` element is appended
/// to `forwarded` (fresh for untrusted peers, whose value was stripped).
/// `local_port` None: `x-forwarded-port` is omitted unless a trusted peer sent a
/// valid one (never derived from `Host`). `proto=` is this hop's connection scheme;
/// `host=` is the client's effective host (URI authority, else `Host`).
fn apply_forwarded_headers(
    headers: &mut HeaderMap,
    peer: IpAddr,
    local_port: Option<u16>,
    proto: &'static str,
    client_host: Option<&str>,
    cfg: &FwdCfg<'_>,
) {
    strip_noncanonical_spellings(headers);
    let trusted = cfg.trusted;
    let xfp = HeaderName::from_static("x-forwarded-proto");
    let xport = HeaderName::from_static("x-forwarded-port");
    let peer_trusted = trusted.contains(peer);
    let client = if peer_trusted {
        // Rightmost non-empty value across all lines (the hop closest to us
        // wrote it); anything that isn't a scheme token falls back to the
        // connection's proto.
        let scheme = rightmost(headers, &xfp)
            .filter(|v| {
                v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            })
            .and_then(|v| HeaderValue::from_str(v).ok());
        headers.insert(&xfp, scheme.unwrap_or(HeaderValue::from_static(proto)));
        let port = rightmost(headers, &xport)
            .and_then(|v| {
                v.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0 && v.bytes().all(|b| b.is_ascii_digit()))
            })
            .or(local_port);
        set_port(headers, &xport, port);
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
        entries
            .into_iter()
            .rev()
            .find(|e| !matches!(e, Some(ip) if trusted.contains(*ip)))
            .flatten()
            .unwrap_or(peer)
    } else {
        headers.insert(&xfp, HeaderValue::from_static(proto));
        for h in UNTRUSTED_STRIPPED {
            headers.remove(h);
        }
        set_port(headers, &xport, local_port);
        peer
    };
    set_real_ip(headers, client);
    match cfg.xff {
        XffMode::Replace => {
            if let Ok(v) = HeaderValue::from_str(&client.to_canonical().to_string()) {
                headers.insert("x-forwarded-for", v);
            }
        }
        _ => append_forwarded_for(headers, peer),
    }
    if cfg.forwarded_header {
        append_forwarded(headers, peer, proto, client_host);
    }
}

fn set_port(headers: &mut HeaderMap, name: &HeaderName, port: Option<u16>) {
    match port {
        Some(p) => {
            headers.insert(name, HeaderValue::from(p));
        }
        None => {
            headers.remove(name);
        }
    }
}

/// Append `for=<peer>;proto=<proto>;host=<Host>` to the `forwarded` list.
fn append_forwarded(headers: &mut HeaderMap, peer: IpAddr, proto: &str, host: Option<&str>) {
    let mut el = format!("for={};proto={proto}", forwarded_node(peer));
    // `host` is a token only without ':' etc.; quote anything else.
    if let Some(h) = host {
        if h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        {
            el.push_str(&format!(";host={h}"));
        } else {
            el.push_str(&format!(
                ";host=\"{}\"",
                h.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
    }
    let existing: Vec<&str> = headers
        .get_all(header::FORWARDED)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let value = if existing.is_empty() {
        el
    } else {
        format!("{}, {el}", existing.join(", "))
    };
    if let Ok(v) = HeaderValue::from_str(&value) {
        headers.insert(header::FORWARDED, v);
    }
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

    #[test]
    fn host_validation() {
        let h = |vals: &[&[u8]]| {
            let mut m = HeaderMap::new();
            for v in vals {
                m.append("host", HeaderValue::from_bytes(v).unwrap());
            }
            m
        };
        use http::Version as V;
        let root: http::Uri = "/".parse().unwrap();
        for bad in [
            &b"a, b"[..],
            b"u@x",
            b"x/y",
            b"",
            b"a\xe9.example",
            b"a:b",
            b"x:99999",
            b"x:",
            b"[::1]:",
            b"[::1]:x",
            b"x:+80",
            b"*",
        ] {
            assert!(bad_host(&h(&[bad]), &root, V::HTTP_11), "{bad:?}");
        }
        assert!(bad_host(&h(&[b"a", b"b"]), &root, V::HTTP_11));
        for ok in [
            "a.example",
            "a.example:8443",
            "a:65535",
            "[::1]",
            "[::1]:80",
            "[::1]:8443",
            "LOCALHOST",
        ] {
            assert!(!bad_host(&h(&[ok.as_bytes()]), &root, V::HTTP_11), "{ok}");
        }
        // No Host: 1.1 is bad, 1.0 allowed.
        assert!(bad_host(&HeaderMap::new(), &root, V::HTTP_11));
        assert!(!bad_host(&HeaderMap::new(), &root, V::HTTP_10));
        let abs: http::Uri = "http://a.example/".parse().unwrap();
        assert!(!bad_host(&HeaderMap::new(), &abs, V::HTTP_11));
        assert!(!bad_host(&h(&[b"u@x"]), &abs, V::HTTP_11));
        assert!(bad_host(&h(&[b"a", b"b"]), &abs, V::HTTP_11));
    }

    #[test]
    fn connection_cannot_delete_host() {
        let mut m = HeaderMap::new();
        m.insert("host", HeaderValue::from_static("a.example"));
        m.insert("connection", HeaderValue::from_static("Host, x-other"));
        m.insert("x-other", HeaderValue::from_static("1"));
        strip_hop_by_hop(&mut m);
        assert!(m.get("host").is_some() && m.get("x-other").is_none());
        // Same on a response strip.
        let mut r = HeaderMap::new();
        r.insert("host", HeaderValue::from_static("upstream"));
        r.insert("connection", HeaderValue::from_static("host"));
        strip_hop_by_hop(&mut r);
        assert!(r.get("host").is_some() && r.get("connection").is_none());
    }

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
        apply_forwarded_headers(
            &mut m,
            peer.parse().unwrap(),
            None,
            proto,
            None,
            &cfg(&tp(trusted), false, XffMode::Append),
        );
        m
    }

    fn cfg(t: &TrustedProxies, forwarded_header: bool, xff: XffMode) -> FwdCfg<'_> {
        FwdCfg {
            trusted: t,
            forwarded_header,
            xff,
        }
    }

    fn host_of<'a>(h: &[(&str, &'a str)]) -> Option<&'a str> {
        h.iter().find(|(k, _)| *k == "host").map(|(_, v)| *v)
    }

    fn run_with(
        trusted: &[&str],
        peer: &str,
        port: Option<u16>,
        fh: bool,
        xff: XffMode,
        h: &[(&str, &str)],
    ) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in h {
            m.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        let t = tp(trusted);
        apply_forwarded_headers(
            &mut m,
            peer.parse().unwrap(),
            port,
            "http",
            host_of(h),
            &cfg(&t, fh, xff),
        );
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
            None,
            "http",
            None,
            &cfg(&tp(&["10.0.0.0/8"]), false, XffMode::Append),
        );
        assert_eq!(get(&m, "x-real-ip"), Some("10.1.2.3"));
        // a bad line left of a good untrusted entry is never reached
        let mut m = HeaderMap::new();
        m.append("x-forwarded-for", HeaderValue::from_bytes(b"\x80").unwrap());
        m.append("x-forwarded-for", "5.5.5.5".parse().unwrap());
        apply_forwarded_headers(
            &mut m,
            "10.1.2.3".parse().unwrap(),
            None,
            "http",
            None,
            &cfg(&tp(&["10.0.0.0/8"]), false, XffMode::Append),
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

    const HOP: [(&str, &str); 4] = [
        ("x-forwarded-ssl", "on"),
        ("x-forwarded-scheme", "https"),
        ("x-forwarded-prefix", "/evil"),
        ("x-forwarded-port", "4443"),
    ];

    #[test]
    fn untrusted_strips_ssl_scheme_prefix_port_all_instances() {
        let mut h = HOP.to_vec();
        h.push(("x-forwarded-prefix", "/evil2"));
        h.push(("x-forwarded-port", "1"));
        let m = run_with(&[], "1.2.3.4", Some(8080), false, XffMode::Append, &h);
        for k in [
            "x-forwarded-ssl",
            "x-forwarded-scheme",
            "x-forwarded-prefix",
        ] {
            assert_eq!(m.get_all(k).iter().count(), 0, "{k}");
        }
        assert_eq!(m.get_all("x-forwarded-port").iter().count(), 1);
        assert_eq!(get(&m, "x-forwarded-port"), Some("8080"));
        // no listener port: nothing forged survives
        let m = run_with(&[], "1.2.3.4", None, false, XffMode::Append, &h);
        assert_eq!(get(&m, "x-forwarded-port"), None);
    }

    #[test]
    fn trusted_keeps_ssl_scheme_prefix_and_valid_port() {
        let m = run_with(
            &["10.0.0.0/8"],
            "10.1.2.3",
            Some(8080),
            false,
            XffMode::Append,
            &HOP,
        );
        assert_eq!(get(&m, "x-forwarded-ssl"), Some("on"));
        assert_eq!(get(&m, "x-forwarded-scheme"), Some("https"));
        assert_eq!(get(&m, "x-forwarded-prefix"), Some("/evil"));
        assert_eq!(get(&m, "x-forwarded-port"), Some("4443"));
        let h = [
            ("x-forwarded-port", "80"),
            ("x-forwarded-port", "443, 8443"),
        ];
        let m = run_with(
            &["10.0.0.0/8"],
            "10.1.2.3",
            Some(8080),
            false,
            XffMode::Append,
            &h,
        );
        assert_eq!(get(&m, "x-forwarded-port"), Some("8443"));
        for bad in ["abc", "0", "70000", "-1", "+80", " "] {
            let h = [("x-forwarded-port", bad)];
            let m = run_with(
                &["10.0.0.0/8"],
                "10.1.2.3",
                Some(8080),
                false,
                XffMode::Append,
                &h,
            );
            assert_eq!(get(&m, "x-forwarded-port"), Some("8080"), "{bad:?}");
        }
    }

    #[test]
    fn xff_replace_sets_client_only() {
        let h = [("x-forwarded-for", "6.6.6.6, 5.5.5.5")];
        let m = run_with(&[], "1.2.3.4", None, false, XffMode::Replace, &h);
        assert_eq!(get(&m, "x-forwarded-for"), Some("1.2.3.4"));
        assert_eq!(m.get_all("x-forwarded-for").iter().count(), 1);
        let m = run_with(
            &["10.0.0.0/8"],
            "10.1.2.3",
            None,
            false,
            XffMode::Replace,
            &h,
        );
        assert_eq!(get(&m, "x-forwarded-for"), Some("5.5.5.5"));
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
        let m = run_with(&[], "1.2.3.4", None, false, XffMode::Append, &h);
        assert_eq!(
            get(&m, "x-forwarded-for"),
            Some("6.6.6.6, 5.5.5.5, 1.2.3.4")
        );
    }

    #[test]
    fn forwarded_header_output() {
        let host = [("host", "app.example")];
        let m = run_with(&[], "1.2.3.4", None, true, XffMode::Append, &host);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=1.2.3.4;proto=http;host=app.example")
        );
        // untrusted: client Forwarded is stripped, element is fresh
        let h = [("host", "app.example:8080"), ("forwarded", "for=6.6.6.6")];
        let m = run_with(&[], "1.2.3.4", None, true, XffMode::Append, &h);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=1.2.3.4;proto=http;host=\"app.example:8080\"")
        );
        // trusted: appended to the list; proto= is this hop's connection scheme
        let h = [("forwarded", "for=6.6.6.6"), ("x-forwarded-proto", "https")];
        let m = run_with(&["10.0.0.0/8"], "10.1.2.3", None, true, XffMode::Append, &h);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=6.6.6.6, for=10.1.2.3;proto=http")
        );
        // IPv6 quoted + bracketed (RFC 7239 section 6); v4-mapped shown as v4
        let m = run_with(&[], "2001:db8::1", None, true, XffMode::Append, &[]);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=\"[2001:db8::1]\";proto=http")
        );
        let m = run_with(&[], "::ffff:1.2.3.4", None, true, XffMode::Append, &[]);
        assert_eq!(get(&m, "forwarded"), Some("for=1.2.3.4;proto=http"));
        // off by default: untrusted Forwarded just stripped
        let m = run_with(&[], "1.2.3.4", None, false, XffMode::Append, &h);
        assert_eq!(get(&m, "forwarded"), None);
    }

    #[test]
    fn noncanonical_spellings_are_stripped_for_every_peer() {
        let h = [
            ("x_real_ip", "6.6.6.6"),
            ("x_forwarded_prefix", "/evil"),
            ("X_Forwarded_For", "6.6.6.6"),
            ("forwarded", "for=1.1.1.1"),
            ("x_custom", "keep"),
            ("x.forwarded.for", "6.6.6.6"),
            ("X.Real.IP", "6.6.6.6"),
            ("x-forwarded.proto", "https"),
            ("x__forwarded_for", "6.6.6.6"),
            ("x-.forwarded.for", "6.6.6.6"),
            ("x.custom", "keep"),
        ];
        for trusted in [&[][..], &["10.0.0.0/8"][..]] {
            let m = run_with(trusted, "10.1.2.3", None, false, XffMode::Append, &h);
            for k in ["x_real_ip", "x_forwarded_prefix", "x_forwarded_for"] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            for k in ["x.forwarded.for", "x.real.ip", "x-forwarded.proto"] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            for k in ["x__forwarded_for", "x-.forwarded.for"] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            assert_eq!(get(&m, "x.custom"), Some("keep"));
            assert_eq!(get(&m, "x_custom"), Some("keep"));
            assert_eq!(get(&m, "x-forwarded-for"), Some("10.1.2.3"));
        }
    }

    #[test]
    fn forwarded_host_is_the_clients_even_when_rewritten_elsewhere() {
        // host= comes from the caller-supplied client host, not the headers.
        let mut m = HeaderMap::new();
        m.insert("host", "upstream.internal:9".parse().unwrap());
        let t = tp(&[]);
        apply_forwarded_headers(
            &mut m,
            "1.2.3.4".parse().unwrap(),
            None,
            "http",
            Some("client.example"),
            &cfg(&t, true, XffMode::Append),
        );
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=1.2.3.4;proto=http;host=client.example")
        );
    }

    #[test]
    fn no_listener_port_never_uses_host() {
        let h = [("host", "x:443")];
        let m = run_with(&[], "1.2.3.4", None, false, XffMode::Append, &h);
        assert_eq!(get(&m, "x-forwarded-port"), None);
    }

    #[test]
    fn private_peer_detection() {
        for ip in [
            "10.1.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_peer(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "2001:db8::1",
            "fec0::1",
        ] {
            assert!(!is_private_peer(ip.parse().unwrap()), "{ip}");
        }
    }
}
