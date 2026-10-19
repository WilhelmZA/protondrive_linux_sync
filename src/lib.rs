//! neutronsync core library.
//!
//! Bidirectional folder sync for Proton Drive through the `neutronsync-drive`
//! sidecar, a helper built on Proton's official Drive SDK. The sidecar offers
//! one-shot primitives and a change feed; this crate adds the three-way-merge
//! engine and the watch daemon on top.
//!
//! The library is separate from the binaries so the CLI and the GUI share one
//! core (config, engine, backend, state).

pub mod auth_signal;
pub mod backend;
pub mod changefeed;
pub mod config;
pub mod datefmt;
pub mod driveapi;
pub mod engine;
pub mod events;
pub mod ignore;
pub mod logger;
pub mod models;
pub mod process_lock;
pub mod remote;
pub mod service;
pub mod state;
pub mod stats;
pub mod trash;
pub mod tray_policy;
#[cfg(feature = "gui")]
pub mod updater;
pub mod watcher;

/// Starter config written by `neutronsync init` (kept in sync with
/// neutronsync.example.toml).
pub const EXAMPLE_CONFIG: &str = r#"# neutronsync configuration. See README.md.

[cli]
# Optional sidecar path; default: next to neutronsync, then on PATH.
# sidecar = "/path/to/neutronsync-drive"
# Concurrent folder reconciles during a full walk (0 = auto from CPU count,
# capped at 8). Higher is faster but Proton rate-limits; back off on errors.
# scan_threads = 8
# Concurrent file downloads (0 = auto, 4).
# download_threads = 4

[options]
# Proton's per-user root. Remote paths in [[pair]] resolve under this unless
# they start with "/".
remote_root = "/my-files"

# Propagate deletions across sides. Recoverable both ways: remote -> Proton
# online trash, local -> desktop trash (see local_delete). A missing baseline
# unions both sides, so a lost sync state never triggers a mass delete.
propagate_deletes = true
# How many files one sync may delete from one folder pair before it stops and
# asks: at most max_delete_files, and at most max_delete_percent of the files
# the pair tracks (never fewer than 25, so a small folder can be emptied). A
# run over that holds every deletion back, keeps the files on both sides, and
# the Activity page (or `neutronsync sync`) tells you how to answer. Set one
# option to 0 to drop that limit; both at 0 switch the check off.
# max_delete_files = 200
# max_delete_percent = 30
# How a propagated deletion removes the LOCAL copy:
#   trash   send it to the freedesktop trash via gio (recoverable)
#   remove  unlink permanently (no recovery)
local_delete = "trash"

# Conflict policy when a file changed on BOTH sides since the last sync:
#   keep-both  keep both versions (local copy is renamed) - never loses data
#   newer      keep whichever has the newer mtime (falls back to keep-both)
#   skip       leave both untouched and warn
conflict = "keep-both"

# How to detect a changed file: "size", "size+mtime", or "sha1".
compare = "size+mtime"

# Watch mode (`neutronsync watch`): seconds between safety-net full walks of
# every folder (default one day), and the debounce window that lets a burst of
# local file events settle.
# full_walk_interval = 86400
debounce = 2

# In-app updater (GUI): which release channel to follow, and whether to check
# for a new version on launch (notify only - it never installs anything).
# update_channel = "stable"    # or "prerelease"
# check_on_launch = false

# Add one [[pair]] per folder you want kept in sync. None are created for you;
# add them here or from the GUI. Example (uncomment and edit):
# [[pair]]
# name = "documents"
# local = "~/Documents"
# remote = "Documents"        # -> /my-files/Documents
# exclude = ["APPS"]          # sub-paths to skip; the Proton copy is never touched
"#;
