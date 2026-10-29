//! `Policy` driven through `Proxy::handle`: body cap (413, breaker untouched)
//! and `HeaderPolicy` overrides / `asserted`.

use ferryman_core::{build_table, ConfigToml, SharedTable, TrustedProxies, XffMode};
use ferryman_proxy::{ConnCtx, HeaderPolicy, Policy, Proxy};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::Response;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Upstream that drains the body and answers with the request head it saw.
async fn upstream(hits: Arc<AtomicUsize>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            let hits = hits.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let head = format!("{:?}", req.headers()).to_ascii_lowercase();
                        let _ = req.into_body().collect().await;
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(head))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    a
}

fn table(up: SocketAddr, extra: &str) -> SharedTable {
    let cfg: ConfigToml = toml::from_str(&format!(
        "failure_threshold = 1\n{extra}\n[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n"
    ))
    .unwrap();
    Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ))
}

async fn proxy_addr(t: SharedTable, policy: Policy) -> SocketAddr {
    let proxy = Proxy::new(t, policy);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, peer) = l.accept().await.unwrap();
            let (proxy, conn) = (proxy.clone(), Arc::new(ConnCtx::new(peer, addr)));
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let (p, c) = (proxy.clone(), conn.clone());
                    async move { Ok::<_, Infallible>(p.handle(req, &c).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    addr
}

/// Send raw bytes, return everything until close.
async fn raw(addr: SocketAddr, req: &str) -> String {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn capped(n: u64) -> Policy {
    let mut p = Policy::default();
    p.max_request_body_bytes = Some(n);
    p
}

const GET: &str = "GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n";

#[tokio::test]
async fn declared_over_limit_is_413_before_admission() {
    let hits = Arc::new(AtomicUsize::new(0));
    let up = upstream(hits.clone()).await;
    let p = proxy_addr(table(up, ""), capped(10)).await;
    let out = raw(
        p,
        "POST /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-length: 11\r\n\r\n12345678901",
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    assert!(out.contains("payload too large"), "{out}");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "never reached the upstream");
    // At the limit passes; breaker (threshold 1) is untouched.
    let ok = raw(
        p,
        "POST /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-length: 10\r\n\r\n1234567890",
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}

#[tokio::test]
async fn streamed_over_limit_is_413_and_breaker_untouched() {
    let hits = Arc::new(AtomicUsize::new(0));
    let up = upstream(hits.clone()).await;
    let p = proxy_addr(table(up, ""), capped(10)).await;
    let big = "chunked";
    let out = raw(
        p,
        &format!(
            "POST /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\ntransfer-encoding: {big}\r\n\r\n6\r\nabcdef\r\n6\r\nabcdef\r\n0\r\n\r\n"
        ),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    // failure_threshold = 1: a recorded failure would make this a 503.
    let ok = raw(p, GET).await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    // Small chunked body under the cap passes.
    let ok = raw(
        p,
        "POST /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}

#[tokio::test]
async fn default_policy_has_no_cap() {
    let up = upstream(Arc::default()).await;
    let p = proxy_addr(table(up, ""), Policy::default()).await;
    let body = "x".repeat(100_000);
    let out = raw(
        p,
        &format!(
            "POST /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
}

#[tokio::test]
async fn header_policy_overrides_beat_the_table() {
    let up = upstream(Arc::default()).await;
    // Table: trust loopback, append XFF, no Forwarded.
    let t = table(up, "trusted_proxies = [\"127.0.0.0/8\"]");
    let forged = "GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\nx-forwarded-for: 6.6.6.6\r\nx-real-ip: 6.6.6.6\r\n\r\n";
    let out = raw(proxy_addr(t.clone(), Policy::default()).await, forged).await;
    assert!(out.contains("\"x-real-ip\": \"6.6.6.6\""), "{out}");
    assert!(!out.contains("\"forwarded\""), "{out}");

    let mut pol = Policy::default();
    let mut hp = HeaderPolicy::default();
    hp.trusted_proxies = Some(TrustedProxies::default());
    hp.xff = Some(XffMode::Replace);
    hp.forwarded_header = Some(true);
    pol.headers = hp;
    let out = raw(proxy_addr(t, pol).await, forged).await;
    assert!(out.contains("\"x-forwarded-for\": \"127.0.0.1\""), "{out}");
    assert!(out.contains("\"x-real-ip\": \"127.0.0.1\""), "{out}");
    assert!(out.contains("\"forwarded\""), "{out}");
}

#[tokio::test]
async fn asserted_names_are_removed_in_every_spelling() {
    let up = upstream(Arc::default()).await;
    let mut pol = Policy::default();
    let mut hp = HeaderPolicy::default();
    hp.asserted = vec!["x-tenant".parse().unwrap(), "x-real-ip".parse().unwrap()];
    pol.headers = hp;
    let p = proxy_addr(table(up, ""), pol).await;
    let out = raw(
        p,
        "GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\nx-tenant: evil\r\nx_tenant: evil\r\nX.Tenant: evil\r\nx-other: keep\r\n\r\n",
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(!out.contains("tenant"), "{out}");
    // Handed to the embedder: ferryman's derived x-real-ip is removed too.
    assert!(!out.contains("x-real-ip"), "{out}");
    assert!(out.contains("\"x-other\": \"keep\""), "{out}");
}

fn proxy_asserting(names: &[&str]) -> Proxy {
    let cfg: ConfigToml =
        toml::from_str("[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n").unwrap();
    let table: SharedTable = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let mut p = Policy::default();
    p.headers.asserted = names.iter().map(|n| n.parse().unwrap()).collect();
    Proxy::new(table, p)
}

#[test]
#[should_panic(expected = "may not list `content-length`")]
fn asserting_a_framing_header_panics() {
    proxy_asserting(&["content-length"]);
}

#[test]
#[should_panic(expected = "may not list `host`")]
fn asserting_host_panics() {
    proxy_asserting(&["x-ok", "host"]);
}

#[test]
#[should_panic(expected = "may not list `proxy-authorization`")]
fn asserting_a_hop_by_hop_header_panics() {
    proxy_asserting(&["proxy-authorization"]);
}

#[test]
#[should_panic(expected = "may not list `x-request-id`")]
fn asserting_the_request_id_panics() {
    proxy_asserting(&["x-request-id"]);
}

#[test]
#[should_panic(expected = "may not list `x_request_id`")]
fn asserting_a_denied_name_in_another_spelling_panics() {
    proxy_asserting(&["x_request_id"]);
}

#[test]
fn every_denied_name_panics_and_trust_governed_names_do_not() {
    for n in [
        "host",
        "content-length",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "x-request-id",
    ] {
        assert!(
            std::panic::catch_unwind(|| proxy_asserting(&[n])).is_err(),
            "{n}"
        );
    }
    proxy_asserting(&["x-real-ip", "x-forwarded-for", "forwarded", "x-tenant"]);
}
