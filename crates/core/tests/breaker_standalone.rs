use ferryman_core::Admission::{Normal, Probe};
use ferryman_core::{Breaker, BreakerConfig, CircuitState};
use std::time::Duration;

#[test]
fn defaults_match_config_defaults() {
    let c = BreakerConfig::default();
    assert_eq!(c.failure_threshold, 3);
    assert_eq!(c.cooldown, Duration::from_secs(30));
    assert!(c.validate().is_ok());
}

#[test]
fn rejects_invalid_config() {
    let e = Breaker::new(BreakerConfig::default().with_cooldown(Duration::from_micros(999)));
    assert!(e.err().unwrap().to_string().contains("cooldown"));
    let e = Breaker::new(BreakerConfig::default().with_cooldown(Duration::ZERO));
    assert!(e.is_err());
    let e = Breaker::new(BreakerConfig::default().with_failure_threshold(0));
    assert!(e.err().unwrap().to_string().contains("failure_threshold"));
    assert!(Breaker::new(BreakerConfig::default().with_cooldown(Duration::from_millis(1))).is_ok());
}

#[test]
fn standalone_lifecycle() {
    let b = Breaker::new(
        BreakerConfig::default()
            .with_failure_threshold(2)
            .with_cooldown(Duration::from_millis(200))
            .with_name("standalone"),
    )
    .unwrap();
    assert_eq!(b.state(), CircuitState::Closed);
    assert_eq!(b.try_acquire(), Some(Normal));
    b.record_failure(Normal);
    b.record_failure(Normal);
    assert_eq!(b.state(), CircuitState::Open);
    assert_eq!(b.try_acquire(), None);

    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(b.try_acquire(), Some(Probe));
    assert_eq!(b.state(), CircuitState::HalfOpen);
    assert_eq!(b.try_acquire(), None);
    b.record_success(Probe);
    assert_eq!(b.state(), CircuitState::Closed);
    assert_eq!(b.try_acquire(), Some(Normal));
}

#[test]
fn unnamed_breaker_works() {
    let b = Breaker::new(BreakerConfig::default().with_failure_threshold(1)).unwrap();
    b.record_failure(Normal);
    assert_eq!(b.state(), CircuitState::Open);
}
