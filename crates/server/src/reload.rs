//! Filesystem-watch driven hot-reload of the routing table.
//!
//! Watches the config file's *parent directory*, non-recursively, rather
//! than the file itself: editors that save via rename-and-replace (write a
//! temp file, then rename it over the original) emit a remove + create on
//! the directory rather than a `Modify` on a stable inode, and a watch on
//! the file alone misses those. An event is acted on only when one of its
//! paths' file name matches the config file's.
//!
//! One save can emit several events (e.g. `MODIFY` then `CLOSE_WRITE`), and
//! the first may fire while the file is half written. Events are therefore
//! coalesced: a reload runs once the directory has been quiet for
//! [`DEBOUNCE`].
//!
//! Limits: `health_interval_secs` changes need a restart (the health loop's
//! ticker is fixed at startup). Kubernetes ConfigMap mounts swap a `..data`
//! symlink rather than touching `config.toml`, so those updates are not
//! seen here; restart the pod or point `--config` at a regular file.

use ferryman_core::{build_table, load_config, RouteTable, SharedTable};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// Quiet period after the last matching event before reloading.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Spin up a `notify` watcher on `path`'s parent directory and return it.
/// Dropping the returned watcher cancels the subscription.
pub fn watch_config(path: &Path, table: SharedTable) -> notify::Result<RecommendedWatcher> {
    let path = path.to_path_buf();
    let dir = watch_dir(&path);
    let file_name: Option<OsString> = path.file_name().map(|n| n.to_os_string());

    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(event) => event,
                Err(e) => {
                    // e.g. inotify queue overflow: we may have missed a save,
                    // so reload anyway; a spurious reload is harmless.
                    tracing::warn!(error = %e, "config watch error");
                    let _ = tx.send(());
                    return;
                }
            };
            if event
                .paths
                .iter()
                .any(|p| p.file_name() == file_name.as_deref())
            {
                let _ = tx.send(());
            }
        })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    // Exits when the watcher (and with it `tx`) is dropped.
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            loop {
                match rx.recv_timeout(DEBOUNCE) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if let Some(new_table) = reload_once(&path, &table) {
                table.store(Arc::new(new_table));
                table.load().publish_gauges();
                tracing::info!(path = %path.display(), "config reloaded");
            }
        }
    });
    Ok(watcher)
}

fn watch_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Re-read and rebuild the routing table, keeping the old one on any error
/// (config unreadable, unparsable, or failing validation). Returns `None`
/// on error, having already logged the full error chain.
fn reload_once(path: &Path, table: &SharedTable) -> Option<RouteTable> {
    let cfg = load_config(path)
        .inspect_err(|e| {
            tracing::error!(path = %path.display(), error = %format!("{e:#}"), "config reload failed; keeping old table")
        })
        .ok()?;
    let prev = table.load_full();
    build_table(cfg, Some(&prev))
        .inspect_err(|e| {
            tracing::error!(path = %path.display(), error = %format!("{e:#}"), "config reload failed; keeping old table")
        })
        .ok()
}
