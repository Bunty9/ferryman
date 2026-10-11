//! Admin server: /metrics, /healthz, /readyz.
use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::test]
async fn admin_endpoints() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        metrics::counter!("ferryman_requests_total", "route" => "/a").increment(1);
    });
    let draining = Arc::new(AtomicBool::new(false));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(ferryman::admin::serve_admin(l, handle, draining.clone()));
    let c = reqwest::Client::new();
    let get = |p: &str| c.get(format!("http://{addr}{p}")).send();

    let r = get("/metrics").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/plain; version=0.0.4");
    assert!(r.text().await.unwrap().contains("ferryman_requests_total"));
    assert_eq!(get("/healthz").await.unwrap().status(), 200);
    assert_eq!(get("/readyz").await.unwrap().status(), 200);
    assert_eq!(get("/health").await.unwrap().status(), 200);
    let r = c
        .post(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    let r = c
        .head(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.bytes().await.unwrap().is_empty());
    assert_eq!(get("/nope").await.unwrap().status(), 404);
    assert_eq!(get("/").await.unwrap().status(), 404);

    draining.store(true, Ordering::Relaxed);
    assert_eq!(get("/readyz").await.unwrap().status(), 503);
    assert_eq!(get("/healthz").await.unwrap().status(), 200);
    assert_eq!(get("/metrics").await.unwrap().status(), 200);
}

#[tokio::test]
async fn idle_admin_connection_is_dropped() {
    use tokio::io::AsyncReadExt;
    let handle = PrometheusBuilder::new().build_recorder().handle();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(ferryman::admin::serve_admin(
        l,
        handle,
        Arc::new(AtomicBool::new(false)),
    ));
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(std::time::Duration::from_secs(15), s.read(&mut buf))
        .await
        .expect("admin server should close the silent connection")
        .unwrap_or(0);
    // hyper may answer 408 before closing; either way the socket is closed.
    let _ = n;
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), s.read(&mut buf))
        .await
        .expect("closed")
        .unwrap_or(0);
    assert_eq!(n, 0);
}
