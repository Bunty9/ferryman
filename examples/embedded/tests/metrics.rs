//! Asserts on real `/metrics` content — deliberately its own test binary
//! (own process), because that content only exists behind an *installed*
//! process-global Prometheus recorder (see the "install the recorder once"
//! rule in README.md): ferryman records metrics through the global
//! `metrics::` macros, not through whatever handle `start` was handed, so
//! an uninstalled handle (what `tests/embedded.rs` uses, to let many tests
//! share one process without colliding over the single global recorder
//! slot) renders empty no matter how much traffic is proxied. Only one
//! `install_recorder()` call is safe per process, so this is the one place
//! it happens and the one place `/metrics` content is checked.

use ferryman_embedded_example::{prometheus_builder, start, Settings};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::time::Duration;
use tokio::net::TcpListener;

/// A single always-up stub upstream: this test only needs one successful
/// proxied request to populate the histogram and gauge series it asserts
/// on, unlike `tests/embedded.rs` which also needs a toggleable failure.
async fn spawn_stub() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(|_req: Request<Incoming>| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"up"))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn metrics_endpoint_reports_ferryman_series_after_a_proxied_request() {
    let upstream = spawn_stub().await;

    let dir = std::env::temp_dir().join(format!(
        "ferryman-embedded-metrics-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "health_interval_secs = 30\ndefault_cooldown_secs = 30\n\
             [[routes]]\nprefix = \"/svc\"\nupstream = \"http://{upstream}\"\n"
        ),
    )
    .unwrap();

    // The one process-global recorder install this test binary makes — see
    // the module doc comment above for why this test needs its own
    // process to do that safely.
    let metrics = prometheus_builder(Duration::from_secs(30))
        .expect("prometheus_builder")
        .install_recorder()
        .expect("install_recorder");

    let settings = Settings {
        config: config_path,
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        tls: None,
    };
    let running = start(settings, metrics).await.expect("start");

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/svc/x", running.proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body = client
        .get(format!("http://{}/metrics", running.admin_addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("ferryman_upstream_alive"), "{body}");
    assert!(
        body.contains("ferryman_request_duration_seconds_bucket"),
        "{body}"
    );

    tokio::time::timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown did not complete within 5s")
        .expect("shutdown returned an error");

    let _ = std::fs::remove_dir_all(&dir);
}
