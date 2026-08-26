# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/), and the project aims to follow [Semantic Versioning](https://semver.org/). New work lands under `[Unreleased]` and moves to a dated version when it ships.

## [Unreleased]

### Fixed
- cli-drive 0.8.0 changed the `filesystem list -j` shape: `activeRevision` is no longer wrapped in `{ok,value}`, so NeutronSync missed `claimedSize` / `claimedModificationTime` and fell back to the encrypted `totalStorageSize`. Every file then looked modified and the watcher re-uploaded (and conflict-copied) the tree. Both JSON shapes are accepted now, and encrypted size is never used as content size.
- After that thrash, same-size files with drifted mtimes were still treated as dual edits and keep-both multiplied conflict copies. Same size (unless both sha1s disagree) now refreshes the baseline instead of conflicting, and `*(conflict *)*` names are ignored so conflict copies cannot re-enter sync.

### Changed
- Targets Proton Drive CLI **0.8.0**. That release dropped `--conflict-strategy`; upload/download now use separate `--file-conflict-strategy` / `--folder-conflict-strategy` flags (download overwrite is `remove`, not `replace`). Defaults and the example config were updated, and configs that still have the old `-c` / `--conflict-strategy` form are rewritten on load so existing installs keep working after you upgrade the CLI.

### Added
- `scripts/promote.sh X.Y.Z` promotes a pre-release to stable, and `--demote` puts it back. GitHub carries only two badges, "Latest" on a single release and "Pre-release" on each flagged one, so a stable release that is not the newest stable shows nothing at all. The script therefore clears the pre-release flag, moves the "Latest" badge, and marks the title `NeutronSync vX.Y.Z (stable)` so the distinction stays visible down the list.
- `scripts/release.sh` cuts a release in one command: it bumps the version in `Cargo.toml` and `Cargo.lock`, closes the changelog's `[Unreleased]` section as `## [x.y.z] - <date>`, commits, and creates the signed tag. It refuses to run on a dirty tree, on a version that already exists, or on an empty `[Unreleased]`, and it stops before pushing, so nothing reaches GitHub without a deliberate `git push`. `--dry-run` prints the release body it would publish.

### Changed
- A release fails fast rather than shipping notes nobody can read. The workflow now checks, before the eight-minute build, that the tag matches the version in `Cargo.toml` and that `CHANGELOG.md` has a non-empty section for it. Previously a missing section only warned, and the release shipped with a bare "see CHANGELOG" link as its body.
- Every tag ships as a pre-release, and promotion to stable is a separate decision. 0.3.3 made a plain tag a stable release, which meant a build was declared stable at the moment it was cut, before it had run anywhere. The release workflow now always publishes as a pre-release and never moves the "Latest" badge; a build is promoted once it has proven itself, with `gh release edit vX.Y.Z --prerelease=false --latest`. The updater's `prerelease` channel still sees every tag as it lands, and its `stable` channel only ever offers a promoted one.
- Release notes on GitHub now carry the changelog section for each version. Earlier releases had GitHub's generated body, a bare compare link, so the release page said nothing about what changed. The 0.1.1 through 0.3.2 notes were rewritten from this file, and titles read `NeutronSync vX.Y.Z`.

## [0.3.3] - 2026-07-28

### Added
- The app now installs its own updates. "Check for updates" becomes "Download and install", which fetches the release asset matching how this copy was installed (`.deb` or `.rpm`), shows download progress, verifies it, installs it through the system package manager via `pkexec` (one password prompt), and offers to restart. Installing through the package manager is deliberate: overwriting the binaries directly leaves dpkg reporting whatever version was last packaged, which is how a machine ends up showing 0.1.0 in App Center while running 0.3.1. A copy no package manager owns is downloaded and left for you to install. Nothing is ever downloaded or installed without you pressing that button: there is no timer, no install on launch, and a check never installs.
- Downloads are verified before anything is installed. The SHA-256 the release API publishes for the asset is checked against the file, and a mismatch discards the download and installs nothing. Verification fails closed: an asset with no published checksum is refused rather than trusted.
- Failed operations record why they failed. The `error` column on the operations history holds the reason, so "what is broken and why" is one query instead of cross-referencing the text log by timestamp.

### Fixed
- The GUI no longer reports "an operation failed". The pair banner, the activity feed, and the stored history all carry the real reason (for example `upload …: No paths matched: …`), which was previously discarded the moment it reached the frontend and survived only in `sync.log`.
- `sync.log` is rotated. It grew without bound (19 MB in five days on a busy tree); it now rolls over at 8 MB keeping one previous generation, so the logs cost at most 16 MB.
- Log timestamps are readable. Lines were stamped with raw epoch seconds (`1785138207`), in the one file a human actually reads; they now read `2026-07-28 07:43:17Z`, with the zone marked because the reader is probably not on UTC.

### Changed
- Releases are published, not drafted. A tag push produced a draft, and the updater skips drafts, so every release needed a manual publish before the app could see it. Pushing a tag is now the decision to release. A plain tag (`v0.3.3`) is a stable release; a tag with a suffix (`v0.4.0-rc1`) is flagged as a pre-release.
- Release notes come from `CHANGELOG.md` instead of GitHub's generated commit list, so a release says what changed and why rather than repeating commit subjects.

## [0.3.2] - 2026-07-28

### Fixed
- Files whose names contain `[`, `*`, `?`, `{` or `\` now transfer. The `proton-drive` CLI expands every local path argument as a glob, so a real file called `[RTA06]_Change_of_bond_contributors_....pdf` was read as a one-character class, matched nothing, and failed with "No paths matched" on every retry forever. Local paths are now quoted (each metacharacter wrapped in a single-character class) before being handed to the CLI, for uploads and for the download destination folder, which the CLI globs as well.

## [0.3.1] - 2026-07-27

### Fixed
- First-run onboarding actually appears. The sign-in page was gated on the `proton-drive` binary being present, so a machine without the CLI (the case that needs onboarding most) went straight to the main window with no hint that anything was missing. The page now shows unless a working, signed-in CLI is confirmed.
- A signed-out session is no longer misread as an empty Proton Drive. When the system keyring is unreachable (locked, headless session, no D-Bus), the CLI fails with a message ending in "No such file or directory", which the listing classifier treated as "folder not found". The app then reported "Signed in" while every sync errored, and the watcher's signed-out detection never fired. Session-load failures are now classified as "not signed in": the GUI shows the sign-in page and the watch daemon pauses until the session is back.
- An idle window no longer shows a stale frame while the startup account probe runs; the result could otherwise go unseen until the next click.
- The GNOME dock icon no longer shows Proton's logo. Old builds installed Proton Drive's own artwork as the scalable icon and newer builds never replaced it; the desktop integration now installs the NeutronSync atom mark as both the raster and scalable icon (healing machines that ran an old build), and the packages ship the scalable icon too.

### Changed
- The sign-in page was redesigned. Without the CLI it now walks through two concrete steps (a link to Proton's download page and a copyable `proton-drive --version` check) with a single "Check again" action; with the CLI present it offers "Sign in with Proton" and shows the probe result instead of a raw status string.

## [0.3.0] - 2026-07-25

### Added
- Built-in junk-file ignore. Files that should never be synced are now skipped automatically, wherever they appear: office and editor lock/temp files (`~$…`, `.~lock.*`, `*.tmp`, `*.laccdb`, vim/emacs swap and backup files), partial downloads (`*.part`, `*.crdownload`, `*.filepart`), OS metadata (`.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, `$RECYCLE.BIN`), and the private state folders of other sync clients (`.sync`, `.stfolder`, `.stversions`, `.dropbox*`, `*.unison`, …). Patterns match on any path component, so a junk *directory* is pruned whole. It reuses the exclude path, so it is non-destructive: a match that was already uploaded is simply forgotten from tracking, never deleted from either side.
- Signed-out handling. When the `proton-drive` session expires, the watch daemon now detects it, surfaces a single clear "Signed out of Proton — sign in to resume" banner (instead of a pile of per-folder transfer errors), pauses syncing, and resumes automatically the moment the session is back. A logout is distinguished from a genuine transfer error, so an unrelated failure never trips it.

### Changed
- The periodic full walk now runs on a background thread. A file changed while a walk is in progress is reconciled within seconds instead of waiting for the whole walk to finish; previously the (single-threaded) walk blocked the change queue for its entire duration. Only one walk runs at a time and it winds down promptly on shutdown or sign-out. Overlapping a change sync with the walk is safe — it is the same scoped, positive-confirmation reconcile the walk already runs concurrently per folder.

## [0.2.1] - 2026-07-25

### Added
- Streaming full walk. The full reconcile now walks the tree folder-by-folder and transfers as it goes (a concurrent breadth-first walk of shallow, direct-children reconciles), instead of scanning the whole tree and only then applying. Uploads and downloads overlap the walk, so a large tree starts syncing immediately rather than after a long scan. Each folder is reconciled by the same scoped, positive-confirmation primitive, so containment and the data-safety invariants hold per folder.

### Fixed
- Self-feeding watch loop. The daemon's own reconcile reads were firing filesystem access/attribute events, which marked the just-read folders "hot" and triggered endless re-scanning, so the watcher never settled. It now only treats real content changes (create, write, delete, rename) as changes and ignores access/metadata events.
- Deletion safety (from an adversarial review of the folder-scoped reconcile): a failed local directory read no longer reads as "everything deleted"; a sub-folder's baseline rows are pruned only when its delete actually completed (not on a cancelled or failed one); and a remote "not found" is treated as a missing listing (deletes suppressed) rather than an empty one.
- Packaging shipped no app icon, so the `.deb`/`.rpm` launcher fell back to a generic system icon and a second, differently-named entry. The packages now ship the NeutronSync icon, the desktop entry uses it (with a matching `StartupWMClass`), and the app writes its user-level desktop file under the same name so it shadows the packaged one instead of duplicating it.

### Changed
- Startup and change-event bursts reconcile their folders concurrently (a worker pool), instead of one slow CLI cold-start at a time, so a batch of folders syncs about as fast as the full walk rather than sequentially.
- Change-triggered syncs process the newest change first, so a fresh edit is never starved behind a backlog of older or no-op folder checks.
- The Activity banner shows the folder currently being checked during a single-folder (shallow) reconcile, instead of a generic "Scanning folder pairs".

## [0.1.1] - 2026-07-24

### Fixed
- Data loss on an interrupted sync. A run that was cancelled (or killed) before its transfers ran used to record the not-yet-transferred files in the baseline as if they were synced. The next run then saw those files missing locally and propagated a deletion of the remote copy. The baseline is now committed by positive confirmation: a file is recorded only after its transfer actually completes, so a failed or cancelled operation leaves the previous state intact and is simply retried next run, never mistaken for a deletion.
- Excluded folders were still walked during the scan (and only skipped later). They are now pruned from the scan itself, on both the remote tree walk and the local walk, so an excluded sub-tree is never listed, descended, or stat-walked.

### Added
- Sub-folder-scoped sync. A local change now syncs only the folder it happened in, not the whole pair; the engine can scan/reconcile/commit a single sub-tree (never seeing anything outside it, so it can't be mistaken for a deletion).
- Live activity across processes. The background tray daemon publishes its state, so an open GUI window shows what the daemon is doing (scanning, syncing, per-file operations) in real time instead of appearing idle.
- CLI syncs are recorded to the activity feed. Any `neutronsync sync` now writes its operations to the same store the GUI reads, so a sync run from the command line shows up in the GUI's Activity view (previously only GUI- and tray-daemon-initiated syncs appeared there).

### Changed
- Watch scheduling, prioritized and cheap. A local change triggers a **shallow** reconcile of just the folder whose direct contents changed (files merged, immediate sub-folders created/removed but not descended into) — deeper changes arrive as their own watch events, so touching a file never re-walks a subtree. On startup, folders with fresh local changes sync first, then recently-active ("hot") folders, then the full walk. The full walk is paced to how long a walk actually takes (about six times its duration, floored at `poll_interval`), so a large tree is not re-walked constantly while small/active folders stay fresh.
- The watch daemon reloads the config file at the start of each reconcile, so exclusion and setting changes take effect without restarting it.
- Tray "Quit" now closes the open window as well as the background daemon, giving one clean exit. Closing the window still leaves the tray syncing in the background.

## [0.1.0] - 2026-07-24

### Added
- Bidirectional folder sync on the official `proton-drive` CLI, with a three-way-merge engine and a per-pair baseline snapshot (stored in SQLite).
- CLI (`neutronsync`): `init`, `login`, `logout`, `status`, `doctor`, `sync` (`--dry-run` / `--resync` / `--json`), and `watch`.
- Native GUI (`neutronsync-gui`, `gui` feature): egui/eframe app with a Folders, Activity, Settings, Account, and About layout, a remote-folder browser, and an optional system tray. Original "neutron" atom logo.
- Per-pair selective sync: exclude sub-folders (in the GUI or via `exclude` in the config). Excluded paths are ignored by the engine and never touched on Proton. Excluding an already-synced folder freezes both copies; NeutronSync offers to remove the local copy (to the desktop trash) to free space while the cloud copy stays. Re-including re-downloads from the cloud, so excluding can never delete remote data.
- Watch daemon: filesystem events (debounced) plus a "hot" cache of recently active folders and a periodic full rescan as the safety net. Single-instance locked.
- Concurrent remote tree scan (`scan_threads`) and concurrent, retrying downloads, since the CLI has no recursive listing.
- Local hash cache: in `sha1` mode only files whose size or mtime changed are re-hashed.
- In-app update check (GUI): follows a `stable` or `prerelease` channel from GitHub releases and notifies when a newer version exists. It never downloads or installs anything itself.
- GUI backend (`service::Controller`): observable `AppState`, a structured `SyncEvent` stream, and cancellation.
- Recoverable deletes both ways: remote to Proton's online trash, local to the desktop trash (via `gio`, with an XDG-trash fallback).
- Trust model made explicit in the GUI: the sign-in and About screens state that logging in signs the CLI into Proton and that NeutronSync never sees your credentials, plus a note on how change detection works and a not-affiliated disclaimer.
- Packaging: `.deb`, `.rpm`, and a portable binary tarball via the release workflow; systemd user units. Commits and tags are GPG-signed.

### Fixed
- Data-safety: a failed upload or download no longer records post-transfer state in the baseline. Only files that fully synced this run are written; a failed transfer leaves that file's baseline row untouched so the next run retries it in the correct direction. A timeout on one file can no longer cause the stale side to overwrite a newer edit, and never invalidates the rest of the sync.
- Data-safety: a transient local stat failure (a file locked or renamed mid-scan) no longer makes a known file look deleted; its baseline entry is carried forward and the file is retried.
- Data-safety: a remote folder that reports "not found" while being walked is treated as a partial scan (deletions suppressed), not as an empty folder, so a transient listing failure can't trigger a mass local delete.
- Data-safety: a path that is a file on one side and a directory on the other is surfaced and skipped instead of being silently left diverged.
- Data-safety: a missing local root (an unmounted drive) is refused rather than mistaken for a mass deletion; the watch daemon no longer recreates a previously-synced root that has gone missing. A vanished remote base folder likewise suppresses deletions for that run.

### Security
- Path traversal: remote node names are validated to a single safe path component, and every local path built from a remote name is re-checked to stay within the pair's root, so a crafted or shared name (`..`, absolute, or with an embedded separator) cannot write outside the sync folder.
- Argument injection: every `proton-drive` invocation passes `--` before its positional paths, so a file or folder named like a flag (`-c`, `--help`) is not interpreted as one.
- Log privacy: the sync log (which records file paths) is created in a `0700` directory as a `0600` file, and the per-run metadata cache dir is created `0700`, so neither is world-readable on a multi-user host.
- `credentials_store` is validated against its allowlist (`keychain`, `pass`, `unsafe_file`) before being exported to the CLI.

### Changed
- `init` writes an empty config (no sample "documents" pair); add your own `[[pair]]` entries.
- The config-path override environment variable is `NEUTRONSYNC_CONFIG`.
- SQLite baseline: `busy_timeout` is set so a concurrent writer waits briefly instead of failing immediately.

### Notes
- Requires the official `proton-drive` CLI on `PATH` (not bundled).
- `gio` (glib) is used for recoverable local deletes; an XDG-trash fallback is used if it is missing.
