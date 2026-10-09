//! PaaS env (`PORT`, `FERRYMAN_CONFIG_TOML`) and shutdown (`shutdown_delay_secs`,
//! `drain_timeout_secs`), driven through the real binary.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Proc {
    child: Child,
    proxy: SocketAddr,
    admin: SocketAddr,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn addr_in(line: &str) -> SocketAddr {
    let rest = line.split("\"addr\":\"").nth(1).unwrap();
    rest.split('"').next().unwrap().parse().unwrap()
}

/// Starts the binary with inline `toml` and waits for both listeners to log
/// their bound addresses (JSON lines on stdout).
fn start(toml: &str, port: &str) -> Proc {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .args(["--config", "/nonexistent/ignored.toml"])
        .args(["--metrics-bind", "127.0.0.1:0"])
        .env("PORT", port)
        .env("FERRYMAN_CONFIG_TOML", toml)
        .env_remove("FERRYMAN_BIND")
        .env_remove("FERRYMAN_METRICS_BIND")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let (mut proxy, mut admin) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while proxy.is_none() || admin.is_none() {
        let l = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("listener log lines");
        if l.contains("ferryman listening") {
            proxy = Some(addr_in(&l));
        } else if l.contains("admin listener bound") {
            admin = Some(addr_in(&l));
        }
    }
    let (proxy, admin) = (proxy.unwrap(), admin.unwrap());
    // The port is 0.0.0.0-bound; connect through loopback.
    let proxy = SocketAddr::from(([127, 0, 0, 1], proxy.port()));
    Proc {
        child,
        proxy,
        admin,
    }
}

/// Status code of `GET path` (Connection: close), or None if the exchange fails.
fn get(addr: SocketAddr, path: &str) -> Option<u16> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.split(' ').nth(1)?.parse().ok()
}

fn free_port() -> u16 {
    // Racy by nature (released before the child binds); a collision would fail the test loudly.
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const ROUTE: &str =
    "local_health_path = \"/up\"\n[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n";

#[test]
fn port_env_and_inline_config_serve() {
    let port = free_port();
    let p = start(ROUTE, &port.to_string());
    assert_eq!(p.proxy.port(), port, "PORT selects the proxy port");
    assert_eq!(get(p.proxy, "/up"), Some(200), "inline config is live");
    assert_eq!(get(p.admin, "/readyz"), Some(200));
}

#[test]
fn check_honours_inline_config() {
    let run = |toml: &str| {
        Command::new(env!("CARGO_BIN_EXE_ferryman"))
            .args(["check", "--config", "/nonexistent/ignored.toml"])
            .env("FERRYMAN_CONFIG_TOML", toml)
            .output()
            .unwrap()
    };
    let o = run(ROUTE);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stderr).contains("config ok: 1 routes"));
    assert_eq!(run("drain_timeout_secs = 0\n").status.code(), Some(1));
}

#[test]
fn shutdown_delay_keeps_serving_then_drain_is_bounded() {
    // Upstream that accepts and never answers: the in-flight request.
    let hang = TcpListener::bind("127.0.0.1:0").unwrap();
    let up = hang.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in hang.incoming().flatten() {
            held.push(s);
        }
    });
    let toml = format!("shutdown_delay_secs = 2\ndrain_timeout_secs = 1\n{ROUTE}")
        .replace("http://127.0.0.1:1", &format!("http://{up}"));
    let mut p = start(&toml, "0");

    let mut inflight = TcpStream::connect(p.proxy).unwrap();
    inflight
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(get(p.admin, "/readyz"), Some(200));
    let t0 = Instant::now();
    Command::new("kill")
        .args(["-TERM", &p.child.id().to_string()])
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // During the delay: not ready, but still serving.
    assert_eq!(get(p.admin, "/readyz"), Some(503));
    assert_eq!(get(p.proxy, "/up"), Some(200), "still accepting in delay");
    assert!(p.child.try_wait().unwrap().is_none());

    // Exit = delay (2 s) + drain (1 s, cuts the stuck request); not the 25 s default.
    let status = loop {
        if let Some(s) = p.child.try_wait().unwrap() {
            break s;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(6),
            "did not exit in time"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let took = t0.elapsed();
    assert!(status.success(), "{status:?}");
    assert!(
        took >= Duration::from_millis(2900),
        "exited early: {took:?}"
    );
    let mut b = [0u8; 16];
    assert!(
        matches!(inflight.read(&mut b), Ok(0) | Err(_)),
        "request cut, no response"
    );
}
