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

use crate::Error;
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
///
/// Non-exhaustive: matching without a wildcard arm does not compile.
///
/// ```compile_fail,E0004
/// use ferryman_core::CircuitState;
/// fn f(s: CircuitState) -> u8 {
///     match s {
///         CircuitState::Closed => 0,
///         CircuitState::Open => 1,
///         CircuitState::HalfOpen => 2,
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
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
///
/// Non-exhaustive: matching without a wildcard arm does not compile.
///
/// ```compile_fail,E0004
/// use ferryman_core::Admission;
/// fn f(a: Admission) -> u8 {
///     match a {
///         Admission::Normal => 0,
///         Admission::Probe => 1,
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Admission {
    /// Ordinary request through a closed circuit. Its result only counts
    /// while the circuit is still closed.
    Normal,
    /// The single half-open probe, or an active health check. Its result is
    /// authoritative: success closes the circuit, failure of a half-open
    /// probe reopens it (and restarts the cooldown). A failing probe while
    /// already open changes nothing.
    Probe,
}

/// Circuit breaker settings. Build with [`Default`] and the `with_*` setters;
/// [`Breaker::new`] validates them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BreakerConfig {
    /// Consecutive failures that open the circuit. Must be at least 1.
    pub failure_threshold: u32,
    /// How long the circuit stays open before one probe is let through.
    /// Must be at least 1 ms.
    pub cooldown: Duration,
    /// Log label only: standalone breakers never publish metric gauges.
    /// Keep it to a small fixed set of names.
    pub name: Option<String>,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
            name: None,
        }
    }
}

impl BreakerConfig {
    /// Consecutive failures that open the circuit (must be at least 1).
    pub fn with_failure_threshold(mut self, failure_threshold: u32) -> Self {
        self.failure_threshold = failure_threshold;
        self
    }

    /// Time to stay open before admitting a probe. Validated by
    /// [`Breaker::new`]: must be at least 1 ms.
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Label used in transition logs only (no gauges). Keep it to a small
    /// fixed set of names, not per-request values.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Check the invariants [`Breaker::new`] relies on.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidBreakerConfig`] if the threshold is 0 or the cooldown
    /// is under 1 ms.
    pub fn validate(&self) -> Result<(), Error> {
        if self.failure_threshold < 1 {
            return Err(Error::InvalidBreakerConfig {
                reason: "failure_threshold must be >= 1, got 0".into(),
                upstream: None,
            });
        }
        if self.cooldown < Duration::from_millis(1) {
            return Err(Error::InvalidBreakerConfig {
                reason: format!("cooldown must be >= 1ms, got {:?}", self.cooldown),
                upstream: None,
            });
        }
        Ok(())
    }
}

/// Lock-free circuit breaker. Cheap to share (wrap in `Arc`); an upstream's
/// breaker is shared between every route that points at the backend, and
/// across hot reloads.
///
/// ```
/// use ferryman_core::{Admission, Breaker, BreakerConfig, CircuitState};
///
/// let breaker = Breaker::new(BreakerConfig::default().with_failure_threshold(1))?;
/// let ticket = breaker.try_acquire().expect("closed circuit admits");
/// breaker.record_failure(ticket);
/// // `CircuitState` and `Admission` are #[non_exhaustive]: keep a wildcard arm.
/// let label = match breaker.state() {
///     CircuitState::Closed => "closed",
///     CircuitState::Open => "open",
///     _ => "half-open or newer",
/// };
/// assert_eq!(label, "open");
/// assert!(breaker.try_acquire().is_none());
/// match Admission::Normal {
///     Admission::Probe => {}
///     _ => {}
/// }
/// # Ok::<(), ferryman_core::Error>(())
/// ```
pub struct Breaker {
    name: Option<String>,
    /// Only upstream breakers write the `ferryman_*` gauges.
    gauges: bool,
    state: AtomicU8,
    consecutive_failures: AtomicU32,
    opened_at_millis: AtomicU64,
    // Config lives in atomics so a hot reload can retune a shared breaker.
    failure_threshold: AtomicU32,
    cooldown_millis: AtomicU64,
}

impl std::fmt::Debug for Breaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Breaker")
            .field("name", &self.name)
            .field("state", &self.state())
            .field(
                "failure_threshold",
                &self.failure_threshold.load(Ordering::Relaxed),
            )
            .field(
                "cooldown",
                &Duration::from_millis(self.cooldown_millis.load(Ordering::Relaxed)),
            )
            .finish()
    }
}

impl Breaker {
    /// Create a closed breaker. Fails if `config` is invalid.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidBreakerConfig`] (see [`BreakerConfig::validate`]).
    pub fn new(config: BreakerConfig) -> Result<Self, Error> {
        config.validate()?;
        Ok(Self::unchecked(
            config.name,
            false,
            config.cooldown,
            config.failure_threshold,
        ))
    }

    /// Validated breaker that writes the per-upstream gauges.
    pub(crate) fn for_upstream(name: String, config: &BreakerConfig) -> Result<Self, Error> {
        config.validate().map_err(|e| match e {
            Error::InvalidBreakerConfig { reason, .. } => Error::InvalidBreakerConfig {
                reason,
                upstream: Some(name.clone()),
            },
            e => e,
        })?;
        Ok(Self::unchecked(
            Some(name),
            true,
            config.cooldown,
            config.failure_threshold,
        ))
    }

    /// No validation; callers validate (or, in tests, want odd values).
    pub(crate) fn unchecked(
        name: Option<String>,
        gauges: bool,
        cooldown: Duration,
        failure_threshold: u32,
    ) -> Self {
        let b = Self {
            name,
            gauges,
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

    pub fn state(&self) -> CircuitState {
        CircuitState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// May this request be sent to the upstream right now?
    ///
    /// `Closed` admits everyone as [`Admission::Normal`]. `Open` admits
    /// exactly one caller per cooldown window as the half-open
    /// [`Admission::Probe`]. `HalfOpen` refuses everyone else, unless the
    /// in-flight probe has been out longer than a cooldown (lost or
    /// cancelled), in which case a fresh probe is admitted.
    pub fn try_acquire(&self) -> Option<Admission> {
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

    pub fn record_success(&self, admission: Admission) {
        match admission {
            Admission::Probe => {
                // A health check passing says nothing about whether real
                // requests are failing, so it must not reset their count.
                if self.state() == CircuitState::Closed {
                    return;
                }
                self.consecutive_failures.store(0, Ordering::Relaxed);
                let prev = CircuitState::from_u8(
                    self.state
                        .swap(CircuitState::Closed as u8, Ordering::AcqRel),
                );
                if prev != CircuitState::Closed {
                    self.log_transition(prev, CircuitState::Closed);
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

    pub fn record_failure(&self, admission: Admission) {
        match (admission, self.state()) {
            (Admission::Probe, CircuitState::HalfOpen) => {
                // Stamp before publishing Open so no reader pairs the Open
                // state with a stale timestamp.
                self.opened_at_millis.store(now_millis(), Ordering::Release);
                self.transition(CircuitState::HalfOpen, CircuitState::Open);
            }
            // A failing health check while already open must not push the
            // cooldown out: otherwise a broken health endpoint (tick < cooldown)
            // keeps the circuit open forever and no request-path probe ever
            // gets a slot. Only a failed half-open probe re-stamps.
            (Admission::Probe, CircuitState::Open) => {}
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
            self.log_transition(from, to);
            self.set_gauges();
        }
        ok
    }

    /// Only called after a state-changing CAS/swap, never on the hot path.
    fn log_transition(&self, from: CircuitState, to: CircuitState) {
        let name = self.name.as_deref().unwrap_or("unnamed");
        if to == CircuitState::Open {
            tracing::warn!(upstream = %name, ?from, ?to, "circuit breaker state change");
        } else {
            tracing::info!(upstream = %name, ?from, ?to, "circuit breaker state change");
        }
    }

    /// Publish the *current* state (not a transition target), so racing
    /// transitions can't leave the gauge showing a stale value.
    pub(crate) fn set_gauges(&self) {
        let (true, Some(name)) = (self.gauges, &self.name) else {
            return;
        };
        let state = self.state();
        metrics::gauge!("ferryman_circuit_state", "upstream" => name.clone())
            .set(state as u8 as f64);
        let alive = if state == CircuitState::Closed {
            1.0
        } else {
            0.0
        };
        metrics::gauge!("ferryman_upstream_alive", "upstream" => name.clone()).set(alive);
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
        Breaker::unchecked(
            Some("test:1".to_string()),
            true,
            Duration::from_millis(cooldown_ms),
            threshold,
        )
    }

    /// Open the breaker, wait out the cooldown, and take the probe.
    fn half_open(b: &Breaker) {
        b.record_failure(N);
        thread::sleep(Duration::from_millis(250));
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
        let b = Arc::new(breaker(200, 1));
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Open);
        thread::sleep(Duration::from_millis(250));

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
        let b = breaker(200, 1);
        half_open(&b);
        b.record_failure(P);
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.try_acquire(), None);
    }

    #[test]
    fn probe_failure_while_open_does_not_restamp() {
        // Health ticks (every 50 ms) are shorter than the cooldown (200 ms)
        // and always fail; a request-path probe must still get its slot.
        let b = breaker(200, 1);
        b.record_failure(N);
        for _ in 0..6 {
            thread::sleep(Duration::from_millis(50));
            b.record_failure(P);
        }
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.try_acquire(), Some(P), "cooldown must have elapsed");
        b.record_success(P);
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn probe_success_closes() {
        let b = breaker(200, 1);
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
    fn health_success_does_not_reset_request_failures() {
        let b = breaker(10_000, 2);
        b.record_failure(N);
        b.record_success(P);
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::Open);
    }

    #[test]
    fn late_normal_results_are_ignored() {
        let b = breaker(200, 1);
        b.record_failure(N);
        // A slow request admitted while closed finishes after the trip.
        b.record_success(N);
        assert_eq!(b.state(), CircuitState::Open);

        thread::sleep(Duration::from_millis(250));
        assert_eq!(b.try_acquire(), Some(P));
        // Another late failure must not reopen while the probe is out.
        b.record_failure(N);
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn stuck_half_open_probe_recovers() {
        let b = breaker(200, 1);
        half_open(&b);
        // Probe never reports back. After another cooldown window a new
        // probe must still be allowed through instead of sticking forever.
        thread::sleep(Duration::from_millis(250));
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
