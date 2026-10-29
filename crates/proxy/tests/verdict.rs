//! `VerdictPolicy` through `Proxy::handle`: which breaker verdicts each
//! policy gives for response-body failures, client hang-ups and client stalls.

use ferryman_core::{build_table, CircuitState, ConfigToml, SharedTable};
use ferryman_proxy::{ConnCtx, Policy, Proxy, VerdictPolicy};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BOTH: [VerdictPolicy; 2] = [VerdictPolicy::HeadOnly, VerdictPolicy::DeferNormalToBodyEnd];

/// Raw upstream. After the request (head, plus a chunked body if declared):
/// `/trunc` answers a 200 chunked head and one chunk, then closes without the
/// terminating chunk; `/slow` the same but holds the connection open for 2 s;
/// `/bad` the same with a 502 head; anything else a complete 200 `ok`.
async fn upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    let done = {
                        let t = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                        t.contains("\r\n\r\n")
                            && (!t.contains("transfer-encoding: chunked")
                                || t.ends_with("0\r\n\r\n"))
                    };
                    if done {
                        break;
                    }
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let line = req.lines().next().unwrap_or("");
                let chunk = "transfer-encoding: chunked\r\n\r\n5\r\nhello\r\n";
                if line.contains(" /svc/trunc") {
                    let _ = s
                        .write_all(format!("HTTP/1.1 200 OK\r\n{chunk}").as_bytes())
                        .await;
                } else if line.contains(" /svc/bad") {
                    let _ = s
                        .write_all(format!("HTTP/1.1 502 Bad Gateway\r\n{chunk}").as_bytes())
                        .await;
                } else if line.contains(" /svc/slow") {
                    let _ = s
                        .write_all(format!("HTTP/1.1 200 OK\r\n{chunk}").as_bytes())
                        .await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } else {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                }
                let _ = s.shutdown().await;
            });
        }
    });
    a
}

struct Rig {
    addr: SocketAddr,
    table: SharedTable,
}

impl Rig {
    async fn new(up: SocketAddr, threshold: u32, verdict: VerdictPolicy) -> Self {
        Self::with_cooldown(up, threshold, verdict, 60).await
    }

    async fn with_cooldown(
        up: SocketAddr,
        threshold: u32,
        verdict: VerdictPolicy,
        cooldown_secs: u32,
    ) -> Self {
        let cfg: ConfigToml = toml::from_str(&format!(
            "failure_threshold = {threshold}\ndefault_cooldown_secs = {cooldown_secs}\n\
             request_body_idle_timeout_secs = 1\n\
             [[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n"
        ))
        .unwrap();
        let table: SharedTable = Arc::new(arc_swap::ArcSwap::from_pointee(
            build_table(cfg, None).unwrap(),
        ));
        let mut policy = Policy::default();
        policy.verdict = verdict;
        let proxy = Proxy::new(table.clone(), policy);
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
        Self { addr, table }
    }

    fn circuit(&self) -> CircuitState {
        self.table.load().lookup("/svc/").unwrap().upstream.state()
    }

    /// GET `path`, read until the connection closes; returns the raw reply.
    async fn get(&self, path: &str) -> String {
        let mut c = TcpStream::connect(self.addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Read the head and first chunk of `path`, then hang up.
    async fn get_then_hang_up(&self, path: &str) {
        let mut c = TcpStream::connect(self.addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let mut seen = Vec::new();
        let mut tmp = [0u8; 512];
        while !String::from_utf8_lossy(&seen).contains("hello") {
            let n = c.read(&mut tmp).await.unwrap();
            assert!(n > 0, "closed early: {seen:?}");
            seen.extend_from_slice(&tmp[..n]);
        }
        drop(c);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The reason edge needs `DeferNormalToBodyEnd`: a 200 head must not reset
/// the failure count before its body is done, so three resets in a row open
/// the breaker. `HeadOnly` records success at the head and never opens.
#[tokio::test]
async fn upstream_body_resets_open_the_breaker_only_under_defer() {
    for v in BOTH {
        let r = Rig::new(upstream().await, 3, v).await;
        for i in 1..=3 {
            let out = r.get("/svc/trunc").await;
            assert!(out.starts_with("HTTP/1.1 200"), "{v:?} {i}: {out}");
            assert!(!out.contains("0\r\n\r\n"), "body must be cut: {out}");
        }
        let want = if v == VerdictPolicy::DeferNormalToBodyEnd {
            CircuitState::Open
        } else {
            CircuitState::Closed
        };
        assert_eq!(r.circuit(), want, "{v:?}");
    }
}

/// A client that hangs up mid-body is no verdict at all: never a failure,
/// and (under Defer) never a success that would reset the count either.
#[tokio::test]
async fn client_hang_up_mid_body_records_no_failure() {
    for v in BOTH {
        let r = Rig::new(upstream().await, 1, v).await;
        r.get_then_hang_up("/svc/slow").await;
        assert_eq!(r.circuit(), CircuitState::Closed, "{v:?}: not a failure");
    }
    let r = Rig::new(upstream().await, 2, VerdictPolicy::DeferNormalToBodyEnd).await;
    r.get("/svc/trunc").await;
    r.get_then_hang_up("/svc/slow").await;
    assert_eq!(r.circuit(), CircuitState::Closed);
    r.get("/svc/trunc").await;
    assert_eq!(r.circuit(), CircuitState::Open, "hang-up was not a success");
}

/// A clean end of a sized or chunked body is the deferred success: it resets
/// the failure count.
#[tokio::test]
async fn clean_body_end_is_the_deferred_success() {
    let r = Rig::new(upstream().await, 2, VerdictPolicy::DeferNormalToBodyEnd).await;
    r.get("/svc/trunc").await;
    assert!(r.get("/svc/ok").await.ends_with("ok"));
    r.get("/svc/trunc").await;
    assert_eq!(r.circuit(), CircuitState::Closed, "success reset the count");
}

/// A failed head counts once; its broken body must not count again.
#[tokio::test]
async fn failed_head_with_a_broken_body_counts_once() {
    for v in BOTH {
        let r = Rig::new(upstream().await, 2, v).await;
        let out = r.get("/svc/bad").await;
        assert!(out.starts_with("HTTP/1.1 502"), "{v:?}: {out}");
        assert_eq!(
            r.circuit(),
            CircuitState::Closed,
            "{v:?}: one failure of two"
        );
        r.get("/svc/bad").await;
        assert_eq!(r.circuit(), CircuitState::Open, "{v:?}");
    }
}

/// A client that stalled its upload for >= theta (500 ms here) is never
/// blamed for the upstream's body reset afterwards; the same request without
/// the stall is.
#[tokio::test]
async fn client_stall_exempts_a_body_reset() {
    async fn post(r: &Rig, stall: Duration) {
        let mut c = TcpStream::connect(r.addr).await.unwrap();
        c.write_all(
            b"POST /svc/trunc HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\
              transfer-encoding: chunked\r\n\r\n3\r\nabc\r\n",
        )
        .await
        .unwrap();
        tokio::time::sleep(stall).await;
        c.write_all(b"0\r\n\r\n").await.unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out).await;
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"));
    }
    let r = Rig::new(upstream().await, 1, VerdictPolicy::DeferNormalToBodyEnd).await;
    post(&r, Duration::from_millis(700)).await;
    assert_eq!(
        r.circuit(),
        CircuitState::Closed,
        "stalled client: released"
    );
    post(&r, Duration::from_millis(0)).await;
    assert_eq!(r.circuit(), CircuitState::Open, "control: blamed");
}

/// A probe's success at the head closes the breaker; the body error after it
/// is then at most one `Normal` failure (threshold 2: closed after one, open
/// after two), never a second verdict from the probe ticket.
#[tokio::test]
async fn probe_success_at_head_then_body_error_counts_one_failure() {
    let r = Rig::with_cooldown(upstream().await, 2, VerdictPolicy::DeferNormalToBodyEnd, 1).await;
    r.get("/svc/bad").await;
    r.get("/svc/bad").await;
    assert_eq!(r.circuit(), CircuitState::Open);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let out = r.get("/svc/trunc").await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert_eq!(r.circuit(), CircuitState::Closed, "one failure of two");
    r.get("/svc/trunc").await;
    assert_eq!(r.circuit(), CircuitState::Open, "the second one opens it");
}
