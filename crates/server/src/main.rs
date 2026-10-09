//! ferryman CLI — parses args, sets up tracing/metrics, loads the
//! initial routing config, spawns the health-check loop and hot-reload
//! watcher, then hands off to [`ferryman::serve`].

use arc_swap::ArcSwap;
use clap::{Parser, Subcommand};
use ferryman::{admin, reload, tls, LATENCY_BUCKETS};
use ferryman_core::{build_table, health_loop, load_config, SharedTable};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use metrics_util::MetricKindMask;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ferryman", about = "ferryman L7 reverse proxy", version)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Path to the TOML routing config.
    #[arg(
        long,
        global = true,
        env = "FERRYMAN_CONFIG",
        default_value = "config.toml"
    )]
    config: PathBuf,

    /// Bind address for the proxy listener.
    #[arg(long, env = "FERRYMAN_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Bind address for the admin listener (`/metrics`, `/healthz`, `/readyz`).
    #[arg(
        long,
        global = true,
        env = "FERRYMAN_METRICS_BIND",
        default_value = "0.0.0.0:9090"
    )]
    metrics_bind: SocketAddr,

    /// TLS certificate PEM path. Requires `--tls-key`; omit both to serve
    /// plain HTTP.
    #[arg(long, global = true, env = "FERRYMAN_TLS_CERT")]
    tls_cert: Option<PathBuf>,

    /// TLS private key PEM path. Requires `--tls-cert`.
    #[arg(long, global = true, env = "FERRYMAN_TLS_KEY")]
    tls_key: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Validate the config (and TLS files) without binding; exit 0 or 1.
    Check,
    /// GET the admin `/healthz` and exit 0 on 2xx, else 1 (for container
    /// HEALTHCHECKs; the image has no curl).
    Healthcheck {
        /// Full URL to probe. Default: `http://127.0.0.1:<metrics-bind port>/healthz`.
        #[arg(long)]
        url: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match &args.cmd {
        Some(Cmd::Check) => {
            std::process::exit(report(check(&args), |n| eprintln!("config ok: {n} routes")))
        }
        Some(Cmd::Healthcheck { url }) => {
            let url = url.clone().unwrap_or_else(|| {
                // Probe the bind IP itself; only wildcard binds map to loopback.
                let mut a = args.metrics_bind;
                if a.ip().is_unspecified() {
                    a.set_ip(if a.is_ipv4() {
                        std::net::Ipv4Addr::LOCALHOST.into()
                    } else {
                        std::net::Ipv6Addr::LOCALHOST.into()
                    });
                }
                format!("http://{a}/healthz")
            });
            std::process::exit(report(healthcheck(&url).await, |()| {}));
        }
        None => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(tls::load_acceptor(cert, key)?),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must both be set or both omitted"),
    };

    // Load + parse the initial config. Fail fast on first-boot misconfiguration.
    let cfg = load_config(&args.config)?;
    let interval = Duration::from_secs(cfg.health_interval_secs);
    // Validate (bounds on every duration) before any `interval * 3` below.
    let table = build_table(cfg, None)?;

    // Recorder installed before any gauge is set, or the writes go to the
    // no-op recorder. The admin server (spawned once the proxy port is bound) renders it on /metrics. The
    // health loop rewrites every live gauge each tick, so gauges of upstreams
    // removed by a reload expire after a few missed ticks instead of
    // reporting a stale value forever.
    let metrics = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("ferryman_request_duration_seconds".into()),
            LATENCY_BUCKETS,
        )?
        .idle_timeout(MetricKindMask::GAUGE, Some(interval * 3))
        .install_recorder()?;
    let draining = Arc::new(AtomicBool::new(false));
    table.publish_gauges();
    let shared: SharedTable = Arc::new(ArcSwap::from_pointee(table));

    // Background tasks: active health checker + config watcher.
    tokio::spawn(health_loop(shared.clone(), interval));
    let _watcher = reload::watch_config(&args.config, shared.clone())?;

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, tls = tls_acceptor.is_some(), "ferryman listening");

    // Spawned after the proxy port is bound so /readyz is never 200 before
    // the proxy can accept.
    let admin_listener = tokio::net::TcpListener::bind(args.metrics_bind).await?;
    tokio::spawn(admin::serve_admin(
        admin_listener,
        metrics,
        draining.clone(),
    ));
    tracing::info!(addr = %args.metrics_bind, "admin listener bound (/metrics, /healthz, /readyz)");

    let shutdown = async move {
        shutdown_signal().await;
        // Readiness flips before the drain starts; /readyz stays up during it.
        draining.store(true, Ordering::Relaxed);
    };
    ferryman::serve(listener, shared, tls_acceptor, shutdown).await
}

/// Prints the error chain to stderr; returns the process exit code.
fn report<T>(r: anyhow::Result<T>, ok: impl FnOnce(T)) -> i32 {
    match r {
        Ok(v) => {
            ok(v);
            0
        }
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

/// `ferryman check`: same validation as startup, minus binding. Returns the route count.
fn check(args: &Args) -> anyhow::Result<usize> {
    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        tls::load_acceptor(cert, key)?;
    } else if args.tls_cert.is_some() || args.tls_key.is_some() {
        anyhow::bail!("--tls-cert and --tls-key must both be set or both omitted");
    }
    let cfg = load_config(&args.config)?;
    let n = cfg.routes.len();
    build_table(cfg, None)?;
    Ok(n)
}

/// `ferryman healthcheck`: HTTP/1 GET with a 3 s total budget; Ok on 2xx.
async fn healthcheck(url: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let uri: hyper::Uri = url.parse().context("invalid --url")?;
    anyhow::ensure!(uri.scheme_str() == Some("http"), "--url must be http://");
    let host = uri.host().context("--url has no host")?;
    let port = uri.port_u16().unwrap_or(80);
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    tokio::time::timeout(Duration::from_secs(3), async {
        let stream = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
        let (mut tx, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let req = hyper::Request::get(path)
            .header(hyper::header::HOST, uri.authority().unwrap().as_str())
            .body(http_body_util::Empty::<hyper::body::Bytes>::new())?;
        let status = tx.send_request(req).await?.status();
        anyhow::ensure!(status.is_success(), "unhealthy: {status}");
        Ok(())
    })
    .await
    .context("timed out")?
}

/// Resolves on SIGINT or SIGTERM, for graceful shutdown.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        _ = sigint.recv() => tracing::info!("received SIGINT, shutting down"),
    }
}

/// Resolves on Ctrl-C, console close or system shutdown.
#[cfg(windows)]
async fn shutdown_signal() {
    use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
    let mut close = ctrl_close().expect("install CTRL_CLOSE handler");
    let mut shutdown = ctrl_shutdown().expect("install CTRL_SHUTDOWN handler");
    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            r.expect("install Ctrl-C handler");
            tracing::info!("received Ctrl-C, shutting down");
        }
        _ = close.recv() => tracing::info!("received console close, shutting down"),
        _ = shutdown.recv() => tracing::info!("received system shutdown, shutting down"),
    }
}
