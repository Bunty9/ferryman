//! Minimal HTTP/1.1 upstream for local benchmarking: answers every request
//! (including `/health`) with `200 ok`.
//!
//! ```bash
//! cargo run --release -p ferryman-server --example echo_upstream -- 127.0.0.1:8001
//! ```

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::Response;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8001".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("echo_upstream listening on {addr}");
    loop {
        let (stream, _) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let svc = service_fn(|_req| async {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}
