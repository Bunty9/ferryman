//! Per-request handler. Looks up the routing table, rebuilds the URI for
//! the chosen upstream, streams the request straight through via the shared
//! hyper client, records metrics, and turns transport failures into
//! circuit-breaker trips.

use ferryman_core::{Admission, RouteTable, SharedTable, TrustedProxies, XffMode};
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
use tracing::Instrument;

use crate::verdict::{client_fault, ClientFault, Ticket, VerdictPolicy, WatchedBody};
#[allow(deprecated)]
use crate::ProxyClient;
use crate::StreamingClient;
use crate::{ConnCtx, TlsInfo};
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
/// start the upstream timeout only once the whole body is sent. Trailer frames
/// are discarded (client trailers are never forwarded). Never buffers.
pub struct RequestBody {
    inner: Incoming,
    idle: Duration,
    deadline: tokio::time::Instant,
    /// `Policy::max_request_body_bytes`: streamed bytes allowed, and bytes seen.
    max: Option<u64>,
    seen: u64,
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

/// What the timer and the blame checks can see of the request body: whether
/// hyper polled it, and since when the last poll has been left waiting on the
/// client (rather than hyper having stopped reading because the upstream
/// isn't draining it). Shared with [`crate::verdict::WatchedBody`].
pub(crate) struct BodyState {
    polled: AtomicBool,
    /// Start of the current wait on the client, as nanoseconds since `base`
    /// plus one; 0 while not waiting. Set on a `Pending` poll, cleared by
    /// any `Ready` (frame, end of stream, error).
    waiting_since: AtomicU64,
    base: tokio::time::Instant,
    /// theta: how long a client must stall (upload, or pulling the response)
    /// before an upstream failure is put down to that stall.
    pub(crate) stall: Duration,
    /// The request body yielded an error (cap, deadline, client broke off).
    pub(crate) client_failed: AtomicBool,
    /// Sticky: a finished wait on the client lasted at least `stall`.
    stalled_once: AtomicBool,
}

/// theta = max(100 ms, min(1 s, idle / 2)). The floor keeps a zero idle gap
/// from turning response-body blame off entirely.
pub(crate) fn stall_threshold(idle: Duration) -> Duration {
    (idle / 2)
        .min(Duration::from_secs(1))
        .max(Duration::from_millis(100))
}

impl BodyState {
    fn new(idle: Duration) -> Self {
        Self {
            polled: AtomicBool::new(false),
            waiting_since: AtomicU64::new(0),
            base: tokio::time::Instant::now(),
            stall: stall_threshold(idle),
            client_failed: AtomicBool::new(false),
            stalled_once: AtomicBool::new(false),
        }
    }

    fn now_mark(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    /// The last poll of the upload was left waiting on the client. True most
    /// of the time during a normal upload (the proxy drains the client
    /// faster than it sends), so it is no excuse for an upstream failure on
    /// its own; see [`Self::client_stalled`].
    fn waiting_on_client(&self) -> bool {
        self.waiting_since.load(Ordering::Acquire) != 0
    }

    /// The proxy has been waiting on the client's next upload frame for at
    /// least `stall`.
    fn client_stalled(&self) -> bool {
        self.stalled_for(self.waiting_since.load(Ordering::Acquire))
    }

    /// The client stalled its upload for at least `stall` at some point of
    /// this request, or is stalling now. Every blame path checks this: an
    /// upstream that hangs up, breaks its body, or (as a gateway) answers
    /// 502-504 after such a stall may be reacting to it, so it is not blamed.
    pub(crate) fn stalled_during_request(&self) -> bool {
        self.stalled_once.load(Ordering::Acquire) || self.client_stalled()
    }

    /// A wait that started at `since` (a `now_mark`, 0 = none) has lasted at
    /// least `stall`.
    fn stalled_for(&self, since: u64) -> bool {
        since != 0 && Duration::from_nanos(self.now_mark().saturating_sub(since)) >= self.stall
    }
}

pub(crate) struct Eos {
    rx: oneshot::Receiver<()>,
    pub(crate) state: Arc<BodyState>,
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

/// The streamed request body passed `Policy::max_request_body_bytes` (413).
#[derive(Debug)]
pub(crate) struct BodyTooLarge;

impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body too large")
    }
}
impl std::error::Error for BodyTooLarge {}

impl RequestBody {
    fn new(inner: Incoming, idle: Duration, total: Duration, max: Option<u64>) -> (Self, Eos) {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(BodyState::new(idle));
        let mut b = Self {
            inner,
            idle,
            deadline: tokio::time::Instant::now() + total,
            max,
            seen: 0,
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
        let polled = this.poll_inner(cx);
        // Bookkeeping lives here, around `poll_inner`, so every exit is seen,
        // including the total-deadline check at its top.
        let state = &this.state;
        match polled {
            Poll::Pending => {
                if state.waiting_since.load(Ordering::Acquire) == 0 {
                    state
                        .waiting_since
                        .store(state.now_mark(), Ordering::Release);
                }
            }
            Poll::Ready(ref r) => {
                if state.stalled_for(state.waiting_since.swap(0, Ordering::AcqRel)) {
                    state.stalled_once.store(true, Ordering::Release);
                }
                if matches!(r, Some(Err(_))) {
                    state.client_failed.store(true, Ordering::Release);
                }
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

type BodyPoll =
    Poll<Option<Result<hyper::body::Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>>>;

impl RequestBody {
    fn poll_inner(&mut self, cx: &mut Context<'_>) -> BodyPoll {
        let this = self;
        this.state.polled.store(true, Ordering::Release);
        if tokio::time::Instant::now() >= this.deadline {
            return Poll::Ready(Some(Err(BodyTotalTimeout.into())));
        }
        loop {
            return match Pin::new(&mut this.inner).poll_frame(cx) {
                // Client trailers never reach the upstream: they could carry
                // trust-governed header names. Keep polling for the next frame.
                Poll::Ready(Some(Ok(frame))) if frame.is_trailers() => continue,
                Poll::Ready(Some(Ok(frame))) => {
                    if let (Some(max), Some(d)) = (this.max, frame.data_ref()) {
                        this.seen = this.seen.saturating_add(d.len() as u64);
                        if this.seen > max {
                            return Poll::Ready(Some(Err(BodyTooLarge.into())));
                        }
                    }
                    this.idle_sleep = None;
                    // The last frame of a content-length body may not be followed
                    // by another poll, so check here as well as on `None`.
                    this.signal_if_done();
                    Poll::Ready(Some(Ok(frame)))
                }
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(None) => {
                    if let Some(tx) = this.eos.take() {
                        let _ = tx.send(());
                    }
                    Poll::Ready(None)
                }
                Poll::Pending => {
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
            };
        }
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

/// Per-process pipeline handler: routing table, upstream client and policy.
/// Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Proxy {
    inner: Arc<ProxyInner>,
}

struct ProxyInner {
    table: SharedTable,
    client: StreamingClient,
    policy: Policy,
}

/// Behaviour knobs for [`Proxy`]. `Policy::default()` is ferryman's own
/// behaviour: header handling from the route table, no body cap. Construct
/// with `let mut p = Policy::default(); p.field = ...;`; fields are added
/// additively.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Policy {
    /// Header handling overrides.
    pub headers: HeaderPolicy,
    /// 413 when a declared `Content-Length` is over the cap, or when the
    /// streamed body passes it. Never affects the breaker. `None` = no cap.
    pub max_request_body_bytes: Option<u64>,
    /// When a normal admission's success reaches the breaker. Default
    /// [`VerdictPolicy::HeadOnly`].
    pub verdict: VerdictPolicy,
}

/// Overrides for the forwarding-header step. The default uses the route
/// table's `xff`, `trusted_proxies` and `forwarded_header`.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct HeaderPolicy {
    /// `None` = the table's `xff_mode()`.
    pub xff: Option<XffMode>,
    /// `None` = the table's `trusted_proxies()`. `Some(TrustedProxies::default())`
    /// trusts no peer, whatever the table says.
    pub trusted_proxies: Option<TrustedProxies>,
    /// `None` = the table's `forwarded_header()`.
    pub forwarded_header: Option<bool>,
    /// Request header names (lowercase) the embedder asserts. For each, its
    /// non-canonical spellings are removed for every peer (together with the
    /// trust-governed set), and the canonical name is removed after the
    /// forwarding step, whatever the client or that step wrote. Listing a
    /// trust-governed name (e.g. `x-real-ip`) hands it to the embedder.
    pub asserted: Vec<HeaderName>,
    /// `(raw, folded)` per asserted name, filled once by `Proxy::with_client`.
    folded: Vec<(String, String)>,
}

impl HeaderPolicy {
    fn fold_asserted(&mut self) {
        self.folded = fold_asserted(&self.asserted);
    }
}

impl Proxy {
    /// Builds its own upstream client (HTTP/1.1, TCP_NODELAY).
    pub fn new(table: SharedTable, policy: Policy) -> Self {
        Self::with_client(table, crate::client(), policy)
    }

    /// Like [`Proxy::new`] with a caller-supplied upstream client (tests,
    /// embedders).
    pub fn with_client(table: SharedTable, client: StreamingClient, mut policy: Policy) -> Self {
        policy.headers.fold_asserted();
        Self {
            inner: Arc::new(ProxyInner {
                table,
                client,
                policy,
            }),
        }
    }

    /// Handle a single inbound request. Never fails: every expected failure
    /// (no route, breaker open, timeout, transport error) becomes an error
    /// response. `conn` describes the connection the request arrived on; its
    /// peer address and TLS state drive the forwarding headers.
    ///
    /// Streaming semantics: `upstream_timeout_secs` starts once the request
    /// body is complete, `request_body_idle_timeout_secs` applies between
    /// body frames, and `request_body_timeout_secs` caps the whole upload.
    /// The private-peer `trusted_proxies` warning is the caller's job, see
    /// [`warn_if_private_peer`].
    pub async fn handle(&self, req: Request<Incoming>, conn: &ConnCtx) -> Response<ResponseBody> {
        let wrap = |body, idle, total, max| {
            let (b, eos) = RequestBody::new(body, idle, total, max);
            (b, Some(eos))
        };
        let Ok(resp) = handle_inner(
            self.inner.table.clone(),
            self.inner.client.clone(),
            conn,
            req,
            &self.inner.policy,
            wrap,
        )
        .await;
        resp
    }
}

/// `proto` is `"http"` or `"https"`, reflecting whether this connection was
/// TLS-terminated, and is forwarded as `x-forwarded-proto`.
///
/// Thin shim over [`Proxy::handle`]: builds a [`ConnCtx`] from its arguments
/// (the local address is unknown and set to `0.0.0.0:0`).
pub async fn handle_streaming(
    table: SharedTable,
    client: StreamingClient,
    peer: SocketAddr,
    proto: &'static str,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    warn_if_private_peer(&table.load(), peer);
    let conn = shim_ctx(peer, proto);
    Ok(Proxy::with_client(table, client, Policy::default())
        .handle(req, &conn)
        .await)
}

fn shim_ctx(peer: SocketAddr, proto: &str) -> ConnCtx {
    let ctx = ConnCtx::new(peer, SocketAddr::from(([0, 0, 0, 0], 0)));
    if proto == "https" {
        ctx.with_tls(TlsInfo::default())
    } else {
        ctx
    }
}

/// The 0.2.2 handler, kept for compatibility: `upstream_timeout_secs` covers
/// the whole request including the body upload, no body idle timeout, and a
/// timeout counts against the breaker only for bodyless requests. It shares
/// the core with `handle_streaming`, so it still gets the forwarded-header and
/// bad-path fixes. It does not get the client-stall protection (`handle_streaming` only).
/// Request trailers are not dropped on this path (the body is the raw
/// `Incoming`); that is safe only because `Trailer` is a hop-by-hop header and
/// is stripped.
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
    warn_if_private_peer(&table.load(), peer);
    handle_inner(
        table,
        client,
        &shim_ctx(peer, proto),
        req,
        &Policy::default(),
        |b, _, _, _| (b, None),
    )
    .await
}

/// Per-request state for the access log line (`None` = `access_log = false`).
struct Cx {
    started: Instant,
    log: Option<AccessInfo>,
}

struct AccessInfo {
    method: http::Method,
    host: String,
    path: String,
    version: http::Version,
    peer: IpAddr,
    proto: &'static str,
    request_id: Option<String>,
}

const MAX_LOGGED_PATH: usize = 512;
const MAX_REQUEST_ID: usize = 128;

/// Log-safe copy: at most `max` bytes, anything but visible ASCII becomes `?`
/// (no newline/ANSI injection into text logs).
fn sanitize(s: &[u8], max: usize) -> String {
    s.iter()
        .copied()
        .take(max)
        .map(|b| {
            if (0x21..=0x7e).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

fn valid_request_id(v: &HeaderValue) -> bool {
    let b = v.as_bytes();
    !b.is_empty() && b.len() <= MAX_REQUEST_ID && b.iter().all(|c| (0x21..=0x7e).contains(c))
}

/// 128 random-ish bits as hex. Not cryptographic: per-process random hasher
/// keys mixed with a counter; unique enough to correlate log lines.
fn generate_request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let word = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(N.fetch_add(1, Ordering::Relaxed));
        h.finish()
    };
    format!("{:016x}{:016x}", word(), word())
}

/// Keeps a valid incoming `x-request-id`, else replaces it with a fresh one.
fn ensure_request_id(headers: &mut HeaderMap) -> String {
    const NAME: HeaderName = HeaderName::from_static("x-request-id");
    // More than one header is ambiguous: replace (insert drops them all).
    if headers.get_all(NAME).iter().count() == 1 {
        if let Some(v) = headers.get(NAME).filter(|v| valid_request_id(v)) {
            return String::from_utf8_lossy(v.as_bytes()).into_owned();
        }
    }
    let id = generate_request_id();
    headers.insert(
        NAME,
        HeaderValue::from_str(&id).expect("hex is a valid header value"),
    );
    id
}

/// Wraps the core in the request span, sets up the access-log context and the
/// optional `x-request-id` (on the upstream request and the response).
async fn handle_inner<B>(
    table: SharedTable,
    client: Client<HttpConnector, B>,
    conn: &ConnCtx,
    mut req: Request<Incoming>,
    policy: &Policy,
    wrap: impl FnOnce(Incoming, Duration, Duration, Option<u64>) -> (B, Option<Eos>),
) -> Result<Response<ResponseBody>, Infallible>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (peer, proto) = (conn.peer, conn.proto());
    let started = Instant::now();
    // `load_full` (not `load`) because the returned `Arc` is held across
    // `.await` points below; `load`'s guard is meant for short critical
    // sections only.
    let table = table.load_full();
    let echo_id = table.request_id();
    let request_id = if echo_id {
        Some(ensure_request_id(req.headers_mut()))
    } else if table.access_log() {
        req.headers()
            .get("x-request-id")
            .filter(|v| valid_request_id(v))
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    } else {
        None
    };
    let log = table.access_log().then(|| {
        let host = match req.uri().authority() {
            Some(a) => sanitize(a.as_str().as_bytes(), 255),
            None => req
                .headers()
                .get(header::HOST)
                .map_or_else(|| "-".to_string(), |h| sanitize(h.as_bytes(), 255)),
        };
        AccessInfo {
            method: req.method().clone(),
            host,
            path: sanitize(req.uri().path().as_bytes(), MAX_LOGGED_PATH),
            version: req.version(),
            peer: peer.ip(),
            proto,
            request_id: request_id.clone(),
        }
    });
    // Bounded fields only: never the raw path.
    // Skipped entirely when neither the access log nor request ids are on.
    let span = if log.is_some() || echo_id {
        tracing::info_span!(
            target: "ferryman::proxy",
            "request",
            method = %req.method(),
            route = tracing::field::Empty,
            upstream = tracing::field::Empty,
        )
    } else {
        tracing::Span::none()
    };
    let cx = Cx { started, log };
    let Ok(mut resp) = handle_core(table, client, conn, req, policy, wrap, &cx)
        .instrument(span)
        .await;
    // Only with `request_id = true`; a client-supplied id is never echoed otherwise.
    if let Some(v) = request_id
        .filter(|_| echo_id)
        .and_then(|id| HeaderValue::from_str(&id).ok())
    {
        resp.headers_mut().entry("x-request-id").or_insert(v);
    }
    Ok(resp)
}

/// Shared core. `wrap` turns the inbound body into the client's body type and
/// optionally returns an end-of-body signal; without one (legacy), the
/// upstream timeout starts immediately and only counts for bodyless requests.
#[allow(clippy::too_many_arguments)]
async fn handle_core<B>(
    table: Arc<RouteTable>,
    client: Client<HttpConnector, B>,
    conn: &ConnCtx,
    req: Request<Incoming>,
    policy: &Policy,
    wrap: impl FnOnce(Incoming, Duration, Duration, Option<u64>) -> (B, Option<Eos>),
    cx: &Cx,
) -> Result<Response<ResponseBody>, Infallible>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (peer, proto) = (conn.peer, conn.proto());
    // Answered before bad_path/lookup: no upstream, no breaker. Exact match
    // on the raw path. Metric label is the fixed "local_health".
    if table.local_health_path() == Some(req.uri().path())
        && matches!(*req.method(), http::Method::GET | http::Method::HEAD)
    {
        record(cx, "local_health", "none", 200);
        let body = if req.method() == http::Method::HEAD {
            ""
        } else {
            "ok"
        };
        return Ok(error_response(StatusCode::OK, body));
    }

    if ferryman_core::path::bad_path_normalized(req.uri().path())
        || ferryman_core::path::ambiguous_route(&table, req.uri().path())
    {
        record(cx, "none", "none", 400);
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
    }

    if bad_host(req.headers(), req.uri(), req.version()) {
        record(cx, "none", "none", 400);
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad host\n"));
    }

    let Some(route) = table.lookup(req.uri().path()) else {
        record(cx, "none", "none", 404);
        return Ok(error_response(StatusCode::NOT_FOUND, "no route"));
    };
    let route_label = route.prefix.clone();
    let upstream = route.upstream.clone();
    let span = tracing::Span::current();
    span.record("route", route_label.as_str());
    span.record("upstream", &*upstream.name);
    let rewrite_host = route.rewrite_host;
    // bad_path/ambiguous_route already accepted the raw path; strip only
    // after them, on the normalised path, and re-check the result.
    let strip = if route.strip_prefix {
        match route.stripped_path(req.uri().path()) {
            Some(p) => Some((p, route.stripped_prefix().to_string())),
            None => {
                record(cx, "none", "none", 400);
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
        record(cx, &route_label, &upstream.name, 501);
        let h1 = req.version() <= http::Version::HTTP_11;
        let mut resp = error_response(
            StatusCode::NOT_IMPLEMENTED,
            "protocol upgrades are not supported",
        );
        // h1: close, so bytes pipelined behind the Upgrade/CONNECT are never
        // parsed as a request and forwarded.
        if h1 {
            resp.headers_mut()
                .insert(header::CONNECTION, http::HeaderValue::from_static("close"));
        }
        return Ok(resp);
    }

    // Declared length over the cap: refused before admission (no ticket).
    if let Some(max) = policy.max_request_body_bytes {
        let declared = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > max) {
            record(cx, &route_label, &upstream.name, 413);
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload too large",
            ));
        }
    }

    let Some(admission) = upstream.try_acquire() else {
        record(cx, &route_label, &upstream.name, 503);
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream unavailable",
        ));
    };

    // From here every exit gives a verdict or drops the ticket (release).
    let ticket = Ticket::new(upstream.clone(), admission);
    let (mut parts, body) = req.into_parts();
    // `upstream_timeout` starts at end-of-request-body (immediately when
    // bodyless), so a slow upload can't be mistaken for a slow upstream.
    let bodyless = body.is_end_stream();
    let (body, body_done) = wrap(
        body,
        table.request_body_idle_timeout(),
        table.request_body_timeout(),
        policy.max_request_body_bytes,
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
    let hp = &policy.headers;
    let forwarded_header = hp
        .forwarded_header
        .unwrap_or_else(|| table.forwarded_header());
    let client_host: Option<String> = (forwarded_header || !hp.asserted.is_empty())
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
                    ticket.release();
                    record(cx, &route_label, &upstream.name, 400);
                    return Ok(error_response(StatusCode::BAD_REQUEST, "bad path\n"));
                }
            }
        }
        None => parts.uri.path_and_query().cloned(),
    };
    parts.uri = match http::Uri::from_parts(up_parts) {
        Ok(uri) => uri,
        Err(e) => {
            ticket.release();
            record(cx, &route_label, &upstream.name, 502);
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
        client_host.as_deref(),
        &FwdCfg {
            trusted: hp
                .trusted_proxies
                .as_ref()
                .unwrap_or_else(|| table.trusted_proxies()),
            forwarded_header,
            xff: hp.xff.unwrap_or_else(|| table.xff_mode()),
            asserted: &hp.folded,
        },
    );

    // After apply_forwarded_headers (it strips untrusted values).
    if let Some((_, prefix)) = strip.as_ref().filter(|(_, p)| !p.is_empty()) {
        set_forwarded_prefix(&mut parts.headers, prefix);
    }
    // Asserted names are the embedder's alone: drop whatever is there now.
    for n in &hp.asserted {
        parts.headers.remove(n);
    }

    let fwd = Request::from_parts(parts, body);

    let upstream_timeout = table.upstream_timeout;
    // A client that left its upload idle for >= theta (see `stall_threshold`)
    // may have tripped the upstream's own body-read timeout; its 502/5xx then
    // isn't evidence.
    let stall_theta = stall_threshold(table.request_body_idle_timeout());
    let request_state = body_done.as_ref().map(|e| e.state.clone());
    let client_stalled = || {
        request_state
            .as_ref()
            .is_some_and(|s| s.stalled_during_request())
    };
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
                    if !state.polled.swap(false, Ordering::AcqRel)
                        && !state.waiting_on_client()
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
                ticket.failure();
            } else {
                ticket.abandon();
            }
            record(cx, &route_label, &upstream.name, 504);
            Ok(error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream timeout",
            ))
        }
        Some(Err(e)) => {
            // Client-caused failures never reach the breaker: no verdict (not
            // `failure`, and not `success`, which would let a client close an
            // open breaker by aborting an upload).
            if let Some(fault) = client_fault(&e) {
                ticket.release();
                let (status, msg) = match fault {
                    ClientFault::Timeout => {
                        tracing::debug!(target: "ferryman::proxy", "client request body idle timeout");
                        (StatusCode::REQUEST_TIMEOUT, "request body timeout")
                    }
                    ClientFault::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
                    ClientFault::BodyError => {
                        tracing::debug!(target: "ferryman::proxy", error = %e, "client request body failed");
                        (StatusCode::BAD_REQUEST, "request body error")
                    }
                };
                record(cx, &route_label, &upstream.name, status.as_u16());
                return Ok(error_response(status, msg));
            }
            if client_stalled() {
                ticket.release();
            } else {
                ticket.failure();
            }
            tracing::warn!(target: "ferryman::proxy", upstream = %upstream.name, error = %e, "upstream request failed");
            record(cx, &route_label, &upstream.name, 502);
            Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"))
        }
        Some(Ok(resp)) => {
            let status = resp.status();
            let (mut rparts, rbody) = resp.into_parts();
            // 502-504 heads mean "this upstream is unhealthy" (a 500 is one
            // request's bug); a gateway answering them after the client
            // stalled its upload is passing on its backend's reaction.
            // Any other head: a probe succeeds now. A normal admission
            // succeeds now under `HeadOnly` (the body is streamed afterwards
            // with no verdict), or when the body is empty (it may never be
            // polled); otherwise `DeferNormalToBodyEnd` moves the ticket into
            // the `WatchedBody`.
            let mut head_failed = false;
            let mut deferred = None;
            if matches!(status.as_u16(), 502..=504) {
                if client_stalled() {
                    ticket.release();
                } else {
                    ticket.failure();
                    head_failed = true;
                }
            } else if policy.verdict == VerdictPolicy::DeferNormalToBodyEnd
                && ticket.admission() == Admission::Normal
                && !rbody.is_end_stream()
            {
                deferred = Some(ticket);
            } else {
                ticket.success();
            }
            let len = rparts
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            record_len(cx, &route_label, &upstream.name, status.as_u16(), len);

            strip_hop_by_hop(&mut rparts.headers);
            let rbody = match policy.verdict {
                VerdictPolicy::HeadOnly => rbody.boxed(),
                VerdictPolicy::DeferNormalToBodyEnd => WatchedBody {
                    inner: rbody,
                    upstream: upstream.clone(),
                    ticket: deferred,
                    request: request_state.clone(),
                    stall: stall_theta,
                    failed: head_failed,
                    // The gap between the head and the first body poll counts.
                    ready_at: Some(tokio::time::Instant::now()),
                    client_stalled_reading: false,
                }
                .boxed(),
            };
            Ok(Response::from_parts(rparts, rbody))
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
        // `ensure_request_id` ran first; the generated id must survive too.
        if name == header::HOST || name == "x-request-id" {
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
    // What follows the host (after `]` for an IPv6 literal) must be empty or
    // `:` + 1-5 digits <= 65535; `Authority` alone accepts `x:000080`, `[::1]x`.
    let Ok(s) = std::str::from_utf8(b) else {
        return false;
    };
    let rest = if s.starts_with('[') {
        s.find(']').map(|i| &s[i + 1..])
    } else {
        Some(s.find(':').map_or("", |i| &s[i..]))
    };
    match rest {
        Some("") => true,
        Some(r) => r.strip_prefix(':').is_some_and(|p| {
            p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit()) && p.parse::<u16>().is_ok()
        }),
        None => false,
    }
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
    /// Embedder-asserted names: folded like the trust-governed set.
    asserted: &'a [(String, String)],
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

/// Call once per connection (or request): with no `trusted_proxies`, a
/// private-range peer is probably a proxy/LB whose forwarding headers are
/// being discarded; logs one warning per process.
pub fn warn_if_private_peer(table: &RouteTable, peer: SocketAddr) {
    if table.trusted_proxies().is_empty() && is_private_peer(peer.ip()) {
        warn_private_peer_once();
    }
}

fn warn_private_peer_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "ferryman::proxy",
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

/// Fold every non-alphanumeric byte to `-`, collapse runs, trim the ends.
fn fold_name(name: &str) -> String {
    let mut folded = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            folded.push(c);
        } else if !folded.is_empty() && !folded.ends_with('-') {
            folded.push('-');
        }
    }
    folded.truncate(folded.trim_end_matches('-').len());
    folded
}

/// Is `name` (already lowercase) a non-canonical spelling of a trust-governed
/// or asserted header? Runs of separators are collapsed and the ends trimmed
/// (`x__real_ip`, `-x-real-ip` -> `x-real-ip`). Asserted names are folded the
/// same way before comparing, so an asserted `x_tenant_id` or `X-Tenant--Id`
/// also catches the client's `x-tenant-id` and `x.tenant.id`. A name equal to
/// an asserted name is that name's own spelling; it is removed by the
/// canonical removal step, not here.
fn is_noncanonical_spelling(name: &str, asserted: &[(String, String)]) -> bool {
    let normal = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    // Normal names are canonical for the built-in set (exact matches are
    // kept); for asserted names only a different spelling folding to it counts.
    let hit = |folded: &str| {
        !TRUST_GOVERNED.contains(&folded)
            && asserted.iter().any(|(raw, f)| raw != name && f == folded)
    };
    if normal {
        return hit(name);
    }
    let folded = fold_name(name);
    let governed = TRUST_GOVERNED.contains(&folded.as_str());
    // An asserted alias (`x_forwarded_for`) must not strip a trusted peer's
    // canonical trust-governed header, which `normal` names are exempt from.
    governed || hit(&folded)
}

fn fold_asserted(names: &[HeaderName]) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| (n.as_str().to_owned(), fold_name(n.as_str())))
        .collect()
}

/// Remove, for every peer, non-canonical spellings (`X_Real_IP`, `x.forwarded.for`)
/// of trust-governed headers.
fn strip_noncanonical_spellings(headers: &mut HeaderMap, asserted: &[(String, String)]) {
    let bad: Vec<HeaderName> = headers
        .keys()
        .filter(|k| is_noncanonical_spelling(k.as_str(), asserted))
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
/// is set from the connection,
/// `x-forwarded-for` gets the peer appended, `x-real-ip` is overwritten with
/// the peer, and client `forwarded`, `x-forwarded-host`, `-ssl`, `-scheme`,
/// `-prefix` and `-port` are stripped. `x-forwarded-port` is never synthesised.
///
/// Trusted peer: incoming `x-forwarded-proto` / `x-forwarded-port`
/// (rightmost value, port must be valid), `x-forwarded-host`, `-ssl`,
/// `-scheme`, `-prefix` and `forwarded` are kept; a missing or invalid proto
/// comes from the connection, a missing or invalid port is dropped. `x-real-ip` is always
/// overwritten with the rightmost `x-forwarded-for` entry that is not a
/// trusted proxy (`:port` suffixes tolerated; an unparsable entry stops the
/// walk), else the peer. `x-forwarded-for` gets the peer appended
/// (`xff = "append"`) or is set to that client address (`"replace"`).
///
/// With `forwarded_header`, a `for=<peer>;proto=;host=` element is appended
/// to `forwarded` (fresh for untrusted peers, whose value was stripped).
/// `proto=` is this hop's connection scheme;
/// `host=` is the client's effective host (URI authority, else `Host`).
fn apply_forwarded_headers(
    headers: &mut HeaderMap,
    peer: IpAddr,
    proto: &'static str,
    client_host: Option<&str>,
    cfg: &FwdCfg<'_>,
) {
    strip_noncanonical_spellings(headers, cfg.asserted);
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
        let port = rightmost(headers, &xport).and_then(|v| {
            v.parse::<u16>()
                .ok()
                .filter(|p| *p != 0 && v.bytes().all(|b| b.is_ascii_digit()))
        });
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
        headers.remove(&xport);
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

fn record(cx: &Cx, route: &str, upstream: &str, status: u16) {
    record_len(cx, route, upstream, status, None);
}

/// Metrics plus the access log line. `bytes_out` is the response
/// `Content-Length` when known (streamed bodies are not counted).
fn record_len(cx: &Cx, route: &str, upstream: &str, status: u16, bytes_out: Option<u64>) {
    let started = cx.started;
    if let Some(a) = &cx.log {
        tracing::info!(
            target: "ferryman::access",
            method = %a.method,
            host = %a.host,
            path = %a.path,
            route,
            upstream,
            status,
            bytes_out,
            duration_ms = started.elapsed().as_secs_f64() * 1000.0,
            request_id = a.request_id.as_deref(),
            peer = %a.peer,
            scheme = a.proto,
            version = ?a.version,
            "request"
        );
    }
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
            b"[::1]x",
            b"x:000080",
            b"[::1]:000080",
            b"x:65536",
            b"[::1]:65536",
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
            "x:00080",
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
            asserted: &[],
        }
    }

    fn host_of<'a>(h: &[(&str, &'a str)]) -> Option<&'a str> {
        h.iter().find(|(k, _)| *k == "host").map(|(_, v)| *v)
    }

    fn run_with(
        trusted: &[&str],
        peer: &str,
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
        let m = run_with(&[], "1.2.3.4", false, XffMode::Append, &h);
        for k in [
            "x-forwarded-ssl",
            "x-forwarded-scheme",
            "x-forwarded-prefix",
        ] {
            assert_eq!(m.get_all(k).iter().count(), 0, "{k}");
        }
        assert_eq!(m.get_all("x-forwarded-port").iter().count(), 0);
    }

    #[test]
    fn trusted_keeps_ssl_scheme_prefix_and_valid_port() {
        let m = run_with(&["10.0.0.0/8"], "10.1.2.3", false, XffMode::Append, &HOP);
        assert_eq!(get(&m, "x-forwarded-ssl"), Some("on"));
        assert_eq!(get(&m, "x-forwarded-scheme"), Some("https"));
        assert_eq!(get(&m, "x-forwarded-prefix"), Some("/evil"));
        assert_eq!(get(&m, "x-forwarded-port"), Some("4443"));
        let h = [
            ("x-forwarded-port", "80"),
            ("x-forwarded-port", "443, 8443"),
        ];
        let m = run_with(&["10.0.0.0/8"], "10.1.2.3", false, XffMode::Append, &h);
        assert_eq!(get(&m, "x-forwarded-port"), Some("8443"));
        for bad in ["abc", "0", "70000", "-1", "+80", " "] {
            let h = [("x-forwarded-port", bad)];
            let m = run_with(&["10.0.0.0/8"], "10.1.2.3", false, XffMode::Append, &h);
            assert_eq!(get(&m, "x-forwarded-port"), None, "{bad:?}");
        }
    }

    #[test]
    fn xff_replace_sets_client_only() {
        let h = [("x-forwarded-for", "6.6.6.6, 5.5.5.5")];
        let m = run_with(&[], "1.2.3.4", false, XffMode::Replace, &h);
        assert_eq!(get(&m, "x-forwarded-for"), Some("1.2.3.4"));
        assert_eq!(m.get_all("x-forwarded-for").iter().count(), 1);
        let m = run_with(&["10.0.0.0/8"], "10.1.2.3", false, XffMode::Replace, &h);
        assert_eq!(get(&m, "x-forwarded-for"), Some("5.5.5.5"));
        assert_eq!(get(&m, "x-real-ip"), Some("5.5.5.5"));
        let m = run_with(&[], "1.2.3.4", false, XffMode::Append, &h);
        assert_eq!(
            get(&m, "x-forwarded-for"),
            Some("6.6.6.6, 5.5.5.5, 1.2.3.4")
        );
    }

    #[test]
    fn forwarded_header_output() {
        let host = [("host", "app.example")];
        let m = run_with(&[], "1.2.3.4", true, XffMode::Append, &host);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=1.2.3.4;proto=http;host=app.example")
        );
        // untrusted: client Forwarded is stripped, element is fresh
        let h = [("host", "app.example:8080"), ("forwarded", "for=6.6.6.6")];
        let m = run_with(&[], "1.2.3.4", true, XffMode::Append, &h);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=1.2.3.4;proto=http;host=\"app.example:8080\"")
        );
        // trusted: appended to the list; proto= is this hop's connection scheme
        let h = [("forwarded", "for=6.6.6.6"), ("x-forwarded-proto", "https")];
        let m = run_with(&["10.0.0.0/8"], "10.1.2.3", true, XffMode::Append, &h);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=6.6.6.6, for=10.1.2.3;proto=http")
        );
        // IPv6 quoted + bracketed (RFC 7239 section 6); v4-mapped shown as v4
        let m = run_with(&[], "2001:db8::1", true, XffMode::Append, &[]);
        assert_eq!(
            get(&m, "forwarded"),
            Some("for=\"[2001:db8::1]\";proto=http")
        );
        let m = run_with(&[], "::ffff:1.2.3.4", true, XffMode::Append, &[]);
        assert_eq!(get(&m, "forwarded"), Some("for=1.2.3.4;proto=http"));
        // off by default: untrusted Forwarded just stripped
        let m = run_with(&[], "1.2.3.4", false, XffMode::Append, &h);
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
            ("-x-real-ip", "6.6.6.6"),
            ("x-forwarded-for--", "6.6.6.6"),
            ("x--forwarded--for-", "6.6.6.6"),
            ("-forwarded", "for=6.6.6.6"),
            ("x--custom", "keep"),
            ("x.custom", "keep"),
            ("x_forwarded-host", "evil.example"),
            ("x_forwarded_proto", "https"),
        ];
        for trusted in [&[][..], &["10.0.0.0/8"][..]] {
            let m = run_with(trusted, "10.1.2.3", false, XffMode::Append, &h);
            for k in ["x_real_ip", "x_forwarded_prefix", "x_forwarded_for"] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            for k in [
                "x.forwarded.for",
                "x.real.ip",
                "x-forwarded.proto",
                "x_forwarded-host",
                "x_forwarded_proto",
            ] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            for k in [
                "x__forwarded_for",
                "x-.forwarded.for",
                "-x-real-ip",
                "x-forwarded-for--",
                "x--forwarded--for-",
                "-forwarded",
            ] {
                assert_eq!(get(&m, k), None, "{k}");
            }
            assert_eq!(get(&m, "x.custom"), Some("keep"));
            assert_eq!(get(&m, "x_custom"), Some("keep"));
            assert_eq!(get(&m, "x--custom"), Some("keep"));
            assert_eq!(get(&m, "x-forwarded-for"), Some("10.1.2.3"));
        }
    }

    #[test]
    fn override_xff_trusted_and_forwarded_header_feed_fwdcfg() {
        // The overrides reach apply_forwarded_headers through FwdCfg: Replace
        // writes the client only, an empty trust list ignores forged values,
        // forwarded_header = true appends `Forwarded`.
        let h = [
            ("x-forwarded-for", "6.6.6.6"),
            ("x-forwarded-proto", "https"),
        ];
        let hdrs = |h: &[(&str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in h {
                m.append(
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            m
        };
        let hp = HeaderPolicy {
            xff: Some(XffMode::Replace),
            trusted_proxies: Some(TrustedProxies::default()),
            forwarded_header: Some(true),
            ..HeaderPolicy::default()
        };
        // Table says: trust 10/8, append, no Forwarded.
        let table_trusted = tp(&["10.0.0.0/8"]);
        let mut m = hdrs(&h);
        let t = hp.trusted_proxies.as_ref().unwrap_or(&table_trusted);
        apply_forwarded_headers(
            &mut m,
            "10.1.2.3".parse().unwrap(),
            "http",
            Some("h"),
            &FwdCfg {
                trusted: t,
                forwarded_header: hp.forwarded_header.unwrap_or(false),
                xff: hp.xff.unwrap_or(XffMode::Append),
                asserted: &[],
            },
        );
        assert_eq!(get(&m, "x-forwarded-for"), Some("10.1.2.3"));
        assert_eq!(get(&m, "x-forwarded-proto"), Some("http"));
        assert!(get(&m, "forwarded").unwrap().contains("for=10.1.2.3"));
        // No overrides: the table's trust applies, so the forged proto is kept.
        let m = run(&["10.0.0.0/8"], "10.1.2.3", "http", &h);
        assert_eq!(get(&m, "x-forwarded-proto"), Some("https"));
        assert_eq!(get(&m, "forwarded"), None);
    }

    #[test]
    fn asserted_names_join_the_noncanonical_fold() {
        let t = tp(&[]);
        let asserted = fold_asserted(&[HeaderName::from_static("x-tenant")]);
        let mut m = HeaderMap::new();
        for k in ["x_tenant", "X.Tenant", "-x-tenant", "x--tenant", "x_other"] {
            m.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                "v".parse().unwrap(),
            );
        }
        m.append("x-tenant", "canonical".parse().unwrap());
        apply_forwarded_headers(
            &mut m,
            "1.2.3.4".parse().unwrap(),
            "http",
            None,
            &FwdCfg {
                trusted: &t,
                forwarded_header: false,
                xff: XffMode::Append,
                asserted: &asserted,
            },
        );
        for k in ["x_tenant", "x.tenant", "-x-tenant", "x--tenant"] {
            assert_eq!(get(&m, k), None, "{k}");
        }
        assert_eq!(get(&m, "x_other"), Some("v"));
        // The canonical name is the caller's step-21 business, not the fold's.
        assert_eq!(get(&m, "x-tenant"), Some("canonical"));
        // Without `asserted` the same spellings survive.
        assert!(!is_noncanonical_spelling("x_tenant", &[]));
        assert!(is_noncanonical_spelling("x_tenant", &asserted));
    }

    #[test]
    fn asserted_alias_never_strips_trusted_canonical_header() {
        let asserted = fold_asserted(&[HeaderName::from_static("x_forwarded_for")]);
        assert!(!is_noncanonical_spelling("x-forwarded-for", &asserted));
        let t = tp(&["10.0.0.0/8"]);
        let mut m = HeaderMap::new();
        m.append("x-forwarded-for", "9.9.9.9".parse().unwrap());
        apply_forwarded_headers(
            &mut m,
            "10.1.2.3".parse().unwrap(),
            "http",
            None,
            &FwdCfg {
                trusted: &t,
                forwarded_header: false,
                xff: XffMode::Append,
                asserted: &asserted,
            },
        );
        assert_eq!(get(&m, "x-forwarded-for"), Some("9.9.9.9, 10.1.2.3"));
    }

    #[test]
    fn asserted_names_are_folded_before_comparing() {
        for asserted_raw in ["x_tenant_id", "X-Tenant--Id"] {
            let asserted =
                fold_asserted(&[HeaderName::from_bytes(asserted_raw.as_bytes()).unwrap()]);
            for client in ["x-tenant-id", "x.tenant.id", "x__tenant-id"] {
                assert!(
                    is_noncanonical_spelling(client, &asserted),
                    "{asserted_raw} vs {client}"
                );
            }
            assert!(!is_noncanonical_spelling("x-tenant", &asserted));
            assert!(!is_noncanonical_spelling("x-tenant-ids", &asserted));
        }
        // End to end: the client's normal-looking spelling is dropped.
        let asserted = fold_asserted(&[HeaderName::from_static("x_tenant_id")]);
        let mut m = HeaderMap::new();
        m.append("x-tenant-id", "evil".parse().unwrap());
        m.append("x.tenant.id", "evil".parse().unwrap());
        strip_noncanonical_spellings(&mut m, &asserted);
        assert!(m.is_empty(), "{m:?}");
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
    fn connection_header_cannot_strip_host_or_request_id() {
        let mut h = HeaderMap::new();
        h.insert("connection", "x-request-id, host, x-other".parse().unwrap());
        h.insert("host", "a".parse().unwrap());
        h.insert("x-request-id", "abc".parse().unwrap());
        h.insert("x-other", "1".parse().unwrap());
        strip_hop_by_hop(&mut h);
        assert!(h.contains_key("host") && h.contains_key("x-request-id"));
        assert!(!h.contains_key("x-other") && !h.contains_key("connection"));
    }

    #[test]
    fn forwarded_port_never_derived_from_host() {
        let h = [("host", "x:443")];
        let m = run_with(&[], "1.2.3.4", false, XffMode::Append, &h);
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
