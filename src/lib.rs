//! neutronsync core library.
//!
//! Bidirectional folder sync for Proton Drive through the `neutronsync-drive`
//! sidecar, a helper built on Proton's official Drive SDK. The sidecar offers
//! one-shot primitives and a change feed; this crate adds the three-way-merge
//! engine and the watch daemon on top.
//!
//! The library is separate from the binaries so the CLI and the GUI share one
//! core (config, engine, backend, state).

pub mod api_client;
pub mod auth_signal;
pub mod backend;
pub mod bookmarks;
pub mod changefeed;
pub mod config;
pub mod datefmt;
pub mod driveapi;
pub mod engine;
pub mod events;
pub mod fs_mode;
pub mod ignore;
pub mod logger;
pub mod models;
pub mod notify_policy;
pub mod process_lock;
pub mod remote;
pub mod retry;
#[cfg(feature = "serve")]
pub mod server;
pub mod service;
pub mod state;
pub mod stats;
pub mod trash;
pub mod trash_browser;
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
# Where the sidecar keeps the Proton session: "keyring" (the desktop's Secret
# Service, the default) or "file" (an encrypted file, session.enc, in the
# state directory, for a container or a headless server). The file is locked
# with a 32-byte key read from session_key_file (base64 or 64 hex characters)
# or from the NEUTRONSYNC_SESSION_KEY environment variable.
# session_store = "keyring"
# session_key_file = "/path/to/session.key"
# TCP address the desktop tray also binds for /api/v1. The tray always binds
# $XDG_RUNTIME_DIR/neutronsync/api.sock (mode 0600) when a runtime directory
# exists. `neutronsync serve` uses --listen (default 127.0.0.1:8090) and that
# socket when XDG_RUNTIME_DIR is set; it does not read api_listen. Anything
# but a loopback address needs an API token.
# api_listen = "127.0.0.1:8090"
# Token file for the tray's TCP listener when NEUTRONSYNC_API_TOKEN is unset.
# At least 32 characters. The unix socket does not use a bearer token.
# api_token_file = "/path/to/api.token"

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

# Speed limits in KB/s (1 KB = 1024 bytes), 0 = no limit. They apply to all
# folders together and take effect on the next sync. The GUI offers presets
# from 256 KB/s to 10 MB/s under Settings > Network.
# upload_limit_kbps = 0
# download_limit_kbps = 0

# Desktop (GUI). Desktop notifications for files changed in two places, a
# folder whose sync problem does not clear, and a Proton sign-out. Window
# colours: "system" follows the desktop's light or dark preference (dark when
# it reports none), or fix it with "light" or "dark". One file manager sidebar
# bookmark per synced folder; NeutronSync only ever removes lines it added.
# notifications = true
# theme = "system"
# file_manager_bookmarks = false

# Add one [[pair]] per folder you want kept in sync. None are created for you;
# add them here or from the GUI. Example (uncomment and edit):
# [[pair]]
# name = "documents"
# local = "~/Documents"
# remote = "Documents"        # -> /my-files/Documents
# exclude = ["APPS"]          # sub-paths to skip; the Proton copy is never touched
"#;
