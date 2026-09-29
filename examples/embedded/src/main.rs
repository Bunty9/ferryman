//! Thin CLI wrapper around the embedded-library API in `lib.rs`
//! (`Settings` / `start` / `Running::shutdown`), so this example is
//! literally runnable (`cargo run -p ferryman-embedded-example`) as well as
//! embeddable. An app that actually embeds ferryman wouldn't have this
//! file — it would call `ferryman_embedded_example::start` (or copy the
//! handful of lines below) from inside its own `main`, alongside whatever
//! else that app's process does.
//!
//! Mirrors crates/server/src/main.rs's shape (parse args, set up
//! tracing/metrics, boot, wait for a shutdown signal, shut down) so the two
//! are easy to compare line by line.

use clap::Parser;
use ferryman_embedded_example::{prometheus_builder, start, Settings};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "ferryman-embedded-example",
    about = "Reference example: ferryman embedded as a library",
    version
)]
struct Args {
    /// Path to the TOML routing config.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,

    /// Bind address for the proxy listener.
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Bind address for this example's admin server
    /// (`/healthz`, `/status`, `/metrics`).
    #[arg(long, default_value = "0.0.0.0:9091")]
    admin_bind: SocketAddr,

    /// TLS certificate PEM path. Requires `--tls-key`; omit both to serve
    /// plain HTTP.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// TLS private key PEM path. Requires `--tls-cert`.
    #[arg(long)]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must both be set or both omitted"),
    };

    // `start`'s signature (fixed by this crate's public API) only takes a
    // config *path* — it re-reads and parses the file itself. But the
    // Prometheus recorder has to exist, with its gauge idle_timeout sized
    // from `health_interval_secs`, *before* `start` runs (so gauges written
    // during startup aren't dropped on the floor by a no-op recorder — see
    // main.rs's ordering comment in crates/server). So this reads the
    // config a second time, just for that one field; it's a small local
    // TOML file, and it keeps `start` self-contained rather than needing a
    // pre-parsed config threaded through it.
    let cfg = ferryman_core::load_config(&args.config)?;
    let interval = Duration::from_secs(cfg.health_interval_secs);

    // `install_recorder()` sets the *process-global* metrics recorder —
    // call it exactly once per process, and before anything records a
    // metric or reads gauges. A second call returns an error (the global
    // recorder is already set). Tests use `PrometheusBuilder::new()
    // .build_recorder().handle()` instead, which builds a private,
    // uninstalled recorder — see the "install once" rule in README.md.
    let metrics = prometheus_builder(interval)?.install_recorder()?;

    let settings = Settings {
        config: args.config,
        proxy_bind: args.bind,
        admin_bind: args.admin_bind,
        tls,
    };

    let running = start(settings, metrics).await?;
    tracing::info!(
        proxy = %running.proxy_addr,
        admin = %running.admin_addr,
        "embedded ferryman started"
    );

    shutdown_signal().await;
    tracing::info!("shutdown requested, draining");
    running.shutdown().await
}

/// Resolves on SIGINT or SIGTERM, for graceful shutdown.
async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }
}
