//! ferryman-server CLI — parses args, sets up tracing/metrics, loads the
//! initial routing config, spawns the health-check loop and hot-reload
//! watcher, then hands off to [`ferryman_server::serve`].

use arc_swap::ArcSwap;
use clap::Parser;
use ferryman_core::{build_table, health_loop, load_config, SharedTable};
use ferryman_server::{reload, tls};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_util::MetricKindMask;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
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

    /// TLS certificate PEM path. Requires `--tls-key`; omit both to serve
    /// plain HTTP.
    #[arg(long, env = "FERRYMAN_TLS_CERT")]
    tls_cert: Option<PathBuf>,

    /// TLS private key PEM path. Requires `--tls-cert`.
    #[arg(long, env = "FERRYMAN_TLS_KEY")]
    tls_key: Option<PathBuf>,
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

    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(tls::load_acceptor(cert, key)?),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must both be set or both omitted"),
    };

    // Prometheus exporter binds its own listener; the proxy is unaffected by
    // /metrics traffic. Installed before any gauge is set, or the writes go
    // to the no-op recorder.
    // Load + parse the initial config. Fail fast on first-boot misconfiguration.
    let cfg = load_config(&args.config)?;
    let interval = Duration::from_secs(cfg.health_interval_secs);

    // Prometheus exporter binds its own listener; the proxy is unaffected by
    // /metrics traffic. Installed before any gauge is set, or the writes go
    // to the no-op recorder. The health loop rewrites every live gauge each
    // tick, so gauges of upstreams removed by a reload expire after a few
    // missed ticks instead of reporting a stale value forever.
    PrometheusBuilder::new()
        .with_http_listener(args.metrics_bind)
        .idle_timeout(MetricKindMask::GAUGE, Some(interval * 3))
        .install()?;
    tracing::info!(addr = %args.metrics_bind, "metrics listener bound");
    let table = build_table(cfg, None)?;
    table.publish_gauges();
    let shared: SharedTable = Arc::new(ArcSwap::from_pointee(table));

    // Background tasks: active health checker + config watcher.
    tokio::spawn(health_loop(shared.clone(), interval));
    let _watcher = reload::watch_config(&args.config, shared.clone())?;

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, tls = tls_acceptor.is_some(), "ferryman-server listening");

    ferryman_server::serve(listener, shared, tls_acceptor, shutdown_signal()).await
}

/// Resolves on SIGINT or SIGTERM, for graceful shutdown.
async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        _ = sigint.recv() => tracing::info!("received SIGINT, shutting down"),
    }
}
