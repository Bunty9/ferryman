//! Per-request handler. Looks up the routing table, rebuilds the URI for
//! the chosen upstream, streams the request straight through via the shared
//! hyper client, records metrics, and turns transport failures into
//! circuit-breaker trips.

use ferryman_core::SharedTable;
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Incoming};
use hyper::{Request, Response};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Instant;

use crate::ProxyClient;

/// One response body type for both streamed upstream responses and locally
/// generated ones (404/502/503/504), so `handle`'s return type doesn't leak
/// the streaming vs. buffered distinction to callers.
pub type ResponseBody = BoxBody<Bytes, hyper::Error>;

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
pub async fn handle(
    table: SharedTable,
    client: ProxyClient,
    peer: SocketAddr,
    proto: &'static str,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    let started = Instant::now();
    // `load_full` (not `load`) because the returned `Arc` is held across
    // `.await` points below; `load`'s guard is meant for short critical
    // sections only.
    let table = table.load_full();

    let Some(route) = table.lookup(req.uri().path()) else {
        record(started, "none", "none", 404);
        return Ok(error_response(StatusCode::NOT_FOUND, "no route"));
    };
    let route_label = route.prefix.clone();
    let upstream = route.upstream.clone();

    // Upgrades (WebSocket etc.) need both hops spliced together, which this
    // proxy doesn't do; say so instead of forwarding a mangled plain GET.
    if req.headers().contains_key(header::UPGRADE) || req.method() == http::Method::CONNECT {
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
    // Only a request with no body to stream can blame a timeout on the
    // upstream; with a body, a slow client looks exactly like a slow server.
    let bodyless = body.is_end_stream();
    strip_hop_by_hop(&mut parts.headers);
    if parts.version == http::Version::HTTP_2 {
        join_cookies(&mut parts.headers);
    }
    // HTTP/2 carries the host in `:authority`, not `Host`; pin it before the
    // URI is rewritten so upstreams see the same Host for h1 and h2 clients.
    if !parts.headers.contains_key(header::HOST) {
        if let Some(v) = parts
            .uri
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
        {
            parts.headers.insert(header::HOST, v);
        }
    }

    let mut up_parts = upstream.uri.clone().into_parts();
    up_parts.path_and_query = parts.uri.path_and_query().cloned();
    parts.uri = match http::Uri::from_parts(up_parts) {
        Ok(uri) => uri,
        Err(e) => {
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
    append_forwarded_for(&mut parts.headers, peer.ip());
    parts.headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static(proto),
    );

    let fwd = Request::from_parts(parts, body);

    match tokio::time::timeout(table.upstream_timeout, client.request(fwd)).await {
        Err(_elapsed) => {
            if bodyless {
                upstream.record_failure(admission);
            }
            record(started, &route_label, &upstream.name, 504);
            Ok(error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream timeout",
            ))
        }
        Ok(Err(e)) if is_client_body_error(&e) => {
            // The client's request body failed (e.g. it hung up mid-upload).
            // Not the upstream's fault, so the breaker stays out of it.
            tracing::debug!(error = %e, "client request body failed");
            record(started, &route_label, &upstream.name, 400);
            Ok(error_response(
                StatusCode::BAD_REQUEST,
                "request body error",
            ))
        }
        Ok(Err(e)) => {
            upstream.record_failure(admission);
            tracing::warn!(upstream = %upstream.name, error = %e, "upstream request failed");
            record(started, &route_label, &upstream.name, 502);
            Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"))
        }
        Ok(Ok(resp)) => {
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

fn append_forwarded_for(headers: &mut HeaderMap, ip: std::net::IpAddr) {
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
