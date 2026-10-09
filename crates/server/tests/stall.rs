//! A client that stalls its upload (or stops reading the response) must not
//! get the upstream's resulting 502/5xx counted against the breaker.

#![allow(deprecated)] // tokio set_linger: RST needs SO_LINGER 0

use ferryman_core::{build_table, ConfigToml, SharedTable};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn table(extra: &str, up: SocketAddr) -> SharedTable {
    let cfg: ConfigToml = toml::from_str(&format!(
        "failure_threshold = 3\n{extra}\n[[routes]]\nprefix = \"/svc\"\nupstream = \"http://{up}\"\n"
    ))
    .unwrap();
    Arc::new(arc_swap::ArcSwap::from_pointee(
        build_table(cfg, None).unwrap(),
    ))
}

async fn start_proxy(t: SharedTable) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(l, t, None, std::future::pending()));
    a
}

#[derive(Clone, Copy, Debug)]
enum OnStall {
    /// FIN, no response (Go ReadTimeout, gunicorn worker kill, uvicorn h11 timeout).
    Close,
    /// RST (SO_LINGER 0), no response.
    Reset,
    /// Send this status (connection: close) then FIN (nginx client_body_timeout = 408,
    /// or an upstream proxy answering 502/504 for its own backend).
    Respond(u16),
}

/// Raw HTTP/1.1 upstream with a per-read timeout `stall` (like an upstream's
/// client_body_timeout). `GET .../big` streams 64 MiB with a per-write timeout
/// `stall` (like a send/write timeout), closing on timeout.
async fn spawn_raw_stub(stall: Duration, on: OnStall) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(conn(s, stall, on));
        }
    });
    a
}

async fn conn(mut s: TcpStream, stall: Duration, on: OnStall) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut need: Option<(usize, bool)> = None; // (header_end + content_length, big)
    loop {
        if let Some((n, big)) = need {
            if buf.len() >= n {
                if big {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 67108864\r\nconnection: close\r\n\r\n")
                        .await;
                    let block = vec![b'x'; 65536];
                    for _ in 0..1024 {
                        match tokio::time::timeout(stall, s.write_all(&block)).await {
                            Ok(Ok(())) => {}
                            _ => return, // write timeout: close mid-body
                        }
                    }
                } else {
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                }
                return;
            }
        }
        match tokio::time::timeout(stall, s.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => {
                match on {
                    OnStall::Close => {}
                    OnStall::Reset => {
                        let _ = s.set_linger(Some(Duration::ZERO));
                    }
                    OnStall::Respond(code) => {
                        let _ = s
                            .write_all(
                                format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                    .as_bytes(),
                            )
                            .await;
                    }
                }
                return;
            }
        }
        if need.is_none() {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let cl = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                need = Some((end + 4 + cl, head.starts_with("get /svc/big")));
            }
        }
    }
}

async fn status_line(s: &mut TcpStream, within: Duration) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let n = match tokio::time::timeout_at(deadline, s.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return format!("io error: {e}"),
            Err(_) => return "no answer".into(),
        };
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

/// An unauthenticated client: POST with content-length 1000, send 10 bytes, stall.
async fn stalled_upload(proxy: SocketAddr) -> (String, Duration) {
    let t0 = Instant::now();
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"POST /svc/u HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\nonly-a-bit")
        .await
        .unwrap();
    (
        status_line(&mut s, Duration::from_secs(10)).await,
        t0.elapsed(),
    )
}

async fn get(proxy: SocketAddr) -> String {
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"GET /svc/small HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    status_line(&mut s, Duration::from_secs(5)).await
}

/// 3 concurrent stalled uploads (failure_threshold = 3, default idle timeout 30 s),
/// then one innocent GET. Returns (attack statuses, innocent GET status).
async fn attack(on: OnStall) -> (Vec<String>, String) {
    let up = spawn_raw_stub(Duration::from_secs(1), on).await;
    let proxy = start_proxy(table("", up)).await;
    assert!(get(proxy).await.contains(" 200"), "baseline");
    let hs: Vec<_> = (0..3)
        .map(|_| tokio::spawn(stalled_upload(proxy)))
        .collect();
    let mut st = Vec::new();
    for h in hs {
        let (s, el) = h.await.unwrap();
        st.push(format!("{s} after {el:.2?}"));
    }
    let after = get(proxy).await;
    eprintln!("{on:?}: attack={st:?} innocent GET -> {after}");
    (st, after)
}

#[tokio::test]
async fn stalled_upload_close_on_stall_does_not_open_breaker() {
    let (st, after) = attack(OnStall::Close).await;
    assert!(st.iter().all(|s| s.contains(" 502")), "{st:?}");
    assert!(after.contains(" 200"), "{after}");
}

#[tokio::test]
async fn stalled_upload_reset_on_stall_does_not_open_breaker() {
    let (st, after) = attack(OnStall::Reset).await;
    assert!(st.iter().all(|s| s.contains(" 502")), "{st:?}");
    assert!(after.contains(" 200"), "{after}");
}

#[tokio::test]
async fn stalled_upload_502_on_stall_is_forwarded_and_not_counted() {
    let (st, after) = attack(OnStall::Respond(502)).await;
    assert!(st.iter().all(|s| s.contains(" 502")), "{st:?}");
    assert!(after.contains(" 200"), "{after}");
}

#[tokio::test]
async fn stalled_upload_503_on_stall_is_forwarded_and_not_counted() {
    let (st, after) = attack(OnStall::Respond(503)).await;
    assert!(st.iter().all(|s| s.contains(" 503")), "{st:?}");
    assert!(after.contains(" 200"), "{after}");
}

#[tokio::test]
async fn stalled_upload_504_on_stall_is_forwarded_and_not_counted() {
    let (st, after) = attack(OnStall::Respond(504)).await;
    assert!(st.iter().all(|s| s.contains(" 504")), "{st:?}");
    assert!(after.contains(" 200"), "{after}");
}

/// Sequential variant: one attacker connection at a time still opens it when
/// no innocent success lands in between (consecutive-failure counter).
#[tokio::test]
async fn sequential_three_stalls_do_not_open_breaker() {
    let up = spawn_raw_stub(Duration::from_secs(1), OnStall::Close).await;
    let proxy = start_proxy(table("", up)).await;
    for _ in 0..3 {
        let (s, _) = stalled_upload(proxy).await;
        assert!(s.contains(" 502"), "{s}");
    }
    assert!(get(proxy).await.contains(" 200"));
}

/// (c): client reads the response head, then stops reading for > the upstream's
/// write timeout. Upstream closes mid-body; breaker must stay closed.
#[tokio::test]
async fn client_not_reading_response_does_not_count() {
    let up = spawn_raw_stub(Duration::from_secs(1), OnStall::Close).await;
    let proxy = start_proxy(table("", up)).await;
    let mut hs = Vec::new();
    for _ in 0..3 {
        hs.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(proxy).await.unwrap();
            s.write_all(b"GET /svc/big HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            let st = status_line(&mut s, Duration::from_secs(5)).await;
            tokio::time::sleep(Duration::from_millis(2500)).await; // > upstream write timeout
            let mut total = 0usize;
            let mut b = vec![0u8; 1 << 16];
            let err = loop {
                match s.read(&mut b).await {
                    Ok(0) => break None,
                    Ok(n) => total += n,
                    Err(e) => break Some(e.to_string()),
                }
            };
            (st, total, err)
        }));
    }
    for h in hs {
        let (st, total, err) = h.await.unwrap();
        eprintln!("c: {st} received {total} bytes (of 64 MiB + head), err={err:?}");
        assert!(st.contains(" 200"));
        assert!(total < 64 << 20, "response must be truncated");
    }
    let after = get(proxy).await;
    eprintln!("c: innocent GET -> {after}");
    assert!(after.contains(" 200"), "{after}");
}

/// Theta boundary: client pause (400 ms) below theta = 1 s but above the
/// upstream's read timeout (200 ms) is still counted (by design).
#[tokio::test]
async fn pause_below_theta_still_counts() {
    let up = spawn_raw_stub(Duration::from_millis(200), OnStall::Close).await;
    let proxy = start_proxy(table("", up)).await;
    for _ in 0..3 {
        let mut s = TcpStream::connect(proxy).await.unwrap();
        s.write_all(b"POST /svc/u HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\nonly-a-bit")
            .await
            .unwrap();
        let st = status_line(&mut s, Duration::from_secs(5)).await;
        eprintln!("short pause: {st}");
    }
    let after = get(proxy).await;
    eprintln!("short pause: innocent GET -> {after}");
    assert!(after.contains(" 503"), "{after}");
}
