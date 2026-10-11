//! Minimal `sd_notify(3)`: datagrams to `$NOTIFY_SOCKET`, std only (plus
//! `clock_gettime` for `MONOTONIC_USEC`). A no-op when the variable is unset
//! and on non-Unix targets. `NOTIFY_SOCKET` is left in the environment:
//! ferryman spawns no children.

pub struct Notifier(#[cfg(unix)] Option<(std::os::unix::net::UnixDatagram, String)>);

impl Notifier {
    pub fn from_env() -> Self {
        Self::new(std::env::var("NOTIFY_SOCKET").ok().as_deref())
    }

    #[cfg(unix)]
    pub fn new(path: Option<&str>) -> Self {
        Self(path.filter(|p| !p.is_empty()).and_then(|p| {
            let s = std::os::unix::net::UnixDatagram::unbound().ok()?;
            Some((s, p.to_owned()))
        }))
    }

    #[cfg(not(unix))]
    pub fn new(_: Option<&str>) -> Self {
        Self()
    }

    #[cfg(unix)]
    fn send(&self, msg: &str) {
        let Some((sock, path)) = &self.0 else { return };
        let r = match path.strip_prefix('@') {
            #[cfg(target_os = "linux")]
            Some(name) => {
                use std::os::linux::net::SocketAddrExt;
                std::os::unix::net::SocketAddr::from_abstract_name(name)
                    .and_then(|a| sock.send_to_addr(msg.as_bytes(), &a))
            }
            _ => sock.send_to(msg.as_bytes(), path),
        };
        if let Err(e) = r {
            tracing::warn!(error = %e, "sd_notify failed");
        }
    }

    #[cfg(not(unix))]
    fn send(&self, _: &str) {}

    pub fn ready(&self, status: &str) {
        self.send(&format!("READY=1\nSTATUS={status}"));
    }

    pub fn reloading(&self) {
        self.send(&format!("RELOADING=1\nMONOTONIC_USEC={}", monotonic_usec()));
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1\nSTATUS=shutting down");
    }
}

#[cfg(unix)]
fn monotonic_usec() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists on Unix.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

#[cfg(not(unix))]
fn monotonic_usec() -> u64 {
    0
}

#[cfg(all(test, unix))]
mod tests {
    use super::Notifier;
    use std::os::unix::net::UnixDatagram;

    fn recv(s: &UnixDatagram) -> String {
        let mut b = [0u8; 256];
        let n = s.recv(&mut b).unwrap();
        String::from_utf8_lossy(&b[..n]).into_owned()
    }

    #[test]
    fn sends_ready_reloading_stopping() {
        let dir = std::env::temp_dir().join(format!("ferryman-sdn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify");
        let _ = std::fs::remove_file(&path);
        let srv = UnixDatagram::bind(&path).unwrap();
        srv.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let n = Notifier::new(path.to_str());
        n.ready("serving");
        assert_eq!(recv(&srv), "READY=1\nSTATUS=serving");
        n.reloading();
        let m = recv(&srv);
        let usec = m.strip_prefix("RELOADING=1\nMONOTONIC_USEC=").unwrap();
        assert!(usec.parse::<u64>().unwrap() > 0, "{m}");
        n.stopping();
        assert!(recv(&srv).starts_with("STOPPING=1"));
        // Unset / empty: silent no-op.
        Notifier::new(None).ready("x");
        Notifier::new(Some("")).ready("x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("ferryman-sdn-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
        let srv = UnixDatagram::bind_addr(&addr).unwrap();
        srv.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        Notifier::new(Some(&format!("@{name}"))).stopping();
        assert!(recv(&srv).starts_with("STOPPING=1"));
    }
}
