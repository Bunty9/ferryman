//! Lock-free per-upstream circuit breaker.
//!
//! The request hot path (up to 50k rps) must never block on a lock, so all
//! state lives in atomics. `opened_at` is measured in milliseconds since a
//! process-wide monotonic base (`Instant`, not `SystemTime`), so it can't be
//! fooled by wall-clock adjustments.
//!
//! Every admission hands back an [`Admission`] ticket that the caller passes
//! to `record_success` / `record_failure`. Only a [`Admission::Probe`] may
//! move the breaker out of open/half-open; results of ordinary requests that
//! finish after the circuit opened are late news and are ignored.

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

/// Why a request was let through. Pass it back when reporting the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Ordinary request through a closed circuit. Its result only counts
    /// while the circuit is still closed.
    Normal,
    /// The single half-open probe, or an active health check. Its result is
    /// authoritative: success closes the circuit, failure (re)opens it.
    Probe,
}

/// Lock-free circuit breaker for one upstream. Shared (via `Arc`) between
/// every route that points at the backend, and across hot reloads.
pub(crate) struct Breaker {
    name: String,
    state: AtomicU8,
    consecutive_failures: AtomicU32,
    opened_at_millis: AtomicU64,
    // Config lives in atomics so a hot reload can retune a shared breaker.
    failure_threshold: AtomicU32,
    cooldown_millis: AtomicU64,
}

impl Breaker {
    pub(crate) fn new(name: String, cooldown: Duration, failure_threshold: u32) -> Self {
        let b = Self {
            name,
            state: AtomicU8::new(CircuitState::Closed as u8),
            consecutive_failures: AtomicU32::new(0),
            opened_at_millis: AtomicU64::new(0),
            failure_threshold: AtomicU32::new(0),
            cooldown_millis: AtomicU64::new(0),
        };
        b.reconfigure(cooldown, failure_threshold);
        b
    }

    /// Apply new config from a hot reload without touching runtime state.
    pub(crate) fn reconfigure(&self, cooldown: Duration, failure_threshold: u32) {
        self.cooldown_millis
            .store(cooldown.as_millis() as u64, Ordering::Relaxed);
        self.failure_threshold
            .store(failure_threshold, Ordering::Relaxed);
    }

    pub(crate) fn state(&self) -> CircuitState {
        CircuitState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// May this request be sent to the upstream right now?
    ///
    /// `Closed` admits everyone as [`Admission::Normal`]. `Open` admits
    /// exactly one caller per cooldown window as the half-open
    /// [`Admission::Probe`]. `HalfOpen` refuses everyone else, unless the
    /// in-flight probe has been out longer than a cooldown (lost or
    /// cancelled), in which case a fresh probe is admitted.
    pub(crate) fn try_acquire(&self) -> Option<Admission> {
        match self.state() {
            CircuitState::Closed => Some(Admission::Normal),
            CircuitState::Open => {
                // The winner of the timestamp CAS owns the single probe;
                // losers now see a fresh stamp and back off.
                if !self.claim_probe_slot() {
                    return None;
                }
                if self.transition(CircuitState::Open, CircuitState::HalfOpen) {
                    Some(Admission::Probe)
                } else {
                    // Someone closed the circuit meanwhile; that's fine too.
                    Some(Admission::Normal)
                }
            }
            CircuitState::HalfOpen => self.claim_probe_slot().then_some(Admission::Probe),
        }
    }

    /// If a full cooldown has passed since `opened_at`, atomically restamp it
    /// to now. Returns true for exactly one caller per window.
    fn claim_probe_slot(&self) -> bool {
        let opened_at = self.opened_at_millis.load(Ordering::Acquire);
        let now = now_millis();
        if now.saturating_sub(opened_at) < self.cooldown_millis.load(Ordering::Relaxed) {
            return false;
        }
        self.opened_at_millis
            .compare_exchange(opened_at, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn record_success(&self, admission: Admission) {
        match admission {
            Admission::Probe => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                if self
                    .state
                    .swap(CircuitState::Closed as u8, Ordering::AcqRel)
                    != CircuitState::Closed as u8
                {
                    self.set_gauges();
                }
            }
            Admission::Normal => {
                if self.state() == CircuitState::Closed {
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                }
            }
        }
    }

    pub(crate) fn record_failure(&self, admission: Admission) {
        match (admission, self.state()) {
            (Admission::Probe, CircuitState::Open | CircuitState::HalfOpen) => {
                // Stamp before publishing Open so no reader pairs the Open
                // state with a stale timestamp.
                self.opened_at_millis.store(now_millis(), Ordering::Release);
                self.transition(CircuitState::HalfOpen, CircuitState::Open);
            }
            (_, CircuitState::Closed) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
                if failures < self.failure_threshold.load(Ordering::Relaxed) {
                    return;
                }
                self.opened_at_millis.store(now_millis(), Ordering::Release);
                self.transition(CircuitState::Closed, CircuitState::Open);
            }
            // Late result of a request admitted before the circuit opened.
            (Admission::Normal, _) => {}
        }
    }

    /// CAS `from -> to`, publishing gauges on success.
    fn transition(&self, from: CircuitState, to: CircuitState) -> bool {
        let ok = self
            .state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if ok {
            self.set_gauges();
        }
        ok
    }

    /// Publish the *current* state (not a transition target), so racing
    /// transitions can't leave the gauge showing a stale value.
    pub(crate) fn set_gauges(&self) {
        let state = self.state();
        metrics::gauge!("ferryman_circuit_state", "upstream" => self.name.clone())
            .set(state as u8 as f64);
        let alive = if state == CircuitState::Closed {
            1.0
        } else {
            0.0
        };
        metrics::gauge!("ferryman_upstream_alive", "upstream" => self.name.clone()).set(alive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    const N: Admission = Admission::Normal;
    const P: Admission = Admission::Probe;

    fn breaker(cooldown_ms: u64, threshold: u32) -> Breaker {
        Breaker::new(
            "test:1".to_string(),
            Duration::from_millis(cooldown_ms),
            threshold,
        )
    }

    /// Open the breaker, wait out the cooldown, and take the probe.
    fn half_open(b: &Breaker) {
        b.record_failure(N);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(b.try_acquire(), Some(P));
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn closed_always_acquires() {
        let b = breaker(50, 3);
        assert_eq!(b.state(), CircuitState::Closed);
        assert_eq!(b.try_acquire(), Some(N));
    }

    #[test]
    fn opens_after_threshold_failures() {
        let b = breaker(50, 3);
        b.record_failure(N);
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Closed);
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.try_acquire(), None);
    }

    #[test]
    fn success_resets_failure_count() {
        let b = breaker(50, 2);
        b.record_failure(N);
        b.record_success(N);
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_single_probe() {
        let b = Arc::new(breaker(20, 1));
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Open);
        thread::sleep(Duration::from_millis(30));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let b = b.clone();
                thread::spawn(move || b.try_acquire())
            })
            .collect();
        let winners = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .count();
        assert_eq!(winners, 1, "exactly one caller should win the probe");
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn probe_failure_reopens() {
        let b = breaker(20, 1);
        half_open(&b);
        b.record_failure(P);
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.try_acquire(), None);
    }

    #[test]
    fn probe_success_closes() {
        let b = breaker(20, 1);
        half_open(&b);
        b.record_success(P);
        assert_eq!(b.state(), CircuitState::Closed);
        assert_eq!(b.try_acquire(), Some(N));
    }

    #[test]
    fn health_probe_success_closes_open_circuit() {
        let b = breaker(10_000, 1);
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Open);
        b.record_success(P);
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn late_normal_results_are_ignored() {
        let b = breaker(20, 1);
        b.record_failure(N);
        // A slow request admitted while closed finishes after the trip.
        b.record_success(N);
        assert_eq!(b.state(), CircuitState::Open);

        thread::sleep(Duration::from_millis(30));
        assert_eq!(b.try_acquire(), Some(P));
        // Another late failure must not reopen while the probe is out.
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn stuck_half_open_probe_recovers() {
        let b = breaker(20, 1);
        half_open(&b);
        // Probe never reports back. After another cooldown window a new
        // probe must still be allowed through instead of sticking forever.
        thread::sleep(Duration::from_millis(30));
        assert_eq!(b.try_acquire(), Some(P));
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn reconfigure_changes_cooldown() {
        let b = breaker(10_000, 1);
        b.record_failure(N);
        assert_eq!(b.try_acquire(), None);
        b.reconfigure(Duration::from_millis(0), 1);
        assert_eq!(b.try_acquire(), Some(P));
    }
}
