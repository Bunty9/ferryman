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
pub mod guarded;

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
use tokio::time::Instant;

/// Latency buckets (seconds) for the `ferryman_request_duration_seconds`
/// histogram, re-exported from ferryman so an embedding app configures its
/// recorder with exactly the buckets the `ferryman` binary uses.
pub use ferryman::LATENCY_BUCKETS;

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
/// the health-check loop — which, unlike the others, has no
/// cooperative-cancellation hook of its own (see
/// `ferryman_core::health_loop`'s doc comment) — is aborted first, and the
/// proxy and admin listeners and the metrics-upkeep loop are then asked to
/// drain/exit cooperatively via the same shutdown signal and awaited.
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
/// Like `crates/server/src/main.rs` (whose admin server is
/// `ferryman::admin`), this builder never opens its own HTTP listener: this
/// crate's admin server renders `/metrics` itself from the returned
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
/// `metrics` must be the [`PrometheusHandle`] of the *installed* global
/// recorder — i.e. the caller already built and installed it (typically via
/// `prometheus_builder(..)?.install_recorder()?`, as `main.rs` does) before
/// calling `start`. `start` and the rest of ferryman record metrics through
/// the global `metrics::` macros, not through this handle directly, so a
/// handle from an *uninstalled* recorder (e.g.
/// `PrometheusBuilder::new().build_recorder().handle()`) never observes
/// anything ferryman records — `render()`ing it comes back empty regardless
/// of how much traffic was proxied. See README.md's "install once" rule for
/// why `start` takes a ready-made handle rather than building its own
/// recorder, and `tests/metrics.rs` for the one place this crate's test
/// suite actually installs a recorder and asserts on `/metrics` content.
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

/// `ferryman::serve` stops accepting immediately but then drains in-flight
/// connections for up to its own internal 25s timeout
/// (`GRACEFUL_SHUTDOWN_TIMEOUT` in crates/server/src/lib.rs) before dropping
/// them anyway. `Running::shutdown`'s total bound is set comfortably above
/// that so a wedged drain can't hang `shutdown` forever — in production this
/// can legitimately take close to 25s under load; in the test suite there's
/// nothing in flight, so it returns almost immediately.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(30);

/// Await `handle` until `deadline`, aborting it — rather than leaving it
/// detached and still running — if it doesn't finish in time. `deadline` is
/// a shared `Instant` (not a per-call `Duration`), so a caller awaiting
/// several tasks in sequence bounds their *total* wait, not each one
/// separately. `abort_handle()` is taken before the await so it's still
/// available to abort the task after `handle` itself has been consumed by
/// `timeout_at`. Note that `.abort()` only requests cancellation
/// cooperatively (at the task's next await point) and isn't awaited here —
/// the caller gets an error back immediately and doesn't wait for the abort
/// to actually take effect.
async fn await_or_abort(
    handle: JoinHandle<()>,
    deadline: Instant,
    name: &str,
) -> anyhow::Result<()> {
    let abort_handle = handle.abort_handle();
    match tokio::time::timeout_at(deadline, handle).await {
        Ok(r) => r.with_context(|| format!("{name} task panicked")),
        Err(_elapsed) => {
            abort_handle.abort();
            Err(anyhow::anyhow!(
                "{name} task did not shut down within the {SHUTDOWN_BOUND:?} shutdown deadline"
            ))
        }
    }
}

/// Same as [`await_or_abort`], for `proxy_task` specifically: its join
/// result is itself an `anyhow::Result<()>` (from `ferryman::serve`) that
/// must be propagated, not just a panic to report.
async fn await_or_abort_fallible(
    handle: JoinHandle<anyhow::Result<()>>,
    deadline: Instant,
    name: &str,
) -> anyhow::Result<()> {
    let abort_handle = handle.abort_handle();
    match tokio::time::timeout_at(deadline, handle).await {
        Ok(Ok(inner)) => inner,
        Ok(Err(join_err)) => Err(join_err).with_context(|| format!("{name} task panicked")),
        Err(_elapsed) => {
            abort_handle.abort();
            Err(anyhow::anyhow!(
                "{name} task did not shut down within the {SHUTDOWN_BOUND:?} shutdown deadline"
            ))
        }
    }
}

impl Running {
    /// Stop accepting new connections/requests and wait for every
    /// background task `start` spawned to finish, up to `SHUTDOWN_BOUND`
    /// (30s total, not per task — see below). Only meant to be called
    /// once — it consumes `self`.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        // Flip the flag once; every clone of `shutdown_rx` wakes on its next
        // poll of `wait_for`.
        let _ = self.shutdown_tx.send(true);

        // `health_loop` has no shutdown signal wired in (see the comment on
        // `health_task` in `start`) and holds no state that needs an
        // orderly cleanup — each probe is an independent, fire-and-forget
        // `reqwest` call — so this is the only way to stop it. Done first
        // and unconditionally, before any of the awaits below (which can
        // themselves time out or return an error): every path out of this
        // function — success, a task error, or a timeout — leaves it
        // stopped, never still running in the background.
        self.health_task.abort();

        // One deadline, computed once and shared by every await below —
        // not a fresh `SHUTDOWN_BOUND` per task — so three sequential
        // awaits bound the *total* wait to ~30s instead of stacking up to
        // 90s worst case (30s each for proxy, admin, upkeep).
        let deadline = Instant::now() + SHUTDOWN_BOUND;

        // Await every remaining task, aborting (rather than leaving it
        // detached) whichever one doesn't finish by `deadline` — and keep
        // awaiting the others even if an earlier one already errored, so a
        // slow/failed proxy shutdown still results in the admin and upkeep
        // tasks being stopped instead of leaked. The `?`s below then
        // return the *first* error, once every task has been dealt with.
        let proxy_result = await_or_abort_fallible(self.proxy_task, deadline, "proxy").await;
        let admin_result = await_or_abort(self.admin_task, deadline, "admin").await;
        // The upkeep loop watches the same shutdown signal and exits
        // cooperatively (see `start`); by now it should already be done or
        // a single `select!` poll away from it, but await it properly
        // rather than leaving it to finish on its own after `shutdown` has
        // already returned.
        let upkeep_result = await_or_abort(self.upkeep_task, deadline, "upkeep").await;

        proxy_result?;
        admin_result?;
        upkeep_result?;
        Ok(())
    }
}
