//! Reload model: directory watch (rename-replace, ConfigMap `..data` swap),
//! content-hash de-dup, SIGHUP, and TLS certificate hot reload.
use ferryman::reload::Reloader;
use ferryman_core::{build_table, load_config, SharedTable};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ferryman-reload-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Upstream that answers every connection with `200 ok` and closes.
async fn spawn_upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    addr
}

fn route(prefix: &str, up: SocketAddr) -> String {
    format!("[[routes]]\nprefix = \"{prefix}\"\nupstream = \"http://{up}\"\n")
}

fn table_from(path: &Path) -> SharedTable {
    let t = build_table(load_config(path).unwrap(), None).unwrap();
    Arc::new(arc_swap::ArcSwap::from_pointee(t))
}

async fn start_proxy(table: SharedTable, tls: Option<tokio_rustls::TlsAcceptor>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(ferryman::serve(l, table, tls, std::future::pending()));
    addr
}

async fn status(proxy: SocketAddr, path: &str) -> u16 {
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out.split(' ').nth(1).unwrap().parse().unwrap()
}

async fn wait_status(proxy: SocketAddr, path: &str, want: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while status(proxy, path).await != want {
        assert!(Instant::now() < deadline, "{path} never became {want}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn configmap_data_symlink_swap_is_picked_up() {
    use std::os::unix::fs::symlink;
    let up = spawn_upstream().await;
    let dir = tempdir("cm");
    for (v, extra) in [("..v1", ""), ("..v2", &*route("/new", up))] {
        std::fs::create_dir(dir.join(v)).unwrap();
        std::fs::write(dir.join(v).join("config.toml"), route("/old", up) + extra).unwrap();
    }
    symlink("..v1", dir.join("..data")).unwrap();
    symlink("..data/config.toml", dir.join("config.toml")).unwrap();

    let cfg = dir.join("config.toml");
    let table = table_from(&cfg);
    let _w = Reloader::new(Some(cfg), table.clone(), None)
        .watch()
        .unwrap();
    let proxy = start_proxy(table, None).await;
    assert_eq!(status(proxy, "/new").await, 404);

    // What the kubelet does: new symlink, renamed over `..data` atomically.
    symlink("..v2", dir.join("..data_tmp")).unwrap();
    std::fs::rename(dir.join("..data_tmp"), dir.join("..data")).unwrap();
    wait_status(proxy, "/new", 200).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unchanged_bytes_do_not_swap_the_table() {
    let up = spawn_upstream().await;
    let dir = tempdir("dedup");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, route("/a", up)).unwrap();
    let table = table_from(&cfg);
    let _w = Reloader::new(Some(cfg.clone()), table.clone(), None)
        .watch()
        .unwrap();
    // Let the watcher's startup re-check finish before writing in place.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = table.load_full();

    // Same bytes, written in place and via rename-replace.
    std::fs::write(&cfg, route("/a", up)).unwrap();
    std::fs::write(dir.join("t"), route("/a", up)).unwrap();
    std::fs::rename(dir.join("t"), &cfg).unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        Arc::ptr_eq(&before, &table.load_full()),
        "table was swapped"
    );

    // Positive control: changed bytes do swap.
    std::fs::write(&cfg, route("/b", up)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Arc::ptr_eq(&before, &table.load_full()) {
        assert!(Instant::now() < deadline, "changed config never reloaded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn forced_reload_ignores_the_hash_and_bad_config_keeps_old_table() {
    let up = spawn_upstream().await;
    let dir = tempdir("force");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, route("/a", up)).unwrap();
    let table = table_from(&cfg);
    let r = Reloader::new(Some(cfg.clone()), table.clone(), None);
    let t0 = table.load_full();
    r.reload(false);
    assert!(Arc::ptr_eq(&t0, &table.load_full()), "unforced + unchanged");
    r.reload(true);
    let t1 = table.load_full();
    assert!(!Arc::ptr_eq(&t0, &t1), "SIGHUP-style reload must swap");

    std::fs::write(&cfg, "this is not [toml").unwrap();
    r.reload(true);
    assert!(Arc::ptr_eq(&t1, &table.load_full()), "bad config kept old");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn sighup_reloads_instead_of_terminating() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let dir = tempdir("sighup");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "local_health_path = \"/up\"\n[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n").unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .arg("--config")
        .arg(&cfg)
        .args(["--bind", "127.0.0.1:0", "--metrics-bind", "127.0.0.1:0"])
        .env_remove("FERRYMAN_CONFIG_TOML")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut child = Kill(child);
    let mut lines = BufReader::new(stdout).lines();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in lines.by_ref().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let wait_for = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let l = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("no log line containing {needle:?}"));
            if l.contains(needle) {
                return;
            }
        }
    };
    wait_for("admin listener bound");
    let hup = Command::new("kill")
        .args(["-HUP", &child.0.id().to_string()])
        .status()
        .unwrap();
    assert!(hup.success());
    wait_for("received SIGHUP");
    wait_for("config reloaded");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "SIGHUP killed ferryman"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- TLS ----

#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

type ClientTls = tokio_rustls::client::TlsStream<TcpStream>;

/// Handshake with the proxy; returns the connection and the DER of the leaf
/// certificate it presented.
async fn tls_connect(addr: SocketAddr) -> (ClientTls, Vec<u8>) {
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let s = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect("localhost".try_into().unwrap(), tcp)
        .await
        .unwrap();
    let der = s.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    (s, der)
}

fn fixture_der(name: &str) -> Vec<u8> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    CertificateDer::from_pem_file(Path::new(FIXTURES).join(name))
        .unwrap()
        .to_vec()
}

/// Atomically (rename) install fixture `cert`/`key` as dir/cert.pem, dir/key.pem.
fn install(dir: &Path, cert: &str, key: &str) {
    for (src, dst) in [(cert, "cert.pem"), (key, "key.pem")] {
        let tmp = dir.join("tmp");
        std::fs::copy(Path::new(FIXTURES).join(src), &tmp).unwrap();
        std::fs::rename(&tmp, dir.join(dst)).unwrap();
    }
}

struct TlsRig {
    dir: PathBuf,
    proxy: SocketAddr,
    _w: notify::RecommendedWatcher,
}

async fn tls_rig(tag: &str) -> TlsRig {
    let dir = tempdir(tag);
    install(&dir, "test-cert.pem", "test-key.pem");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "local_health_path = \"/up\"\n[[routes]]\nprefix = \"/\"\nupstream = \"http://127.0.0.1:1\"\n").unwrap();
    let table = table_from(&cfg);
    let (acceptor, tls) =
        ferryman::tls::load_reloadable(&dir.join("cert.pem"), &dir.join("key.pem")).unwrap();
    let w = Reloader::new(Some(cfg), table.clone(), Some(tls))
        .watch()
        .unwrap();
    let proxy = start_proxy(table, Some(acceptor)).await;
    TlsRig { dir, proxy, _w: w }
}

async fn wait_cert(proxy: SocketAddr, want: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while tls_connect(proxy).await.1 != want {
        assert!(Instant::now() < deadline, "new cert never served");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn tls_cert_swap_serves_new_cert_and_spares_old_connections() {
    let rig = tls_rig("tls-swap").await;
    let (cert1, cert2) = (fixture_der("test-cert.pem"), fixture_der("test-cert-2.pem"));
    let (mut old, der) = tls_connect(rig.proxy).await;
    assert_eq!(der, cert1);

    install(&rig.dir, "test-cert-2.pem", "test-key-2.pem");
    wait_cert(rig.proxy, &cert2).await;

    // The connection established before the swap still works.
    old.write_all(b"GET /up HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    old.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn bad_tls_files_keep_the_old_cert_and_a_later_good_pair_recovers() {
    let rig = tls_rig("tls-bad").await;
    let (cert1, cert2) = (fixture_der("test-cert.pem"), fixture_der("test-cert-2.pem"));

    // Garbage cert, then a mismatched cert/key pair.
    std::fs::write(rig.dir.join("cert.pem"), "not a certificate").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(tls_connect(rig.proxy).await.1, cert1, "garbage cert");
    std::fs::copy(
        Path::new(FIXTURES).join("test-cert-2.pem"),
        rig.dir.join("cert.pem"),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(tls_connect(rig.proxy).await.1, cert1, "mismatched pair");

    // Fixing the key completes the pair: the watcher is not wedged.
    install(&rig.dir, "test-cert-2.pem", "test-key-2.pem");
    wait_cert(rig.proxy, &cert2).await;
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn busy_directory_cannot_starve_the_reload() {
    let up = spawn_upstream().await;
    let dir = tempdir("busy");
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, route("/a", up)).unwrap();
    let table = table_from(&cfg);
    let _w = Reloader::new(Some(cfg.clone()), table.clone(), None)
        .watch()
        .unwrap();
    let proxy = start_proxy(table, None).await;

    // An unrelated file in the same directory is written every 50 ms.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s2, log) = (stop.clone(), dir.join("noise.log"));
    let noise = std::thread::spawn(move || {
        while !s2.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = std::fs::write(&log, "x");
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&cfg, route("/a", up) + &route("/b", up)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while status(proxy, "/b").await != 200 {
        assert!(Instant::now() < deadline, "reload starved by noise");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    noise.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
