//! `ferryman check` / `ferryman healthcheck` subcommands.
use metrics_exporter_prometheus::PrometheusBuilder;
use std::process::{Command, Output};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .args(args)
        .env_remove("FERRYMAN_CONFIG")
        .env_remove("FERRYMAN_METRICS_BIND")
        .output()
        .unwrap()
}

fn write_cfg(name: &str, body: &str) -> String {
    let p = std::env::temp_dir().join(format!("ferryman-cli-{}-{name}.toml", std::process::id()));
    std::fs::write(&p, body).unwrap();
    p.to_str().unwrap().to_owned()
}

#[test]
fn check_valid_config() {
    let p = write_cfg(
        "ok",
        "[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n",
    );
    let o = run(&["check", "--config", &p]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stderr).contains("config ok: 1 routes"));
}

#[test]
fn check_invalid_config_and_bad_tls() {
    let p = write_cfg(
        "bad",
        "failure_threshold = 0\n[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n",
    );
    let o = run(&["check", "--config", &p]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("failure_threshold"));

    let ok = write_cfg(
        "ok2",
        "[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n",
    );
    let o = run(&[
        "check",
        "--config",
        &ok,
        "--tls-cert",
        "/nonexistent.pem",
        "--tls-key",
        "/nonexistent.key",
    ]);
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn version_still_works() {
    assert_eq!(run(&["--version"]).status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn healthcheck_up_and_down() {
    let handle = PrometheusBuilder::new().build_recorder().handle();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(ferryman::admin::serve_admin(
        l,
        handle,
        Arc::new(AtomicBool::new(false)),
    ));
    let bind = addr.to_string();
    assert_eq!(
        run(&["healthcheck", "--metrics-bind", &bind]).status.code(),
        Some(0)
    );
    let url = format!("http://{addr}/healthz");
    assert_eq!(run(&["healthcheck", "--url", &url]).status.code(), Some(0));
    // 404 path is non-2xx.
    let url = format!("http://{addr}/nope");
    assert_eq!(run(&["healthcheck", "--url", &url]).status.code(), Some(1));

    // Closed port: refused immediately.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/healthz", closed.local_addr().unwrap());
    drop(closed);
    let t = Instant::now();
    assert_eq!(run(&["healthcheck", "--url", &url]).status.code(), Some(1));
    assert!(t.elapsed() < Duration::from_secs(4));
}

async fn admin_on(addr: &str) -> Option<std::net::SocketAddr> {
    let l = tokio::net::TcpListener::bind(addr).await.ok()?;
    let a = l.local_addr().unwrap();
    let handle = PrometheusBuilder::new().build_recorder().handle();
    tokio::spawn(ferryman::admin::serve_admin(
        l,
        handle,
        Arc::new(AtomicBool::new(false)),
    ));
    Some(a)
}

#[tokio::test(flavor = "multi_thread")]
async fn healthcheck_default_url_uses_bind_ip() {
    // Specific IPv4 bind (and IPv6 loopback when the host has it).
    for bind in ["127.0.0.1:0", "[::1]:0"] {
        let Some(a) = admin_on(bind).await else {
            continue;
        };
        let a = a.to_string();
        assert_eq!(
            run(&["healthcheck", "--metrics-bind", &a]).status.code(),
            Some(0),
            "{a}"
        );
    }
    // Wildcard bind maps to loopback.
    let a = admin_on("127.0.0.1:0").await.unwrap();
    let wild = format!("0.0.0.0:{}", a.port());
    assert_eq!(
        run(&["healthcheck", "--metrics-bind", &wild]).status.code(),
        Some(0)
    );
}

#[test]
fn check_tls_branches() {
    let ok = write_cfg(
        "tls",
        "[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n",
    );
    // Committed test-only fixture (examples/full-stack/certs is generated at
    // runtime and gitignored, so it's absent on a fresh CI checkout).
    let certs = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let (cert, key) = (
        format!("{certs}test-cert.pem"),
        format!("{certs}test-key.pem"),
    );
    let o = run(&[
        "check",
        "--config",
        &ok,
        "--tls-cert",
        &cert,
        "--tls-key",
        &key,
    ]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let o = run(&["check", "--config", &ok, "--tls-cert", &cert]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("both be set"));
}
