//! Own test binary: tracing callsite interest is cached globally.

use ferryman_core::Admission::Normal;
use ferryman_core::{Breaker, BreakerConfig};
use std::sync::{Arc, Mutex};

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
fn unnamed_breaker_logs_fallback_label() {
    let buf = Buf::default();
    let w = buf.clone();
    tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .with_ansi(false)
        .init();
    let b = Breaker::new(BreakerConfig::default().with_failure_threshold(1)).unwrap();
    b.record_failure(Normal);
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("upstream=unnamed") && out.contains("to=Open"),
        "{out}"
    );
}
