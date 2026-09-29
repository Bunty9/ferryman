//! End-to-end test for the embedded-library API: start a real embedded
//! ferryman instance via `start`, proxy a request through it to a
//! toggleable stub upstream, exercise the admin endpoints, and shut it
//! back down.

use ferryman_embedded_example::{start, Settings};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Tells the *proxy's own upstream-facing client* not to
/// pool this connection (`Connection: close`). Without this, the first
/// "up" request in the test below leaves a keep-alive connection pooled in
/// ferryman's hyper client; toggling the stub `down` only affects newly
/// *accepted* connections, so the very next proxied request would silently
/// reuse the still-open pooled connection and succeed instead of hitting
/// the (now connection-dropping) `down` branch below.
fn ok_no_keepalive(body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .header("connection", "close")
        .body(Full::new(body.into()))
        .unwrap()
}

/// Serve one accepted connection with `handler`, HTTP/1.1 only (matching
/// what ferryman's proxy always speaks to upstreams).
async fn serve_one<F, Fut>(stream: tokio::net::TcpStream, handler: F)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    let io = TokioIo::new(stream);
    let svc = service_fn(move |req| {
        let handler = handler.clone();
        async move { Ok::<_, Infallible>(handler(req).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc)
        .await;
}

/// Copied from `crates/server/tests/proxy.rs::spawn_toggle_stub` (see
/// CLAUDE.md "Testing patterns"): a stub that can be switched "down", where
/// it accepts and immediately drops every connection so the proxy sees a
/// transport error. Never simulate a dead upstream by dropping/rebinding a
/// port instead — that races with other tests running in parallel.
async fn spawn_toggle_stub(down: Arc<AtomicBool>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            if down.load(Ordering::SeqCst) {
                drop(stream);
                continue;
            }
            tokio::spawn(serve_one(stream, |_req: Request<Incoming>| async {
                ok_no_keepalive("up")
            }));
        }
    });
    addr
}

#[tokio::test]
async fn embedded_instance_proxies_reports_status_and_shuts_down() {
    let down = Arc::new(AtomicBool::new(false));
    let upstream = spawn_toggle_stub(down.clone()).await;

    // A real temp file (not an in-memory string): `start` calls
    // `ferryman_core::load_config`, which reads from disk like any real
    // deployment's config.toml would.
    let dir = std::env::temp_dir().join(format!(
        "ferryman-embedded-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "failure_threshold = 1\ndefault_cooldown_secs = 30\n\
             [[routes]]\nprefix = \"/svc\"\nupstream = \"http://{upstream}\"\n"
        ),
    )
    .unwrap();

    let settings = Settings {
        config: config_path.clone(),
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        tls: None,
    };
    // `build_recorder()` (unlike `install_recorder()`) never touches the
    // process-global metrics recorder, so many `#[tokio::test]`s across this
    // whole test binary can each build their own handle without colliding —
    // see the "install-recorder-once" rule in README.md.
    let metrics = PrometheusBuilder::new().build_recorder().handle();

    let running = start(settings, metrics).await.expect("start");
    let client = reqwest::Client::new();

    // 1. A normal request is proxied through to the healthy stub.
    let resp = client
        .get(format!("http://{}/svc/x", running.proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 2. /status reports the breaker closed.
    let status = client
        .get(format!("http://{}/status", running.admin_addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(status.contains("\"state\":\"closed\""), "{status}");

    // 3. Take the stub down. failure_threshold = 1 trips the breaker open on
    // the very first failed request, synchronously before the 502 is
    // returned (see crates/core/src/breaker.rs::record_failure), so /status
    // is guaranteed to already read "open" right after.
    down.store(true, Ordering::SeqCst);
    let resp = client
        .get(format!("http://{}/svc/x", running.proxy_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    let status = client
        .get(format!("http://{}/status", running.admin_addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(status.contains("\"state\":\"open\""), "{status}");

    // 4. /healthz is a plain liveness check for the embedding app, not the
    // routed upstreams' health.
    let resp = client
        .get(format!("http://{}/healthz", running.admin_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 5. Shutdown completes quickly: nothing is in flight, and the stub
    // upstream has no long-lived connections open to drain.
    tokio::time::timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown did not complete within 5s")
        .expect("shutdown returned an error");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression test: `start` used to spawn the health-check loop *before*
/// attempting either bind, so a `start` that failed because a port was
/// already taken (the most likely real startup failure) still left an
/// orphaned health-check loop running — one per failed attempt, for an
/// embedding app that retries `start` after an `Err`. `start` now does all
/// fallible work (config load, table build, TLS load, both binds, the
/// watcher) before spawning anything, so a bind failure here must return
/// `Err` without a `Running` to show for it and without leaving any task
/// behind.
#[tokio::test]
async fn start_fails_when_admin_bind_is_already_taken() {
    // Bind the port ourselves first so `start`'s own admin bind is
    // guaranteed to fail with "address in use".
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_bind = taken.local_addr().unwrap();

    let dir = std::env::temp_dir().join(format!(
        "ferryman-embedded-test-bindfail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    // The upstream doesn't need to exist: `start` must fail at the admin
    // bind, long before anything would try to reach it.
    std::fs::write(
        &config_path,
        "[[routes]]\nprefix = \"/svc\"\nupstream = \"http://127.0.0.1:1\"\n",
    )
    .unwrap();

    let settings = Settings {
        config: config_path,
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind,
        tls: None,
    };
    let metrics = PrometheusBuilder::new().build_recorder().handle();

    // `Running` doesn't implement `Debug` (it holds `JoinHandle`s and a
    // `notify::RecommendedWatcher`, neither of which need it for anything
    // else this crate does), so unwrap the `Result::Err` side by hand
    // instead of `expect_err`, which requires the `Ok` side to be `Debug`.
    let result = start(settings, metrics).await;
    let err = match result {
        Ok(_) => panic!("admin_bind is already taken by `taken`, start must fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("binding admin listener"), "{err}");

    drop(taken);
    let _ = std::fs::remove_dir_all(&dir);
}
