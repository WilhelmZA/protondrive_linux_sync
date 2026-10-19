//! Watch daemon. A local filesystem change (inotify via `notify`, recursive)
//! triggers a *shallow* reconcile of just the folder whose direct contents
//! changed; deeper changes arrive as their own events. Remote changes arrive
//! through the backend's change feed (`watcher_feed.rs`) and enter the same
//! shallow-folder queue. A full walk runs on first sync, on a refresh notice,
//! and once per `full_walk_interval` as a safety net (see `docs/SYNC_MODEL.md`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::config::{Config, Pair};
use crate::datefmt::now_epoch;
use crate::engine::{run_sync_shallow_many, run_sync_streaming};
use crate::events::{EventSink, SyncEvent};
use crate::logger::Logger;
use crate::stats::Stats;

#[path = "watcher_feed.rs"]
mod feed;

/// A sub-folder counts as "hot" if it saw >= this many changes in the window.
const HOT_WINDOW_SECS: i64 = 1800;
const HOT_THRESHOLD: i64 = 1;
/// While signed out, re-probe the session no more often than this, so a
/// logged-out daemon idles cheaply instead of spawning a probe every tick.
const AUTH_REPROBE_SECS: i64 = 20;

/// Announce a sign-in state change: emit a structured event (so the GUI banner
/// flips) and log it once.
fn note_auth(events: Option<&dyn EventSink>, log: &Logger, signed_in: bool) {
    if let Some(sink) = events {
        sink.emit(&SyncEvent::Auth { signed_in });
    }
    if signed_in {
        log.info("watch: signed back in to Proton — resuming");
    } else {
        log.warn("watch: Proton session signed out — pausing sync until you sign in");
    }
}

enum Msg {
    /// A debounced local change under pair index `usize`, in sub-folder `sub`
    /// (POSIX path relative to the pair root; "" means the root). Synced scoped
    /// to just that folder, not the whole pair.
    Sub { pair: usize, sub: String },
}

/// The sub-folder of a pair-relative file path (everything before the last
/// separator; "" for a top-level file).
fn folder_of(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Whether a filesystem event is a real content/structure change worth syncing.
/// We IGNORE access (reads) and metadata-only (attribute/mtime) events: our own
/// reconcile READS folders and sets mtimes on downloads, and if those counted as
/// changes they would mark the just-touched folders "hot" and drive an endless
/// self-feeding re-scan loop. Everything else (create, write, delete, rename,
/// or an unspecified modify) is treated as a real change, conservatively.
fn is_content_change(kind: &notify_debouncer_full::notify::EventKind) -> bool {
    use notify_debouncer_full::notify::event::{AccessKind, AccessMode, ModifyKind};
    use notify_debouncer_full::notify::EventKind;
    match kind {
        // A completed WRITE (file closed after being written, IN_CLOSE_WRITE) is a
        // real change — many tools only signal a save this way, so we must keep it.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        // Other access (reads, opens, read-closes) and metadata/mtime touches are
        // our own reconcile's footprint; counting them self-triggers a re-scan.
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => false,
        // Create / delete / data-modify / rename / unspecified modify = real.
        _ => true,
    }
}

/// Single-instance lock so a watch daemon and, say, a systemd timer don't run
/// the same pairs at once. Checks PID, start time and boot ID against /proc.
struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("watch.lock");
        if crate::process_lock::is_alive(&path) {
            bail!(
                "a neutronsync watch is already running; lock file {}",
                path.display()
            );
        }
        crate::process_lock::write(&path)?;
        Ok(DaemonLock { path })
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        crate::process_lock::remove_owned(&self.path);
    }
}

/// Reload the config from its source file so edits made while the daemon runs
/// (exclusions, delete policy, compare mode, …) take effect on the next
/// reconcile — no restart needed. Falls back to the config the daemon started
/// with when there's no source path or the reload fails (e.g. a half-written
/// file mid-save), so a bad read never disrupts syncing.
fn reload_cfg(base: &Config, log: &Logger) -> Config {
    match base.source_path.as_deref() {
        Some(p) => match crate::config::load(p.to_str()) {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "watch: keeping running config (reload failed: {e})"
                ));
                base.clone()
            }
        },
        None => base.clone(),
    }
}

/// Run the watch loop until `stop` is set (GUI toggle) or the process is killed.
pub fn watch(cfg: &Config, pairs: Vec<Pair>, log: &Logger, stop: &AtomicBool) -> Result<()> {
    watch_with(cfg, pairs, log, stop, None)
}

/// Like [`watch`], but forwards structured events to `events` (used by the GUI
/// service so the watch daemon feeds the same progress state).
pub fn watch_with(
    cfg: &Config,
    pairs: Vec<Pair>,
    log: &Logger,
    stop: &AtomicBool,
    events: Option<&dyn EventSink>,
) -> Result<()> {
    if pairs.is_empty() {
        bail!("no pairs to watch");
    }
    let _lock = DaemonLock::acquire(&cfg.state_dir)?;
    let stats = Stats::open(&cfg.state_dir).ok().map(Arc::new);

    // Canonical local roots. Only create a missing root for a brand-new pair
    // (so inotify can attach). If a pair has synced before and its root is now
    // missing, it's most likely an unmounted drive — do NOT recreate it as an
    // empty directory, which would look like a mass deletion. Leave it be; the
    // reconcile refuses that pair and warns until it's restored.
    let roots: Vec<PathBuf> = pairs
        .iter()
        .map(|p| {
            let has_base = stats
                .as_ref()
                .and_then(|s| s.has_baseline(&p.name).ok())
                .unwrap_or(false);
            if !p.local.exists() && !has_base {
                let _ = std::fs::create_dir_all(&p.local);
            }
            std::fs::canonicalize(&p.local).unwrap_or_else(|_| p.local.clone())
        })
        .collect();

    let (tx, rx) = mpsc::channel::<Msg>();

    // 1) Start watching BEFORE the (possibly long) initial full walk, so local
    //    changes made while it runs are captured and processed right after,
    //    rather than lost during a blind startup window.
    // 2) Debounced FS watcher -> record hotness + queue a scoped sync per folder.
    let handler_roots = roots.clone();
    let handler_names: Vec<String> = pairs.iter().map(|p| p.name.clone()).collect();
    let handler_stats = stats.clone();
    let handler_tx = tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_secs(cfg.debounce_secs.max(1)),
        None,
        move |res: DebounceEventResult| {
            let events = match res {
                Ok(ev) => ev,
                Err(_) => return,
            };
            // Collect the distinct (pair, sub-folder) pairs that changed, record
            // hotness, and queue a SCOPED sync for each — just the folder the
            // change happened in, never the whole pair.
            let mut hit: BTreeSet<(usize, String)> = BTreeSet::new();
            for ev in events {
                // Skip our own reads / mtime-touches so they can't mark folders
                // hot and cause an endless re-scan loop.
                if !is_content_change(&ev.kind) {
                    continue;
                }
                for path in &ev.paths {
                    if let Some(i) = handler_roots.iter().position(|r| path.starts_with(r)) {
                        if let Ok(rel) = path.strip_prefix(&handler_roots[i]) {
                            let rel = rel.to_string_lossy().replace('\\', "/");
                            if rel.is_empty() {
                                continue;
                            }
                            if let Some(st) = &handler_stats {
                                let _ = st.record_change(&handler_names[i], &rel, now_epoch());
                            }
                            // Reconcile the folder whose direct contents changed:
                            // the path itself if it's a directory, else its
                            // parent (also correct for a just-deleted path).
                            let folder = if path.is_dir() {
                                rel.clone()
                            } else {
                                folder_of(&rel)
                            };
                            hit.insert((i, folder));
                        }
                    }
                }
            }
            for (i, folder) in hit {
                let _ = handler_tx.send(Msg::Sub {
                    pair: i,
                    sub: folder,
                });
            }
        },
    )?;
    for r in &roots {
        if let Err(e) = debouncer.watch(r, RecursiveMode::Recursive) {
            // A missing root (e.g. an unmounted drive) must not kill the whole
            // daemon; that pair still reconciles on the periodic rescan (and is
            // refused there until the folder is back).
            log.warn(&format!(
                "watch: cannot watch {} ({e}); it will still reconcile periodically",
                r.display()
            ));
        }
    }
    let backend = crate::backend::select(cfg)?;
    let Some(change_feed) = backend.change_feed() else {
        bail!("watch: the backend has no change feed");
    };
    feed::watch(cfg, &pairs, log, stop, events, change_feed, &rx)
}

#[cfg(test)]
mod tests {
    use super::is_content_change;
    use notify_debouncer_full::notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify_debouncer_full::notify::EventKind;

    #[test]
    fn ignores_reads_and_metadata_keeps_real_changes() {
        // Reads and attribute/mtime touches must NOT count (they self-trigger).
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Any)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        // But a completed write (IN_CLOSE_WRITE) IS a real change.
        assert!(is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_content_change(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Any)
        )));
        assert!(!is_content_change(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
        // Real content/structure changes must count.
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Any
        ))));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Any)));
    }
}
