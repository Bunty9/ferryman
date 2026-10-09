//! ferryman-core — routing table, per-upstream circuit breaker, active
//! health checker, and TOML config schema.
//!
//! The server crate wires these primitives together behind a hyper service.
//! Everything in this crate is reload-safe: `SharedTable` is an
//! `Arc<ArcSwap<RouteTable>>` so a new table can be hot-swapped in without
//! touching live connections.
//!
//! ```
//! use ferryman_core::{build_table, ConfigToml};
//!
//! let cfg: ConfigToml = toml::from_str(r#"
//!     [[routes]]
//!     prefix = "/svc-a"
//!     upstream = "http://127.0.0.1:8001"
//! "#)?;
//! let table = build_table(cfg, None)?;
//!
//! let route = table.lookup("/svc-a/users").expect("route");
//! assert!(table.lookup("/svc-ab").is_none());
//! if let Some(ticket) = route.upstream.try_acquire() {
//!     // ... forward the request, then report the outcome:
//!     route.upstream.record_success(ticket);
//! } else {
//!     // circuit open: answer 503
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod breaker;
pub mod config;
mod error;
pub mod health;
mod proxies;
pub mod route;

pub use breaker::{Breaker, BreakerConfig, CircuitState};
pub use config::{build_table, load_config, ConfigToml, RouteToml};
pub use error::Error;
pub use health::health_loop;
pub use proxies::TrustedProxies;
pub use route::{Admission, Route, RouteTable, SharedTable, Upstream};
