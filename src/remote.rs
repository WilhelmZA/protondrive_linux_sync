//! The `Remote` trait the engine syncs against, plus the helpers every backend
//! shares. The one production backend is [`crate::driveapi::DriveApi`]; tests
//! use in-memory fakes.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::models::{DownloadJob, Entry, TreeScan, Uploaded};

/// The remote operations the engine needs. A fake implements this in tests.
pub trait Remote {
    fn change_feed(&self) -> Option<&dyn crate::changefeed::RemoteChangeFeed> {
        None
    }
    /// Forces planning only, including local operations and state persistence.
    fn read_only(&self) -> bool {
        false
    }

    fn list_dir(&self, remote_path: &str) -> Result<Vec<Entry>>;

    /// Like [`list_dir`] but distinguishes a genuine "not found" from an empty
    /// listing (a transport error is still `Err`). The default treats every
    /// successful listing as `Listed` — backends with no not-found signal never
    /// report `NotFound`. `DriveApi` overrides it. Callers that would delete on
    /// the strength of an empty listing should use this and suppress deletes on
    /// `NotFound`, so a misclassified transient error can't drive a deletion.
    fn list_dir_probe(&self, remote_path: &str) -> Result<ListOutcome> {
        Ok(ListOutcome::Listed(self.list_dir(remote_path)?))
    }

    fn create_folder(&self, parent: &str, name: &str) -> Result<()>;
    /// Upload a local file into `remote_parent`. Returns what was actually
    /// sent when the backend knows (it snapshots the file first); `None` means
    /// the engine must describe the upload from its own stat of the file.
    fn upload(&self, local_path: &str, remote_parent: &str) -> Result<Option<Uploaded>>;
    fn download(&self, remote_path: &str, local_dest: &str) -> Result<()>;
    fn trash(&self, remote_path: &str) -> Result<()>;

    /// Rename or move a remote node from absolute `from` to absolute `to`.
    /// Default: unsupported (the engine then falls back to delete + re-upload).
    fn rename(&self, _from: &str, _to: &str) -> Result<()> {
        anyhow::bail!("rename/move not supported by this backend")
    }

    /// List an entire subtree. `progress` is called with each remote path as it
    /// is listed. `exclude` is called with each child's relative path; a subtree
    /// it accepts is skipped entirely — never listed, never descended — so
    /// excluded folders cost nothing and can't contaminate the scan. The default
    /// is a sequential recursive walk; `DriveApi` overrides it with the sidecar's
    /// own walk. A folder that can't be listed is recorded in
    /// `TreeScan::failed` and the walk continues (partial result).
    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        let mut out = Vec::new();
        let mut failed = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let here = crate::config::remote_join(base, &rel);
            progress(&here);
            let entries = match self.list_dir(&here) {
                Ok(e) => e,
                Err(_) => {
                    failed.push(rel);
                    continue;
                }
            };
            for e in entries {
                let child_rel = if rel.is_empty() {
                    e.path.clone()
                } else {
                    format!("{rel}/{}", e.path)
                };
                if exclude(&child_rel) {
                    continue;
                }
                let is_dir = e.is_dir;
                out.push((
                    child_rel.clone(),
                    Entry {
                        path: child_rel.clone(),
                        ..e
                    },
                ));
                if is_dir {
                    stack.push(child_rel);
                }
            }
        }
        Ok(TreeScan {
            entries: out,
            failed,
            // The sequential fallback lists via `list_dir`, which can't tell a
            // missing base from an empty one; only the concurrent walk detects
            // it. Report "present" here.
            root_missing: false,
        })
    }

    /// Download many files. Default = sequential; `DriveApi` overrides with a
    /// bounded worker pool. `on_start`/`on_done` fire per job (their
    /// sinks must be thread-safe). Stops early if `cancel` is set. `on_done`
    /// receives Ok or the typed error, so the engine can tell a temporary
    /// failure from a permanent one.
    #[allow(clippy::type_complexity)]
    fn download_many(
        &self,
        jobs: &[DownloadJob],
        _threads: usize,
        cancel: &AtomicBool,
        on_start: &(dyn Fn(&DownloadJob) + Sync),
        on_done: &(dyn Fn(&DownloadJob, Result<()>) + Sync),
    ) {
        for j in jobs {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            on_start(j);
            let r = self.download(&j.remote_path, &j.dest_dir);
            on_done(j, r);
        }
    }
}

/// Outcome of listing one remote directory: either it listed (possibly empty)
/// or the backend reported it as not found — distinct from a transport error, which
/// is returned as `Err` so it can be retried/treated as a partial scan.
pub enum ListOutcome {
    Listed(Vec<Entry>),
    NotFound,
}

/// Whether a backend error string is the "not authenticated" signal — the
/// session has expired or the user is logged out — as opposed to a genuine
/// transfer failure (a bad path, a network blip, a rate limit). Callers use this
/// to surface a single clear "signed out, sign in to resume" state instead of
/// treating a logout as a pile of per-file errors. Kept conservative so an
/// unrelated 4xx never trips it.
pub fn is_not_logged_in(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("login first")
        || s.contains("need to login")
        || s.contains("need to log in")
        || s.contains("not logged in")
        || s.contains("please login")
        || s.contains("please log in")
        || s.contains("no active session")
        // The keyring/secret-service is unreachable (locked, headless, no D-Bus):
        // there is no session, which for us means "not signed in", NOT "node
        // not found" (the message ends in "No such file or directory").
        || s.contains("failed to load session")
}

/// Whether a remote node name is a single, safe local path component: not
/// empty, not "." or "..", and free of path separators or NUL. Rejecting the
/// rest stops a hostile or shared node name from escaping a pair's local root.
pub(crate) fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::{is_not_logged_in, is_safe_component};

    #[test]
    fn detects_not_logged_in_signal() {
        assert!(is_not_logged_in("You need to login first"));
        assert!(is_not_logged_in("Error: not logged in"));
        assert!(is_not_logged_in("Please log in to continue"));
        assert!(is_not_logged_in("no active session"));
        assert!(is_not_logged_in(
            "Failed to load session from secrets (ensure you have secrets \
             available, read the README for more information): Could not \
             connect: No such file or directory (code: 1)"
        ));
        // Genuine transfer failures must NOT read as a logout.
        assert!(!is_not_logged_in("Node not found"));
        assert!(!is_not_logged_in("upload /x failed: connection reset"));
        assert!(!is_not_logged_in("rate limited, try again"));
        assert!(!is_not_logged_in(""));
    }

    #[test]
    fn rejects_unsafe_node_names() {
        assert!(is_safe_component("file.txt"));
        assert!(is_safe_component("a normal name.pdf"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b")); // embedded separator
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("x\0y")); // NUL
    }
}
