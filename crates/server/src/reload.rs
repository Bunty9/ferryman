//! Filesystem-watch driven hot-reload of the routing table.
//!
//! Editor-style writes typically emit a `Modify` event on the file we
//! actually watch; some tools rename-and-replace which would surface
//! as a remove + create on the parent directory. Phase 1 watches the file
//! directly — good enough for `vim`, `nano`, and most editors that write
//! in place.

use arc_swap::ArcSwap;
use ferryman_core::{build_table, load_config, RouteTable, SharedTable};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::Arc;

/// Spin up a `notify` watcher on `path` and return it. Dropping the returned
/// watcher cancels the subscription.
pub fn watch_config(path: &Path, table: SharedTable) -> notify::Result<RecommendedWatcher> {
    let p = path.to_path_buf();
    let mut w: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else {
                return;
            };
            if !matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                return;
            }
            match reload_once(&p, &table) {
                Some(new_table) => {
                    table.store(Arc::new(new_table));
                    tracing::info!(path = %p.display(), "config reloaded");
                }
                None => {
                    tracing::error!(path = %p.display(), "config reload failed; keeping old table");
                }
            }
        })?;
    w.watch(path, RecursiveMode::NonRecursive)?;
    Ok(w)
}

fn reload_once(path: &Path, table: &SharedTable) -> Option<RouteTable> {
    let cfg = load_config(path).ok()?;
    let prev = table.load_full();
    build_table(cfg, Some(&prev)).ok()
}

/// Re-exported for completeness — callers may want to construct the initial
/// `SharedTable` themselves rather than via `main()`. Marked `allow(dead_code)`
/// because Phase 1 only uses it from tests / integration code (none yet);
/// CI runs with `-D warnings`.
#[allow(dead_code)]
pub fn new_shared(table: RouteTable) -> SharedTable {
    Arc::new(ArcSwap::from_pointee(table))
}
