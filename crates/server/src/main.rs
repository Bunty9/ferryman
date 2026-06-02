//! ferryman-server — hyper-based reverse proxy with circuit breakers, active
//! health checks, and hot-reloaded TOML routing rules.
//!
//! Phase 1 goal: bind a TCP listener, serve requests through `proxy::handle`,
//! spawn the health-check loop, and start a filesystem watcher that
//! atomically swaps the routing table on config change.

mod proxy;
mod reload;

use arc_swap::ArcSwap;
use clap::Parser;
use ferryman_core::{build_table, health_loop, ConfigToml, SharedTable};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HttpAutoBuilder;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ferryman-server", about = "ferryman L7 reverse proxy")]
struct Args {
    /// Path to the TOML routing config.
    #[arg(long, env = "FERRYMAN_CONFIG", default_value = "config.toml")]
    config: PathBuf,

    /// Bind address for the proxy listener.
    #[arg(long, env = "FERRYMAN_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Bind address for the Prometheus `/metrics` listener.
    #[arg(long, env = "FERRYMAN_METRICS_BIND", default_value = "0.0.0.0:9090")]
    metrics_bind: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let args = Args::parse();

    // Load + parse the initial config. Fail fast on first-boot misconfiguration.
    let raw = std::fs::read_to_string(&args.config)?;
    let cfg: ConfigToml = toml::from_str(&raw)?;
    let interval = Duration::from_secs(cfg.health_interval_secs);
    let table = build_table(cfg)?;
    let shared: SharedTable = Arc::new(ArcSwap::from_pointee(table));

    // Prometheus exporter binds its own listener; the proxy is unaffected by
    // /metrics traffic.
    PrometheusBuilder::new()
        .with_http_listener(args.metrics_bind)
        .install()?;
    tracing::info!(addr = %args.metrics_bind, "metrics listener bound");

    // Background tasks: active health checker + config watcher.
    tokio::spawn(health_loop(shared.clone(), interval));
    let _watcher = reload::watch_config(&args.config, shared.clone())?;

    // Shared hyper client for upstream forwarding.
    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(HttpConnector::new());

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, "ferryman-server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let table = shared.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| proxy::handle(table.clone(), client.clone(), req));
            if let Err(e) = HttpAutoBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!(?peer, ?e, "connection closed with error");
            }
        });
    }
}

/// Placeholder for a future `/metrics` router served via tower (e.g. when we
/// want auth on the metrics surface). The Prometheus exporter currently owns
/// the listener directly — see `main()`.
#[allow(dead_code)]
fn metrics_router() {
    todo!("Phase 2: optional tower router for /metrics with auth + scrape filters");
}
