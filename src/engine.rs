//! The bidirectional sync engine.
//!
//! Three-way merge: a per-pair baseline snapshot records the last state the two
//! sides agreed on. Each run scans the current local and remote trees and, for
//! every path, classifies it against the baseline (created/modified/deleted)
//! independently per side. Combining the two verdicts decides the action and
//! which side wins - the sync logic the proton-drive CLI lacks.
//!
//! Safety: deletions never propagate unless enabled; a missing baseline unions
//! both sides (never mass-deletes); conflicts keep both versions by default.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;

use crate::events::{EventSink, SyncEvent};

use crate::config::{remote_join, strip_root, Config, ConflictPolicy, LocalDelete, Pair};
use crate::datefmt::{epoch_to_stamp, now_epoch};
use crate::logger::Logger;
use crate::models::{same_content, Action, Change, Compare, DownloadJob, Entry, Op, Plan};
use crate::protoncli::{ListOutcome, ProtonCli, Remote};
use crate::trash::trash_local;

pub struct SyncResult {
    pub pair: String,
    pub applied: usize,
    pub errors: Vec<String>,
    pub plan_summary: String,
    pub was_in_sync: bool,
}

/// Aggregate outcome of syncing a set of pairs. Shared by the CLI, GUI and
/// the watch daemon so they all drive the engine the same way.
pub struct RunSummary {
    pub applied: usize,
    pub errors: usize,
    pub pairs: usize,
}

/// One-shot reconcile of the given pairs with a fresh ProtonCli.
pub fn run_sync(
    cfg: &Config,
    pairs: &[Pair],
    dry_run: bool,
    resync: bool,
    log: &Logger,
) -> RunSummary {
    run_sync_with(cfg, pairs, dry_run, resync, log, None, None)
}

/// Like [`run_sync`], but with an optional structured-event sink and an optional
/// cancellation flag - used by the GUI/service for progress and stop control.
pub fn run_sync_with(
    cfg: &Config,
    pairs: &[Pair],
    dry_run: bool,
    resync: bool,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: None,
                text: "proton-drive not found on PATH".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    let mut engine = Engine::new(cfg, proton, log, dry_run);
    engine.set_observer(events, cancel);
    let (mut applied, mut errors) = (0usize, 0usize);
    for pair in pairs {
        if cancel.map_or(false, |c| c.load(Ordering::Relaxed)) {
            break;
        }
        match engine.sync_pair(pair, resync) {
            Ok(r) => {
                applied += r.applied;
                errors += r.errors.len();
            }
            Err(e) => {
                errors += 1;
                log.error(&format!("pair {:?} failed: {e}", pair.name));
                if let Some(sink) = events {
                    sink.emit(&SyncEvent::Error {
                        pair: Some(pair.name.clone()),
                        text: e.to_string(),
                    });
                }
            }
        }
    }
    RunSummary {
        applied,
        errors,
        pairs: pairs.len(),
    }
}

/// Reconcile a single pair restricted to one sub-folder (`scope`, relative to
/// the pair root). Used by the watch daemon so a local change or a hot folder
/// syncs just that sub-tree instead of re-walking the whole pair.
pub fn run_sync_scoped(
    cfg: &Config,
    pair: &Pair,
    scope: &str,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    let mut engine = Engine::new(cfg, proton, log, false);
    engine.set_observer(events, cancel);
    match engine.sync_pair_scoped(pair, false, Some(scope)) {
        Ok(r) => RunSummary {
            applied: r.applied,
            errors: r.errors.len(),
            pairs: 1,
        },
        Err(e) => {
            log.error(&format!("pair {:?} [{scope}] failed: {e}", pair.name));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::Error {
                    pair: Some(pair.name.clone()),
                    text: e.to_string(),
                });
            }
            RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            }
        }
    }
}

/// Folders (POSIX paths relative to the pair root) that hold a local file which
/// is new or changed versus the baseline. Local-only and fast (no network), so
/// the watcher can upload fresh local work first, before the slow remote walk.
/// Excluded sub-trees are skipped. Comparison is size+mtime (conservative: a
/// false positive just triggers a scoped sync that reconciles correctly).
pub fn local_change_folders(cfg: &Config, pair: &Pair) -> Vec<String> {
    let base = crate::state::load_baseline(&cfg.state_dir, &pair.name).unwrap_or_default();
    let root = &pair.local;
    if !root.exists() {
        return Vec::new();
    }
    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(
            |e| match e.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
                Some(r) => !pair.is_excluded(&r.replace('\\', "/")),
                None => true,
            },
        )
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.to_str())
        {
            Some(r) => r.replace('\\', "/"),
            None => continue,
        };
        let changed = match base.get(&rel) {
            None => true, // new local file
            Some(b) => {
                let meta = entry.metadata().ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let mtime = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                b.size != size || b.mtime != mtime
            }
        };
        if changed {
            let folder = rel
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            folders.insert(folder);
        }
    }
    folders.into_iter().collect()
}

/// Reconcile only the direct children of one folder in a pair (non-recursive).
/// Used by the watch daemon for change-triggered and hot-folder syncs, so a
/// local change touches just that folder's listing rather than re-walking its
/// whole subtree (recursive inotify delivers deeper changes as their own events).
pub fn run_sync_shallow(
    cfg: &Config,
    pair: &Pair,
    folder: &str,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    // Show which folder is being checked (so the banner isn't a generic
    // "Scanning folder pairs" during a single-folder reconcile).
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanProgress {
            pair: pair.name.clone(),
            folders: 0,
            current: remote_join(&pair.remote, folder),
        });
    }
    let mut engine = Engine::new(cfg, proton, log, false);
    engine.set_observer(events, cancel);
    match engine.sync_pair_shallow(pair, folder) {
        Ok((r, _children)) => RunSummary {
            applied: r.applied,
            errors: r.errors.len(),
            pairs: 1,
        },
        Err(e) => {
            log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::Error {
                    pair: Some(pair.name.clone()),
                    text: e.to_string(),
                });
            }
            RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            }
        }
    }
}

/// Streaming full walk: BFS the folder tree and shallow-reconcile each folder as
/// it is discovered, transferring immediately, a bounded number of folders in
/// parallel. Unlike the batch walk (`run_sync`) it does not scan the whole tree
/// before acting, so uploads and downloads overlap the walk. Each folder is
/// reconciled by the same scoped, positive-confirmation shallow primitive, so
/// containment and every data-safety invariant hold per folder. Concurrent
/// workers commit disjoint baseline rows (SQLite serialises the writes).
pub fn run_sync_streaming(
    cfg: &Config,
    pair: &Pair,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "proton-drive not found on PATH".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    // Refuse a vanished local root (unmounted drive) rather than walk into an
    // empty mountpoint and read every remote file as a deletion.
    if !pair.local.exists() {
        log.error(&format!(
            "local folder {} does not exist — refusing streaming walk",
            pair.local.display()
        ));
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "local folder missing — refused".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 1,
        };
    }

    // One shared baseline snapshot for the whole walk. Each folder reads only its
    // own (disjoint) rows from it, and only its own reconcile writes them, so a
    // snapshot taken now stays correct throughout.
    let base = match crate::state::load_baseline(&cfg.state_dir, &pair.name) {
        Ok(b) => std::sync::Arc::new(b),
        Err(e) => {
            log.error(&format!("baseline load failed for {:?}: {e}", pair.name));
            return RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            };
        }
    };

    let threads = if cfg.scan_threads > 0 {
        cfg.scan_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8)
    };

    let applied = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let counter = AtomicUsize::new(0);
    let is_cancelled = || cancel.map_or(false, |c| c.load(Ordering::Relaxed));

    // One pair-level scan for the whole walk (the per-folder reconciles are
    // silent at the pair level), so the GUI shows a single stable "scanning …"
    // with a climbing folder count instead of flapping Scanning<->Synced.
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });
    }

    let mut frontier: Vec<String> = vec![String::new()]; // start at the pair root
    while !frontier.is_empty() && !is_cancelled() {
        let queue: Mutex<std::collections::VecDeque<String>> =
            Mutex::new(frontier.drain(..).collect());
        let next: Mutex<Vec<String>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    // One Engine (and CLI cache) per worker, reused across the
                    // folders it handles this level to amortise CLI startup.
                    let proton = ProtonCli::new(cfg);
                    let mut eng = Engine::new(cfg, proton, log, false);
                    eng.set_observer(events, cancel);
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let folder = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };
                        let Some(folder) = folder else { break };
                        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                        if let Some(sink) = events {
                            sink.emit(&SyncEvent::ScanProgress {
                                pair: pair.name.clone(),
                                folders: n,
                                current: remote_join(&pair.remote, &folder),
                            });
                        }
                        match eng.sync_pair_shallow_with_base(pair, &folder, &base) {
                            Ok((r, children)) => {
                                applied.fetch_add(r.applied, Ordering::Relaxed);
                                errors.fetch_add(r.errors.len(), Ordering::Relaxed);
                                next.lock().unwrap().extend(children);
                            }
                            Err(e) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                                log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
                            }
                        }
                    }
                });
            }
        });
        frontier = next.into_inner().unwrap();
        frontier.sort();
        frontier.dedup();
    }

    if !is_cancelled() {
        let _ = crate::state::set_last_synced(&cfg.state_dir, &pair.name, now_epoch());
    }
    let applied = applied.load(Ordering::Relaxed);
    let errors = errors.load(Ordering::Relaxed);
    if let Some(sink) = events {
        sink.emit(&SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied,
            errors,
            tracked: counter.load(Ordering::Relaxed),
        });
    }
    RunSummary {
        applied,
        errors,
        pairs: 1,
    }
}

/// Reconcile a specific SET of folders in a pair concurrently (shallow, no
/// descent). Used by the watch daemon for the startup local-change / hot phases
/// and change-event bursts, so a batch of folders runs through a worker pool
/// (like the streaming walk) instead of one slow CLI cold-start at a time. Each
/// folder is the same scoped, positive-confirmation shallow reconcile; workers
/// share one read-only baseline snapshot and commit disjoint rows.
pub fn run_sync_shallow_many(
    cfg: &Config,
    pair: &Pair,
    folders: &[String],
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    if folders.is_empty() {
        return RunSummary {
            applied: 0,
            errors: 0,
            pairs: 0,
        };
    }
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    if !pair.local.exists() {
        log.error(&format!(
            "local folder {} does not exist — refusing shallow batch",
            pair.local.display()
        ));
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 1,
        };
    }
    let base = match crate::state::load_baseline(&cfg.state_dir, &pair.name) {
        Ok(b) => std::sync::Arc::new(b),
        Err(e) => {
            log.error(&format!("baseline load failed for {:?}: {e}", pair.name));
            return RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            };
        }
    };
    let threads = if cfg.scan_threads > 0 {
        cfg.scan_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8)
    };
    let queue: Mutex<std::collections::VecDeque<String>> =
        Mutex::new(folders.iter().cloned().collect());
    let applied = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let counter = AtomicUsize::new(0);
    let is_cancelled = || cancel.map_or(false, |c| c.load(Ordering::Relaxed));
    // One pair-level scan for the whole batch (per-folder reconciles are silent
    // at the pair level), so the GUI shows one stable "scanning …" with a
    // climbing folder count rather than flapping per folder.
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });
    }
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let proton = ProtonCli::new(cfg);
                let mut eng = Engine::new(cfg, proton, log, false);
                eng.set_observer(events, cancel);
                loop {
                    if is_cancelled() {
                        break;
                    }
                    let folder = {
                        let mut q = queue.lock().unwrap();
                        q.pop_front()
                    };
                    let Some(folder) = folder else { break };
                    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(sink) = events {
                        sink.emit(&SyncEvent::ScanProgress {
                            pair: pair.name.clone(),
                            folders: n,
                            current: remote_join(&pair.remote, &folder),
                        });
                    }
                    match eng.sync_pair_shallow_with_base(pair, &folder, &base) {
                        Ok((r, _children)) => {
                            applied.fetch_add(r.applied, Ordering::Relaxed);
                            errors.fetch_add(r.errors.len(), Ordering::Relaxed);
                        }
                        Err(e) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
                        }
                    }
                }
            });
        }
    });
    let applied = applied.load(Ordering::Relaxed);
    let errors = errors.load(Ordering::Relaxed);
    if let Some(sink) = events {
        sink.emit(&SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied,
            errors,
            tracked: counter.load(Ordering::Relaxed),
        });
    }
    RunSummary {
        applied,
        errors,
        pairs: 1,
    }
}

pub struct Engine<'a, R: Remote> {
    cfg: &'a Config,
    remote: R,
    log: &'a Logger,
    dry_run: bool,
    gio: Option<PathBuf>,
    ensured: HashSet<String>,
    events: Option<&'a dyn EventSink>,
    cancel: Option<&'a AtomicBool>,
}

impl<'a, R: Remote> Engine<'a, R> {
    pub fn new(cfg: &'a Config, remote: R, log: &'a Logger, dry_run: bool) -> Self {
        Engine {
            cfg,
            remote,
            log,
            dry_run,
            gio: which_gio(),
            ensured: HashSet::new(),
            events: None,
            cancel: None,
        }
    }

    /// Attach a structured-event sink and/or a cancellation flag (used by the
    /// GUI/service). Optional; the CLI leaves both unset.
    pub fn set_observer(
        &mut self,
        events: Option<&'a dyn EventSink>,
        cancel: Option<&'a AtomicBool>,
    ) {
        self.events = events;
        self.cancel = cancel;
    }

    fn emit(&self, ev: SyncEvent) {
        if let Some(sink) = self.events {
            sink.emit(&ev);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.map_or(false, |c| c.load(Ordering::Relaxed))
    }

    // --- scanning -----------------------------------------------------------
    /// Walk the local tree. In sha1 mode, files whose size+mtime match the
    /// baseline reuse the stored hash instead of being re-read (a
    /// (size,mtime)->sha1 cache), so only changed files are hashed.
    fn scan_local(
        &self,
        root: &Path,
        baseline: &BTreeMap<String, Entry>,
        pair: &Pair,
        scope: Option<&str>,
    ) -> BTreeMap<String, Entry> {
        let mut out = BTreeMap::new();
        if !root.exists() {
            return out;
        }
        // A scoped sync walks only `root/<scope>` but still yields paths relative
        // to `root`, so keys line up with the baseline. A missing scoped folder
        // returns empty (the union vs the scoped baseline then handles it).
        let start = match scope {
            Some(s) => root.join(s),
            None => root.to_path_buf(),
        };
        if !start.exists() {
            return out;
        }
        let want_sha1 = self.cfg.compare == Compare::Sha1;
        // Prune excluded subtrees from the walk itself (`filter_entry` stops
        // descent), so an excluded folder is never even stat-walked locally —
        // matching the remote scan and keeping excludes truly invisible.
        for entry in walkdir::WalkDir::new(&start)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(
                |e| match e.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
                    Some(r) => !pair.is_excluded(&r.replace('\\', "/")),
                    None => true,
                },
            )
            .filter_map(|e| e.ok())
        {
            let rel = match entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
            {
                Some(r) => r.replace('\\', "/"),
                None => continue,
            };
            let ft = entry.file_type();
            if ft.is_dir() {
                out.insert(
                    rel.clone(),
                    Entry {
                        path: rel,
                        is_dir: true,
                        ..Default::default()
                    },
                );
            } else if ft.is_file() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    // A transient stat failure (a file locked or renamed
                    // mid-walk, an I/O blip) must NOT make a known file look
                    // deleted. If we synced it before, carry its baseline entry
                    // forward so it stays classified Unchanged and is retried,
                    // not trashed.
                    Err(_) => {
                        if let Some(b) = baseline.get(&rel) {
                            out.insert(rel.clone(), b.clone());
                        }
                        continue;
                    }
                };
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let sha1 = if want_sha1 {
                    // Reuse the cached hash when size+mtime are unchanged.
                    match baseline.get(&rel) {
                        Some(b)
                            if !b.is_dir
                                && b.size == size
                                && b.mtime == mtime
                                && b.sha1.is_some() =>
                        {
                            b.sha1.clone()
                        }
                        _ => sha1_file(entry.path()),
                    }
                } else {
                    None
                };
                out.insert(
                    rel.clone(),
                    Entry {
                        path: rel,
                        is_dir: false,
                        size,
                        mtime,
                        sha1,
                        remote_id: None,
                    },
                );
            }
        }
        out
    }

    /// Returns the remote tree, the relative folders that could not be listed
    /// (empty = a complete scan), and whether the pair's remote base folder
    /// itself was not found (a vanished/moved base, distinct from an empty one).
    fn scan_remote(
        &self,
        pair: &Pair,
        scope: Option<&str>,
    ) -> Result<(BTreeMap<String, Entry>, Vec<String>, bool)> {
        // The concurrent tree walk lives in the Remote impl; here we supply a
        // progress sink (log line + structured ScanProgress), an exclude
        // predicate (so excluded subtrees are never walked), and flatten the
        // (relpath, entry) pairs into a map. When `scope` is set we walk only
        // that remote sub-folder, but re-prefix every result with the scope so
        // keys stay relative to the pair root and line up with the baseline.
        let scoped_base = scope.map(|s| remote_join(&pair.remote, s));
        let base = scoped_base.as_deref().unwrap_or(pair.remote.as_str());
        let pair_name = pair.name.as_str();
        let log = self.log;
        let events = self.events;
        let counter = AtomicUsize::new(0);
        let progress = |p: &str| {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            log.info(&format!("scanning  {p}"));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::ScanProgress {
                    pair: pair_name.to_string(),
                    folders: n,
                    current: p.to_string(),
                });
            }
        };
        // `list_tree` yields paths relative to `base`; map them back to
        // pair-root-relative before checking excludes and keying the map.
        let full_rel = |under_base: &str| -> String {
            match scope {
                Some(s) => format!("{s}/{under_base}"),
                None => under_base.to_string(),
            }
        };
        let exclude = |rel: &str| pair.is_excluded(&full_rel(rel));
        let scan = self.remote.list_tree(base, &exclude, &progress)?;
        let mut out = BTreeMap::new();
        for (rel, mut e) in scan.entries {
            let key = full_rel(&rel);
            e.path = key.clone();
            out.insert(key, e);
        }
        Ok((out, scan.failed, scan.root_missing))
    }

    // --- planning -----------------------------------------------------------
    /// Returns the plan, the prospective new baseline, whether the scan was
    /// INCOMPLETE (some folders unreadable, or a side came back suspiciously
    /// empty), and the set of baseline rows to PRUNE because they now fall under
    /// an excluded sub-path. On an incomplete scan the caller must suppress
    /// deletions.
    fn plan(
        &self,
        pair: &Pair,
        resync: bool,
        scope: Option<&str>,
    ) -> Result<(Plan, BTreeMap<String, Entry>, bool, HashSet<String>)> {
        // Load the baseline first so the local scan can reuse cached hashes. For
        // a scoped sync, restrict the baseline to rows strictly UNDER the scope
        // so nothing outside it is ever seen as missing (and thus deleted) — the
        // scan, reconcile and additive commit all stay within the sub-tree.
        let base: BTreeMap<String, Entry> = if resync {
            BTreeMap::new()
        } else {
            let full = crate::state::load_baseline(&self.cfg.state_dir, &pair.name)?;
            match scope {
                Some(s) => {
                    let prefix = format!("{s}/");
                    full.into_iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .collect()
                }
                None => full,
            }
        };
        if base.is_empty() && !resync {
            self.log.info(&format!(
                "No baseline for {:?} yet; establishing one (union of both sides).",
                pair.name
            ));
        }
        self.emit(SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });

        // A missing local root (typically an unmounted drive) makes every known
        // file look deleted. Refuse the pair outright rather than trash the
        // remote copy or re-download the whole tree into the empty mountpoint.
        if !resync && !base.is_empty() && !pair.local.exists() {
            anyhow::bail!(
                "local folder {} does not exist — refusing to sync {:?} so a missing \
                 mount can't be mistaken for a mass deletion. Restore/remount it and \
                 re-run (use --resync to rebuild the baseline from scratch).",
                pair.local.display(),
                pair.name
            );
        }

        let local = self.scan_local(&pair.local, &base, pair, scope);
        let (remote, remote_failed, remote_root_missing) = self.scan_remote(pair, scope)?;
        let mut incomplete = !remote_failed.is_empty();
        if incomplete {
            self.log.warn(&format!(
                "remote scan incomplete for {:?}: {} folder(s) could not be listed; \
                 syncing without deletions this run. Unreadable: {:?}",
                pair.name,
                remote_failed.len(),
                remote_failed,
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: format!(
                    "{} folder(s) couldn't be listed — synced without deletions, will retry",
                    remote_failed.len()
                ),
            });
        }

        // The pair's remote base folder vanishing while a baseline exists is a
        // catastrophe signature (base deleted or moved on another device). An
        // empty-but-present base is a legitimate "all remote files deleted" and
        // is left to propagate; only a genuinely missing base is guarded. Sync
        // additively but suppress deletions so it can't be mirrored as a mass
        // local delete — local files simply re-upload and rebuild the base.
        if !resync && !base.is_empty() && remote_root_missing {
            incomplete = true;
            self.log.warn(&format!(
                "remote base folder for {:?} was not found but {} path(s) are on \
                 record; syncing without deletions this run to avoid a mass delete. \
                 If you moved or removed it deliberately, re-run with --resync.",
                pair.name,
                base.len()
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "remote base folder missing — synced without deletions to avoid data loss"
                    .into(),
            });
        }

        if !pair.exclude.is_empty() {
            self.log.info(&format!(
                "excluding {} sub-path(s) from {:?}: {:?}",
                pair.exclude.len(),
                pair.name,
                pair.exclude
            ));
        }

        let cmp = self.cfg.compare;
        let ls = classify(&local, &base, cmp);
        let rs = classify(&remote, &base, cmp);

        let mut plan = Plan::default();
        let mut new_base: BTreeMap<String, Entry> = BTreeMap::new();

        let mut keys: Vec<&String> = local
            .keys()
            .chain(remote.keys())
            .chain(base.keys())
            .collect();
        keys.sort();
        keys.dedup();

        for path in keys {
            // Excluded sub-paths are invisible to the engine: no upload, no
            // download, and crucially no delete on either side. They are not
            // added to new_base, and their old baseline rows are pruned (below)
            // so a later re-include can only ever re-download, never delete.
            if pair.is_excluded(path) {
                continue;
            }
            let lc = *ls.get(path).unwrap_or(&Change::Absent);
            let rc = *rs.get(path).unwrap_or(&Change::Absent);
            self.decide(
                path,
                lc,
                rc,
                local.get(path),
                remote.get(path),
                base.get(path),
                &mut plan,
                &mut new_base,
            );
        }

        // Baseline rows now under an excluded sub-path are forgotten. This is a
        // pure DB cleanup (no filesystem or remote effect) that makes re-include
        // behave like a fresh folder — union/redownload, never a delete.
        let prune_excluded: HashSet<String> = base
            .keys()
            .filter(|k| pair.is_excluded(k))
            .cloned()
            .collect();

        // Collapse delete+create pairs of identical content into a single
        // rename/move (keeps large files from being re-uploaded on rename).
        // Skip on an incomplete scan: a "missing" source may just be in an
        // unlisted folder, so a rename could wrongly move a remote file.
        if !incomplete {
            detect_renames(&mut plan, &base, &local, &remote);
        }
        Ok((plan, new_base, incomplete, prune_excluded))
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        path: &str,
        lc: Change,
        rc: Change,
        le: Option<&Entry>,
        re: Option<&Entry>,
        be: Option<&Entry>,
        plan: &mut Plan,
        new_base: &mut BTreeMap<String, Entry>,
    ) {
        let is_dir = le.or(re).or(be).map(|e| e.is_dir).unwrap_or(false);

        // Both unchanged -> keep as-is.
        if lc == Change::Unchanged && rc == Change::Unchanged {
            if let Some(e) = le.or(re) {
                new_base.insert(path.to_string(), e.clone());
            }
            return;
        }

        // Type clash: the path is a file on one side and a directory on the
        // other. The file/dir merge logic can't reconcile that, so surface it
        // and skip — never silently leave the two sides diverged or advance the
        // baseline to the wrong type. Leaving the baseline row untouched means
        // it keeps being flagged until the user removes or renames one side.
        if let (Some(a), Some(b)) = (le, re) {
            if a.is_dir != b.is_dir {
                let msg = format!(
                    "{path}: type mismatch — file on one side, directory on the \
                     other; skipped. Remove or rename one side to resolve."
                );
                self.log.warn(&msg);
                self.emit(SyncEvent::Error {
                    pair: None,
                    text: msg,
                });
                return;
            }
        }

        if is_dir {
            self.decide_dir(path, lc, rc, le, re, plan, new_base);
            return;
        }

        let same_now =
            matches!((le, re), (Some(a), Some(b)) if same_content(a, b, self.cfg.compare));

        use Change::*;
        // one side changed, the other did not
        if matches!(lc, Created | Modified) && rc == Unchanged {
            self.emit_upload(path, le, plan, new_base, lc.label());
            return;
        }
        if matches!(rc, Created | Modified) && lc == Unchanged {
            self.emit_download(path, re, plan, new_base, rc.label());
            return;
        }
        // one side created, other absent
        if lc == Created && rc == Absent {
            self.emit_upload(path, le, plan, new_base, "new local");
            return;
        }
        if rc == Created && lc == Absent {
            self.emit_download(path, re, plan, new_base, "new remote");
            return;
        }
        // both created / both modified
        if matches!(lc, Created | Modified) && matches!(rc, Created | Modified) {
            if same_now {
                if let Some(e) = le {
                    new_base.insert(path.to_string(), e.clone());
                }
                return;
            }
            // Same byte size but different mtimes: usually metadata drift (a
            // download/touch, or Proton's claimedModificationTime vs a local
            // clock), not a real dual edit. Emitting keep-both here multiplies
            // conflict copies forever. Prefer size equality; if both sides carry
            // sha1 and they differ, fall through to a real conflict.
            if let (Some(a), Some(b)) = (le, re) {
                if a.size == b.size && a.size > 0 {
                    let sha_clash = matches!(
                        (&a.sha1, &b.sha1),
                        (Some(x), Some(y)) if x != y
                    );
                    if !sha_clash {
                        if let Some(e) = le {
                            new_base.insert(path.to_string(), e.clone());
                        }
                        return;
                    }
                }
            }
            self.emit_conflict(path, le, re, plan, new_base);
            return;
        }
        // deletions
        if lc == Deleted && rc == Unchanged {
            self.emit_delete_remote(path, re, plan, new_base);
            return;
        }
        if rc == Deleted && lc == Unchanged {
            self.emit_delete_local(path, le, plan, new_base);
            return;
        }
        if lc == Deleted && rc == Deleted {
            return; // gone on both sides
        }
        // delete-vs-change: never lose the surviving edit
        if lc == Deleted && rc == Modified {
            self.emit_download(
                path,
                re,
                plan,
                new_base,
                "deleted locally but modified remotely; keeping remote",
            );
            return;
        }
        if rc == Deleted && lc == Modified {
            self.emit_upload(
                path,
                le,
                plan,
                new_base,
                "deleted remotely but modified locally; keeping local",
            );
            return;
        }
        // fallback: keep whatever exists, nothing destructive
        if let Some(e) = le.or(re) {
            new_base.insert(path.to_string(), e.clone());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_dir(
        &self,
        path: &str,
        lc: Change,
        rc: Change,
        le: Option<&Entry>,
        re: Option<&Entry>,
        plan: &mut Plan,
        new_base: &mut BTreeMap<String, Entry>,
    ) {
        use Change::*;
        if lc == Created && re.is_none() {
            plan.add(Op::new(Action::MkdirRemote, path, true, "new local dir"));
            if let Some(e) = le {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if rc == Created && le.is_none() {
            plan.add(Op::new(Action::MkdirLocal, path, true, "new remote dir"));
            if let Some(e) = re {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if lc == Deleted && rc == Unchanged {
            if self.cfg.propagate_deletes {
                plan.add(Op::new(
                    Action::DeleteRemote,
                    path,
                    true,
                    "dir removed locally",
                ));
            } else if let Some(e) = re {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if rc == Deleted && lc == Unchanged {
            if self.cfg.propagate_deletes {
                plan.add(Op::new(
                    Action::DeleteLocal,
                    path,
                    true,
                    "dir removed remotely",
                ));
            } else if let Some(e) = le {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if let Some(e) = le.or(re) {
            new_base.insert(path.to_string(), e.clone());
        }
    }

    fn emit_upload(
        &self,
        path: &str,
        e: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
        reason: &str,
    ) {
        plan.add(Op::new(Action::Upload, path, false, reason));
        if let Some(e) = e {
            nb.insert(path.to_string(), e.clone());
        }
    }

    fn emit_download(
        &self,
        path: &str,
        e: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
        reason: &str,
    ) {
        plan.add(Op::new(Action::Download, path, false, reason));
        if let Some(e) = e {
            nb.insert(path.to_string(), e.clone());
        }
    }

    fn emit_conflict(
        &self,
        path: &str,
        le: Option<&Entry>,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        match self.cfg.conflict {
            ConflictPolicy::Skip => {
                plan.add(Op::new(
                    Action::Noop,
                    path,
                    false,
                    "conflict skipped (both changed)",
                ));
            }
            ConflictPolicy::Newer => match (le.and_then(|e| e.mtime), re.and_then(|e| e.mtime)) {
                (Some(lm), Some(rm)) if lm >= rm => {
                    self.emit_upload(path, le, plan, nb, "conflict: local newer")
                }
                (Some(_), Some(_)) => {
                    self.emit_download(path, re, plan, nb, "conflict: remote newer")
                }
                _ => self.emit_conflict_keep_both(path, re, plan, nb),
            },
            ConflictPolicy::KeepBoth => self.emit_conflict_keep_both(path, re, plan, nb),
        }
    }

    fn emit_conflict_keep_both(
        &self,
        path: &str,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        plan.add(Op::new(
            Action::Conflict,
            path,
            false,
            "both sides changed; keeping both",
        ));
        if let Some(e) = re {
            nb.insert(path.to_string(), e.clone()); // original name holds remote version
        }
    }

    fn emit_delete_remote(
        &self,
        path: &str,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        if self.cfg.propagate_deletes {
            plan.add(Op::new(
                Action::DeleteRemote,
                path,
                false,
                "removed locally",
            ));
        } else {
            plan.add(Op::new(
                Action::Noop,
                path,
                false,
                "removed locally; delete not propagated",
            ));
            if let Some(e) = re {
                nb.insert(path.to_string(), e.clone());
            }
        }
    }

    fn emit_delete_local(
        &self,
        path: &str,
        le: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        if self.cfg.propagate_deletes {
            plan.add(Op::new(
                Action::DeleteLocal,
                path,
                false,
                "removed remotely",
            ));
        } else {
            plan.add(Op::new(
                Action::Noop,
                path,
                false,
                "removed remotely; delete not propagated",
            ));
            if let Some(e) = le {
                nb.insert(path.to_string(), e.clone());
            }
        }
    }

    // --- execution ----------------------------------------------------------
    /// Returns the result and the set of paths whose delete/rename actually
    /// COMPLETED this run (a subset of what was planned — cancelled or failed
    /// ops are absent), so callers can act only on confirmed deletions.
    fn apply(
        &mut self,
        pair: &Pair,
        plan: Plan,
        mut new_base: BTreeMap<String, Entry>,
        prune_excluded: HashSet<String>,
    ) -> (SyncResult, HashSet<String>) {
        let mut result = SyncResult {
            pair: pair.name.clone(),
            applied: 0,
            errors: Vec::new(),
            plan_summary: plan.summary(),
            was_in_sync: plan.is_empty(),
        };

        let mut ops: Vec<Op> = plan.ops;
        ops.sort_by_key(order_key);

        // Downloads are independent (each self-creates its parent dir and only
        // reads the baseline), so they run through a concurrent pool. Everything
        // else — mkdirs, uploads, conflicts, renames, deletes — stays on this
        // thread in order (conflicts/renames mutate the new baseline).
        let (downloads, rest): (Vec<Op>, Vec<Op>) =
            ops.into_iter().partition(|o| o.action == Action::Download);

        let needs_base = rest.iter().any(|o| {
            matches!(
                o.action,
                Action::Upload | Action::MkdirRemote | Action::Conflict
            )
        });
        if needs_base && !self.dry_run {
            if let Err(e) = self.ensure_remote_base(pair) {
                let msg = format!("remote base {}: {e}", pair.remote);
                self.log.error(&msg);
                result.errors.push(msg);
                return (result, HashSet::new());
            }
        }

        // Per-file bookkeeping for the additive commit, by POSITIVE
        // confirmation: a row that `decide()` added to `new_base` at plan time
        // is provisional until its op actually completes. `pending` is every
        // such op's path (deletes/no-ops add no row — deletes drop theirs via
        // `deleted_ok` — so they're excluded); `done` collects the ones that
        // succeed. Any pending row NOT in `done` at commit time — a FAILED
        // transfer OR an op the run never reached because it was CANCELLED — is
        // dropped, so its previous baseline row is left exactly as it was (or
        // stays absent for a brand-new file) and it is re-detected and retried
        // next run. This stops a partial/cancelled run from recording a
        // never-transferred file as 'synced', which the next run would otherwise
        // read as a deletion and propagate.
        let pending: HashSet<String> = downloads
            .iter()
            .chain(rest.iter())
            .filter(|o| {
                !matches!(
                    o.action,
                    Action::DeleteRemote | Action::DeleteLocal | Action::Noop
                )
            })
            .map(|o| o.path.clone())
            .collect();
        let mut done: HashSet<String> = HashSet::new();
        let mut deleted_ok: HashSet<String> = HashSet::new();

        for op in &rest {
            if self.cancelled() {
                self.log
                    .warn("cancelled; stopping before the next operation");
                self.emit(SyncEvent::Info {
                    text: "cancelled".into(),
                });
                break;
            }
            self.log.info(&op.describe());
            if op.action == Action::Noop {
                continue;
            }
            if self.dry_run {
                continue;
            }
            self.emit(SyncEvent::OpStarted {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
            });
            let outcome = self.apply_op(pair, op, &mut new_base);
            let ok = outcome.is_ok();
            // The bare cause, without the "<action> <path>: " prefix the log
            // line carries: the event already names the action and the path.
            let err_text = outcome.as_ref().err().map(|e| e.to_string());
            match outcome {
                Ok(()) => {
                    result.applied += 1;
                    match op.action {
                        Action::DeleteRemote | Action::DeleteLocal => {
                            deleted_ok.insert(op.path.clone());
                        }
                        Action::RenameRemote | Action::RenameLocal => {
                            if let Some(from) = &op.from {
                                deleted_ok.insert(from.clone());
                            }
                        }
                        _ => {}
                    }
                    // Confirm this op's provisional baseline row (harmless for
                    // deletes/no-ops, which aren't in `pending`).
                    done.insert(op.path.clone());
                }
                Err(e) => {
                    let msg = format!("{} {}: {e}", op.action.label(), op.path);
                    self.log.error(&msg);
                    result.errors.push(msg);
                }
            }
            self.emit(SyncEvent::OpFinished {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
                ok,
                error: err_text,
            });
        }

        // Concurrent download phase.
        if !downloads.is_empty() && !self.cancelled() {
            if self.dry_run {
                for op in &downloads {
                    self.log.info(&op.describe());
                }
            } else {
                self.run_downloads(pair, &downloads, &new_base, &mut result, &mut done);
            }
        }

        // Positive-confirmation commit: a provisional row survives ONLY if its
        // op completed this run. Drop every pending row not in `done` — a failed
        // transfer OR an op skipped because the run was cancelled before reaching
        // it — so its previous baseline row survives untouched (or stays absent
        // for a brand-new file). Next run re-detects the change and retries it in
        // the correct direction — a timeout or a cancel never invalidates the
        // rest of the sync, and a never-transferred file is never recorded as
        // synced (which the next run would read as a deletion).
        for rel in &pending {
            if !done.contains(rel) {
                new_base.remove(rel);
            }
        }

        // Additive per-file commit: only files that fully synced this run are
        // written (as 'synced'); unseen rows are left intact, so a partial or
        // interrupted run still persists exactly the files that did complete.
        if !self.dry_run {
            // Forget baseline rows now under an excluded sub-path (pure DB
            // cleanup, no filesystem or remote effect) so a later re-include
            // re-downloads rather than propagating a delete.
            if !prune_excluded.is_empty() {
                self.log.info(&format!(
                    "pruning {} baseline row(s) now under an excluded path for {:?}",
                    prune_excluded.len(),
                    pair.name
                ));
                deleted_ok.extend(prune_excluded);
            }
            if let Err(e) = crate::state::commit_baseline(
                &self.cfg.state_dir,
                &pair.name,
                &new_base,
                &deleted_ok,
            ) {
                self.log.warn(&format!(
                    "could not commit baseline for {:?}: {e}",
                    pair.name
                ));
            }
            let retrying = pending.iter().filter(|r| !done.contains(*r)).count();
            if retrying > 0 {
                self.log.warn(&format!(
                    "{retrying} file(s) not completed for {:?} (failed or cancelled); \
                     they retry next run.",
                    pair.name
                ));
            }
            let _ = crate::state::set_last_synced(
                &self.cfg.state_dir,
                &pair.name,
                crate::datefmt::now_epoch(),
            );
        }
        (result, deleted_ok)
    }

    /// Run the download ops through the backend's concurrent pool, emitting the
    /// same per-op events as the sequential path so the GUI shows every file in
    /// flight. Reads `new_base` for target mtimes only (no mutation), so it's
    /// safe to share across the worker threads.
    fn run_downloads(
        &self,
        pair: &Pair,
        downloads: &[Op],
        new_base: &BTreeMap<String, Entry>,
        result: &mut SyncResult,
        done_down: &mut HashSet<String>,
    ) {
        let mut jobs: Vec<DownloadJob> = Vec::new();
        for op in downloads {
            // Guard the destination against a remote-controlled name escaping
            // the sync root; skip (don't abort the batch) anything unsafe.
            let local_full = match safe_join(&pair.local, &op.path) {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    self.log.error(&msg);
                    result.errors.push(msg);
                    continue;
                }
            };
            let dest_dir = local_full
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| pair.local.clone());
            jobs.push(DownloadJob {
                rel: op.path.clone(),
                remote_path: remote_join(&pair.remote, &op.path),
                dest_dir: dest_dir.to_string_lossy().into_owned(),
                mtime: new_base.get(&op.path).and_then(|e| e.mtime),
            });
        }

        let events = self.events;
        let log = self.log;
        let pair_name = pair.name.as_str();
        let local_root = &pair.local;
        let applied = AtomicUsize::new(0);
        let errs: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let done_rels: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let fallback = AtomicBool::new(false);
        let cancel_ref: &AtomicBool = self.cancel.unwrap_or(&fallback);

        let on_start = |j: &DownloadJob| {
            log.info(&format!("download    file {} (new remote)", j.rel));
            if let Some(s) = events {
                s.emit(&SyncEvent::OpStarted {
                    pair: pair_name.to_string(),
                    action: "download".into(),
                    path: j.rel.clone(),
                });
            }
        };
        let on_done = |j: &DownloadJob, r: std::result::Result<(), String>| {
            let ok = r.is_ok();
            let err_text = r.as_ref().err().cloned();
            match r {
                Ok(()) => {
                    applied.fetch_add(1, Ordering::Relaxed);
                    match_mtime(&local_root.join(&j.rel), j.mtime);
                    done_rels.lock().unwrap().push(j.rel.clone());
                }
                Err(e) => {
                    let msg = format!("download {}: {e}", j.rel);
                    log.error(&msg);
                    errs.lock().unwrap().push(msg);
                }
            }
            if let Some(s) = events {
                s.emit(&SyncEvent::OpFinished {
                    pair: pair_name.to_string(),
                    action: "download".into(),
                    path: j.rel.clone(),
                    ok,
                    error: err_text,
                });
            }
        };

        self.remote.download_many(
            &jobs,
            self.cfg.download_threads,
            cancel_ref,
            &on_start,
            &on_done,
        );

        result.applied += applied.load(Ordering::Relaxed);
        result.errors.extend(errs.into_inner().unwrap());
        done_down.extend(done_rels.into_inner().unwrap());
    }

    fn apply_op(
        &mut self,
        pair: &Pair,
        op: &Op,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = safe_join(&pair.local, &op.path)?;
        let remote_full = remote_join(&pair.remote, &op.path);
        match op.action {
            Action::MkdirRemote => self.ensure_remote_dir(&pair.remote, &op.path)?,
            Action::MkdirLocal => {
                std::fs::create_dir_all(&local_full)?;
            }
            Action::Upload => {
                let parent = parent_rel(&op.path);
                self.ensure_remote_dir(&pair.remote, parent)?;
                self.remote.upload(
                    &local_full.to_string_lossy(),
                    &remote_join(&pair.remote, parent),
                )?;
            }
            Action::Download => {
                let dest_dir = local_full
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(pair.local.clone());
                std::fs::create_dir_all(&dest_dir)?;
                self.remote
                    .download(&remote_full, &dest_dir.to_string_lossy())?;
                if let Some(e) = new_base.get(&op.path) {
                    match_mtime(&local_full, e.mtime);
                }
            }
            Action::DeleteRemote => self.remote.trash(&remote_full)?,
            Action::DeleteLocal => self.delete_local(&local_full, op.is_dir)?,
            Action::RenameRemote => {
                let from = op.from.as_deref().unwrap_or(&op.path);
                self.remote
                    .rename(&remote_join(&pair.remote, from), &remote_full)?;
            }
            Action::RenameLocal => {
                let from = op.from.as_deref().unwrap_or(&op.path);
                let from_full = safe_join(&pair.local, from)?;
                if let Some(parent) = local_full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&from_full, &local_full)?;
                if let Some(e) = new_base.get(&op.path) {
                    match_mtime(&local_full, e.mtime);
                }
            }
            Action::Conflict => self.resolve_conflict(pair, &op.path, new_base)?,
            Action::Noop => {}
        }
        Ok(())
    }

    fn resolve_conflict(
        &mut self,
        pair: &Pair,
        rel: &str,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = pair.local.join(rel);
        let stamp = epoch_to_stamp(now_epoch());
        let conflict_rel = conflict_name(rel, &stamp);
        let conflict_full = pair.local.join(&conflict_rel);
        self.log
            .info(&format!("  conflict: keeping local copy as {conflict_rel}"));
        if local_full.exists() {
            std::fs::rename(&local_full, &conflict_full)?;
        }
        // bring the remote version down to the original name
        let dest_dir = local_full
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(pair.local.clone());
        std::fs::create_dir_all(&dest_dir)?;
        self.remote
            .download(&remote_join(&pair.remote, rel), &dest_dir.to_string_lossy())?;
        // push the renamed local copy up so both versions exist on both sides
        let parent = parent_rel(&conflict_rel);
        self.ensure_remote_dir(&pair.remote, parent)?;
        self.remote.upload(
            &conflict_full.to_string_lossy(),
            &remote_join(&pair.remote, parent),
        )?;
        let size = std::fs::metadata(&conflict_full)
            .map(|m| m.len())
            .unwrap_or(0);
        new_base.insert(
            conflict_rel.clone(),
            Entry {
                path: conflict_rel,
                is_dir: false,
                size,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn ensure_remote_base(&mut self, pair: &Pair) -> Result<()> {
        if let Some(rel) = strip_root(&self.cfg.remote_root, &pair.remote) {
            let rel = rel.to_string();
            self.ensure_remote_dir(&self.cfg.remote_root.clone(), &rel)?;
        }
        Ok(())
    }

    fn ensure_remote_dir(&mut self, base: &str, rel: &str) -> Result<()> {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return Ok(());
        }
        let mut acc = String::new();
        for part in rel.split('/') {
            let parent = remote_join(base, &acc);
            let full = if acc.is_empty() {
                part.to_string()
            } else {
                format!("{acc}/{part}")
            };
            if self.ensured.contains(&full) {
                acc = full;
                continue;
            }
            self.remote.create_folder(&parent, part)?;
            self.ensured.insert(full.clone());
            acc = full;
        }
        Ok(())
    }

    fn delete_local(&self, path: &Path, is_dir: bool) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        match self.cfg.local_delete {
            LocalDelete::Trash => trash_local(path, self.gio.as_deref()),
            LocalDelete::Remove => {
                if is_dir {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
                Ok(())
            }
        }
    }

    // --- top-level ----------------------------------------------------------
    /// Reconcile a whole pair (local tree <-> remote tree).
    pub fn sync_pair(&mut self, pair: &Pair, resync: bool) -> Result<SyncResult> {
        self.sync_pair_scoped(pair, resync, None)
    }

    /// Reconcile a pair, optionally restricted to a single sub-folder (`scope`,
    /// a POSIX path relative to the pair root). A scoped run scans, reconciles,
    /// and commits only within that sub-tree, so a local change can be synced
    /// for just its folder instead of re-walking the whole pair. Nothing outside
    /// the scope is looked at, so it can never be seen as deleted.
    pub fn sync_pair_scoped(
        &mut self,
        pair: &Pair,
        resync: bool,
        scope: Option<&str>,
    ) -> Result<SyncResult> {
        // An empty scope means the pair ROOT — i.e. the whole pair, not a
        // sub-folder called "". Normalise it to None so paths aren't prefixed
        // with a stray leading "/" (which would misclassify every entry and
        // mass-recreate folders). A root-level file change thus does a full sync.
        let scope = scope.filter(|s| !s.is_empty());
        self.log.info(&format!(
            "=== pair {:?}{} : {} <-> {} ===",
            pair.name,
            scope.map(|s| format!(" [{s}]")).unwrap_or_default(),
            pair.local.display(),
            pair.remote
        ));
        self.emit(SyncEvent::PairStarted {
            pair: pair.name.clone(),
        });
        let (mut plan, new_base, incomplete, prune_excluded) = self.plan(pair, resync, scope)?;
        if incomplete {
            // Safety: never delete or move based on a partial view of the remote.
            plan.ops.retain(|op| {
                !matches!(
                    op.action,
                    Action::DeleteRemote
                        | Action::DeleteLocal
                        | Action::RenameRemote
                        | Action::RenameLocal
                )
            });
        }
        if plan.is_empty() {
            self.log.info(&format!(
                "already in sync ({} files tracked).",
                new_base.len()
            ));
        } else {
            self.log.info(&format!("plan: {}", plan.summary()));
        }
        self.emit(SyncEvent::Planned {
            pair: pair.name.clone(),
            total_ops: plan.actionable().count(),
            counts: plan
                .counts()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        });
        let tracked = new_base.len();
        let (result, _deleted_ok) = self.apply(pair, plan, new_base, prune_excluded);
        self.emit(SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied: result.applied,
            errors: result.errors.len(),
            tracked,
        });
        Ok(result)
    }

    /// Reconcile ONLY the direct children of `folder` (POSIX path relative to
    /// the pair root; "" = the root). Non-recursive: files in `folder` merge
    /// normally, and immediate sub-folders are created/removed but NOT descended
    /// into. Deeper changes arrive as their own watch events (recursive inotify),
    /// and remote-only changes are caught by the hot pass and the full walk — so
    /// a local change touches just its folder's listing, never a whole subtree.
    pub fn sync_pair_shallow(
        &mut self,
        pair: &Pair,
        folder: &str,
    ) -> Result<(SyncResult, Vec<String>)> {
        let base_all = crate::state::load_baseline(&self.cfg.state_dir, &pair.name)?;
        self.sync_pair_shallow_with_base(pair, folder, &base_all)
    }

    /// Sequential streaming walk (BFS): shallow-reconcile each folder as it is
    /// discovered, descending into the children it reports, transferring as it
    /// goes. Same per-folder primitive and one shared baseline snapshot as the
    /// concurrent [`run_sync_streaming`]; kept single-threaded here so it works
    /// with any `Remote` (including the test fake) and is the tested reference
    /// for the walk's semantics.
    pub fn sync_pair_streaming(&mut self, pair: &Pair) -> Result<SyncResult> {
        let base = crate::state::load_baseline(&self.cfg.state_dir, &pair.name)?;
        let mut applied = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut frontier: Vec<String> = vec![String::new()];
        while !frontier.is_empty() {
            if self.cancelled() {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for folder in frontier.drain(..) {
                if self.cancelled() {
                    break;
                }
                let (r, children) = self.sync_pair_shallow_with_base(pair, &folder, &base)?;
                applied += r.applied;
                errors.extend(r.errors);
                next.extend(children);
            }
            next.sort();
            next.dedup();
            frontier = next;
        }
        if !self.dry_run && !self.cancelled() {
            let _ = crate::state::set_last_synced(
                &self.cfg.state_dir,
                &pair.name,
                crate::datefmt::now_epoch(),
            );
        }
        Ok(SyncResult {
            pair: pair.name.clone(),
            applied,
            was_in_sync: applied == 0 && errors.is_empty(),
            errors,
            plan_summary: String::new(),
        })
    }

    /// Like [`sync_pair_shallow`] but with a pre-loaded baseline snapshot, so a
    /// streaming walk can share one snapshot across many folders instead of
    /// reloading it per folder. Returns the result plus the immediate sub-folders
    /// to descend into (present on either side after the reconcile; a folder
    /// deleted this run is not returned).
    pub fn sync_pair_shallow_with_base(
        &mut self,
        pair: &Pair,
        folder: &str,
        base_all: &BTreeMap<String, Entry>,
    ) -> Result<(SyncResult, Vec<String>)> {
        let folder = folder.trim_matches('/');
        let label = if folder.is_empty() {
            "(root)".to_string()
        } else {
            folder.to_string()
        };
        self.log
            .info(&format!("=== pair {:?} <{label}> shallow ===", pair.name));
        // NOTE: no PairStarted/Planned/PairFinished here. This runs per-folder,
        // often hundreds of times inside one streaming walk; emitting pair-level
        // events per folder would flap the GUI (Scanning<->Synced, path blanked)
        // and never show a stable progress line. The callers (run_sync_shallow,
        // run_sync_shallow_many, run_sync_streaming) emit ONE ScanStarted, a
        // per-folder ScanProgress, and ONE PairFinished around the whole batch.
        if !pair.local.exists() {
            anyhow::bail!(
                "local folder {} does not exist — refusing shallow sync",
                pair.local.display()
            );
        }

        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        // Direct children only: a key is "<prefix><name>" with no further '/'.
        let is_direct = |k: &str| match k.strip_prefix(&prefix) {
            Some(rest) => !rest.is_empty() && !rest.contains('/'),
            None => false,
        };
        let base_direct: BTreeMap<String, Entry> = base_all
            .iter()
            .filter(|(k, _)| is_direct(k) && !pair.is_excluded(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // LOCAL direct children (depth 1, no recursion).
        let want_sha1 = self.cfg.compare == Compare::Sha1;
        let dir_path = if folder.is_empty() {
            pair.local.clone()
        } else {
            pair.local.join(folder)
        };
        let mut local_direct: BTreeMap<String, Entry> = BTreeMap::new();
        // A local `read_dir` failure (a permissions blip, an I/O error, or fd
        // exhaustion under the concurrent walk) must NOT be read as "everything
        // here was deleted" — that would trash still-present remote files. Flag
        // it and suppress deletes this run, exactly like a failed remote listing.
        let local_failed = match std::fs::read_dir(&dir_path) {
            Err(e) => {
                self.log.warn(&format!(
                    "shallow: local listing failed for {}: {e}; syncing without deletions",
                    dir_path.display()
                ));
                true
            }
            Ok(rd) => {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().replace('\\', "/");
                    let rel = format!("{prefix}{name}");
                    if pair.is_excluded(&rel) {
                        continue;
                    }
                    let ft = match e.file_type() {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    if ft.is_dir() {
                        local_direct.insert(
                            rel.clone(),
                            Entry {
                                path: rel,
                                is_dir: true,
                                ..Default::default()
                            },
                        );
                    } else if ft.is_file() {
                        match e.metadata() {
                            Ok(m) => {
                                let size = m.len();
                                let mtime = m
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs() as i64);
                                let sha1 = if want_sha1 {
                                    match base_direct.get(&rel) {
                                        Some(b)
                                            if !b.is_dir
                                                && b.size == size
                                                && b.mtime == mtime
                                                && b.sha1.is_some() =>
                                        {
                                            b.sha1.clone()
                                        }
                                        _ => sha1_file(&e.path()),
                                    }
                                } else {
                                    None
                                };
                                local_direct.insert(
                                    rel.clone(),
                                    Entry {
                                        path: rel,
                                        is_dir: false,
                                        size,
                                        mtime,
                                        sha1,
                                        remote_id: None,
                                    },
                                );
                            }
                            // A transient stat failure must not look like a
                            // delete: carry the baseline entry forward so the
                            // file stays Unchanged and is retried, never trashed.
                            Err(_) => {
                                if let Some(b) = base_direct.get(&rel) {
                                    local_direct.insert(rel.clone(), b.clone());
                                }
                            }
                        }
                    }
                }
                false
            }
        };

        // REMOTE direct children (single non-recursive listing). A transport
        // error means we can't see the folder — suppress deletes this run rather
        // than act on a blind listing. (A genuinely absent folder lists empty.)
        let remote_path = remote_join(&pair.remote, folder);
        let (remote_direct, remote_failed) = match self.remote.list_dir_probe(&remote_path) {
            Ok(ListOutcome::Listed(entries)) => {
                let mut m = BTreeMap::new();
                for mut e in entries {
                    let rel = format!("{prefix}{}", e.path);
                    if pair.is_excluded(&rel) {
                        continue;
                    }
                    e.path = rel.clone();
                    m.insert(rel, e);
                }
                (m, false)
            }
            // Not found: a folder the parent just listed as present now reports
            // missing — a race (trashed elsewhere) or a transient error misread
            // as not-found. Either way, don't propagate deletes on that basis;
            // treat it as a failed listing (empty, deletes suppressed).
            Ok(ListOutcome::NotFound) => {
                self.log.warn(&format!(
                    "shallow: remote folder {remote_path:?} not found; \
                     syncing without deletions"
                ));
                (BTreeMap::new(), true)
            }
            Err(err) => {
                self.log.warn(&format!(
                    "shallow: remote listing failed for {remote_path:?}: {err}; \
                     syncing without deletions"
                ));
                (BTreeMap::new(), true)
            }
        };

        let cmp = self.cfg.compare;
        let ls = classify(&local_direct, &base_direct, cmp);
        let rs = classify(&remote_direct, &base_direct, cmp);
        let mut plan = Plan::default();
        let mut new_base: BTreeMap<String, Entry> = BTreeMap::new();
        let mut keys: Vec<&String> = local_direct
            .keys()
            .chain(remote_direct.keys())
            .chain(base_direct.keys())
            .collect();
        keys.sort();
        keys.dedup();
        for path in keys {
            let lc = *ls.get(path).unwrap_or(&Change::Absent);
            let rc = *rs.get(path).unwrap_or(&Change::Absent);
            self.decide(
                path,
                lc,
                rc,
                local_direct.get(path),
                remote_direct.get(path),
                base_direct.get(path),
                &mut plan,
                &mut new_base,
            );
        }
        // Never delete or move on a blind view of either side: if the local
        // read_dir OR the remote listing failed, strip all destructive ops.
        if remote_failed || local_failed {
            plan.ops.retain(|op| {
                !matches!(
                    op.action,
                    Action::DeleteRemote
                        | Action::DeleteLocal
                        | Action::RenameRemote
                        | Action::RenameLocal
                )
            });
        }
        // Baseline rows for direct children that are now EXCLUDED. (`base_direct`
        // already filters excludes out, so scan the full snapshot instead.) These
        // are pruned so a later re-include re-downloads rather than deleting.
        let prune_excluded: HashSet<String> = base_all
            .keys()
            .filter(|k| is_direct(k) && pair.is_excluded(k))
            .cloned()
            .collect();

        if plan.is_empty() {
            self.log
                .info(&format!("shallow <{label}>: already in sync."));
        } else {
            self.log
                .info(&format!("shallow <{label}> plan: {}", plan.summary()));
        }

        // A deleted direct sub-folder has descendants in the baseline that the
        // apply's per-key commit won't touch; collect them for cleanup after.
        let deleted_dirs: Vec<String> = plan
            .ops
            .iter()
            .filter(|o| matches!(o.action, Action::DeleteRemote | Action::DeleteLocal) && o.is_dir)
            .map(|o| o.path.clone())
            .collect();

        let tracked = new_base.len();
        let (result, deleted_ok) = self.apply(pair, plan, new_base, prune_excluded);

        // Purge descendant baseline rows ONLY for sub-folders whose delete
        // actually COMPLETED (in `deleted_ok`). A delete that was cancelled or
        // failed leaves the folder in place, so its rows must survive — otherwise
        // the baseline would diverge from reality and cause spurious re-work.
        if !self.dry_run {
            let mut descendants: HashSet<String> = HashSet::new();
            for d in deleted_dirs.iter().filter(|d| deleted_ok.contains(*d)) {
                let pfx = format!("{d}/");
                for k in base_all.keys() {
                    if k.starts_with(&pfx) {
                        descendants.insert(k.clone());
                    }
                }
            }
            if !descendants.is_empty() {
                let _ = crate::state::commit_baseline(
                    &self.cfg.state_dir,
                    &pair.name,
                    &BTreeMap::new(),
                    &descendants,
                );
            }
        }

        let _ = tracked; // pair-level events are emitted by the caller, not here

        // Immediate sub-folders to descend into on a streaming walk: dirs present
        // on either side after the reconcile, minus any deleted this run.
        let deleted: HashSet<&String> = deleted_dirs.iter().collect();
        let mut children: Vec<String> = local_direct
            .iter()
            .chain(remote_direct.iter())
            .filter(|(k, e)| e.is_dir && !deleted.contains(k))
            .map(|(k, _)| k.clone())
            .collect();
        children.sort();
        children.dedup();
        Ok((result, children))
    }
}

/// Join `rel` onto `root`, guaranteeing the result stays within `root`. Only
/// normal path components are allowed; any `..`, absolute, or prefix component
/// (which could escape the sync root) is rejected. Every local filesystem path
/// built from a remote-controlled name must go through this.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => anyhow::bail!("refusing unsafe path {rel:?}: it escapes the sync root"),
        }
    }
    Ok(out)
}

// --- free helpers -----------------------------------------------------------
fn classify(
    current: &BTreeMap<String, Entry>,
    base: &BTreeMap<String, Entry>,
    cmp: Compare,
) -> HashMap<String, Change> {
    let mut status = HashMap::new();
    let mut keys: Vec<&String> = current.keys().chain(base.keys()).collect();
    keys.sort();
    keys.dedup();
    for path in keys {
        let inc = current.get(path);
        let inb = base.get(path);
        let change = match (inc, inb) {
            (Some(_), None) => Change::Created,
            (None, Some(_)) => Change::Deleted,
            (None, None) => Change::Absent,
            (Some(c), Some(b)) => {
                if same_content(c, b, cmp) {
                    Change::Unchanged
                } else {
                    Change::Modified
                }
            }
        };
        status.insert(path.clone(), change);
    }
    status
}

fn order_key(op: &Op) -> (u8, i64) {
    let rank = match op.action {
        Action::MkdirRemote | Action::MkdirLocal => 0,
        Action::Upload | Action::Download | Action::Conflict => 1,
        Action::RenameRemote | Action::RenameLocal => 1,
        Action::DeleteRemote | Action::DeleteLocal => 2,
        Action::Noop => 1,
    };
    let depth = op.path.matches('/').count() as i64;
    match op.action {
        // mkdir shallow-first; deletes deep-first; transfers shallow-first
        Action::DeleteRemote | Action::DeleteLocal => (rank, -depth),
        _ => (rank, depth),
    }
}

fn parent_rel(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Two entries are the same file moved/renamed: identical size AND a matching
/// strong signal (sha1 if both have it, else mtime). Size alone is too weak.
fn is_rename_match(a: &Entry, b: &Entry) -> bool {
    if a.is_dir || b.is_dir || a.size != b.size {
        return false;
    }
    if let (Some(x), Some(y)) = (&a.sha1, &b.sha1) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.mtime, b.mtime) {
        return x == y;
    }
    false
}

/// Rewrite delete+create pairs with identical content into a single rename op.
/// `from_entry` gives the baseline content of a deleted path; `to_entry` gives
/// the new content of a created path.
fn rewrite_renames<'a>(
    plan: &mut Plan,
    del: Action,
    create: Action,
    rename: Action,
    from_entry: impl Fn(&str) -> Option<&'a Entry>,
    to_entry: impl Fn(&str) -> Option<&'a Entry>,
) {
    let creates: Vec<usize> = plan
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.action == create && !o.is_dir)
        .map(|(i, _)| i)
        .collect();
    let dels: Vec<usize> = plan
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.action == del && !o.is_dir)
        .map(|(i, _)| i)
        .collect();

    let mut used_del: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut renamed: Vec<(usize, String)> = Vec::new(); // (create index, from path)
    for &c in &creates {
        let to = plan.ops[c].path.clone();
        let Some(te) = to_entry(&to) else { continue };
        for &d in &dels {
            if used_del.contains(&d) {
                continue;
            }
            let from = plan.ops[d].path.clone();
            let Some(fe) = from_entry(&from) else {
                continue;
            };
            if is_rename_match(fe, te) {
                used_del.insert(d);
                renamed.push((c, from));
                break;
            }
        }
    }
    if renamed.is_empty() {
        return;
    }
    for (c, from) in renamed {
        let to = plan.ops[c].path.clone();
        plan.ops[c] = Op::rename(rename, from, to, "renamed");
    }
    let mut i = 0usize;
    plan.ops.retain(|_| {
        let keep = !used_del.contains(&i);
        i += 1;
        keep
    });
}

/// Detect local renames (delete-remote + upload) and remote renames
/// (delete-local + download) by matching content, collapsing each into a move.
fn detect_renames(
    plan: &mut Plan,
    base: &BTreeMap<String, Entry>,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) {
    rewrite_renames(
        plan,
        Action::DeleteRemote,
        Action::Upload,
        Action::RenameRemote,
        |p| base.get(p),
        |p| local.get(p),
    );
    rewrite_renames(
        plan,
        Action::DeleteLocal,
        Action::Download,
        Action::RenameLocal,
        |p| base.get(p),
        |p| remote.get(p),
    );
}

fn conflict_name(rel: &str, stamp: &str) -> String {
    let p = Path::new(rel);
    let dir = p.parent().and_then(|d| d.to_str()).unwrap_or("");
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());
    let name = match ext {
        Some(e) => format!("{stem} (conflict {stamp}).{e}"),
        None => format!("{stem} (conflict {stamp})"),
    };
    if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    }
}

fn match_mtime(path: &Path, mtime: Option<i64>) {
    let secs = match mtime {
        Some(s) if s >= 0 => s as u64,
        _ => return,
    };
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
    }
}

fn sha1_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = sha1_smol::Sha1::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.digest().to_string())
}

fn which_gio() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("gio");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::safe_join;
    use std::path::Path;

    #[test]
    fn safe_join_allows_nested_but_rejects_escapes() {
        let root = Path::new("/sync/root");
        assert_eq!(
            safe_join(root, "a/b.txt").unwrap(),
            Path::new("/sync/root/a/b.txt")
        );
        assert_eq!(safe_join(root, "./a").unwrap(), Path::new("/sync/root/a"));
        // A remote-controlled name that tries to climb out is refused.
        assert!(safe_join(root, "../escape").is_err());
        assert!(safe_join(root, "a/../../escape").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
    }
}
