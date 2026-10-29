//! `Proxy::handle` driven directly (no `ferryman::serve`): the forwarded
//! scheme comes from the `ConnCtx`.

use ferryman_core::{build_table, ConfigToml, SharedTable};
use ferryman_proxy::{ConnCtx, Policy, Proxy, TlsInfo};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Upstream that answers with the request head it received.
async fn echo_upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
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
    a
}

fn table(up: SocketAddr) -> SharedTable {
    let cfg: ConfigToml = toml::from_str(&format!(
        "[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n"
    ))
    .unwrap();
    Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ))
}

/// Serve `Proxy::handle` with a fixed `ConnCtx` flavour; GET /svc/ and return the body.
async fn get_via(tls: bool) -> String {
    let up = echo_upstream().await;
    let proxy = Proxy::new(table(up), Policy::default());
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let (s, peer) = l.accept().await.unwrap();
        let mut conn = ConnCtx::new(peer, addr);
        if tls {
            conn = conn.with_tls(TlsInfo::new(Some(b"http/1.1".to_vec()), vec![]));
        }
        let conn = Arc::new(conn);
        let svc = service_fn(move |req| {
            let (p, c) = (proxy.clone(), conn.clone());
            async move { Ok::<_, Infallible>(p.handle(req, &c).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(s), svc)
            .await;
    });
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    out
}

#[tokio::test]
async fn plain_conn_forwards_http() {
    let out = get_via(false).await;
    assert!(out.contains("x-forwarded-proto: http\r\n"), "{out}");
}

#[tokio::test]
async fn tls_conn_forwards_https() {
    let out = get_via(true).await;
    assert!(out.contains("x-forwarded-proto: https\r\n"), "{out}");
}
