//! Every 0.3.0 public path, via `ferryman::`. cargo-semver-checks reports
//! cross-crate re-exports as missing (false positive); this proves them.

#![allow(deprecated)]

use ferryman::proxy::{self, RequestBody};
use ferryman::{ProxyClient, StreamingClient, LATENCY_BUCKETS};
use ferryman_core::{build_table, ConfigToml};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn stub() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_: Request<Incoming>| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from("up"))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    a
}

async fn get(addr: SocketAddr) -> String {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /svc/ HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn every_030_public_path_compiles_and_runs() {
    let up = stub().await;
    let cfg: ConfigToml = toml::from_str(&format!(
        "[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n"
    ))
    .unwrap();
    let table = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));

    assert!(!LATENCY_BUCKETS.is_empty());
    let sc: StreamingClient = Client::builder(TokioExecutor::new()).build_http::<RequestBody>();
    let pc: ProxyClient = Client::builder(TokioExecutor::new()).build_http::<Incoming>();

    // ferryman::serve
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(
        l,
        table.clone(),
        None,
        std::future::pending(),
    ));
    assert!(get(a).await.starts_with("HTTP/1.1 200"));

    // proxy::handle_streaming and the deprecated proxy::handle
    for legacy in [false, true] {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        let (table, sc, pc) = (table.clone(), sc.clone(), pc.clone());
        tokio::spawn(async move {
            let (s, peer) = l.accept().await.unwrap();
            let svc = service_fn(move |req| {
                let (table, sc, pc) = (table.clone(), sc.clone(), pc.clone());
                async move {
                    if legacy {
                        proxy::handle(table, pc, peer, "http", req).await
                    } else {
                        proxy::handle_streaming(table, sc, peer, "http", req).await
                    }
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(s), svc)
                .await;
        });
        let out = get(a).await;
        assert!(out.starts_with("HTTP/1.1 200"), "legacy={legacy}: {out}");
        assert!(out.ends_with("up"), "{out}");
    }
    let _ = Full::new(Bytes::new()).collect().await;
}
