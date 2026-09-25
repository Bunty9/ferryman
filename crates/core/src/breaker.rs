//! Lock-free per-upstream circuit breaker.
//!
//! The request hot path (up to 50k rps) must never block on a lock, so all
//! state lives in atomics. `opened_at` is measured in milliseconds since a
//! process-wide monotonic base (`Instant`, not `SystemTime`), so it can't be
//! fooled by wall-clock adjustments.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn base_instant() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn now_millis() -> u64 {
    base_instant().elapsed().as_millis() as u64
}

/// Circuit breaker state. Mirrors the `ferryman_circuit_state` gauge values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CircuitState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl CircuitState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => CircuitState::Open,
            2 => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }
}

/// Lock-free circuit breaker for one upstream. Shared (via `Arc`) between
/// every `Upstream` clone / route that points at the same backend.
pub(crate) struct Breaker {
    name: String,
    state: AtomicU8,
    consecutive_failures: AtomicU32,
    opened_at_millis: AtomicU64,
    failure_threshold: u32,
    cooldown: Duration,
}

impl Breaker {
    pub(crate) fn new(name: String, cooldown: Duration, failure_threshold: u32) -> Self {
        let b = Self {
            name,
            state: AtomicU8::new(CircuitState::Closed as u8),
            consecutive_failures: AtomicU32::new(0),
            opened_at_millis: AtomicU64::new(0),
            failure_threshold,
            cooldown,
        };
        b.set_gauges(CircuitState::Closed);
        b
    }

    /// Copy runtime state (not config) from a breaker built for a previous
    /// route table, so a hot reload doesn't reset an open circuit.
    pub(crate) fn adopt_state(&self, prev: &Breaker) {
        self.consecutive_failures.store(
            prev.consecutive_failures.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.opened_at_millis.store(
            prev.opened_at_millis.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.state
            .store(prev.state.load(Ordering::Acquire), Ordering::Release);
    }

    pub(crate) fn state(&self) -> CircuitState {
        CircuitState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// May this request be sent to the upstream right now?
    ///
    /// `Closed` always allows. `Open` allows exactly one caller through per
    /// cooldown window (the half-open probe). `HalfOpen` normally refuses
    /// everyone else, unless the in-flight probe has itself been stuck
    /// longer than the cooldown (lost/cancelled), in which case a fresh
    /// probe is allowed.
    pub(crate) fn try_acquire(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // The winner of the timestamp CAS owns the single probe;
                // losers now see a fresh stamp and back off.
                if !self.claim_probe_slot() {
                    return false;
                }
                // A concurrent record_success may already have closed the
                // circuit; either way this request may proceed.
                if self
                    .state
                    .compare_exchange(
                        CircuitState::Open as u8,
                        CircuitState::HalfOpen as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    self.set_gauges(CircuitState::HalfOpen);
                }
                true
            }
            // The probe that won the half-open slot may be lost (never
            // called record_success/record_failure); after another cooldown
            // a fresh probe is allowed.
            CircuitState::HalfOpen => self.claim_probe_slot(),
        }
    }

    /// If a full cooldown has passed since `opened_at`, atomically restamp it
    /// to now. Returns true for exactly one caller per window.
    fn claim_probe_slot(&self) -> bool {
        let opened_at = self.opened_at_millis.load(Ordering::Acquire);
        let now = now_millis();
        if now.saturating_sub(opened_at) < self.cooldown.as_millis() as u64 {
            return false;
        }
        self.opened_at_millis
            .compare_exchange(opened_at, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let prev = self
            .state
            .swap(CircuitState::Closed as u8, Ordering::AcqRel);
        if prev != CircuitState::Closed as u8 {
            self.set_gauges(CircuitState::Closed);
        }
    }

    pub(crate) fn record_failure(&self) {
        match self.state() {
            CircuitState::HalfOpen => {
                // Stamp before publishing Open so no reader pairs the Open
                // state with a stale timestamp.
                self.opened_at_millis.store(now_millis(), Ordering::Release);
                if self
                    .state
                    .compare_exchange(
                        CircuitState::HalfOpen as u8,
                        CircuitState::Open as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    self.set_gauges(CircuitState::Open);
                }
            }
            CircuitState::Open => {
                // Already open; nothing to do.
            }
            CircuitState::Closed => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
                if failures < self.failure_threshold {
                    return;
                }
                self.opened_at_millis.store(now_millis(), Ordering::Release);
                if self
                    .state
                    .compare_exchange(
                        CircuitState::Closed as u8,
                        CircuitState::Open as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    self.set_gauges(CircuitState::Open);
                }
            }
        }
    }

    fn set_gauges(&self, state: CircuitState) {
        metrics::gauge!("ferryman_circuit_state", "upstream" => self.name.clone())
            .set(state as u8 as f64);
        metrics::gauge!("ferryman_upstream_alive", "upstream" => self.name.clone()).set(
            if state == CircuitState::Closed {
                1.0
            } else {
                0.0
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn breaker(cooldown_ms: u64, threshold: u32) -> Breaker {
        Breaker::new(
            "test:1".to_string(),
            Duration::from_millis(cooldown_ms),
            threshold,
        )
    }

    #[test]
    fn closed_always_acquires() {
        let b = breaker(50, 3);
        assert_eq!(b.state(), CircuitState::Closed);
        assert!(b.try_acquire());
    }

    #[test]
    fn opens_after_threshold_failures() {
        let b = breaker(50, 3);
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Closed);
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        assert!(!b.try_acquire());
    }

    #[test]
    fn half_open_single_probe() {
        let b = Arc::new(breaker(20, 1));
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        thread::sleep(Duration::from_millis(30));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = b.clone();
            handles.push(thread::spawn(move || b.try_acquire()));
        }
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&ok| ok)
            .count();
        assert_eq!(
            winners, 1,
            "exactly one caller should win the half-open probe"
        );
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_failure_reopens() {
        let b = breaker(20, 1);
        b.record_failure();
        thread::sleep(Duration::from_millis(30));
        assert!(b.try_acquire());
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        assert!(!b.try_acquire());
    }

    #[test]
    fn half_open_success_closes() {
        let b = breaker(20, 1);
        b.record_failure();
        thread::sleep(Duration::from_millis(30));
        assert!(b.try_acquire());
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_success();
        assert_eq!(b.state(), CircuitState::Closed);
        assert!(b.try_acquire());
    }

    #[test]
    fn stuck_half_open_probe_recovers() {
        let b = breaker(20, 1);
        b.record_failure();
        thread::sleep(Duration::from_millis(30));
        assert!(b.try_acquire());
        assert_eq!(b.state(), CircuitState::HalfOpen);

        // Probe never reports back. After another cooldown window a new
        // probe must still be allowed through instead of sticking forever.
        thread::sleep(Duration::from_millis(30));
        assert!(b.try_acquire());
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn adopt_state_preserves_open_across_rebuild() {
        let old = breaker(1000, 1);
        old.record_failure();
        assert_eq!(old.state(), CircuitState::Open);

        let fresh = breaker(1000, 1);
        assert_eq!(fresh.state(), CircuitState::Closed);
        fresh.adopt_state(&old);
        assert_eq!(fresh.state(), CircuitState::Open);
    }
}
