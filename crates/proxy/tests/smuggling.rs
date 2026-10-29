//! The shared smuggling suite against `Proxy::handle` served by a bare hyper
//! HTTP/1 connection over plain TCP.

use ferryman_core::{build_table, ConfigToml};
use ferryman_proxy::testkit::{run_vector, RawUpstream};
use ferryman_proxy::{ConnCtx, Policy, Proxy};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

async fn vector(name: &str) {
    let up = RawUpstream::start().await;
    let cfg: ConfigToml = toml::from_str(&format!(
        "[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{}\"\n",
        up.addr()
    ))
    .unwrap();
    let table = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let proxy = Proxy::new(table, Policy::default());
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
    let connect = || async move { TcpStream::connect(addr).await.unwrap() };
    run_vector(name, &connect, &up).await;
}

macro_rules! vectors {
    ($($n:ident)*) => { $(
        #[tokio::test]
        async fn $n() { vector(stringify!($n)).await }
    )* };
}

vectors! {
    malformed_requests_are_rejected_and_never_forwarded
    cl_plus_te_te_wins_cl_dropped_second_request_never_forwarded
    accepted_oddities_are_normalised_upstream
    truncated_chunked_body_is_not_completed_upstream
    oversized_request_line_and_headers_are_rejected
    pipelined_requests_are_forwarded_separately_in_order
    ambiguous_body_length_follows_content_length_exactly
    upgrade_is_501_and_never_forwarded
    upgrade_pipelined_bytes_never_reach_upstream
    expect_continue_with_rejected_body_does_not_desync
}
