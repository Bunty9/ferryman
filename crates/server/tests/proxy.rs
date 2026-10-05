//! Integration tests for the proxy: real sockets, tiny hyper upstream
//! stubs, and `ferryman::serve` driven end to end.

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
    tokio::spawn(ferryman::serve(
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

/// Send forged forwarding headers through a proxy with the given extra
/// config and return the echoed upstream view.
async fn forged_headers_echo(extra_cfg: &str) -> String {
    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "{extra_cfg}\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;
    reqwest::Client::new()
        .get(format!("http://{proxy}/svc-a/x"))
        .header("x-real-ip", "6.6.6.6")
        .header("X-Real-IP", "7.7.7.7")
        .header("forwarded", "for=6.6.6.6;proto=https")
        .header("Forwarded", "for=7.7.7.7")
        .header("x-forwarded-host", "evil.example")
        .header("X-Forwarded-Host", "evil2.example")
        .header("x-forwarded-proto", "https")
        .header("X-Forwarded-Proto", "https")
        .header("x-forwarded-for", "6.6.6.6")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn untrusted_peer_cannot_forge_forwarded_headers() {
    // Empty list and a list that does not contain the peer behave the same.
    for cfg in ["", "trusted_proxies = [\"10.0.0.0/8\"]"] {
        let body = forged_headers_echo(cfg).await;
        assert!(body.contains("x-real-ip: 127.0.0.1"), "{body}");
        assert_eq!(body.matches("x-real-ip:").count(), 1, "{body}");
        assert!(!body.contains("x-real-ip: 6.6.6.6"), "{body}");
        assert!(!body.contains("x-real-ip: 7.7.7.7"), "{body}");
        assert!(!body.contains("x-forwarded-proto: https"), "{body}");
        assert_eq!(body.matches("x-forwarded-proto:").count(), 1, "{body}");
        assert!(!body.contains("6.6.6.6;"), "{body}");
        assert!(!body.contains("forwarded:"), "{body}");
        assert!(!body.contains("x-forwarded-host"), "{body}");
        assert!(!body.contains("evil.example"), "{body}");
        assert!(body.contains("x-forwarded-proto: http\n"), "{body}");
        // 0.2.2 behaviour: XFF is appended to, not replaced.
        assert!(
            body.contains("x-forwarded-for: 6.6.6.6, 127.0.0.1"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn trusted_peer_forwarded_headers_are_preserved() {
    let body = forged_headers_echo("trusted_proxies = [\"127.0.0.1/32\"]").await;
    assert!(body.contains("x-forwarded-proto: https\n"), "{body}");
    assert!(body.contains("x-forwarded-host: evil.example"), "{body}");
    // Derived from XFF (rightmost untrusted = 6.6.6.6), never the forged header.
    assert!(body.contains("x-real-ip: 6.6.6.6"), "{body}");
    assert!(!body.contains("x-real-ip: 7.7.7.7"), "{body}");
    assert_eq!(body.matches("x-real-ip:").count(), 1, "{body}");
    assert!(
        body.contains("forwarded: for=6.6.6.6;proto=https"),
        "{body}"
    );
    assert!(
        body.contains("x-forwarded-for: 6.6.6.6, 127.0.0.1"),
        "{body}"
    );
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

async fn read_status_line(s: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut chunk))
            .await
            .expect("proxy answered in time")
            .unwrap();
        buf.extend_from_slice(&chunk[..n]);
        if n == 0 || buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn slow_upload_is_not_an_upstream_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 2\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    s.write_all(
        b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 20\r\nconnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    for _ in 0..5 {
        s.write_all(b"abcd").await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with(&"abcd".repeat(5)), "{resp}");
}

#[tokio::test]
async fn stalled_upload_is_408_and_does_not_trip_breaker() {
    use tokio::io::AsyncWriteExt;

    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "request_body_idle_timeout_secs = 1\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\nonly-a-bit")
        .await
        .unwrap();
    let status = read_status_line(&mut s).await;
    assert!(status.starts_with("HTTP/1.1 408"), "{status}");

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 200, "breaker must stay closed");
}

#[tokio::test]
async fn trickled_upload_hits_total_cap_with_408() {
    use tokio::io::AsyncWriteExt;

    let upstream = spawn_stub(echo).await;
    let table = shared_table(parse_cfg(&format!(
        "request_body_timeout_secs = 2\nrequest_body_idle_timeout_secs = 1\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let (mut r, mut w) = s.into_split();
    w.write_all(b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\n")
        .await
        .unwrap();
    // One byte every 0.5 s (inside the idle timeout) for 4 s.
    tokio::spawn(async move {
        for _ in 0..8 {
            if w.write_all(b"x").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let start = std::time::Instant::now();
    let status = read_status_line(&mut r).await;
    assert!(status.starts_with("HTTP/1.1 408"), "{status}");
    assert!(
        start.elapsed() < Duration::from_millis(4500),
        "{:?}",
        start.elapsed()
    );

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 200, "breaker must stay closed");
}

#[tokio::test]
async fn chunked_upload_to_slow_upstream_is_504() {
    use tokio::io::AsyncWriteExt;

    let upstream = spawn_stub(|req| async move {
        let _ = req.into_body().collect().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        ok("too-slow")
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;
    let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    s.write_all(
        b"POST /svc-a HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n",
    )
    .await
    .unwrap();
    let st = read_status_line(&mut s).await;
    assert!(st.starts_with("HTTP/1.1 504"), "{st}");
}

#[tokio::test]
async fn upstream_that_stops_reading_upload_is_504_and_counts_against_breaker() {
    use tokio::io::AsyncWriteExt;

    // Accepts and never reads: the proxy's write buffer fills and hyper stops
    // polling the request body.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 1\nrequest_body_idle_timeout_secs = 1\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let (mut r, mut w) = s.into_split();
    tokio::spawn(async move {
        let _ = w
            .write_all(b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 33554432\r\n\r\n")
            .await;
        let chunk = vec![0u8; 64 * 1024];
        for _ in 0..512 {
            if w.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });
    let start = std::time::Instant::now();
    let st = read_status_line(&mut r).await;
    assert!(st.starts_with("HTTP/1.1 504"), "{st}");
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 503, "breaker must count the stuck upload");
}

#[tokio::test]
async fn slow_draining_upstream_with_fast_client_is_408_not_a_breaker_failure() {
    use tokio::io::AsyncWriteExt;

    // Reads steadily but slowly: the proxy's buffer stays full while the
    // client is fast, so only the client's total cap can end this upload.
    let upstream = spawn_stub(|req| async move {
        let mut body = req.into_body();
        while body.frame().await.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ok("done")
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 1\nrequest_body_timeout_secs = 2\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let s = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let (mut r, mut w) = s.into_split();
    tokio::spawn(async move {
        let _ = w
            .write_all(b"POST /svc-a HTTP/1.1\r\nhost: x\r\ncontent-length: 4294967296\r\n\r\n")
            .await;
        let chunk = vec![0u8; 64 * 1024];
        while w.write_all(&chunk).await.is_ok() {}
    });
    let st = read_status_line(&mut r).await;
    assert!(st.starts_with("HTTP/1.1 408"), "{st}");

    let resp = reqwest::get(format!("http://{proxy}/svc-a")).await.unwrap();
    assert_eq!(resp.status(), 200, "a client must not trip the breaker");
}

#[tokio::test]
async fn slow_upstream_after_body_is_504_and_counts_against_breaker() {
    let upstream = spawn_stub(|req| async move {
        let _ = req.into_body().collect().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        ok("too-slow")
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "upstream_timeout_secs = 1\nfailure_threshold = 1\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;
    let client = reqwest::Client::new();
    let post = || {
        client
            .post(format!("http://{proxy}/svc-a"))
            .body("payload")
            .send()
    };

    assert_eq!(post().await.unwrap().status(), 504);
    assert_eq!(post().await.unwrap().status(), 503);
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

/// Keep-alive idle timeout is configurable: after one request, a connection
/// idle for 3 s is closed when `keepalive_timeout_secs = 2` and still
/// reusable when it is 5.
#[tokio::test]
async fn keepalive_timeout_is_configurable() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(s: &mut tokio::net::TcpStream) -> bool {
        let req = b"GET /svc-a/x HTTP/1.1\r\nHost: t\r\n\r\n";
        if s.write_all(req).await.is_err() {
            return false;
        }
        // Read the whole response (head + Content-Length body) so a split
        // write can't leave a stray byte that makes a closed socket look open.
        let mut data = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(n) if n > 0 => data.extend_from_slice(&buf[..n]),
                _ => return false,
            }
            let text = String::from_utf8_lossy(&data).to_lowercase();
            if let Some(i) = text.find("\r\n\r\n") {
                let len = text[..i]
                    .split("content-length:")
                    .nth(1)
                    .and_then(|r| r.lines().next())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if data.len() >= i + 4 + len {
                    return true;
                }
            }
        }
    }

    for (keepalive, expect_open) in [(2, false), (5, true)] {
        let upstream = spawn_stub(|_req| async { ok("a") }).await;
        let table = shared_table(parse_cfg(&format!(
            "keepalive_timeout_secs = {keepalive}\n[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
        )));
        let proxy = start_proxy(table).await;
        let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
        assert!(get(&mut s).await, "first request");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(get(&mut s).await, expect_open, "keepalive {keepalive}s");
    }
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
    let _watcher = ferryman::reload::watch_config(&config_path, table.clone()).unwrap();
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

/// 0.2.2-style embedding (old client type + `proxy::handle`) must keep compiling
/// and working.
#[tokio::test]
#[allow(deprecated)]
async fn legacy_handle_and_proxy_client_still_work() {
    use hyper_util::rt::TokioExecutor;

    let upstream = spawn_stub(|_req| async { ok("legacy") }).await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/svc-a\"\nupstream = \"http://{upstream}\"\n"
    )));
    let client: ferryman::ProxyClient =
        hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let svc = service_fn(move |req| {
            ferryman::proxy::handle(table.clone(), client.clone(), peer, "http", req)
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), svc)
            .await;
    });
    let resp = reqwest::get(format!("http://{addr}/svc-a")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "legacy");
}

#[tokio::test]
async fn dot_segments_get_400_and_never_reach_upstream() {
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let upstream = spawn_stub(move |req| {
        h.fetch_add(1, Ordering::SeqCst);
        echo(req)
    })
    .await;
    let table = shared_table(parse_cfg(&format!(
        "[[routes]]\nprefix = \"/api\"\nupstream = \"http://{upstream}\"\n"
    )));
    let proxy = start_proxy(table).await;

    let get = |path: &'static str| async move {
        let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    };

    for p in [
        "/api/../admin",
        "/api/%2e%2E/admin",
        "/api/..%2fadmin",
        "/api/..;/admin",
        "/api/a%5cb",
        "/api/x?q=/../ok-in-query-only/../",
    ] {
        let out = get(p).await;
        if p.contains('?') {
            assert!(out.starts_with("HTTP/1.1 200"), "{p}: {out}");
            continue;
        }
        assert!(out.starts_with("HTTP/1.1 400"), "{p}: {out}");
        assert!(out.ends_with("bad path\n"), "{p}: {out}");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "only the query-only request"
    );

    for p in ["/api/.well-known/x", "/api/a..b/"] {
        let out = get(p).await;
        assert!(out.starts_with("HTTP/1.1 200"), "{p}: {out}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}
