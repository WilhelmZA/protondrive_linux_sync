//! Core data types shared across the engine.

use serde::Serialize;

/// A single node (file or directory) at a path relative to a pair root.
///
/// `path` is always POSIX-style and relative (no leading slash). Directories
/// have `is_dir == true` and their size/mtime/sha1 are ignored for comparison.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Epoch seconds. Local: file mtime. Remote: `claimedModificationTime`
    /// (the preserved original), so the two sides agree.
    pub mtime: Option<i64>,
    /// SHA-1 of the content. Remote side gets it from `claimedDigests.sha1`;
    /// local side is hashed only when `compare == Sha1`.
    pub sha1: Option<String>,
    /// Opaque remote node uid, when known.
    pub remote_id: Option<String>,
}

/// What an upload actually sent: the metadata of the bytes now on the remote.
/// Backends that snapshot the file before transfer report the snapshot, so the
/// baseline can describe the remote copy exactly, however the local file moved
/// meanwhile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uploaded {
    pub size: u64,
    pub mtime: Option<i64>,
    pub sha1: Option<String>,
}

/// How to decide whether two files differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compare {
    Size,
    SizeMtime,
    Sha1,
}

/// Whether two file entries have the same content under `compare`.
///
/// Symmetric and robust: a richer signal is used only when BOTH sides carry it,
/// otherwise it degrades (sha1 -> size+mtime -> size). This avoids false
/// "modified" verdicts when one side lacks an mtime or a hash.
pub fn same_content(a: &Entry, b: &Entry, compare: Compare) -> bool {
    if a.is_dir || b.is_dir {
        return a.is_dir && b.is_dir;
    }
    if compare == Compare::Sha1 {
        if let (Some(x), Some(y)) = (&a.sha1, &b.sha1) {
            return x == y;
        }
    }
    if matches!(compare, Compare::Sha1 | Compare::SizeMtime) {
        if let (Some(x), Some(y)) = (a.mtime, b.mtime) {
            return a.size == b.size && x == y;
        }
    }
    a.size == b.size
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Unchanged,
    Created,
    Modified,
    Deleted,
    Absent,
}

impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Change::Unchanged => "unchanged",
            Change::Created => "created",
            Change::Modified => "modified",
            Change::Deleted => "deleted",
            Change::Absent => "absent",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Noop,
    Upload,
    Download,
    DeleteRemote,
    DeleteLocal,
    MkdirRemote,
    MkdirLocal,
    Conflict,
    /// Rename/move on the remote side (a local rename detected by content).
    RenameRemote,
    /// Rename/move on the local side (a remote rename detected by content).
    RenameLocal,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Noop => "noop",
            Action::Upload => "upload",
            Action::Download => "download",
            Action::DeleteRemote => "delete-remote",
            Action::DeleteLocal => "delete-local",
            Action::MkdirRemote => "mkdir-remote",
            Action::MkdirLocal => "mkdir-local",
            Action::Conflict => "conflict",
            Action::RenameRemote => "rename-remote",
            Action::RenameLocal => "rename-local",
        }
    }
}

/// How the engine settled a file that changed on both sides. Serialises as
/// `kept_both`, `kept_local`, `kept_remote` or `skipped` (the HTTP API).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub enum ConflictOutcome {
    /// Both versions kept; the local one renamed to a "(conflict …)" copy.
    KeptBoth,
    /// The newer local version replaced the remote one.
    KeptLocal,
    /// The newer remote version replaced the local one.
    KeptRemote,
    /// Left as it is until the user chooses.
    Skipped,
}

impl ConflictOutcome {
    /// Stable text form, as stored in `stats.db`.
    pub fn label(self) -> &'static str {
        match self {
            ConflictOutcome::KeptBoth => "kept-both",
            ConflictOutcome::KeptLocal => "kept-local",
            ConflictOutcome::KeptRemote => "kept-remote",
            ConflictOutcome::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "kept-both" => ConflictOutcome::KeptBoth,
            "kept-local" => ConflictOutcome::KeptLocal,
            "kept-remote" => ConflictOutcome::KeptRemote,
            "skipped" => ConflictOutcome::Skipped,
            _ => return None,
        })
    }
}

/// One planned operation against a single path.
#[derive(Clone, Debug)]
pub struct Op {
    pub action: Action,
    pub path: String,
    pub is_dir: bool,
    pub reason: String,
    /// Source path for rename/move actions (`path` is the destination).
    pub from: Option<String>,
    /// Set when this op settles a both-sides-changed file, so `apply` can
    /// report the outcome once the op has actually succeeded.
    pub conflict: Option<ConflictOutcome>,
}

impl Op {
    pub fn new(
        action: Action,
        path: impl Into<String>,
        is_dir: bool,
        reason: impl Into<String>,
    ) -> Self {
        Op {
            action,
            path: path.into(),
            is_dir,
            reason: reason.into(),
            from: None,
            conflict: None,
        }
    }

    /// Like [`Op::new`], tagged with the conflict outcome it carries out.
    pub fn settling(
        action: Action,
        path: impl Into<String>,
        reason: impl Into<String>,
        outcome: ConflictOutcome,
    ) -> Self {
        Op {
            conflict: Some(outcome),
            ..Op::new(action, path, false, reason)
        }
    }

    /// A rename/move op: `from` -> `to` (stored in `path`).
    pub fn rename(
        action: Action,
        from: impl Into<String>,
        to: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Op {
            action,
            path: to.into(),
            is_dir: false,
            reason: reason.into(),
            from: Some(from.into()),
            conflict: None,
        }
    }

    pub fn describe(&self) -> String {
        let kind = if self.is_dir { "dir" } else { "file" };
        let note = if self.reason.is_empty() {
            String::new()
        } else {
            format!("  ({})", self.reason)
        };
        match &self.from {
            Some(f) => format!(
                "{:<14} {:<4} {} -> {}{}",
                self.action.label(),
                kind,
                f,
                self.path,
                note
            ),
            None => format!(
                "{:<14} {:<4} {}{}",
                self.action.label(),
                kind,
                self.path,
                note
            ),
        }
    }
}

/// Result of walking a remote subtree: the entries found, plus the relative
/// folder paths that could not be listed (after retries). A non-empty `failed`
/// means the view is PARTIAL — callers must not treat missing paths as
/// deletions, or a transient listing failure could destroy data.
#[derive(Default)]
pub struct TreeScan {
    pub entries: Vec<(String, Entry)>,
    pub failed: Vec<String>,
    /// The pair's remote base folder itself was not found (distinct from an
    /// empty one). Signals a vanished/moved base so the engine can suppress
    /// deletions rather than read it as "every remote file was deleted".
    pub root_missing: bool,
}

/// One file to fetch, for the concurrent download pool. `mtime` (from the new
/// baseline) is stamped onto the local file after a successful download so it
/// matches the remote, keeping size+mtime comparisons stable next run.
#[derive(Clone)]
pub struct DownloadJob {
    pub rel: String,         // pair-relative path (events, logging)
    pub remote_path: String, // absolute proton path to download
    pub dest_dir: String,    // local parent directory
    pub mtime: Option<i64>,
}

#[derive(Default)]
pub struct Plan {
    pub ops: Vec<Op>,
}

impl Plan {
    pub fn add(&mut self, op: Op) {
        self.ops.push(op);
    }

    pub fn actionable(&self) -> impl Iterator<Item = &Op> {
        self.ops.iter().filter(|o| o.action != Action::Noop)
    }

    pub fn is_empty(&self) -> bool {
        self.actionable().next().is_none()
    }

    /// (action label -> count), for a compact summary line.
    pub fn counts(&self) -> Vec<(&'static str, usize)> {
        let mut acc: Vec<(&'static str, usize)> = Vec::new();
        for op in self.actionable() {
            let lbl = op.action.label();
            if let Some(e) = acc.iter_mut().find(|(k, _)| *k == lbl) {
                e.1 += 1;
            } else {
                acc.push((lbl, 1));
            }
        }
        acc
    }

    pub fn summary(&self) -> String {
        self.counts()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}
