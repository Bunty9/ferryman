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

    /// Path to the TOML routing config (default `config.toml`). Ignored when
    /// `FERRYMAN_CONFIG_TOML` (inline TOML) is set.
    #[arg(long, global = true, env = "FERRYMAN_CONFIG")]
    config: Option<PathBuf>,

    /// Bind address for the proxy listener. Order: `--bind`, `FERRYMAN_BIND`,
    /// `0.0.0.0:$PORT`, `0.0.0.0:8080`.
    #[arg(long, env = "FERRYMAN_BIND")]
    bind: Option<SocketAddr>,

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

/// `--bind` > `FERRYMAN_BIND` (both parsed by clap) > `0.0.0.0:$PORT` > `0.0.0.0:8080`.
/// An empty `PORT` counts as unset; a non-empty invalid one is an error.
fn resolve_bind(bind: Option<SocketAddr>, port: Option<&str>) -> anyhow::Result<SocketAddr> {
    if let Some(b) = bind {
        return Ok(b);
    }
    let port = match port.map(str::trim) {
        None | Some("") => 8080,
        Some(p) => p
            .parse::<u16>()
            .map_err(|e| anyhow::anyhow!("invalid PORT={p:?}: {e}"))?,
    };
    Ok(SocketAddr::from(([0, 0, 0, 0], port)))
}

/// Where the config comes from. Non-empty `FERRYMAN_CONFIG_TOML` (inline TOML)
/// always wins over `--config` / `FERRYMAN_CONFIG` (the image's default CMD
/// passes `--config`, so a flag cannot be the tiebreaker); no file is
/// watched then.
fn load_cfg(args: &Args) -> anyhow::Result<(ferryman_core::ConfigToml, Option<PathBuf>)> {
    use anyhow::Context;
    match std::env::var("FERRYMAN_CONFIG_TOML") {
        Ok(raw) if !raw.trim().is_empty() => {
            Ok((raw.parse().context("parsing FERRYMAN_CONFIG_TOML")?, None))
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("FERRYMAN_CONFIG_TOML is not valid UTF-8")
        }
        _ => {
            let path = args.config.clone().unwrap_or_else(|| "config.toml".into());
            Ok((load_config(&path)?, Some(path)))
        }
    }
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
    let (cfg, config_path) = load_cfg(&args)?;
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
    let _watcher = match &config_path {
        Some(p) => Some(reload::watch_config(p, shared.clone())?),
        None => {
            tracing::info!("config from FERRYMAN_CONFIG_TOML: file watch disabled, no hot reload");
            None
        }
    };

    let bind = resolve_bind(args.bind, std::env::var("PORT").ok().as_deref())?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %listener.local_addr()?, tls = tls_acceptor.is_some(), "ferryman listening");

    // Spawned after the proxy port is bound so /readyz is never 200 before
    // the proxy can accept.
    let admin_listener = tokio::net::TcpListener::bind(args.metrics_bind).await?;
    let admin_addr = admin_listener.local_addr()?;
    tokio::spawn(admin::serve_admin(
        admin_listener,
        metrics,
        draining.clone(),
    ));
    tracing::info!(addr = %admin_addr, "admin listener bound (/metrics, /healthz, /readyz)");

    let table = shared.clone();
    let signalled = Arc::new(tokio::sync::Notify::new());
    let first = signalled.clone();
    let shutdown = async move {
        shutdown_signal().await;
        first.notify_one();
        // Readiness flips first; /readyz stays up (503) through the delay and
        // the drain. The proxy keeps accepting until the delay elapses.
        draining.store(true, Ordering::Relaxed);
        let delay = table.load().shutdown_delay();
        if !delay.is_zero() {
            tracing::info!(?delay, "shutdown_delay: not ready, still serving");
            tokio::time::sleep(delay).await;
        }
    };
    // A second signal during the delay or drain exits at once with 130.
    let res = tokio::select! {
        r = ferryman::serve(listener, shared, tls_acceptor, shutdown) => r,
        () = second_signal(&signalled) => {
            tracing::warn!("second signal during shutdown, exiting immediately");
            std::process::exit(130);
        }
    };
    // Exit explicitly: a stuck spawn_blocking (DNS) must not outlive the drain bound.
    res?;
    std::process::exit(0);
}

/// Resolves on a signal arriving after the first one was taken.
async fn second_signal(first: &tokio::sync::Notify) {
    first.notified().await;
    shutdown_signal().await;
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
    let (cfg, _) = load_cfg(args)?;
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

#[cfg(test)]
mod tests {
    use super::resolve_bind;

    #[test]
    fn bind_order() {
        let b = Some("127.0.0.1:1".parse().unwrap());
        let ok = |b, p| resolve_bind(b, p).unwrap().to_string();
        assert_eq!(ok(b, Some("9000")), "127.0.0.1:1");
        assert_eq!(ok(None, Some("9000")), "0.0.0.0:9000");
        assert_eq!(ok(None, None), "0.0.0.0:8080");
        assert_eq!(ok(None, Some("")), "0.0.0.0:8080");
        for bad in ["abc", "70000", "-1"] {
            let e = resolve_bind(None, Some(bad)).unwrap_err().to_string();
            assert!(e.contains("PORT"), "{e}");
        }
        assert_eq!(ok(b, Some("abc")), "127.0.0.1:1");
    }
}
