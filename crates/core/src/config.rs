//! TOML config schema + reload-time builder.
//!
//! `config.toml` is the authoritative source for routing rules. The reload
//! task in `crates/server/src/reload.rs` re-reads this file on filesystem
//! events, calls [`build_table`], and atomically swaps the result into the
//! `SharedTable`.

use crate::route::{RouteTable, Upstream};
use serde::Deserialize;

/// Top-level config file.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigToml {
    /// Active health-check interval in seconds.
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// Default circuit-breaker cooldown if a route doesn't override.
    #[serde(default = "default_cooldown")]
    pub default_cooldown_secs: u64,
    pub routes: Vec<RouteToml>,
}

fn default_health_interval() -> u64 {
    5
}
fn default_cooldown() -> u64 {
    30
}

/// One routing rule.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteToml {
    /// Path prefix to match (e.g. `/svc-a`).
    pub prefix: String,
    /// Upstream URI, e.g. `http://localhost:8001`.
    pub upstream: String,
    /// Per-route circuit-breaker cooldown override.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

/// Build a [`RouteTable`] from a parsed [`ConfigToml`]. Returns an error if
/// any upstream URI fails to parse — the caller should keep the old table
/// in that case.
pub fn build_table(cfg: ConfigToml) -> anyhow::Result<RouteTable> {
    let default_cooldown = cfg.default_cooldown_secs;
    let mut rules = Vec::with_capacity(cfg.routes.len());
    for r in cfg.routes {
        let uri: http::Uri = r.upstream.parse()?;
        let cooldown = r.cooldown_secs.unwrap_or(default_cooldown);
        rules.push((r.prefix, Upstream::new(uri, cooldown)));
    }
    Ok(RouteTable::new(rules))
}
