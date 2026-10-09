//! Sole test in this file so it runs in its own process: tracing caches
//! callsite interest globally, and other tests' threads would otherwise
//! race a thread-local subscriber.

use ferryman_core::Admission::{Normal, Probe};
use ferryman_core::{BreakerConfig, Upstream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn transitions_log_once_and_noops_do_not() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .with_ansi(false)
        .init();
    let u = Upstream::new(
        "http://test:1".parse().unwrap(),
        BreakerConfig::default()
            .with_cooldown(Duration::from_millis(200))
            .with_failure_threshold(1),
    )
    .unwrap();
    u.record_failure(Normal); // closed -> open
    u.record_failure(Normal); // late normal result: no change
    u.record_failure(Probe); // probe failure while open: no change, no re-stamp
    u.record_success(Normal); // no change
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(u.try_acquire(), Some(Probe)); // open -> half-open
    u.record_failure(Probe); // half-open -> open
    u.record_success(Probe); // open -> closed
    u.record_success(Probe); // closed probe success: no-op

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].contains("WARN") && lines[0].contains("upstream=test:1"));
    assert!(lines[0].contains("from=Closed") && lines[0].contains("to=Open"));
    assert!(lines[1].contains("INFO") && lines[1].contains("from=Open"));
    assert!(lines[1].contains("to=HalfOpen"));
    assert!(lines[2].contains("WARN") && lines[2].contains("from=HalfOpen"));
    assert!(lines[2].contains("to=Open"));
    assert!(lines[3].contains("INFO") && lines[3].contains("to=Closed"));
}
