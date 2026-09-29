//! Reference example: embedding `ferryman` as a library inside your own
//! async application, instead of running the standalone `ferryman` binary
//! as a separate process next to your app.
//!
//! This is what an app that wants a reverse proxy *inline* — sharing a
//! process, a tracing subscriber, a metrics recorder, and a shutdown signal
//! with the rest of its own code — would copy and adapt. See README.md for
//! when that's worth it versus just running the binary.
//!
//! The public surface is deliberately small: [`Settings`] in,
//! [`start`] to boot, [`Running`] out, [`Running::shutdown`] to stop.
//! `main.rs` is a thin CLI wrapper around exactly that surface, so this
//! crate is runnable on its own (`cargo run -p ferryman-embedded-example`)
//! as well as embeddable.

mod admin;

use anyhow::Context;
use arc_swap::ArcSwap;
use ferryman_core::{build_table, health_loop, load_config, SharedTable};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use notify::RecommendedWatcher;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Latency buckets (seconds) for the `ferryman_request_duration_seconds`
/// histogram — the same values as `ferryman::LATENCY_BUCKETS`
/// (crates/server/src/lib.rs).
///
/// Duplicated here rather than imported: this example is written against
/// ferryman 0.1.0 as published on crates.io, and that release doesn't
/// export the constant yet (it was added on the unreleased `examples`
/// branch this repo is built from). Once you depend on ferryman >= 0.1.1,
/// delete this const and use `ferryman::LATENCY_BUCKETS` instead.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Everything needed to start an embedded ferryman instance.
pub struct Settings {
    /// Path to the TOML routing config (see `config.toml` in this
    /// directory for the schema).
    pub config: PathBuf,
    /// Bind address for the proxy listener. Use `127.0.0.1:0` (or
    /// `0.0.0.0:0`) to let the OS pick a free port — `Running::proxy_addr`
    /// then reports which one, which is how the test in `tests/embedded.rs`
    /// avoids clashing with other processes on the box.
    pub proxy_bind: SocketAddr,
    /// Bind address for this crate's own admin server
    /// (`/healthz`, `/status`, `/metrics`).
    pub admin_bind: SocketAddr,
    /// `(cert_path, key_path)` to terminate TLS on the proxy listener, or
    /// `None` to serve plain HTTP.
    pub tls: Option<(PathBuf, PathBuf)>,
}

/// A running embedded ferryman instance, returned by [`start`].
///
/// Always end its life with [`Running::shutdown`] rather than dropping it:
/// dropping a `Running` does *not* stop the background tasks it spawned
/// (Tokio doesn't cancel a `JoinHandle`'s task on drop), so a dropped
/// `Running` leaks its proxy listener, its admin listener, its active
/// health-check loop, its metrics-upkeep loop, and its config watcher —
/// every one of them keeps running forever. `shutdown` stops all of them:
/// the proxy and admin listeners are asked to drain and are awaited, the
/// metrics-upkeep loop exits cooperatively via the same shutdown signal,
/// and the health-check loop — which, unlike the others, has no
/// cooperative-cancellation hook of its own (see
/// `ferryman_core::health_loop`'s doc comment) — is aborted once the proxy
/// has finished draining.
pub struct Running {
    /// Where the proxy ended up listening (useful when `Settings::proxy_bind`
    /// asked for an ephemeral port).
    pub proxy_addr: SocketAddr,
    /// Where the admin server ended up listening.
    pub admin_addr: SocketAddr,
    /// The live routing table, e.g. for an embedding app that wants to
    /// inspect upstream state itself instead of (or in addition to)
    /// scraping `/status`.
    pub table: SharedTable,
    shutdown_tx: watch::Sender<bool>,
    health_task: JoinHandle<()>,
    proxy_task: JoinHandle<anyhow::Result<()>>,
    admin_task: JoinHandle<()>,
    upkeep_task: JoinHandle<()>,
    // Held only so the filesystem watch subscription outlives `start`;
    // dropping it would cancel hot-reload. Never read directly.
    _watcher: RecommendedWatcher,
}

/// Build (but do not install) a `PrometheusBuilder` configured the same way
/// ferryman's own binary configures its exporter (see
/// `crates/server/src/main.rs`): explicit histogram buckets for the
/// request-duration metric (the exporter's defaults are tuned for
/// second-scale web latencies and are too coarse below ~1ms), and a gauge
/// `idle_timeout` of three health-check intervals so a gauge for an
/// upstream removed by a hot reload stops being reported instead of
/// reporting a stale value forever (see health.rs / reload.rs).
///
/// Unlike `crates/server/src/main.rs`, this builder is never told to open
/// its own HTTP listener (`with_http_listener`): this crate's admin server
/// (`admin.rs`) renders `/metrics` itself from the returned
/// [`PrometheusHandle`], so an embedding app that already runs its own
/// admin/ops HTTP surface can fold ferryman's metrics into it instead of
/// ferryman opening a second port nobody asked for.
pub fn prometheus_builder(health_interval: Duration) -> anyhow::Result<PrometheusBuilder> {
    Ok(PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("ferryman_request_duration_seconds".into()),
            LATENCY_BUCKETS,
        )?
        .idle_timeout(MetricKindMask::GAUGE, Some(health_interval * 3)))
}

/// Start an embedded ferryman instance: load and validate `settings.config`,
/// build the routing table, publish its gauges, spawn the active health
/// checker and the config-file watcher, then bind and serve both the proxy
/// and the admin listeners in the background.
///
/// `metrics` is a [`PrometheusHandle`] the *caller* already built and
/// installed (typically via `prometheus_builder(..)?.install_recorder()?`,
/// as `main.rs` does) — see README.md's "install once" rule for why `start`
/// takes a ready-made handle rather than building its own recorder.
pub async fn start(settings: Settings, metrics: PrometheusHandle) -> anyhow::Result<Running> {
    // --- Everything fallible happens first, before a single task is
    // spawned. A bind failing because the port is already taken is the
    // most likely way `start` fails in practice; if a background task
    // (e.g. the health-check loop) were already running by the time that
    // happens, an embedding app that retries `start` after an `Err` would
    // accumulate one orphaned task per failed attempt, since returning
    // `Err` from `start` gives the caller no `Running` to call `shutdown`
    // on and thus no way to stop what's already spawned. Keeping every
    // `?` above every `tokio::spawn` is what makes `start` safe to retry.
    let cfg = load_config(&settings.config)?;
    let interval = Duration::from_secs(cfg.health_interval_secs);

    let table = build_table(cfg, None)?;
    let shared: SharedTable = Arc::new(ArcSwap::from_pointee(table));

    let tls = match &settings.tls {
        Some((cert, key)) => Some(
            ferryman::tls::load_acceptor(cert, key)
                .context("loading TLS cert/key for the proxy listener")?,
        ),
        None => None,
    };

    let proxy_listener = TcpListener::bind(settings.proxy_bind)
        .await
        .with_context(|| format!("binding proxy listener on {}", settings.proxy_bind))?;
    let proxy_addr = proxy_listener.local_addr()?;

    let admin_listener = TcpListener::bind(settings.admin_bind)
        .await
        .with_context(|| format!("binding admin listener on {}", settings.admin_bind))?;
    let admin_addr = admin_listener.local_addr()?;

    // Hot-reload watcher: re-reads and hot-swaps the table on config file
    // changes. Its `RecommendedWatcher` must be kept alive (stored in
    // `Running`) or the subscription is cancelled immediately.
    let watcher = ferryman::reload::watch_config(&settings.config, shared.clone())
        .context("starting config file watcher")?;

    // --- Nothing below this point is fallible: only spawns follow.
    //
    // Gauges are only ever visible through a recorder that's already
    // installed; since `metrics` is the caller's installed recorder, this
    // is safe now.
    shared.load().publish_gauges();

    // Active health checker: probes every upstream's /health on a fixed
    // interval and reports results through the same breakers the proxy
    // reads. It has no cooperative-cancellation hook of its own (see its
    // doc comment in crates/core/src/health.rs — "Cancel by aborting the
    // spawned task"), so unlike the proxy/admin/upkeep tasks below, it
    // isn't wired to `shutdown_rx` at all; `Running::shutdown` aborts this
    // handle directly instead.
    let health_task: JoinHandle<()> = tokio::spawn(health_loop(shared.clone(), interval));

    // `watch` (not `oneshot`) so the same signal can be cloned and awaited
    // by three independent tasks (proxy, admin, upkeep) without consuming
    // it — a `watch::Receiver::wait_for` on each clone wakes once `shutdown`
    // sends `true`, no matter how many clones exist or when they started
    // watching (last-value-wins, so there's no missed-wakeup race).
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let proxy_table = shared.clone();
    let mut proxy_shutdown_rx = shutdown_rx.clone();
    let proxy_task: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        ferryman::serve(
            proxy_listener,
            proxy_table,
            tls,
            wait_for_shutdown(&mut proxy_shutdown_rx),
        )
        .await
    });

    let admin_table = shared.clone();
    let admin_metrics = metrics.clone();
    let admin_shutdown_rx = shutdown_rx.clone();
    let admin_task: JoinHandle<()> = tokio::spawn(admin::run(
        admin_listener,
        admin_table,
        admin_metrics,
        admin_shutdown_rx,
    ));

    // The Prometheus recorder buffers histogram/summary state internally
    // and needs periodic "upkeep" (decaying old buckets, dropping metrics
    // past their idle_timeout) to avoid growing unboundedly — see the
    // "Upkeep and maintenance" section of metrics-exporter-prometheus's
    // crate docs. `PrometheusBuilder::install` spawns this for you as part
    // of its own HTTP-listener exporter task; `install_recorder` (what this
    // example uses, since the admin server renders `/metrics` itself) does
    // not, so the caller is responsible for calling `run_upkeep()`
    // periodically. This task is the one place that happens; it exits
    // cooperatively via `shutdown_rx`, same as the proxy and admin tasks,
    // and `Running::shutdown` awaits it (tracked below, not fire-and-forget).
    let mut upkeep_shutdown_rx = shutdown_rx.clone();
    let upkeep_task: JoinHandle<()> = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => metrics.run_upkeep(),
                _ = upkeep_shutdown_rx.wait_for(|&v| v) => break,
            }
        }
    });

    Ok(Running {
        proxy_addr,
        admin_addr,
        table: shared,
        shutdown_tx,
        health_task,
        proxy_task,
        admin_task,
        upkeep_task,
        _watcher: watcher,
    })
}

/// Resolves once `rx` carries `true`. `watch::Receiver::wait_for` is the
/// right primitive here (over e.g. a `oneshot`) because it's cloneable —
/// `start` clones `shutdown_rx` once per task that needs to observe the
/// same shutdown signal.
async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    // The closure re-checks the *current* value first, so a task that
    // starts watching after `shutdown` has already fired still resolves
    // immediately instead of hanging.
    let _ = rx.wait_for(|&v| v).await;
}

impl Running {
    /// Stop accepting new connections/requests and wait for every
    /// background task `start` spawned to finish, up to a generous bound.
    /// Idempotent to call (`shutdown_tx.send` on an instance with no live
    /// receivers is a harmless no-op), but only meant to be called once —
    /// it consumes `self`.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        // Flip the flag once; every clone of `shutdown_rx` wakes on its next
        // poll of `wait_for`.
        let _ = self.shutdown_tx.send(true);

        // `ferryman::serve` stops accepting immediately but then drains
        // in-flight connections for up to its own internal 25s timeout
        // (`GRACEFUL_SHUTDOWN_TIMEOUT` in crates/server/src/lib.rs) before
        // dropping them anyway. Bound the wait comfortably above that so a
        // wedged drain can't hang `shutdown` forever — in production this
        // can legitimately take close to 25s under load; in the test suite
        // there's nothing in flight, so it returns almost immediately.
        const SHUTDOWN_BOUND: Duration = Duration::from_secs(30);

        let proxy_result = tokio::time::timeout(SHUTDOWN_BOUND, self.proxy_task)
            .await
            .context("proxy task did not shut down in time")?
            .context("proxy task panicked")?;
        proxy_result?;

        tokio::time::timeout(SHUTDOWN_BOUND, self.admin_task)
            .await
            .context("admin task did not shut down in time")?
            .context("admin task panicked")?;

        // The upkeep loop watches the same shutdown signal and exits
        // cooperatively (see `start`); by now it should already be done or
        // a single `select!` poll away from it, but await it properly
        // rather than leaving it to finish on its own after `shutdown` has
        // already returned.
        tokio::time::timeout(SHUTDOWN_BOUND, self.upkeep_task)
            .await
            .context("upkeep task did not shut down in time")?
            .context("upkeep task panicked")?;

        // `health_loop` has no shutdown signal wired in (see the comment on
        // `health_task` in `start`) and holds no state that needs an
        // orderly cleanup — each probe is an independent, fire-and-forget
        // `reqwest` call — so aborting it here, after the proxy has
        // finished draining, is correct: no request or probe result is
        // lost, it just stops being scheduled.
        self.health_task.abort();

        Ok(())
    }
}
