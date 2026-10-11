//! `sd_notify` end to end: READY / RELOADING+READY on SIGHUP / STOPPING on SIGTERM.
#![cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::process::{Command, Stdio};
use std::time::Duration;

fn recv(s: &UnixDatagram) -> String {
    let mut b = [0u8; 512];
    let n = s.recv(&mut b).expect("notify message");
    String::from_utf8_lossy(&b[..n]).into_owned()
}

#[test]
fn notifies_ready_reloading_stopping() {
    let dir = std::env::temp_dir().join(format!("ferryman-sd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("notify");
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        "[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n",
    )
    .unwrap();
    let srv = UnixDatagram::bind(&sock).unwrap();
    srv.set_read_timeout(Some(Duration::from_secs(15))).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .args(["--config", cfg.to_str().unwrap()])
        .args(["--bind", "127.0.0.1:0", "--metrics-bind", "127.0.0.1:0"])
        .env("NOTIFY_SOCKET", &sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let kill = |sig: &str| {
        assert!(Command::new("kill")
            .args([sig, &pid])
            .status()
            .unwrap()
            .success())
    };

    assert!(recv(&srv).starts_with("READY=1"));
    kill("-HUP");
    assert!(recv(&srv).starts_with("RELOADING=1\nMONOTONIC_USEC="));
    assert!(recv(&srv).starts_with("READY=1"));
    kill("-TERM");
    assert!(recv(&srv).starts_with("STOPPING=1"));
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}
