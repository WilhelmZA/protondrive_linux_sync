//! Structured events emitted by the engine, for rich frontends.
//!
//! The CLI is happy with text logs, but a GUI wants typed progress: which pair
//! is scanning, how many folders seen, the plan totals, per-operation
//! start/finish (to drive a progress bar), and per-pair completion. The engine
//! emits these to an optional [`EventSink`]; `service::Controller` turns them
//! into observable [`crate::service::AppState`].

/// A single structured event about sync progress.
#[derive(Clone, Debug)]
pub enum SyncEvent {
    /// A pair's reconcile is starting.
    PairStarted { pair: String },
    /// The remote tree scan has begun for a pair.
    ScanStarted { pair: String },
    /// Progress while walking the remote tree (folders listed so far).
    ScanProgress {
        pair: String,
        folders: usize,
        current: String,
    },
    /// The plan has been computed: total actionable ops + a per-action breakdown.
    Planned {
        pair: String,
        total_ops: usize,
        counts: Vec<(String, usize)>,
    },
    /// An operation is about to run.
    OpStarted {
        pair: String,
        action: String,
        path: String,
    },
    /// An operation finished (ok=false means it errored). `error` carries the
    /// reason when it failed, so a frontend and the ops history can show WHY
    /// rather than a bare "an operation failed".
    OpFinished {
        pair: String,
        action: String,
        path: String,
        ok: bool,
        error: Option<String>,
    },
    /// A pair finished reconciling.
    PairFinished {
        pair: String,
        applied: usize,
        errors: usize,
        tracked: usize,
    },
    /// Free-form informational line (mirrors a log line).
    Info { text: String },
    /// An error, optionally attributed to a pair.
    Error { pair: Option<String>, text: String },
    /// The proton-drive session's sign-in state changed. Emitted by the watch
    /// daemon when it detects the session has expired (signed out) or come back,
    /// so a logged-out state surfaces as one clear banner instead of a pile of
    /// per-folder transfer errors.
    Auth { signed_in: bool },
    /// A run refused to delete because it would have removed more files than
    /// the allowance permits (see [`crate::engine::DeleteAllowance`]). Emitted
    /// once per run per pair, after the run. `local` counts files that would
    /// have left this computer, `remote` files that would have left Proton
    /// Drive, and `tracked` the files the pair tracked when the run started.
    DeletesHeld {
        pair: String,
        local: usize,
        remote: usize,
        tracked: usize,
    },
    /// A full run of the pair held nothing back, so its stored hold is gone.
    DeletesReleased { pair: String },
    /// A file that changed on both sides has been settled. `copy` is the
    /// pair-relative path of the renamed local version when both were kept.
    ConflictSettled {
        pair: String,
        path: String,
        outcome: crate::models::ConflictOutcome,
        copy: Option<String>,
    },
}

/// Receiver of [`SyncEvent`]s. Implementations must be cheap and thread-safe:
/// `emit` is called from the engine, including from concurrent scan workers.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &SyncEvent);
}

/// A no-op sink (used when no frontend is observing).
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _ev: &SyncEvent) {}
}
