//! neutronsync core library.
//!
//! Bidirectional folder sync for Proton Drive built on Proton's official
//! `proton-drive` CLI. The CLI has no sync engine of its own; this crate adds
//! the three-way-merge engine on top of its one-shot primitives.
//!
//! The library is deliberately separate from the CLI binary so a future GUI can
//! depend on the same core (config, engine, adapter, state).

pub mod auth_signal;
pub mod config;
pub mod datefmt;
pub mod engine;
pub mod events;
pub mod ignore;
pub mod logger;
pub mod models;
pub mod protoncli;
pub mod service;
pub mod state;
pub mod stats;
pub mod trash;
#[cfg(feature = "gui")]
pub mod updater;
pub mod watcher;

/// Starter config written by `neutronsync init` (kept in sync with
/// neutronsync.example.toml).
pub const EXAMPLE_CONFIG: &str = r#"# neutronsync configuration. See README.md.

[cli]
# Path to the official proton-drive binary. Leave as-is to find it on $PATH.
binary = "proton-drive"
# Both upload and download prompt interactively without a conflict strategy,
# which would hang a scheduled run - so we always pass one. cli-drive ≥ 0.8.0
# splits file and folder strategies; upload overwrites with replace, download
# with remove (the 0.8 name for the same overwrite).
upload_flags = ["--file-conflict-strategy", "replace", "--folder-conflict-strategy", "replace"]
download_flags = ["--file-conflict-strategy", "remove", "--folder-conflict-strategy", "remove"]
# The CLI caches directory metadata and serves it stale; a throwaway cache per
# run keeps listings honest. Set false to reuse the CLI's cache (faster, risky).
fresh_cache = true
# The CLI has no recursive listing, so the remote tree is walked one folder per
# process. This many run concurrently (0 = auto from CPU count, capped at 8).
# Higher is faster but Proton rate-limits; back off if you see errors.
# scan_threads = 8
# Optional: where the CLI stores your session ("keychain", "pass", "unsafe_file").
# credentials_store = "keychain"

[options]
# Proton's per-user root. Remote paths in [[pair]] resolve under this unless
# they start with "/".
remote_root = "/my-files"

# Propagate deletions across sides. Recoverable both ways: remote -> Proton
# online trash, local -> desktop trash (see local_delete). A missing baseline
# unions both sides, so a lost sync state never triggers a mass delete.
propagate_deletes = true
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

# Watch mode (`neutronsync watch`): seconds between periodic full rescans (the
# safety net, and the only way remote-side changes are noticed), and the
# debounce window that lets a burst of local file events settle.
poll_interval = 300
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
