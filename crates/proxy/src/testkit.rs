//! HTTP/1 request-smuggling / desync regression suite (ROADMAP Q31), shared
//! by ferryman and ferryman-edge.
//!
//! For test use; not covered by semver (changes only in minor releases).
//!
//! The caller serves the proxy and routes prefix `/svc` (any Host) to
//! [`RawUpstream::addr`]; [`run_smuggling`] then drives every vector through
//! `connect`, which opens a fresh client stream (TCP, TLS, ...) each call.
//!
//! Raw-TCP clients talk to the proxy under test; the upstream is a raw
//! `TcpListener` that records the exact bytes it receives, so every vector
//! asserts both what the client sees and what (if anything) was forwarded.
//! Behaviour is hyper 1.x / httparse's; these tests pin it so an upgrade or a
//! WebSocket splice (B1) cannot regress it.
//!
//! Rejected with 400 and nothing upstream: duplicate/invalid Content-Length
//! (`+3`, `3 3`, `0x3`, `3, 3`, differing duplicates), TE other than exactly
//! `chunked` (`chunked, identity`, `xchunked`, duplicated TE), TE on
//! HTTP/1.0, obs-fold, NUL/CR in values, space before the colon, request
//! line junk, bad chunk sizes (overflow, negative, non-hex, bare-LF
//! extension, missing chunk CRLF; these 400 after the head, never forwarded
//! as a complete request).
//! CL + TE: TE wins, Content-Length is dropped upstream, the connection is
//! closed, and a trailing smuggled request is never forwarded.
//! Normalised on forward (re-serialised by hyper, so no ambiguity): bare-LF
//! line endings, chunk extensions, `chunked ` / `CHUNKED`, absolute-form
//! (authority wins over Host), equal duplicate Content-Length.
//! Pipelining: requests are forwarded separately, responses in order.
//! `Upgrade` / `Expect`: 501 with `connection: close`; nothing, including pipelined bytes, goes upstream.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

/// Names of every vector, in run order; valid entries for `skip`.
pub const VECTORS: &[&str] = &[
    "malformed_requests_are_rejected_and_never_forwarded",
    "cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded",
    "accepted_oddities_are_normalised_upstream",
    "truncated_chunked_body_is_not_completed_upstream",
    "oversized_request_line_and_headers_are_rejected",
    "pipelined_requests_are_forwarded_separately_in_order",
    "ambiguous_body_length_follows_content_length_exactly",
    "upgrade_is_501_and_never_forwarded",
    "upgrade_pipelined_bytes_never_reach_upstream",
    "expect_continue_with_rejected_body_does_not_desync",
];

/// Raw upstream: records every byte per connection, answers 200 once per
/// request head (`\r\n\r\n`) seen so far.
pub struct RawUpstream {
    addr: SocketAddr,
    rec: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Per-connection byte counts at the last `reset` (pooled upstream
    /// connections are reused across vectors).
    base: Mutex<Vec<usize>>,
}

impl RawUpstream {
    pub async fn start() -> Self {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let rec: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let r = rec.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    continue;
                };
                let r = r.clone();
                let idx = {
                    let mut g = r.lock().unwrap();
                    g.push(Vec::new());
                    g.len() - 1
                };
                tokio::spawn(async move {
                    let mut answered = 0;
                    let mut buf = [0u8; 4096];
                    loop {
                        let Ok(n) = s.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        let heads = {
                            let mut g = r.lock().unwrap();
                            g[idx].extend_from_slice(&buf[..n]);
                            g[idx].windows(4).filter(|w| *w == b"\r\n\r\n").count()
                        };
                        while answered < heads {
                            answered += 1;
                            if s.write_all(OK).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Self {
            addr,
            rec,
            base: Default::default(),
        }
    }

    /// Address the proxy's `/svc` route must point at.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Forget everything recorded so far.
    fn reset(&self) {
        *self.base.lock().unwrap() = self.rec.lock().unwrap().iter().map(Vec::len).collect();
    }

    fn received(&self) -> Vec<Vec<u8>> {
        let b = self.base.lock().unwrap();
        let g = self.rec.lock().unwrap();
        (0..g.len())
            .map(|i| g[i][b.get(i).copied().unwrap_or(0)..].to_vec())
            .collect()
    }
}

/// Read until EOF or until `idle` passes with no data after the first
/// bytes. The first read waits up to 5s so a loaded box cannot make a slow
/// response look like silence (hang-proof either way).
pub async fn read_all<S: AsyncRead + Unpin>(s: &mut S, idle: Duration) -> Vec<u8> {
    read_all_wait(s, Duration::from_secs(5), idle).await
}

async fn read_all_wait<S: AsyncRead + Unpin>(
    s: &mut S,
    first: Duration,
    idle: Duration,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut wait = first;
    while let Ok(Ok(n)) = tokio::time::timeout(wait, s.read(&mut buf)).await {
        wait = idle;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Number of request lines the upstream received.
fn reqs(up: &str) -> usize {
    up.matches(" HTTP/1.1\r\n").count()
}

fn status(c: &str) -> &str {
    c.split("\r\n").next().unwrap_or("")
}

/// Fresh connection, send `raw`, return (client-visible bytes, bytes the
/// upstream received since the vector started).
async fn probe<C, F, S>(connect: &C, up: &RawUpstream, raw: &[u8]) -> (String, String)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    up.reset();
    let mut s = connect().await;
    s.write_all(raw).await.unwrap();
    let c = read_all(&mut s, Duration::from_millis(250)).await;
    let u: Vec<u8> = up.received().into_iter().flatten().collect();
    (
        String::from_utf8_lossy(&c).into_owned(),
        String::from_utf8_lossy(&u).into_owned(),
    )
}

/// Run every vector not named in `skip` (names from [`VECTORS`], e.g. to
/// leave out HTTP/1-only ones against an h2-only listener). Vectors run
/// sequentially; the vector name is printed before each (visible on failure).
pub async fn run_smuggling<C, F, S>(connect: C, upstream: &RawUpstream, skip: &[&str])
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    for name in VECTORS.iter().filter(|n| !skip.contains(n)) {
        run_vector(name, &connect, upstream).await;
    }
}

/// Run one vector by name (panics on an unknown name or a failed assertion).
pub async fn run_vector<C, F, S>(name: &str, connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    eprintln!("smuggling vector: {name}");
    match name {
        "malformed_requests_are_rejected_and_never_forwarded" => {
            malformed_requests_are_rejected_and_never_forwarded(connect, up).await
        }
        "cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded" => {
            cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded(connect, up).await
        }
        "accepted_oddities_are_normalised_upstream" => {
            accepted_oddities_are_normalised_upstream(connect, up).await
        }
        "truncated_chunked_body_is_not_completed_upstream" => {
            truncated_chunked_body_is_not_completed_upstream(connect, up).await
        }
        "oversized_request_line_and_headers_are_rejected" => {
            oversized_request_line_and_headers_are_rejected(connect, up).await
        }
        "pipelined_requests_are_forwarded_separately_in_order" => {
            pipelined_requests_are_forwarded_separately_in_order(connect, up).await
        }
        "ambiguous_body_length_follows_content_length_exactly" => {
            ambiguous_body_length_follows_content_length_exactly(connect, up).await
        }
        "upgrade_is_501_and_never_forwarded" => {
            upgrade_is_501_and_never_forwarded(connect, up).await
        }
        "upgrade_pipelined_bytes_never_reach_upstream" => {
            upgrade_pipelined_bytes_never_reach_upstream(connect, up).await
        }
        "expect_continue_with_rejected_body_does_not_desync" => {
            expect_continue_with_rejected_body_does_not_desync(connect, up).await
        }
        _ => panic!("unknown smuggling vector {name:?}"),
    }
}

async fn malformed_requests_are_rejected_and_never_forwarded<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    const H: &str = "POST /svc/a HTTP/1.1\r\nhost: x\r\n";
    let evil = "GET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n";
    let cases: Vec<(&str, String, &str)> = vec![
        ("dup CL differing", format!("{H}content-length: 3\r\ncontent-length: 5\r\n\r\nabcde{evil}"), "400"),
        ("CL +3", format!("{H}content-length: +3\r\n\r\nabc"), "400"),
        ("CL '3 3'", format!("{H}content-length: 3 3\r\n\r\nabc"), "400"),
        ("CL hex", format!("{H}content-length: 0x3\r\n\r\nabc"), "400"),
        ("CL list", format!("{H}content-length: 3, 3\r\n\r\nabc"), "400"),
        ("TE chunked, identity", format!("{H}transfer-encoding: chunked, identity\r\n\r\n0\r\n\r\n{evil}"), "400"),
        ("TE xchunked", format!("{H}transfer-encoding: xchunked\r\n\r\n0\r\n\r\n{evil}"), "400"),
        ("TE duplicated", format!("{H}transfer-encoding: chunked\r\ntransfer-encoding: identity\r\n\r\n0\r\n\r\n{evil}"), "400"),
        ("TE on HTTP/1.0", format!("POST /svc/a HTTP/1.0\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n{evil}"), "400"),
        ("obs-fold value", "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a: 1\r\n  folded\r\n\r\n".into(), "400"),
        ("obs-fold first line", "GET /svc/a HTTP/1.1\r\n host: x\r\n\r\n".into(), "400"),
        ("NUL in value", "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a: a\0b\r\n\r\n".into(), "400"),
        ("CR in value", "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a: a\rb\r\n\r\n".into(), "400"),
        ("space before colon", "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a : 1\r\n\r\n".into(), "400"),
        ("space before colon CL", format!("{H}content-length : 3\r\n\r\nabc{evil}"), "400"),
        ("request-line junk", "GET /svc/a HTTP/1.1 junk\r\nhost: x\r\n\r\n".into(), "400"),
        ("chunk size overflow", format!("{H}transfer-encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFF\r\nabc\r\n0\r\n\r\n{evil}"), "400"),
        ("chunk size negative", format!("{H}transfer-encoding: chunked\r\n\r\n-1\r\nabc\r\n0\r\n\r\n{evil}"), "400"),
        ("chunk size non-hex", format!("{H}transfer-encoding: chunked\r\n\r\nzz\r\nabc\r\n0\r\n\r\n{evil}"), "400"),
        ("chunk ext bare LF", format!("{H}transfer-encoding: chunked\r\n\r\n3;a\nb\r\nabc\r\n0\r\n\r\n{evil}"), "400"),
        ("chunk data missing CRLF", format!("{H}transfer-encoding: chunked\r\n\r\n3\r\nabcXX0\r\n\r\n{evil}"), "400"),
    ];
    for (n, raw, want) in cases {
        let (c, u) = probe(connect, up, raw.as_bytes()).await;
        assert_eq!(&status(&c)[9..12], want, "{n}: {c:?}");
        assert!(
            !u.contains("evil") && !u.contains("content-length: 5"),
            "{n}: {u:?}"
        );
        // A chunked body that fails to decode must not look complete.
        assert!(!u.ends_with("0\r\n\r\n"), "{n}: {u:?}");
        // Header-level rejects send nothing; body-level ones may have sent
        // a head with a truncated body, which the upstream sees as aborted.
        if !n.starts_with("chunk") {
            assert_eq!(u, "", "{n}");
        }
    }
}

async fn cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded<C, F, S>(
    connect: &C,
    up: &RawUpstream,
) where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (c, u) = probe(connect, up, b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\ncontent-length: 4\r\n\r\n0\r\n\r\nGET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK", "{c:?}");
    assert!(c.contains("connection: close"), "{c:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
    assert!(
        u.contains("transfer-encoding: chunked") && !u.contains("content-length"),
        "{u:?}"
    );
    assert!(!u.contains("evil"), "{u:?}");
}

async fn accepted_oddities_are_normalised_upstream<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    // equal duplicate CL, `chunked `, `CHUNKED`, chunk extension, bare LF,
    // absolute-form with mismatched Host, lowercase and unknown methods.
    let (c, u) = probe(
        connect,
        up,
        b"POST /svc/a HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\ncontent-length: 3\r\n\r\nabc",
    )
    .await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert_eq!(u.matches("content-length").count(), 1, "{u:?}");

    for te in ["chunked ", "CHUNKED"] {
        let raw =
            format!("POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: {te}\r\n\r\n0\r\n\r\n");
        let (c, u) = probe(connect, up, raw.as_bytes()).await;
        assert_eq!(status(&c), "HTTP/1.1 200 OK", "{te}");
        assert!(
            u.contains("transfer-encoding: chunked\r\n\r\n0\r\n\r\n"),
            "{te}: {u:?}"
        );
    }

    let (c, u) = probe(connect, up, b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n3;a=b;c\r\nabc\r\n0\r\n\r\n").await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert!(
        u.ends_with("\r\n\r\n3\r\nabc\r\n0\r\n\r\n"),
        "extension dropped: {u:?}"
    );

    for raw in [
        &b"GET /svc/a HTTP/1.1\nhost: x\n\n"[..],
        b"GET /svc/a HTTP/1.1\r\nhost: x\nx-a: 1\r\n\r\n",
    ] {
        let (c, u) = probe(connect, up, raw).await;
        assert_eq!(status(&c), "HTTP/1.1 200 OK");
        assert!(!u.contains("\n\n") && reqs(&u) == 1, "{u:?}");
    }

    let (c, u) = probe(
        connect,
        up,
        b"GET http://a.example/svc/a HTTP/1.1\r\nhost: b.example\r\n\r\n",
    )
    .await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert!(
        u.contains("host: a.example") && !u.contains("b.example"),
        "{u:?}"
    );

    let (_, u) = probe(connect, up, b"PURGE /svc/a HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(
        u.starts_with("PURGE /svc/a HTTP/1.1\r\n"),
        "unknown method passes: {u:?}"
    );
    let (_, u) = probe(connect, up, b"get /svc/a HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(reqs(&u), 1, "lowercase method is a (distinct) token: {u:?}");
}

async fn truncated_chunked_body_is_not_completed_upstream<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Missing final CRLF after the 0 chunk: the client never finishes the
    // body, so the proxy must not invent the terminator.
    let (_, u) = probe(
        connect,
        up,
        b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n",
    )
    .await;
    assert!(!u.ends_with("0\r\n\r\n"), "{u:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
}

async fn oversized_request_line_and_headers_are_rejected<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let long = format!("GET /svc/{} HTTP/1.1\r\nhost: x\r\n\r\n", "a".repeat(70000));
    let (c, u) = probe(connect, up, long.as_bytes()).await;
    assert_eq!(&status(&c)[9..12], "414", "{c:?}");
    assert_eq!(u, "");
    let big = format!(
        "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a: {}\r\n\r\n",
        "a".repeat(1 << 20)
    );
    let (c, u) = probe(connect, up, big.as_bytes()).await;
    assert!(["431", "400"].contains(&&status(&c)[9..12]), "{c:?}");
    assert_eq!(u, "");
}

async fn pipelined_requests_are_forwarded_separately_in_order<C, F, S>(
    connect: &C,
    up: &RawUpstream,
) where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (c, u) = probe(
        connect,
        up,
        b"GET /svc/1 HTTP/1.1\r\nhost: x\r\n\r\nGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n",
    )
    .await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
    assert!(u.find("/svc/1").unwrap() < u.find("/svc/2").unwrap());

    // Body then request: framed by Content-Length exactly.
    let (c, u) = probe(connect, up, b"POST /svc/1 HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\n\r\nabcGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
    assert!(u.contains("\r\n\r\nabcGET /svc/2 HTTP/1.1\r\n"), "{u:?}");
}

async fn ambiguous_body_length_follows_content_length_exactly<C, F, S>(
    connect: &C,
    up: &RawUpstream,
) where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    // CL says 1 but 3 body bytes follow: ferryman frames by CL, so the
    // leftover `bc` + next request line is a SECOND request with method
    // `bcGET` (what any CL-honouring server does). It is forwarded as its
    // own request, never glued into the first body.
    let (c, u) = probe(connect, up, b"POST /svc/1 HTTP/1.1\r\nhost: x\r\ncontent-length: 1\r\n\r\nabcGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert!(u.contains("content-length: 1\r\nx-forwarded-proto: http\r\nx-real-ip: 127.0.0.1\r\nx-forwarded-for: 127.0.0.1\r\n\r\na"), "{u:?}");
    assert!(u.contains("bcGET /svc/2 HTTP/1.1\r\n"), "{u:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
}

async fn upgrade_is_501_and_never_forwarded<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (c, u) = probe(
        connect,
        up,
        b"GET /svc/ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n",
    )
    .await;
    assert_eq!(&status(&c)[9..12], "501", "{c:?}");
    assert_eq!(u, "");
}

async fn upgrade_pipelined_bytes_never_reach_upstream<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (c, u) = probe(connect, up, b"GET /svc/ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\nGET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(
        c.starts_with("HTTP/1.1 501") && c.contains("connection: close"),
        "{c:?}"
    );
    assert_eq!(c.matches("HTTP/1.1 ").count(), 1, "{c:?}");
    assert_eq!(u, "");
}

async fn expect_continue_with_rejected_body_does_not_desync<C, F, S>(connect: &C, up: &RawUpstream)
where
    C: Fn() -> F,
    F: Future<Output = S>,
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Body sent eagerly: 501 + close, the pipelined request is dropped.
    let (c, u) = probe(connect, up, b"POST /svc/ws HTTP/1.1\r\nhost: x\r\nexpect: 100-continue\r\nconnection: upgrade\r\nupgrade: websocket\r\ncontent-length: 5\r\n\r\nabcdeGET /svc/next HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(c.starts_with("HTTP/1.1 501"), "{c:?}");
    assert!(!c.contains("100 Continue"), "{c:?}");
    assert!(c.contains("connection: close"), "{c:?}");
    assert_eq!(u, "", "closed after 501: nothing pipelined is forwarded");

    // Body withheld: after the 501 the connection is either closed or still
    // framed; the body bytes sent later must never be read as a request.
    up.reset();
    let mut s = connect().await;
    s.write_all(b"POST /svc/ws HTTP/1.1\r\nhost: x\r\nexpect: 100-continue\r\nconnection: upgrade\r\nupgrade: websocket\r\ncontent-length: 5\r\n\r\n").await.unwrap();
    let first = read_all(&mut s, Duration::from_millis(250)).await;
    assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 501"));
    let _ = s
        .write_all(
            b"GET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\nGET /svc/next HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
    let _ = read_all_wait(
        &mut s,
        Duration::from_millis(250),
        Duration::from_millis(250),
    )
    .await;
    let up: String = up
        .received()
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    // 5 body bytes `GET /` are swallowed if the connection stayed open; the
    // remainder is then not a valid request line. Either way: no `evil`.
    assert!(!up.contains("evil"), "{up:?}");
}
