//! `Hooks` end to end through `Proxy::handle`, with a dummy auth + stamp hook.

use ferryman_core::{build_table, CircuitState, ConfigToml, SharedTable};
use ferryman_proxy::{
    full_body, AssertedHeaders, BoxFuture, ConnCtx, Hooks, Policy, Proxy, ResponseBody, StampCtx,
};
use http::{HeaderName, HeaderValue, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct Tenant(String);
#[derive(Clone)]
struct Boom;

/// 401 without `authorization`; otherwise the header value becomes the tenant.
/// `x-boom` makes `stamp` panic.
struct Auth;

impl Hooks for Auth {
    fn pre_dispatch<'a>(
        &'a self,
        req: &'a mut Request<Incoming>,
        _ctx: &'a ConnCtx,
    ) -> BoxFuture<'a, Result<(), Response<ResponseBody>>> {
        Box::pin(async move {
            let Some(who) = req.headers().get("authorization") else {
                return Err(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(full_body("nope"))
                    .unwrap());
            };
            let who = who.to_str().unwrap().to_string();
            let boom = req.headers().contains_key("x-boom");
            req.extensions_mut().insert(Tenant(who));
            if boom {
                req.extensions_mut().insert(Boom);
            }
            Ok(())
        })
    }

    fn stamp(&self, cx: &StampCtx<'_>, headers: &mut AssertedHeaders<'_>) {
        assert!(cx.extensions.get::<Boom>().is_none(), "stamp panics");
        let t = cx.extensions.get::<Tenant>().unwrap();
        assert!(headers.insert(
            &HeaderName::from_static("x-tenant"),
            HeaderValue::from_str(&t.0).unwrap()
        ));
        // Not listed in `asserted`: refused.
        assert!(!headers.insert(
            &HeaderName::from_static("x-unlisted"),
            HeaderValue::from_static("stamped")
        ));
    }
}

struct Rig {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    table: SharedTable,
}

async fn rig() -> Rig {
    let hits = Arc::new(AtomicUsize::new(0));
    let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up.local_addr().unwrap();
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = up.accept().await.unwrap();
            h.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{head}",
                            head.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    let cfg: ConfigToml = toml::from_str(&format!(
        "request_id = true\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up_addr}\"\n"
    ))
    .unwrap();
    let table: SharedTable = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let mut p = Policy::default();
    p.headers.asserted = vec![HeaderName::from_static("x-tenant")];
    p.hooks = Some(Arc::new(Auth));
    let proxy = Proxy::new(table.clone(), p);
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
    Rig { addr, hits, table }
}

impl Rig {
    async fn send(&self, extra: &str) -> String {
        let mut c = TcpStream::connect(self.addr).await.unwrap();
        c.write_all(
            format!("GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\n{extra}\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out).await;
        out
    }

    fn state(&self) -> CircuitState {
        self.table.load().lookup("/svc/").unwrap().upstream.state()
    }
}

#[tokio::test]
async fn short_circuit_401_never_reaches_upstream_or_breaker() {
    let r = rig().await;
    let out = r.send("x-request-id: abc123\r\n").await;
    assert!(out.starts_with("HTTP/1.1 401"), "{out}");
    assert!(out.ends_with("nope"), "{out}");
    assert!(
        out.to_ascii_lowercase().contains("x-request-id: abc123"),
        "{out}"
    );
    assert_eq!(r.hits.load(Ordering::SeqCst), 0);
    assert_eq!(r.state(), CircuitState::Closed);
}

#[tokio::test]
async fn extension_reaches_stamp_and_client_spellings_are_stripped() {
    let r = rig().await;
    let out = r
        .send("authorization: alice\r\nx-tenant: evil\r\nX_Tenant: evil2\r\nx.tenant: evil3\r\nx-unlisted: client\r\n")
        .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert_eq!(out.matches("x-tenant: alice\r\n").count(), 1, "{out}");
    assert!(!out.contains("evil"), "{out}");
    assert!(
        !out.contains("x_tenant") && !out.contains("x.tenant"),
        "{out}"
    );
    // The stamp of an unlisted name was refused (client's own copy is its business).
    assert!(!out.contains("x-unlisted: stamped"), "{out}");
    assert_eq!(r.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panicking_stamp_drops_the_connection_and_spares_the_breaker() {
    let r = rig().await;
    // failure_threshold = 1: a recorded failure would open the breaker.
    let out = r.send("authorization: bob\r\nx-boom: 1\r\n").await;
    assert!(!out.starts_with("HTTP/1.1 200"), "{out}");
    assert_eq!(
        r.hits.load(Ordering::SeqCst),
        0,
        "stamp runs before dispatch"
    );
    assert_eq!(r.state(), CircuitState::Closed);
    let ok = r.send("authorization: bob\r\n").await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}
