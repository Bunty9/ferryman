//! HTTP/1 request-smuggling / desync regression suite (ROADMAP Q31), run
//! against `ferryman::serve`. The vectors and their expectations live in
//! `ferryman_proxy::testkit` (shared with ferryman-edge); see there for the
//! full list of what is rejected, normalised or never forwarded.

use ferryman_core::{build_table, ConfigToml};
use ferryman_proxy::testkit::{run_vector, RawUpstream};
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
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(l, table, None, std::future::pending()));
    let connect = || async move { TcpStream::connect(proxy).await.unwrap() };
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
