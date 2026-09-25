//! ferryman-core — routing table, per-upstream circuit breaker, active
//! health checker, and TOML config schema.
//!
//! The server crate wires these primitives together behind a hyper service.
//! Everything in this crate is reload-safe: `SharedTable` is an
//! `Arc<ArcSwap<RouteTable>>` so a new table can be hot-swapped in without
//! touching live connections.

mod breaker;
pub mod config;
pub mod health;
pub mod route;

pub use breaker::CircuitState;
pub use config::{build_table, load_config, ConfigToml, RouteToml};
pub use health::health_loop;
pub use route::{Admission, Route, RouteTable, SharedTable, Upstream};
