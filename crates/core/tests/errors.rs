//! `ferryman_core::Error` is matchable by category and its messages are stable.

use ferryman_core::{build_table, BreakerConfig, ConfigToml, Error, TrustedProxies, Upstream};
use std::time::Duration;

fn table_err(toml: &str) -> Error {
    let cfg: ConfigToml = toml::from_str(toml).unwrap();
    build_table(cfg, None).err().expect("should be rejected")
}

fn route(prefix: &str, upstream: &str, extra: &str) -> String {
    format!("[[routes]]\nprefix = \"{prefix}\"\nupstream = \"{upstream}\"\n{extra}\n")
}

#[test]
fn config_errors_match_by_variant_and_keep_messages() {
    let e = table_err(&format!(
        "failure_threshold = 0\n{}",
        route("/a", "http://h:1", "")
    ));
    assert!(
        matches!(
            &e,
            Error::InvalidConfig {
                key: "failure_threshold",
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(e.to_string(), "failure_threshold must be >= 1, got 0");

    let e = table_err(&format!(
        "upstream_timeout_secs = 86401\n{}",
        route("/a", "http://h:1", "")
    ));
    assert_eq!(
        e.to_string(),
        "upstream_timeout_secs must be <= 86400, got 86401"
    );

    let e = table_err(&route("a", "http://h:1", ""));
    assert!(matches!(&e, Error::InvalidPrefix { prefix } if prefix == "a"));
    assert_eq!(e.to_string(), "route prefix \"a\" must start with '/'");

    let e = table_err(&(route("/a", "http://h:1", "") + &route("/a", "http://h:2", "")));
    assert!(matches!(&e, Error::DuplicatePrefix { prefix } if prefix == "/a"));
    assert_eq!(e.to_string(), "duplicate route prefix \"/a\"");

    let e = table_err(&route("/a", "https://h:1", ""));
    assert!(matches!(&e, Error::InvalidUpstream { route, upstream, .. }
        if route == "/a" && upstream == "https://h:1"));
    assert_eq!(
        e.to_string(),
        "route \"/a\": upstream \"https://h:1\" has scheme \"https\" — https upstreams are not supported"
    );

    let e = table_err(&route("/a", "http://h:1", "cooldown_secs = 0"));
    assert!(matches!(&e, Error::InvalidCooldown { .. }));
    assert_eq!(e.to_string(), "route \"/a\": cooldown_secs must be >= 1");

    let e = table_err(
        &(route("/a", "http://h:1", "cooldown_secs = 5")
            + &route("/b", "http://h:1", "cooldown_secs = 9")),
    );
    assert!(
        matches!(&e, Error::ConflictingCooldown { route, upstream, cooldown_secs: 9, other_secs: 5 }
        if route == "/b" && upstream == "h:1")
    );
    assert!(e.to_string().starts_with(
        "route \"/b\": upstream h:1 is shared with another route but has a different cooldown (9s vs 5s);"
    ));
}

#[test]
fn cidr_error() {
    let e = TrustedProxies::parse(&["10.0.0.0/33"]).unwrap_err();
    assert!(matches!(&e, Error::InvalidCidr { entry, .. } if entry == "10.0.0.0/33"));
    assert_eq!(
        e.to_string(),
        "trusted_proxies entry \"10.0.0.0/33\": prefix length must be 0..=32"
    );
}

#[test]
fn load_config_errors_keep_source() {
    use std::error::Error as _;
    let dir = std::env::temp_dir().join(format!("ferryman-errors-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("nope.toml");
    let e = ferryman_core::load_config(&missing).unwrap_err();
    assert!(matches!(e, Error::ReadConfig { .. }));
    assert!(e.source().is_some());
    assert_eq!(
        e.to_string(),
        format!("reading config file {}", missing.display())
    );
    let bad = dir.join("bad.toml");
    std::fs::write(&bad, "routes = 3").unwrap();
    let e = ferryman_core::load_config(&bad).unwrap_err();
    assert!(matches!(e, Error::Toml { .. }));
    assert!(e.source().is_some());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn upstream_new_validates_breaker_config() {
    let uri = || "http://h:1".parse().unwrap();
    let c = BreakerConfig::default();
    let e = Upstream::new(uri(), c.clone().with_cooldown(Duration::from_micros(999)))
        .err()
        .unwrap();
    assert!(matches!(e, Error::InvalidBreakerConfig { .. }));
    assert!(Upstream::new(uri(), c.clone().with_cooldown(Duration::ZERO)).is_err());
    let e = Upstream::new(uri(), c.clone().with_failure_threshold(0))
        .err()
        .unwrap();
    assert_eq!(
        e.to_string(),
        "failure_threshold must be >= 1, got 0 (upstream h:1)"
    );
    let u = Upstream::new(uri(), c.with_cooldown(Duration::from_millis(1))).unwrap();
    assert_eq!(u.name, "h:1");
}

#[test]
fn upstream_new_error_names_the_upstream() {
    let e = Upstream::new(
        "http://h:1".parse().unwrap(),
        BreakerConfig::default().with_failure_threshold(0),
    )
    .err()
    .unwrap();
    assert!(matches!(&e, Error::InvalidBreakerConfig { upstream: Some(u), .. } if u == "h:1"));
    assert_eq!(
        e.to_string(),
        "failure_threshold must be >= 1, got 0 (upstream h:1)"
    );
}

#[test]
fn alternate_display_via_anyhow_shows_toml_cause() {
    let path = std::env::temp_dir().join(format!("ferryman-chain-{}.toml", std::process::id()));
    std::fs::write(&path, "routes = 3").unwrap();
    let e = ferryman_core::load_config(&path).unwrap_err();
    std::fs::remove_file(&path).ok();
    let chain = format!("{:#}", anyhow::Error::from(e));
    assert!(chain.contains("parsing config file"), "{chain}");
    assert!(chain.contains("invalid type"), "{chain}");
}
