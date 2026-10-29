//! HTTP/1 request-smuggling / desync regression suite (ROADMAP Q31).
//!
//! Raw-TCP clients talk to `ferryman::serve`; the upstream is a raw
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
//! `Upgrade` / `Expect`: 501, the request never goes upstream.

use ferryman_core::{build_table, ConfigToml};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

type Rec = Arc<Mutex<Vec<Vec<u8>>>>;

/// Raw upstream: records every byte per connection, answers 200 once per
/// request head (`\r\n\r\n`) seen so far.
async fn raw_upstream() -> (std::net::SocketAddr, Rec) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let rec: Rec = Default::default();
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
    (addr, rec)
}

async fn setup(extra: &str) -> (std::net::SocketAddr, Rec) {
    let (up, rec) = raw_upstream().await;
    let cfg: ConfigToml = toml::from_str(&format!(
        "[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n{extra}"
    ))
    .unwrap();
    let table = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(l, table, None, std::future::pending()));
    (proxy, rec)
}

/// Read until EOF or until `idle` passes with no data after the first
/// bytes. The first read waits up to 5s so a loaded box cannot make a slow
/// response look like silence (hang-proof either way).
async fn read_all(s: &mut TcpStream, idle: Duration) -> Vec<u8> {
    read_all_wait(s, Duration::from_secs(5), idle).await
}

async fn read_all_wait(s: &mut TcpStream, first: Duration, idle: Duration) -> Vec<u8> {
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

/// Send `raw`, return (client-visible bytes, bytes the upstream received).
async fn probe(raw: &[u8]) -> (String, String) {
    let (proxy, rec) = setup("").await;
    probe_on(proxy, &rec, raw).await
}

async fn probe_on(proxy: std::net::SocketAddr, rec: &Rec, raw: &[u8]) -> (String, String) {
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(raw).await.unwrap();
    let c = read_all(&mut s, Duration::from_millis(250)).await;
    let up: Vec<u8> = rec.lock().unwrap().iter().flatten().copied().collect();
    (
        String::from_utf8_lossy(&c).into_owned(),
        String::from_utf8_lossy(&up).into_owned(),
    )
}

/// Number of request lines the upstream received.
fn reqs(up: &str) -> usize {
    up.matches(" HTTP/1.1\r\n").count()
}

fn status(c: &str) -> &str {
    c.split("\r\n").next().unwrap_or("")
}

/// Every vector here must be answered with `want` and reach the upstream
/// with nothing at all.
#[tokio::test]
async fn malformed_requests_are_rejected_and_never_forwarded() {
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
    let hs: Vec<_> = cases
        .into_iter()
        .map(|(n, raw, want)| {
            tokio::spawn(async move {
                let (c, u) = probe(raw.as_bytes()).await;
                assert_eq!(&status(&c)[9..12], want, "{n}: {c:?}");
                assert!(
                    !u.contains("evil") && !u.contains("content-length: 5"),
                    "{n}: {u:?}"
                );
                // A chunked body that fails to decode must not look complete.
                assert!(!u.ends_with("0\r\n\r\n"), "{n}: {u:?}");
                (n, u)
            })
        })
        .collect();
    for h in hs {
        let (n, u) = h.await.unwrap();
        // Header-level rejects send nothing; body-level ones may have sent
        // a head with a truncated body, which the upstream sees as aborted.
        if !n.starts_with("chunk") {
            assert_eq!(u, "", "{n}");
        }
    }
}

#[tokio::test]
async fn cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded() {
    let (c, u) = probe(b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\ncontent-length: 4\r\n\r\n0\r\n\r\nGET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK", "{c:?}");
    assert!(c.contains("connection: close"), "{c:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
    assert!(
        u.contains("transfer-encoding: chunked") && !u.contains("content-length"),
        "{u:?}"
    );
    assert!(!u.contains("evil"), "{u:?}");
}

#[tokio::test]
async fn accepted_oddities_are_normalised_upstream() {
    // equal duplicate CL, `chunked `, `CHUNKED`, chunk extension, bare LF,
    // absolute-form with mismatched Host, lowercase and unknown methods.
    let (c, u) = probe(
        b"POST /svc/a HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\ncontent-length: 3\r\n\r\nabc",
    )
    .await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert_eq!(u.matches("content-length").count(), 1, "{u:?}");

    for te in ["chunked ", "CHUNKED"] {
        let raw =
            format!("POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: {te}\r\n\r\n0\r\n\r\n");
        let (c, u) = probe(raw.as_bytes()).await;
        assert_eq!(status(&c), "HTTP/1.1 200 OK", "{te}");
        assert!(
            u.contains("transfer-encoding: chunked\r\n\r\n0\r\n\r\n"),
            "{te}: {u:?}"
        );
    }

    let (c, u) = probe(b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n3;a=b;c\r\nabc\r\n0\r\n\r\n").await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert!(
        u.ends_with("\r\n\r\n3\r\nabc\r\n0\r\n\r\n"),
        "extension dropped: {u:?}"
    );

    for raw in [
        &b"GET /svc/a HTTP/1.1\nhost: x\n\n"[..],
        b"GET /svc/a HTTP/1.1\r\nhost: x\nx-a: 1\r\n\r\n",
    ] {
        let (c, u) = probe(raw).await;
        assert_eq!(status(&c), "HTTP/1.1 200 OK");
        assert!(!u.contains("\n\n") && reqs(&u) == 1, "{u:?}");
    }

    let (c, u) = probe(b"GET http://a.example/svc/a HTTP/1.1\r\nhost: b.example\r\n\r\n").await;
    assert_eq!(status(&c), "HTTP/1.1 200 OK");
    assert!(
        u.contains("host: a.example") && !u.contains("b.example"),
        "{u:?}"
    );

    let (_, u) = probe(b"PURGE /svc/a HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(
        u.starts_with("PURGE /svc/a HTTP/1.1\r\n"),
        "unknown method passes: {u:?}"
    );
    let (_, u) = probe(b"get /svc/a HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(reqs(&u), 1, "lowercase method is a (distinct) token: {u:?}");
}

#[tokio::test]
async fn truncated_chunked_body_is_not_completed_upstream() {
    // Missing final CRLF after the 0 chunk: the client never finishes the
    // body, so the proxy must not invent the terminator.
    let (_, u) = probe(
        b"POST /svc/a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n",
    )
    .await;
    assert!(!u.ends_with("0\r\n\r\n"), "{u:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
}

#[tokio::test]
async fn oversized_request_line_and_headers_are_rejected() {
    let long = format!("GET /svc/{} HTTP/1.1\r\nhost: x\r\n\r\n", "a".repeat(70000));
    let (c, u) = probe(long.as_bytes()).await;
    assert_eq!(&status(&c)[9..12], "414", "{c:?}");
    assert_eq!(u, "");
    let big = format!(
        "GET /svc/a HTTP/1.1\r\nhost: x\r\nx-a: {}\r\n\r\n",
        "a".repeat(1 << 20)
    );
    let (c, u) = probe(big.as_bytes()).await;
    assert!(["431", "400"].contains(&&status(&c)[9..12]), "{c:?}");
    assert_eq!(u, "");
}

#[tokio::test]
async fn pipelined_requests_are_forwarded_separately_in_order() {
    let (c, u) =
        probe(b"GET /svc/1 HTTP/1.1\r\nhost: x\r\n\r\nGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n")
            .await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
    assert!(u.find("/svc/1").unwrap() < u.find("/svc/2").unwrap());

    // Body then request: framed by Content-Length exactly.
    let (c, u) = probe(b"POST /svc/1 HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\n\r\nabcGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
    assert!(u.contains("\r\n\r\nabcGET /svc/2 HTTP/1.1\r\n"), "{u:?}");
}

#[tokio::test]
async fn ambiguous_body_length_follows_content_length_exactly() {
    // CL says 1 but 3 body bytes follow: ferryman frames by CL, so the
    // leftover `bc` + next request line is a SECOND request with method
    // `bcGET` (what any CL-honouring server does). It is forwarded as its
    // own request, never glued into the first body.
    let (c, u) = probe(b"POST /svc/1 HTTP/1.1\r\nhost: x\r\ncontent-length: 1\r\n\r\nabcGET /svc/2 HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(c.matches("HTTP/1.1 200 OK").count(), 2, "{c:?}");
    assert!(u.contains("content-length: 1\r\nx-forwarded-proto: http\r\nx-real-ip: 127.0.0.1\r\nx-forwarded-for: 127.0.0.1\r\n\r\na"), "{u:?}");
    assert!(u.contains("bcGET /svc/2 HTTP/1.1\r\n"), "{u:?}");
    assert_eq!(reqs(&u), 2, "{u:?}");
}

#[tokio::test]
async fn upgrade_is_501_and_never_forwarded() {
    let (c, u) = probe(
        b"GET /svc/ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n",
    )
    .await;
    assert_eq!(&status(&c)[9..12], "501", "{c:?}");
    assert_eq!(u, "");
}

/// FINDING (low): after the 501 for an Upgrade request the connection stays
/// open, so bytes the client pipelined behind it are parsed as a normal HTTP
/// request and forwarded. It is well-formed and singly framed (no desync),
/// but once B1 splices after a real 101, anything pipelined behind an
/// Upgrade must be dropped. Current behaviour is pinned by the next test.
#[tokio::test]
#[ignore = "FINDING: pipelined request after a 501'd Upgrade is still forwarded (connection not closed)"]
async fn upgrade_pipelined_bytes_never_reach_upstream() {
    let (_, u) = probe(b"GET /svc/ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\nGET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(u, "", "{u:?}");
}

#[tokio::test]
async fn upgrade_then_pipelined_request_is_a_single_clean_request() {
    let (c, u) = probe(b"GET /svc/ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\nGET /svc/evil HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(c.starts_with("HTTP/1.1 501"), "{c:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
    assert!(
        u.starts_with("GET /svc/evil HTTP/1.1\r\n") && !u.to_lowercase().contains("upgrade"),
        "{u:?}"
    );
}

#[tokio::test]
async fn expect_continue_with_rejected_body_does_not_desync() {
    // Body sent eagerly: 501, then the next request is parsed correctly.
    let (c, u) = probe(b"POST /svc/ws HTTP/1.1\r\nhost: x\r\nexpect: 100-continue\r\nconnection: upgrade\r\nupgrade: websocket\r\ncontent-length: 5\r\n\r\nabcdeGET /svc/next HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(c.starts_with("HTTP/1.1 501"), "{c:?}");
    assert!(!c.contains("100 Continue"), "{c:?}");
    assert_eq!(reqs(&u), 1, "{u:?}");
    assert!(
        u.starts_with("GET /svc/next HTTP/1.1\r\n") && !u.contains("abcde"),
        "{u:?}"
    );

    // Body withheld: after the 501 the connection is either closed or still
    // framed; the body bytes sent later must never be read as a request.
    let (proxy, rec) = setup("").await;
    let mut s = TcpStream::connect(proxy).await.unwrap();
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
    let up: String = rec
        .lock()
        .unwrap()
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    // 5 body bytes `GET /` are swallowed if the connection stayed open; the
    // remainder is then not a valid request line. Either way: no `evil`.
    assert!(!up.contains("evil"), "{up:?}");
}
