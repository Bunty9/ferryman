//! Hot-reload of the routing table and the TLS certificate.
//!
//! Reloads are triggered by a filesystem watch ([`Reloader::watch`]) and, in
//! the binary, by SIGHUP on Unix ([`Reloader::reload`] with `force`).
//!
//! The watch is on the *directories* holding the config file and the TLS
//! cert/key (non-recursive), and any event there schedules a reload; the
//! trigger is deliberately not filtered by file name. That covers editors
//! that save by rename-and-replace (remove + create on the directory, no
//! `Modify` on a stable inode) and Kubernetes ConfigMap/Secret mounts, which
//! never touch `config.toml` itself but atomically retarget a `..data`
//! symlink in the directory.
//!
//! Events are coalesced: a reload runs once the directory has been quiet for
//! `DEBOUNCE` (200ms), because one save can emit several events and the first
//! may fire while the file is half written. A watch-triggered reload also only
//! acts when the *bytes* of the config (or of the cert+key) differ from those
//! last seen, so `touch`, atomic rewrites with identical content, and the
//! unrelated churn of a ConfigMap volume do not swap the table. SIGHUP is
//! unconditional. A failed reload logs the error and keeps the old table or
//! certificate; the bad bytes are remembered, so the same bad file is not
//! retried until it changes (or SIGHUP).
//!
//! Limits (need a restart): the proxy and admin bind addresses, whether TLS is
//! on at all (a TLS-less start cannot gain a certificate, and the TLS paths
//! themselves are fixed), `health_interval_secs` (the health loop's ticker is
//! fixed at startup) and the metrics exporter settings. `keepalive_timeout_secs`
//! is read when a connection is accepted, so a change applies to new
//! connections only; a rotated certificate likewise applies to new TLS
//! handshakes only. With inline `FERRYMAN_CONFIG_TOML` there is no file, so
//! the config is not reloaded (the TLS files still are). On Windows there is
//! no SIGHUP; the file watch is the only trigger.
//!
//! A directory reached through a symlink is watched at its target as of
//! watch time. For a ConfigMap/Secret mount, point `--config` (and the TLS
//! paths) at the mount's top-level file (`/etc/ferryman/config.toml`), not
//! at `…/..data/config.toml` or a path through a directory symlink that
//! gets retargeted. `subPath` mounts are never updated by the kubelet.

use crate::tls::TlsReloader;
use ferryman_core::{build_table, SharedTable};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Quiet period after the last event before reloading.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Longest a reload may be postponed by a steady stream of events.
const MAX_COALESCE: Duration = Duration::from_secs(2);

/// Reloads the routing table (from `config`) and the TLS certificate (via
/// `tls`) on demand or when the watched directories change.
pub struct Reloader {
    config: Option<PathBuf>,
    table: SharedTable,
    tls: Option<TlsReloader>,
    /// Content hashes of the config file and of cert+key last acted on.
    seen: Mutex<(u64, u64)>,
}

impl Reloader {
    /// `config` is `None` when the config is inline (nothing to re-read);
    /// `tls` comes from [`load_reloadable`](crate::tls::load_reloadable).
    /// The current file contents count as already loaded.
    pub fn new(config: Option<PathBuf>, table: SharedTable, tls: Option<TlsReloader>) -> Arc<Self> {
        let seen = Mutex::new((
            digest(config.iter().map(PathBuf::as_path)),
            digest(tls.iter().flat_map(|t| t.files())),
        ));
        Arc::new(Self {
            config,
            table,
            tls,
            seen,
        })
    }

    /// Reload what changed. With `force` (SIGHUP) the config and certificate
    /// are re-read even if their bytes are unchanged. Failures are logged and
    /// keep the previous table / certificate.
    pub fn reload(&self, force: bool) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = &self.config {
            // Read once: the bytes hashed are the bytes parsed.
            let raw = std::fs::read(path);
            let h = hash_of(raw.as_ref().ok());
            if force || h != seen.0 {
                seen.0 = h;
                if let Some(new_table) = reload_once(path, raw, &self.table) {
                    self.table.store(Arc::new(new_table));
                    self.table.load().publish_gauges();
                    tracing::info!(path = %path.display(), "config reloaded");
                }
            }
        }
        if let Some(tls) = &self.tls {
            let h = digest(tls.files());
            if force || h != seen.1 {
                seen.1 = h;
                match tls.reload() {
                    Ok(()) => tracing::info!("TLS certificate reloaded"),
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "TLS reload failed; keeping old certificate")
                    }
                }
            }
        }
    }

    /// Watch the directories of the config file and TLS files; any event
    /// there triggers a debounced [`reload`](Self::reload)`(false)`. Dropping
    /// the returned watcher cancels the subscription.
    pub fn watch(self: &Arc<Self>) -> notify::Result<RecommendedWatcher> {
        let dirs: BTreeSet<PathBuf> = self
            .config
            .iter()
            .map(PathBuf::as_path)
            .chain(self.tls.iter().flat_map(|t| t.files()))
            .map(watch_dir)
            .collect();

        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Err(e) = res {
                    // e.g. inotify queue overflow: we may have missed a save,
                    // so reload anyway; the content hash makes that harmless.
                    tracing::warn!(error = %e, "config watch error");
                }
                let _ = tx.send(());
            })?;
        for dir in &dirs {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }

        let this = self.clone();
        // Exits when the watcher (and with it `tx`) is dropped.
        std::thread::spawn(move || {
            // Catch an edit made between the initial load and the watch start.
            this.reload(false);
            while rx.recv().is_ok() {
                // Quiet period, but capped: unrelated writes in a watched
                // directory must not postpone the reload forever.
                let first = Instant::now();
                while let Some(left) = MAX_COALESCE.checked_sub(first.elapsed()) {
                    match rx.recv_timeout(DEBOUNCE.min(left)) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                this.reload(false);
            }
        });
        Ok(watcher)
    }
}

/// Watch `path`'s parent directory for config reloads (no TLS). Dropping the
/// returned watcher cancels the subscription. Shorthand for
/// [`Reloader::new`] + [`Reloader::watch`].
pub fn watch_config(path: &Path, table: SharedTable) -> notify::Result<RecommendedWatcher> {
    Reloader::new(Some(path.to_path_buf()), table, None).watch()
}

fn watch_dir(file: &Path) -> PathBuf {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Hash of the bytes of `files` (a missing/unreadable file hashes as such).
/// `DefaultHasher::new()` is deterministic within a process, which is all
/// the de-dup needs.
fn digest<'a>(files: impl IntoIterator<Item = &'a Path>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in files {
        std::fs::read(f).ok().hash(&mut h);
    }
    h.finish()
}

fn hash_of(bytes: Option<&Vec<u8>>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Re-read and rebuild the routing table, keeping the old one on any error
/// (config unreadable, unparsable, or failing validation). Returns `None`
/// on error, having already logged the full error chain.
fn reload_once(
    path: &Path,
    raw: std::io::Result<Vec<u8>>,
    table: &SharedTable,
) -> Option<ferryman_core::RouteTable> {
    // anyhow keeps the `{e:#}` cause chain (toml detail) that Error hides.
    let fail = |e: anyhow::Error| tracing::error!(path = %path.display(), error = %format!("{e:#}"), "config reload failed; keeping old table");
    let cfg = raw
        .map_err(anyhow::Error::from)
        .and_then(|b| Ok(String::from_utf8(b)?))
        .and_then(|s| s.parse::<ferryman_core::ConfigToml>().map_err(Into::into))
        .inspect_err(|e| fail(anyhow::anyhow!("{e:#}")))
        .ok()?;
    let prev = table.load_full();
    build_table(cfg, Some(&prev))
        .map_err(anyhow::Error::from)
        .inspect_err(|e| fail(anyhow::anyhow!("{e:#}")))
        .ok()
}
