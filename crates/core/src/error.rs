//! The error type returned by every fallible `ferryman-core` function.

use std::path::PathBuf;

/// Why a config, upstream, breaker or CIDR list was rejected.
///
/// Match on the variant to react by category; `Display` gives the
/// human-readable message (it names the route prefix and upstream where one
/// is involved). Variants may be added in minor releases.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The config file could not be read.
    #[error("reading config file {}", path.display())]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The config file is not valid TOML for the schema.
    ///
    /// `path` is `None` when parsed from a string or table
    /// ([`ConfigToml::from_str`](crate::ConfigToml), `from_table`).
    #[error("parsing config{}", match path { Some(p) => format!(" file {}", p.display()), None => String::new() })]
    Toml {
        path: Option<PathBuf>,
        #[source]
        source: toml::de::Error,
    },
    /// A top-level key is out of range. `reason` is e.g. `must be >= 1`.
    #[error("{key} {reason}")]
    InvalidConfig { key: &'static str, reason: String },
    /// A route prefix does not start with `/`.
    #[error("route prefix {prefix:?} must start with '/'")]
    InvalidPrefix { prefix: String },
    /// Two routes share a prefix.
    #[error("duplicate route prefix {prefix:?}")]
    DuplicatePrefix { prefix: String },
    /// A route's upstream URI is unusable (bad URI, scheme, host, path...).
    #[error("route {route:?}: {reason}")]
    InvalidUpstream {
        route: String,
        upstream: String,
        reason: String,
        #[source]
        source: Option<http::uri::InvalidUri>,
    },
    /// A route's `cooldown_secs` is out of range.
    #[error("route {route:?}: cooldown_secs {reason}")]
    InvalidCooldown { route: String, reason: String },
    /// Routes sharing one upstream (one breaker) disagree on the cooldown.
    #[error(
        "route {route:?}: upstream {upstream} is shared with another route but has a \
         different cooldown ({cooldown_secs}s vs {other_secs}s); routes to one upstream share one breaker"
    )]
    ConflictingCooldown {
        route: String,
        upstream: String,
        cooldown_secs: u64,
        other_secs: u64,
    },
    /// A route's `health_path` is not an absolute path without query or fragment.
    #[error("route {route:?}: health_path {path:?} must start with '/' and contain no '?' or '#'")]
    InvalidHealthPath { route: String, path: String },
    /// Routes sharing one upstream (one breaker) disagree on a health setting.
    #[error(
        "route {route:?}: upstream {upstream} is shared with another route but has a \
         different {setting} ({value} vs {other}); routes to one upstream share one health check"
    )]
    ConflictingHealth {
        route: String,
        upstream: String,
        /// `"health_path"` or `"health_disabled"`.
        setting: &'static str,
        value: String,
        other: String,
    },
    /// A `trusted_proxies` entry is not a valid CIDR or address.
    #[error("trusted_proxies entry {entry:?}: {reason}")]
    InvalidCidr { entry: String, reason: String },
    /// `BreakerConfig` failed validation (`Breaker::new`, `Upstream::new`).
    /// `upstream` is the `host:port` name when raised by `Upstream::new`.
    #[error("{reason}{}", upstream.as_ref().map(|u| format!(" (upstream {u})")).unwrap_or_default())]
    InvalidBreakerConfig {
        reason: String,
        upstream: Option<String>,
    },
}

const _: fn() = || {
    fn assert<T: std::error::Error + Send + Sync + 'static>() {}
    assert::<Error>();
};
