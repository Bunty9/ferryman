//! Integration tests for the proxy: real sockets, tiny hyper upstream
//! stubs, and `ferryman_server::serve` driven end to end.

use ferryman_core::{build_table, load_config, ConfigToml, SharedTable};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn parse_cfg(toml_str: &str) -> ConfigToml {
    toml::from_str(toml_str).expect("valid test config")
}

fn shared_table(cfg: ConfigToml) -> SharedTable {
    let table = build_table(cfg, None).expect("valid test route table");
    Arc::new(arc_swap::ArcSwap::from_pointee(table))
}

fn ok(body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::new(Full::new(body.into()))
}

/// Serve one accepted connection with `handler`, HTTP/1.1 only (matching
/// what the proxy always speaks to upstreams).
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

/// Spawn a minimal HTTP/1.1 stub upstream on an ephemeral port. `handler`
/// runs once per request; the server task lives for the rest of the process.
async fn spawn_stub<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(serve_one(stream, handler.clone()));
        }
    });
    addr
}

/// A stub that can be switched "down": while down it accepts and drops
/// every connection, so the proxy sees a transport error. Keeps its port
/// for the whole test (no rebinding races with parallel tests).
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
                ok("up")
            }));
        }
    });
    addr
}

/// Start the proxy on an ephemeral port and return its address. Runs for
/// the rest of the process (no shutdown signal is ever sent).
async fn start_proxy(table: SharedTable) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(ferryman_server::serve(
        listener,
        table,
        None,
        std::future::pending(),
    ));
    addr
}

/// Echoes method, URI, sorted request headers, and body back as text, so
/// tests can assert on exactly what reached the upstream.
async fn echo(req: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let mut header_lines: Vec<String> = req
        .headers()
        .iter()
        .map(|(k, v)| format!("{}: {}", k.as_str(), v.to_str().unwrap_or("")))
        .collect();
    header_lines.sort();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let text = format!(
        "{method} {uri}\n{}\n\n{}",
        header_lines.join("\n"),
        String::from_utf8_lossy(&body)
    );
    ok(text)
}

#[tokio::test]
async fn forwards_status_and_body() {
    let upstream = spawn_stub(|_req| async {
        Response::builder()
            .status(201)
            .body(Full::new(Bytes::from_static(b"created")))
            .unwrap()
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::get(format!("http://{proxy}/svc-a/x?y=1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.text().await.unwrap(), "created");
}

#[tokio::test]
async fn path_and_query_reach_upstream_intact() {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::get(format!("http://{proxy}/svc-a/x?y=1"))
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.starts_with("GET /svc-a/x?y=1\n"), "{body}");
}

#[tokio::test]
async fn no_route_is_404() {
    let table = shared_table(parse_cfg(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://127.0.0.1:1\"\n",
    ));
    let proxy = start_proxy(table).await;

    let resp = reqwest::get(format!("http://{proxy}/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.unwrap(), "no route");
}

#[tokio::test]
async fn segment_boundary_does_not_match_longer_name() {
    let upstream = spawn_stub(|_req| async { ok("hit") }).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;
    let client = reqwest::Client::new();

    let hit = client
        .get(format!("http://{proxy}/svc-a/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(hit.status(), 200);

    let miss = client
        .get(format!("http://{proxy}/svc-ab"))
        .send()
        .await
        .unwrap();
    assert_eq!(miss.status(), 404);
}

#[tokio::test]
async fn post_body_is_streamed_through() {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/svc-a/echo"))
        .body("ping-pong-payload")
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.ends_with("ping-pong-payload"), "{body}");
}

#[tokio::test]
async fn strips_hop_by_hop_headers_and_sets_forwarded_headers() {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy}/svc-a/x"))
        .header("connection", "x-drop-me")
        .header("x-drop-me", "should-not-arrive")
        .header("keep-alive", "timeout=5")
        .header("x-custom", "keep-me")
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();

    assert!(body.contains("x-custom: keep-me"), "{body}");
    assert!(!body.contains("x-drop-me"), "{body}");
    assert!(!body.contains("keep-alive"), "{body}");
    assert!(!body.contains("connection:"), "{body}");
    assert!(body.contains("x-forwarded-proto: http"), "{body}");
    assert!(body.contains("x-forwarded-for: 127.0.0.1"), "{body}");
}

#[tokio::test]
async fn dead_upstream_502s_until_breaker_opens() {
    let addr = spawn_toggle_stub(Arc::new(AtomicBool::new(true))).await;

    let table = shared_table(parse_cfg(&format!(
        "failure_threshold = 2\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{addr}\"\n"
    )));
    let proxy = start_proxy(table).await;
    let client = reqwest::Client::new();

    for _ in 0..2 {
        let resp = client
            .get(format!("http://{proxy}/svc-a"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    let resp = client
        .get(format!("http://{proxy}/svc-a"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn upstream_timeout_is_504() {
    let upstream = spawn_stub(|_req| async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        ok("too-slow")
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn half_open_probe_recovers_after_cooldown() {
    let down = Arc::new(AtomicBool::new(true));
    let addr = spawn_toggle_stub(down.clone()).await;

    let table = shared_table(parse_cfg(&format!(
        "failure_threshold = 1\ndefault_cooldown_secs = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{addr}\"\n"
    )));
    let proxy = start_proxy(table).await;
    let client = reqwest::Client::new();
    let get = || client.get(format!("http://{proxy}/svc-a")).send();

    // First request fails and trips the breaker open (threshold 1).
    assert_eq!(get().await.unwrap().status(), 502);
    // Cooldown hasn't elapsed: breaker refuses outright.
    assert_eq!(get().await.unwrap().status(), 503);

    down.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // The half-open probe succeeds and closes the circuit.
    let r = get().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "up");
    assert_eq!(get().await.unwrap().status(), 200);
}

#[tokio::test]
async fn client_abort_mid_body_does_not_trip_breaker() {
    use tokio::io::AsyncWriteExt;

    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "failure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    for _ in 0..3 {
        let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
        s.write_all(b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\nonly-a-bit")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 200, "breaker must not open on client aborts");
}

#[tokio::test]
async fn http2_client_is_forwarded_as_http1_with_host_and_joined_cookies() {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{proxy}/svc-a/x"))
        .header("cookie", "a=1")
        .header("cookie", "b=2")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(&format!("host: {proxy}")), "{body}");
    assert!(body.contains("cookie: a=1; b=2"), "{body}");
}

#[tokio::test]
async fn upgrade_requests_get_501() {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy}/svc-a/ws"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 501);

    // h2c upgrade offers are ignorable and must be served as plain HTTP/1.1.
    let resp = reqwest::Client::new()
        .get(format!("http://{proxy}/svc-a/x"))
        .header("connection", "upgrade, http2-settings")
        .header("upgrade", "h2c")
        .header("http2-settings", "AAMAAABkAAQAAP__")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(!body.contains("upgrade"), "{body}");
}

#[tokio::test]
async fn absolute_form_authority_overrides_host() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    s.write_all(
        b"GET http://a.example/svc-a/x HTTP/1.1\r\nhost: b.example\r\nconnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.contains("host: a.example"), "{out}");
}

#[tokio::test]
async fn idle_connection_is_dropped_after_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let table = shared_table(parse_cfg(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://127.0.0.1:1\"\n",
    ));
    let proxy = start_proxy(table).await;

    // A partial h2 preface keeps the auto builder's version sniff waiting.
    let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"PRI").await.unwrap();
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(15), s.read(&mut buf))
        .await
        .expect("proxy should close the stalled connection")
        .unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn hot_reload_picks_up_new_routes_via_rename_replace() {
    let upstream_a = spawn_stub(|_req| async { ok("a") }).await;
    let upstream_b = spawn_stub(|_req| async { ok("b") }).await;

    let dir = std::env::temp_dir().join(format!(
        "ferryman-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");

    let initial = format!("[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream_a}\"\n");
    std::fs::write(&config_path, &initial).unwrap();

    let cfg = load_config(&config_path).unwrap();
    let table: SharedTable = Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ));
    let _watcher = ferryman_server::reload::watch_config(&config_path, table.clone()).unwrap();
    let proxy = start_proxy(table).await;
    let client = reqwest::Client::new();

    let before = client
        .get(format!("http://{proxy}/svc-b"))
        .send()
        .await
        .unwrap();
    assert_eq!(before.status(), 404);

    // Simulate an editor's rename-and-replace save: write to a sibling temp
    // file, then rename it over the config. This is exactly what watching
    // the parent directory (rather than the file itself) is meant to catch.
    let updated =
        format!("{initial}\n[[routes]]\nprefix = \"/svc-b\"\nupstream = \"http://{upstream_b}\"\n");
    let tmp_path = dir.join("config.toml.tmp");
    std::fs::write(&tmp_path, &updated).unwrap();
    std::fs::rename(&tmp_path, &config_path).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = client
            .get(format!("http://{proxy}/svc-b"))
            .send()
            .await
            .unwrap();
        if resp.status() == 200 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reload did not pick up the new route in time"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = std::fs::remove_dir_all(&dir);
}
