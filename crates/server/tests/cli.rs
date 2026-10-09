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
