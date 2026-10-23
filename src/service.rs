//! UI-agnostic controller for a rich frontend.
//!
//! A [`Controller`] owns the config and an observable [`AppState`]. The GUI (or
//! any frontend) reads a cheap `snapshot()` each frame and issues commands
//! (`sync`, `cancel`, `start_watch`, ...). All the slow work runs on background
//! threads; a [`StateSink`] turns engine [`SyncEvent`]s into the observable
//! state (per-pair phase, progress, activity feed), so the UI just renders it.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config, Pair};
use crate::datefmt::now_epoch;
use crate::engine::{held_sides, run_sync_with, HeldCounts};
use crate::events::{EventSink, SyncEvent};
use crate::logger::Logger;
use crate::stats::{ConflictRecord, HeldDeletes, Stats};
use crate::watcher;

const ACTIVITY_CAP: usize = 500;
/// Settled conflicts kept for the desktop notifier when nothing drains them.
const SETTLED_CAP: usize = 1000;
/// Recheck sessions without a window, including when no watcher is enabled.
const ACCOUNT_REPROBE: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub enum Phase {
    Idle,
    Scanning,
    Syncing,
    Synced,
    Error,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

impl Progress {
    /// 0.0..=1.0 for a progress bar (0 when nothing to do).
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct PairState {
    pub name: String,
    pub local: String,
    pub remote: String,
    pub phase: Phase,
    pub progress: Progress,
    pub scanned_folders: usize,
    pub current_op: Option<String>,
    pub last_error: Option<String>,
    pub last_synced: Option<i64>, // epoch seconds
    pub tracked: usize,
}

impl PairState {
    fn from_pair(p: &Pair) -> Self {
        PairState {
            name: p.name.clone(),
            local: p.local.display().to_string(),
            remote: p.remote.clone(),
            phase: Phase::Idle,
            progress: Progress::default(),
            scanned_folders: 0,
            current_op: None,
            last_error: None,
            last_synced: None,
            tracked: 0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct AccountState {
    pub checked: bool,
    /// On the API backend: the sidecar answered. On the CLI backend: a binary was found on PATH.
    pub backend_ready: bool,
    pub signed_in: bool,
    /// The system keyring is locked (or couldn't be read). The Proton session
    /// may still be in it. This is not a sign-out: don't show the login form.
    #[serde(default)]
    pub keyring_locked: bool,
    /// Proton account name from the API probe. Empty on the CLI backend.
    #[serde(default)]
    pub account: String,
    /// Sidecar path or identity on the API backend; binary version/path on the CLI backend.
    /// On probe failure this holds the error text shown on the sign-in page.
    pub version: String,
    /// A login/version check is in flight (drives the Refresh spinner).
    pub checking: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub enum ActivityKind {
    Info,
    Sync,
    Error,
}

/// A per-file operation, for rendering a rich activity table.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct ActivityOp {
    pub action: String, // "upload" | "download" | "delete" | ...
    pub path: String,
    pub pair: String,
    pub ok: bool,
    /// Why a failed op failed. `None` for successes, and for failures recorded
    /// by a build that predates this field.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct ActivityItem {
    pub ts: i64,
    pub kind: ActivityKind,
    pub text: String,
    /// Present for per-file operations; None for info/error/summary lines.
    pub op: Option<ActivityOp>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct AppState {
    pub pairs: Vec<PairState>,
    pub account: AccountState,
    /// The feed is a deque in the engine. On the wire, and in the OpenAPI
    /// schema, it is a JSON array.
    #[cfg_attr(feature = "serve", schema(value_type = Vec<ActivityItem>))]
    pub activity: VecDeque<ActivityItem>,
    /// Operations currently in flight (rendered as live "uploading…" rows).
    pub active: Vec<ActivityOp>,
    pub busy: bool,
    pub watching: bool,
    /// The watch daemon has detected the proton-drive session is signed out.
    /// Surfaced as a distinct "sign in to resume" banner instead of error spam.
    #[serde(default)]
    pub signed_out: bool,
}

impl AppState {
    fn pair_mut(&mut self, name: &str) -> Option<&mut PairState> {
        self.pairs.iter_mut().find(|p| p.name == name)
    }

    fn push(&mut self, kind: ActivityKind, text: impl Into<String>) {
        self.activity.push_back(ActivityItem {
            ts: now_epoch(),
            kind,
            text: text.into(),
            op: None,
        });
        self.trim();
    }

    fn push_op(&mut self, op: ActivityOp) {
        let kind = if op.ok {
            ActivityKind::Sync
        } else {
            ActivityKind::Error
        };
        let text = op_text(&op);
        self.activity.push_back(ActivityItem {
            ts: now_epoch(),
            kind,
            text,
            op: Some(op),
        });
        self.trim();
    }

    fn trim(&mut self) {
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }
}

/// One activity line for an operation: "upload path" when it worked, and
/// "upload path: reason" when it didn't, so the feed says why on its own.
fn op_text(op: &ActivityOp) -> String {
    match &op.error {
        Some(e) if !e.is_empty() => format!("{} {}: {e}", op.action, op.path),
        _ => format!("{} {}", op.action, op.path),
    }
}

/// A conflict the engine settled, queued for the desktop notifier (see
/// [`Controller::take_settled_conflicts`]). Not part of the published state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledConflict {
    pub pair: String,
    pub path: String,
    pub outcome: crate::models::ConflictOutcome,
}

type SettledQueue = Arc<Mutex<Vec<SettledConflict>>>;

#[derive(Default, Clone)]
struct RunCounts {
    upload: u32,
    download: u32,
    delete: u32,
    other: u32,
    failed: u32,
    /// Dry-run plans emit no `OpFinished`; keep the planned breakdown for the summary.
    from_plan: Option<Vec<(String, usize)>>,
}

impl RunCounts {
    fn note(&mut self, action: &str, ok: bool) {
        if !ok {
            self.failed += 1;
            return;
        }
        match action {
            "upload" => self.upload += 1,
            "download" => self.download += 1,
            "delete" | "delete-remote" | "delete-local" | "trash" => self.delete += 1,
            _ => self.other += 1,
        }
    }

    fn recorded_ops(&self) -> u32 {
        self.upload + self.download + self.delete + self.other + self.failed
    }

    /// Prefer per-op tallies; fall back to the last plan when ops never ran (dry run).
    fn for_summary(&self) -> RunCounts {
        if self.recorded_ops() > 0 {
            return self.clone();
        }
        if let Some(plan) = &self.from_plan {
            let mut c = RunCounts::default();
            for (action, n) in plan {
                for _ in 0..*n {
                    c.note(action, true);
                }
            }
            return c;
        }
        self.clone()
    }
}

fn run_summary(pair: &str, c: &RunCounts) -> String {
    let mut parts: Vec<String> = Vec::new();
    if c.upload > 0 {
        parts.push(format!(
            "Uploaded {} file{}",
            c.upload,
            if c.upload == 1 { "" } else { "s" }
        ));
    }
    if c.download > 0 {
        parts.push(format!(
            "Downloaded {} file{}",
            c.download,
            if c.download == 1 { "" } else { "s" }
        ));
    }
    if c.delete > 0 {
        parts.push(format!(
            "Deleted {} file{}",
            c.delete,
            if c.delete == 1 { "" } else { "s" }
        ));
    }
    if c.other > 0 {
        parts.push(format!(
            "Changed {} file{}",
            c.other,
            if c.other == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() && c.failed == 0 {
        return format!("{pair}: Nothing to do");
    }
    if parts.is_empty() {
        return format!("{pair}: Sync finished with errors");
    }
    if c.failed > 0 {
        format!("{pair}: {}, {} failed", parts.join(", "), c.failed)
    } else {
        format!("{pair}: {}", parts.join(", "))
    }
}

/// Turns engine events into observable [`AppState`]. Thread-safe: `emit` may be
/// called concurrently (scan workers), so it just locks briefly.
pub struct StateSink {
    state: Arc<Mutex<AppState>>,
    stats: Option<Arc<Stats>>,
    settled: SettledQueue,
    runs: Arc<Mutex<std::collections::HashMap<String, RunCounts>>>,
}

impl StateSink {
    pub fn new(
        state: Arc<Mutex<AppState>>,
        stats: Option<Arc<Stats>>,
        settled: SettledQueue,
    ) -> Self {
        StateSink {
            state,
            stats,
            settled,
            runs: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
}

impl EventSink for StateSink {
    fn emit(&self, ev: &SyncEvent) {
        let mut s = self.state.lock().unwrap();
        match ev {
            SyncEvent::PairStarted { pair } => {
                self.runs
                    .lock()
                    .unwrap()
                    .insert(pair.clone(), RunCounts::default());
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = Phase::Scanning;
                    p.progress = Progress::default();
                    p.scanned_folders = 0;
                    p.current_op = None;
                    p.last_error = None;
                }
            }
            SyncEvent::ScanStarted { pair } => {
                self.runs.lock().unwrap().entry(pair.clone()).or_default();
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = Phase::Scanning;
                }
            }
            SyncEvent::ScanProgress {
                pair,
                folders,
                current,
            } => {
                let cur = current.clone();
                if let Some(p) = s.pair_mut(pair) {
                    p.scanned_folders = *folders;
                    p.current_op = Some(format!("scanning {cur}"));
                }
            }
            SyncEvent::Planned {
                pair,
                total_ops,
                counts,
            } => {
                self.runs
                    .lock()
                    .unwrap()
                    .entry(pair.clone())
                    .or_default()
                    .from_plan = Some(counts.clone());
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = if *total_ops == 0 {
                        Phase::Synced
                    } else {
                        Phase::Syncing
                    };
                    p.progress = Progress {
                        done: 0,
                        total: *total_ops,
                    };
                    p.current_op = None;
                }
            }
            SyncEvent::OpStarted { pair, action, path } => {
                let label = format!("{action} {path}");
                if let Some(p) = s.pair_mut(pair) {
                    p.current_op = Some(label);
                }
                s.active.push(ActivityOp {
                    action: action.clone(),
                    path: path.clone(),
                    pair: pair.clone(),
                    ok: true,
                    error: None,
                });
            }
            SyncEvent::OpFinished {
                pair,
                action,
                path,
                ok,
                error,
            } => {
                let ok = *ok;
                if let Some(p) = s.pair_mut(pair) {
                    p.progress.done += 1;
                    if !ok {
                        // The real reason, so the banner and the tray tooltip
                        // say what broke instead of "an operation failed".
                        p.last_error = Some(match error {
                            Some(e) if !e.is_empty() => format!("{action} {path}: {e}"),
                            _ => format!("{action} {path} failed"),
                        });
                    }
                }
                s.active
                    .retain(|a| !(a.pair == *pair && a.path == *path && a.action == *action));
                self.runs
                    .lock()
                    .unwrap()
                    .entry(pair.clone())
                    .or_default()
                    .note(action, ok);
                s.push_op(ActivityOp {
                    action: action.clone(),
                    path: path.clone(),
                    pair: pair.clone(),
                    ok,
                    error: error.clone(),
                });
                drop(s);
                if let Some(st) = &self.stats {
                    let _ = st.record_op(pair, action, path, ok, error.as_deref(), now_epoch());
                }
            }
            SyncEvent::PairFinished {
                pair,
                applied,
                errors,
                tracked,
            } => {
                let (_applied, errors, tracked) = (*applied, *errors, *tracked);
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = if errors > 0 {
                        Phase::Error
                    } else {
                        Phase::Synced
                    };
                    p.progress.done = p.progress.total;
                    p.current_op = None;
                    p.tracked = tracked;
                    if errors == 0 {
                        p.last_synced = Some(now_epoch());
                        p.last_error = None;
                    }
                }
                let counts = self
                    .runs
                    .lock()
                    .unwrap()
                    .remove(pair)
                    .map(|c| c.for_summary())
                    .unwrap_or_default();
                let kind = if errors > 0 || counts.failed > 0 {
                    ActivityKind::Error
                } else {
                    ActivityKind::Sync
                };
                s.push(kind, run_summary(pair, &counts));
            }
            SyncEvent::Auth { signed_in } => {
                s.signed_out = !*signed_in;
                // Banner-only: auth state is not a file operation and clutters
                // the activity feed if logged here.
            }
            SyncEvent::Info { text } => s.push(ActivityKind::Info, text.clone()),
            SyncEvent::Error { pair, text } => {
                if let Some(name) = pair {
                    let t = text.clone();
                    if let Some(p) = s.pair_mut(name) {
                        p.phase = Phase::Error;
                        p.last_error = Some(t);
                    }
                }
                s.push(ActivityKind::Error, text.clone());
            }
            SyncEvent::DeletesHeld {
                pair,
                local,
                remote,
                tracked,
            } => {
                // The hold itself lives in stats.db (the engine stored it); the
                // feed gets one plain line so the run's outcome is visible.
                let counts = HeldCounts {
                    local: *local,
                    remote: *remote,
                    tracked: *tracked,
                };
                s.push(
                    ActivityKind::Error,
                    format!(
                        "{pair}: {} deletions held back ({}). Nothing was deleted.",
                        counts.total(),
                        held_sides(&counts)
                    ),
                );
            }
            SyncEvent::DeletesReleased { pair } => {
                s.push(
                    ActivityKind::Info,
                    format!("{pair}: deletions are no longer held back"),
                );
            }
            SyncEvent::ConflictSettled {
                pair,
                path,
                outcome,
                copy,
            } => {
                drop(s);
                {
                    let mut queue = self.settled.lock().unwrap();
                    if queue.len() < SETTLED_CAP {
                        queue.push(SettledConflict {
                            pair: pair.clone(),
                            path: path.clone(),
                            outcome: *outcome,
                        });
                    }
                }
                if let Some(st) = &self.stats {
                    let _ = st.record_conflict(pair, path, *outcome, copy.as_deref(), now_epoch());
                }
            }
        }
    }
}

/// The backend a rich frontend binds to. Cheap to clone-snapshot; commands are
/// non-blocking (they spawn background threads).
pub struct Controller {
    cfg: Mutex<Config>,
    state: Arc<Mutex<AppState>>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    watch_stop: Mutex<Option<Arc<AtomicBool>>>,
    stats: Option<Arc<Stats>>,
    settled: SettledQueue,
    account_probe_at: Mutex<Option<Instant>>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        // A sync state that cannot be read is said so, up front, instead of
        // the app starting as if it had never synced.
        let (stats, stats_error) = match Stats::open(&cfg.state_dir) {
            Ok(st) => (Some(Arc::new(st)), None),
            Err(e) => (None, Some(format!("Sync state could not be read: {e:#}"))),
        };
        // Seed last_synced from the DB so a folder that has synced before isn't
        // shown as "Initializing…" on a fresh process start.
        let pairs: Vec<PairState> = cfg
            .pairs
            .iter()
            .map(|p| {
                let mut ps = PairState::from_pair(p);
                if let Some(st) = &stats {
                    if let Ok(ls) = st.last_synced(&p.name) {
                        ps.last_synced = ls;
                    }
                }
                ps
            })
            .collect();

        // restore the recent activity feed from the DB so it survives restarts
        let mut activity: VecDeque<ActivityItem> = VecDeque::new();
        if let Some(text) = stats_error {
            activity.push_back(ActivityItem {
                ts: now_epoch(),
                kind: ActivityKind::Error,
                text,
                op: None,
            });
        }
        if let Some(st) = &stats {
            let _ = st.prune_ops(1000);
            if let Ok(recs) = st.recent_ops(100) {
                for r in recs.into_iter().rev() {
                    let kind = if r.ok {
                        ActivityKind::Sync
                    } else {
                        ActivityKind::Error
                    };
                    let op = ActivityOp {
                        action: r.action,
                        path: r.path,
                        pair: r.pair,
                        ok: r.ok,
                        error: r.error,
                    };
                    activity.push_back(ActivityItem {
                        ts: r.ts,
                        kind,
                        text: op_text(&op),
                        op: Some(op),
                    });
                }
            }
        }
        let c = Controller {
            cfg: Mutex::new(cfg),
            state: Arc::new(Mutex::new(AppState {
                pairs,
                activity,
                ..Default::default()
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            watch_stop: Mutex::new(None),
            stats,
            settled: Arc::new(Mutex::new(Vec::new())),
            account_probe_at: Mutex::new(None),
        };
        c.refresh_account(false);
        c
    }

    /// Conflicts settled since the last call, oldest first. The desktop
    /// notifier drains this on every tick; nothing else reads it.
    pub fn take_settled_conflicts(&self) -> Vec<SettledConflict> {
        std::mem::take(&mut *self.settled.lock().unwrap())
    }

    /// A cheap clone of the current state - call this each frame.
    pub fn snapshot(&self) -> AppState {
        self.state.lock().unwrap().clone()
    }

    /// Publish a compact live snapshot to `<state_dir>/status.json` so a
    /// separate process (an open GUI window) can display what THIS process is
    /// doing. Used by the headless tray daemon, which owns the watcher/sync
    /// while a window is open: without this the window has no live view of the
    /// daemon's work. Best-effort and atomic (write-temp-then-rename); the
    /// activity feed is trimmed to keep the file small.
    pub fn publish_status(&self) {
        self.poll_account();
        let mut snap = self.snapshot();
        const KEEP: usize = 100;
        if snap.activity.len() > KEEP {
            let drop = snap.activity.len() - KEEP;
            snap.activity.drain(0..drop);
        }
        let dir = self.cfg.lock().unwrap().state_dir.clone();
        let path = dir.join("status.json");
        let tmp = dir.join("status.json.tmp");
        if let Ok(json) = serde_json::to_vec(&snap) {
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Read a live snapshot published by another process (see
    /// [`Controller::publish_status`]). `None` if absent or unparsable.
    pub fn read_status(state_dir: &Path) -> Option<AppState> {
        let data = std::fs::read(state_dir.join("status.json")).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// The current config (the frontend edits a copy, then `commit_config`).
    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    /// Replace the config and refresh the observable pair list (preserving the
    /// phase/progress of pairs whose names are unchanged).
    pub fn commit_config(&self, cfg: Config) {
        *self.cfg.lock().unwrap() = cfg;
        self.rebuild_pairs();
    }

    fn rebuild_pairs(&self) {
        let cfg = self.cfg.lock().unwrap();
        let mut s = self.state.lock().unwrap();
        let old: HashMap<String, PairState> =
            s.pairs.drain(..).map(|p| (p.name.clone(), p)).collect();
        s.pairs = cfg
            .pairs
            .iter()
            .map(|p| {
                let mut ps = old
                    .get(&p.name)
                    .cloned()
                    .unwrap_or_else(|| PairState::from_pair(p));
                ps.local = p.local.display().to_string();
                ps.remote = p.remote.clone();
                ps
            })
            .collect();
    }

    /// Persist the config to `path`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let cfg = self.cfg.lock().unwrap().clone();
        config::save(&cfg, path)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn is_watching(&self) -> bool {
        self.watch_stop.lock().unwrap().is_some()
    }

    /// Reconcile the named pairs (empty = all) on a background thread.
    pub fn sync(&self, names: Vec<String>, dry_run: bool) {
        self.sync_with(names, dry_run, false, false);
    }

    /// "Restore them": run one pair once with both sides merged afresh, so the
    /// files whose deletion was held back come back.
    pub fn restore_held(&self, name: String) {
        self.sync_with(vec![name], false, true, false);
    }

    /// "Delete them": run one pair once with the deletion guard off, for this
    /// run only. The permission is never written to the config.
    pub fn delete_held(&self, name: String) {
        self.sync_with(vec![name], false, false, true);
    }

    fn sync_with(&self, names: Vec<String>, dry_run: bool, resync: bool, allow_mass_delete: bool) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return; // already running
        }
        self.cancel.store(false, Ordering::SeqCst);
        let cfg = self.cfg.lock().unwrap().clone();
        let pairs = select_pairs(&cfg, &names);
        {
            let mut s = self.state.lock().unwrap();
            s.busy = true;
            for p in &mut s.pairs {
                if names.is_empty() || names.iter().any(|n| n == &p.name) {
                    p.phase = Phase::Scanning;
                    p.progress = Progress::default();
                    p.current_op = None;
                    p.last_error = None;
                }
            }
        }
        let state = self.state.clone();
        let cancel = self.cancel.clone();
        let busy = self.busy.clone();
        let stats = self.stats.clone();
        let settled = self.settled.clone();
        thread::spawn(move || {
            let sink = StateSink::new(state.clone(), stats, settled);
            let log = Logger::new(&cfg.state_dir.join("logs"), false, false);
            run_sync_with(
                &cfg,
                &pairs,
                dry_run,
                resync,
                allow_mass_delete,
                &log,
                Some(&sink),
                Some(&cancel),
            );
            busy.store(false, Ordering::SeqCst);
            let mut g = state.lock().unwrap();
            g.busy = false;
            g.active.clear();
        });
    }

    /// Request cancellation of the current sync (stops before the next op).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Pairs whose deletions are held back, read from `stats.db` so the tray,
    /// the window and the CLI all see the same holds.
    pub fn held_deletes(&self) -> Vec<HeldDeletes> {
        self.stats
            .as_ref()
            .and_then(|st| st.held_deletes().ok())
            .unwrap_or_default()
    }

    /// Conflicts waiting to be looked at, newest first.
    pub fn conflicts(&self) -> Vec<ConflictRecord> {
        self.stats
            .as_ref()
            .and_then(|st| st.conflicts().ok())
            .unwrap_or_default()
    }

    pub fn dismiss_conflict(&self, id: i64) {
        if let Some(st) = &self.stats {
            let _ = st.dismiss_conflict(id);
        }
    }

    pub fn dismiss_all_conflicts(&self) {
        if let Some(st) = &self.stats {
            let _ = st.dismiss_all_conflicts();
        }
    }

    /// Start the watch daemon for the named pairs (empty = all).
    pub fn start_watch(&self, names: Vec<String>) {
        let mut guard = self.watch_stop.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let cfg = self.cfg.lock().unwrap().clone();
        let pairs = select_pairs(&cfg, &names);
        let stop = Arc::new(AtomicBool::new(false));
        *guard = Some(stop.clone());
        self.state.lock().unwrap().watching = true;
        let state = self.state.clone();
        let stats = self.stats.clone();
        let settled = self.settled.clone();
        thread::spawn(move || {
            let sink = StateSink::new(state.clone(), stats, settled);
            let log = Logger::new(&cfg.state_dir.join("logs"), false, false);
            if let Err(e) = watcher::watch_with(&cfg, pairs, &log, &stop, Some(&sink)) {
                let mut s = state.lock().unwrap();
                s.push(ActivityKind::Error, format!("watch: {e}"));
            }
            let mut g = state.lock().unwrap();
            g.watching = false;
            g.active.clear();
        });
    }

    pub fn stop_watch(&self) {
        if let Some(stop) = self.watch_stop.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
        self.state.lock().unwrap().watching = false;
    }

    /// Forget a folder's stored data (baseline, per-file state, stats, activity)
    /// so a later folder of the same name can't inherit stale state. The caller
    /// removes it from the config separately.
    pub fn forget_pair(&self, name: &str) {
        if let Some(st) = &self.stats {
            let _ = st.forget_pair(name);
        }
        let mut s = self.state.lock().unwrap();
        s.pairs.retain(|p| p.name != name);
    }

    /// Wipe all NeutronSync metadata — baseline, per-file state, hot-folder
    /// stats and the activity feed — and stop watching. Local files are NOT
    /// touched. The caller clears the folder list from the config.
    pub fn reset_data(&self) {
        self.stop_watch();
        if let Some(st) = &self.stats {
            let _ = st.wipe();
        }
        let mut s = self.state.lock().unwrap();
        s.pairs.clear();
        s.activity.clear();
        s.active.clear();
        s.watching = false;
    }

    /// Refresh the account panel (sidecar reachable? signed in? which helper) on a
    /// background thread. When `signal_watch` is true and the probe confirms a
    /// session, nudge the tray daemon's watcher to resume immediately.
    pub fn refresh_account(&self, signal_watch: bool) {
        self.start_account_probe(signal_watch, false);
    }

    /// Backend heartbeat for the tray and server publishers. A watcher probes
    /// its own session but does not update the controller's account snapshot.
    /// Keep this independent of window frames, requests, and auto-sync policy.
    pub fn poll_account(&self) {
        self.start_account_probe(false, true);
    }

    fn start_account_probe(&self, signal_watch: bool, only_if_due: bool) {
        let cfg = self.cfg.lock().unwrap().clone();
        let state = self.state.clone();
        {
            let mut last = self.account_probe_at.lock().unwrap();
            let mut s = state.lock().unwrap();
            // Manual requests and both publishers share one in-flight probe.
            if s.account.checking
                || (only_if_due && last.is_some_and(|t| t.elapsed() < ACCOUNT_REPROBE))
            {
                return;
            }
            s.account.checking = true;
            *last = Some(Instant::now());
        }
        thread::spawn(move || {
            let result = crate::driveapi::DriveApi::new(&cfg).and_then(|api| api.status());
            let (usable, signed_in, keyring_locked, account, version) = match result {
                Ok(status) => (
                    true,
                    status["signed_in"] == true,
                    status["keyring_locked"] == true,
                    status["account"].as_str().unwrap_or("").to_string(),
                    crate::driveapi::resolve_sidecar(&cfg)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "neutronsync-drive".into()),
                ),
                Err(error) => (false, false, false, String::new(), error.to_string()),
            };
            let mut s = state.lock().unwrap();
            let recovered = s.account.checked && !s.account.signed_in && signed_in;
            s.account = AccountState {
                checked: true,
                backend_ready: usable,
                signed_in,
                keyring_locked,
                account,
                version,
                checking: false,
            };
            if usable {
                // A locked keyring is not a sign-out. Sync waits; the session stays put.
                s.signed_out = !signed_in && !keyring_locked;
            }
            drop(s);
            if signed_in && (signal_watch || recovered) {
                crate::auth_signal::signal(&cfg.state_dir);
            }
        });
    }
}

fn select_pairs(cfg: &Config, names: &[String]) -> Vec<Pair> {
    if names.is_empty() {
        cfg.pairs.clone()
    } else {
        cfg.pairs
            .iter()
            .filter(|p| names.iter().any(|n| n == &p.name))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(names: &[&str]) -> Arc<Mutex<AppState>> {
        let pairs = names
            .iter()
            .map(|n| PairState {
                name: n.to_string(),
                local: String::new(),
                remote: String::new(),
                phase: Phase::Idle,
                progress: Progress::default(),
                scanned_folders: 0,
                current_op: None,
                last_error: None,
                last_synced: None,
                tracked: 0,
            })
            .collect();
        Arc::new(Mutex::new(AppState {
            pairs,
            ..Default::default()
        }))
    }

    #[test]
    fn sink_maps_events_to_state() {
        let st = state_with(&["docs"]);
        let sink = StateSink::new(st.clone(), None, Default::default());
        sink.emit(&SyncEvent::PairStarted {
            pair: "docs".into(),
        });
        sink.emit(&SyncEvent::Planned {
            pair: "docs".into(),
            total_ops: 2,
            counts: vec![("upload".into(), 2)],
        });
        assert_eq!(st.lock().unwrap().pairs[0].phase, Phase::Syncing);
        sink.emit(&SyncEvent::OpFinished {
            pair: "docs".into(),
            action: "upload".into(),
            path: "a".into(),
            ok: true,
            error: None,
        });
        assert_eq!(st.lock().unwrap().pairs[0].progress.done, 1);

        // A failure carries its reason all the way to the pair banner and the
        // activity line, rather than collapsing to "an operation failed".
        sink.emit(&SyncEvent::OpFinished {
            pair: "docs".into(),
            action: "upload".into(),
            path: "b".into(),
            ok: false,
            error: Some("No paths matched: /x/b".into()),
        });
        {
            let s = st.lock().unwrap();
            assert_eq!(
                s.pairs[0].last_error.as_deref(),
                Some("upload b: No paths matched: /x/b")
            );
            let last = s.activity.back().expect("an activity line");
            assert_eq!(last.text, "upload b: No paths matched: /x/b");
            assert_eq!(
                last.op.as_ref().and_then(|o| o.error.as_deref()),
                Some("No paths matched: /x/b")
            );
        }
        sink.emit(&SyncEvent::PairFinished {
            pair: "docs".into(),
            applied: 2,
            errors: 0,
            tracked: 5,
        });
        let s = st.lock().unwrap();
        assert_eq!(s.pairs[0].phase, Phase::Synced);
        assert_eq!(s.pairs[0].tracked, 5);
        assert!(s.pairs[0].last_synced.is_some());
        assert!(s
            .activity
            .iter()
            .any(|a| a.text.contains("Uploaded 1 file")));
    }

    #[test]
    fn dry_run_summary_uses_planned_counts() {
        let st = state_with(&["docs"]);
        let sink = StateSink::new(st.clone(), None, Default::default());
        sink.emit(&SyncEvent::PairStarted {
            pair: "docs".into(),
        });
        sink.emit(&SyncEvent::Planned {
            pair: "docs".into(),
            total_ops: 1,
            counts: vec![("upload".into(), 1)],
        });
        sink.emit(&SyncEvent::PairFinished {
            pair: "docs".into(),
            applied: 0,
            errors: 0,
            tracked: 1,
        });
        let s = st.lock().unwrap();
        assert!(
            s.activity
                .iter()
                .any(|a| a.text.contains("Uploaded 1 file")),
            "{:?}",
            s.activity.iter().map(|a| &a.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn watcher_run_summary_counts_ops_without_pair_started() {
        let st = state_with(&["docs"]);
        let sink = StateSink::new(st.clone(), None, Default::default());
        sink.emit(&SyncEvent::ScanStarted {
            pair: "docs".into(),
        });
        sink.emit(&SyncEvent::OpFinished {
            pair: "docs".into(),
            action: "upload".into(),
            path: "a.txt".into(),
            ok: true,
            error: None,
        });
        sink.emit(&SyncEvent::PairFinished {
            pair: "docs".into(),
            applied: 1,
            errors: 0,
            tracked: 1,
        });
        let s = st.lock().unwrap();
        assert!(s
            .activity
            .iter()
            .any(|a| a.text.contains("Uploaded 1 file")));
    }

    #[test]
    fn sink_records_holds_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::open(dir.path()).unwrap());
        let st = state_with(&["docs"]);
        let settled: SettledQueue = Default::default();
        let sink = StateSink::new(st.clone(), Some(stats.clone()), settled.clone());
        sink.emit(&SyncEvent::DeletesHeld {
            pair: "docs".into(),
            local: 0,
            remote: 250,
            tracked: 300,
        });
        {
            let s = st.lock().unwrap();
            let last = s.activity.back().unwrap();
            assert_eq!(last.kind, ActivityKind::Error);
            assert_eq!(
                last.text,
                "docs: 250 deletions held back (250 removed on this computer). Nothing was deleted."
            );
        }
        for _ in 0..3 {
            sink.emit(&SyncEvent::ConflictSettled {
                pair: "docs".into(),
                path: "a.txt".into(),
                outcome: crate::models::ConflictOutcome::Skipped,
                copy: None,
            });
        }
        sink.emit(&SyncEvent::ConflictSettled {
            pair: "docs".into(),
            path: "b.txt".into(),
            outcome: crate::models::ConflictOutcome::KeptBoth,
            copy: Some("b (conflict 1).txt".into()),
        });
        let rows = stats.conflicts().unwrap();
        assert_eq!(rows.len(), 2);
        let skipped = rows.iter().find(|r| r.path == "a.txt").unwrap();
        assert_eq!(skipped.seen, 3);
        let both = rows.iter().find(|r| r.path == "b.txt").unwrap();
        assert_eq!(both.copy.as_deref(), Some("b (conflict 1).txt"));
        // Every settled conflict is queued once for the desktop notifier.
        let queued = std::mem::take(&mut *settled.lock().unwrap());
        assert_eq!(queued.len(), 4);
        assert_eq!(queued[3].path, "b.txt");
        assert_eq!(queued[3].outcome, crate::models::ConflictOutcome::KeptBoth);
    }

    #[test]
    fn controller_hands_out_settled_conflicts_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config {
            sidecar: Some(dir.path().join("no-such-sidecar")),
            scan_threads: 0,
            download_threads: 0,
            remote_root: "/my-files".into(),
            propagate_deletes: false,
            max_delete_files: 200,
            max_delete_percent: 30,
            auto_sync: false,
            run_in_tray: false,
            x11_compat: false,
            local_delete: crate::config::LocalDelete::Trash,
            conflict: crate::config::ConflictPolicy::KeepBoth,
            compare: crate::models::Compare::SizeMtime,
            full_walk_interval: 86400,
            scan_interval_secs: 120,
            debounce_secs: 2,
            update_channel: crate::config::UpdateChannel::Stable,
            check_on_launch: false,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            notifications: true,
            theme: crate::config::Theme::System,
            file_manager_bookmarks: false,
            session_store: Default::default(),
            session_key_file: None,
            api_listen: None,
            api_token_file: None,
            state_dir: dir.path().to_path_buf(),
            pairs: vec![],
            source_path: None,
        };
        cfg.pairs.clear();
        let ctrl = Controller::new(cfg);
        assert!(ctrl.take_settled_conflicts().is_empty());
        let sink = StateSink::new(ctrl.state.clone(), None, ctrl.settled.clone());
        sink.emit(&SyncEvent::ConflictSettled {
            pair: "docs".into(),
            path: "a.txt".into(),
            outcome: crate::models::ConflictOutcome::KeptBoth,
            copy: Some("a (conflict 1).txt".into()),
        });
        let first = ctrl.take_settled_conflicts();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pair, "docs");
        assert!(ctrl.take_settled_conflicts().is_empty());
    }

    #[test]
    fn unreadable_sync_state_is_reported_in_the_feed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stats.db"), b"garbage, not a database").unwrap();
        let mut cfg = crate::config::Config {
            sidecar: Some(dir.path().join("no-such-sidecar")),
            scan_threads: 0,
            download_threads: 0,
            remote_root: "/my-files".into(),
            propagate_deletes: false,
            max_delete_files: 200,
            max_delete_percent: 30,
            auto_sync: false,
            run_in_tray: false,
            x11_compat: false,
            local_delete: crate::config::LocalDelete::Trash,
            conflict: crate::config::ConflictPolicy::KeepBoth,
            compare: crate::models::Compare::SizeMtime,
            full_walk_interval: 86400,
            scan_interval_secs: 120,
            debounce_secs: 2,
            update_channel: crate::config::UpdateChannel::Stable,
            check_on_launch: false,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            notifications: true,
            theme: crate::config::Theme::System,
            file_manager_bookmarks: false,
            session_store: Default::default(),
            session_key_file: None,
            api_listen: None,
            api_token_file: None,
            state_dir: dir.path().to_path_buf(),
            pairs: vec![],
            source_path: None,
        };
        cfg.pairs.clear();
        let ctrl = Controller::new(cfg);
        let snap = ctrl.snapshot();
        let line = snap
            .activity
            .iter()
            .find(|a| a.text.starts_with("Sync state could not be read: "))
            .expect("an error line about the sync state");
        assert_eq!(line.kind, ActivityKind::Error);
        assert!(ctrl.held_deletes().is_empty());
        assert!(ctrl.conflicts().is_empty());
    }

    #[test]
    fn progress_fraction() {
        let p = Progress { done: 1, total: 4 };
        assert!((p.fraction() - 0.25).abs() < 1e-6);
        assert_eq!(Progress { done: 0, total: 0 }.fraction(), 0.0);
    }
}
