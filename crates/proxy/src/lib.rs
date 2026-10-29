//! ferryman-proxy: the per-request pipeline ([`pipeline`]) plus the shared
//! upstream client types. `ferryman` re-exports everything here, so its
//! public paths (`ferryman::proxy`, `ferryman::StreamingClient`, ...) are
//! unchanged.

pub mod pipeline;

use hyper::body::Incoming;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The 0.2.2 client type, kept for compatibility with [`pipeline::handle`].
#[deprecated(
    since = "0.2.3",
    note = "use `StreamingClient` with `proxy::handle_streaming`"
)]
pub type ProxyClient = Client<HttpConnector, Incoming>;

/// Shared hyper client used by `ferryman::serve` to forward requests to upstreams.
/// Bodies are streamed straight through, no buffering; the request body is
/// wrapped in [`pipeline::RequestBody`] (idle timeout, end-of-body signal).
pub type StreamingClient = Client<HttpConnector, pipeline::RequestBody>;

/// Latency buckets (seconds) for `ferryman_request_duration_seconds`,
/// spanning sub-millisecond proxy hops up to `upstream_timeout_secs`'s
/// default of 30s, so the slowest (about-to-time-out) requests still land
/// in a real bucket instead of falling into `+Inf`. The duration is measured
/// from request start to response headers, so it includes the request upload.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Build the upstream client used by [`StreamingClient`].
pub fn client() -> StreamingClient {
    let mut connector = HttpConnector::new();
    // Streamed bodies are many small writes; Nagle + delayed ACK would add
    // ~40ms stalls.
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new()).build(connector)
}
