<p align="center">
  <img src="assets/logo.png" width="88" alt="NeutronSync">
</p>

<h1 align="center">NeutronSync</h1>

<p align="center">
  Bidirectional folder sync for Proton Drive on Linux, with a built-in API sidecar.
</p>

> NeutronSync is an independent, unofficial project. It is not affiliated with, endorsed by, or sponsored by Proton AG. "Proton" and "Proton Drive" are trademarks of Proton AG.

NeutronSync keeps local folders and their Proton Drive counterparts in sync through a three-way-merge engine. Its CLI and native GUI share one core library. The bundled `neutronsync-drive` sidecar uses Proton's SDK and owns the API session.

## Features

- **Proton Drive SDK sidecar.** The packaged `neutronsync-drive` helper talks to Proton through the official Drive SDK. You sign in inside NeutronSync; the session lives in your system keyring.
- **Change feed.** Remote edits on other devices arrive within seconds. Local folders are watched live; only the folders that changed are re-checked. A daily full check is the safety net.
- **Bidirectional three-way merge.** A per-pair baseline lets it tell "new on the remote" apart from "deleted locally", so changes on either side are applied correctly instead of blindly mirrored.
- **Safe by default.** Deletions are opt-in and recoverable (remote to Proton trash, local to the desktop trash). A missing baseline unions both sides rather than mass-deleting. Conflicts keep both copies. An unmounted local folder or a vanished remote base is refused, not mistaken for a mass delete.
- **Selective sync.** Exclude sub-folders per pair. Excluded paths are never touched on Proton; you can optionally free up local space by removing the local copy while the cloud copy stays.
- **Live sync.** A watch daemon reconciles on local change (inotify, debounced) and follows Proton's change feed for remote edits.
- **Native GUI or CLI.** An egui desktop app (single binary, optional system tray) or a lean command-line tool. Same engine, same config.
- **In-app updates.** Checks GitHub releases on a channel you choose (stable or pre-release), then downloads and installs the new version for you: it picks the asset matching how you installed (`.deb` or `.rpm`), installs it through your package manager so your package list stays correct, and restarts the app. A copy that no package manager owns is downloaded for you to install by hand.
- **Signed releases.** Commits and tags are GPG-signed; releases ship `.deb`, `.rpm`, and a portable binary tarball.

## Remote backend

The default backend is `api`. One `neutronsync-drive` sidecar serves the GUI, tray daemon, CLI and watch processes in your user session. Uploads replace same-name files with new revisions and preserve whole-second modification times. Downloads appear atomically after size and SHA-1 verification. Deletes go to Proton trash. Use `--dry-run` to preview a plan. Existing baselines remain valid even when remote node identifiers differ.

```toml
[cli]
backend = "api"                       # default; this line may be omitted
sidecar = "/path/to/neutronsync-drive" # optional
```

Without `sidecar`, NeutronSync looks next to its executable, then on `PATH` for `neutronsync-drive`. The package includes it. Set `[cli] backend = "cli"` if the API backend fails and you need the legacy backend. That fallback remains available for one release and requires a separate `proton-drive` installation and sign-in. NeutronSync never silently switches backends. Saving GUI settings preserves an explicit `cli` selection.

The first client starts the session owner. Other clients connect to `$XDG_RUNTIME_DIR/neutronsync-drive/session.sock`. If `XDG_RUNTIME_DIR` is unset, the path is `$HOME/.cache/neutronsync-drive/session.sock`. The socket directory is private, mode `0700`; the owner checks peer UIDs. A lock beside the socket prevents competing owners. The sidecar survives its initiating client; after a sidecar crash, clients reconnect and watchers resume persisted event cursors.

Use `neutronsync sync --dry-run --backend api` to override the config for one run. `--backend cli` selects the CLI instead. API excludes hide paths and their descendants from the plan, but the API backend still lists those folders remotely.

## Requirements

- Linux with a recent Rust toolchain (edition 2021) if building from source.
- An unlocked Secret Service keyring in your desktop session and `secret-tool` (`libsecret-tools` on Debian/Ubuntu; `libsecret` on Fedora/RHEL).
- The official `proton-drive` CLI only for `backend = "cli"` (<https://proton.me/blog/proton-drive-cli>).
- `gio` (from glib, present on most desktops) for recoverable local deletes; a manual XDG-trash fallback is used if it is missing.

## Install

### From a release (Debian/Ubuntu)

```sh
# download the .deb from the Releases page, then:
sudo apt install ./neutronsync_<version>_amd64.deb
```

### From a release (Fedora/RHEL)

```sh
sudo dnf install ./neutronsync-<version>.x86_64.rpm
```

Both packages install `neutronsync`, `neutronsync-gui` and `neutronsync-drive` to `/usr/bin`, plus a desktop launcher and systemd user units. They depend on the package providing `secret-tool`. The binary tarball contains all three binaries; keep them together and provide `secret-tool` and a Secret Service session yourself.

### From source

```sh
# CLI
cargo install --path .                      # -> ~/.cargo/bin/neutronsync
cargo test                                  # engine + unit tests

# Sidecar (Bun 1.4.0)
(cd sidecar && bun install --frozen-lockfile && bun test)
sidecar/build.sh
install -m 755 sidecar/dist/neutronsync-drive ~/.cargo/bin/neutronsync-drive

# GUI (opt-in feature, keeps the CLI dependency-light)
cargo build --release --features gui --bin neutronsync-gui
```

The GUI build needs a few system libraries (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev libxdo-dev libayatana-appindicator3-dev
```

## Quick start

### GUI

Launch **NeutronSync** from your app menu. The sign-in page takes your Proton username and password (and two-factor or mailbox password when Proton asks). For human verification, open the verification page, complete it, then select **Retry sign-in**. Password fields stay masked and are cleared after each attempt. The Account page shows whether you are signed in and lets you sign out or refresh. In Settings → Advanced, **Backend** shows which backend is active and **Full check every** sets how often the API safety-net walk runs (6 h, 12 h, 1 day, or 7 days). Add folder pairs on the Folders page and select **Folders to sync** to set exclusions.

### CLI

```sh
neutronsync init                        # write ~/.config/neutronsync/neutronsync.toml
neutronsync login                       # sidecar sign-in; passwords are not echoed
$EDITOR ~/.config/neutronsync/neutronsync.toml
neutronsync sync --dry-run              # preview
neutronsync sync                        # apply
neutronsync watch                       # live sync: FS events + periodic rescan
```

`login` prompts for username, password and any required TOTP or mailbox password. Human verification offers a browser link and waits for confirmation before retrying. `init` writes an empty config; add your own `[[pair]]` entries. `status` reports the account through `auth.status`. `doctor [REMOTE_PATH]` probes the selected backend. `logout` signs out through that backend. `sync` takes `--dry-run`, `--resync` and `--json`; passing pair names syncs only those.

## How it works

For each folder pair, NeutronSync keeps a baseline snapshot of the last state the two sides agreed on. On every run it scans the current local and remote trees and classifies each path against the baseline (created, modified, or deleted) independently per side. Combining the two verdicts decides the action and which side wins.

**Detecting changes.** Both backends watch local folders through inotify. The API backend also receives remote changes from the sidecar's five-second event poll:

- A local change reconciles only the folder whose direct contents changed (one shallow folder listing), not the whole tree. Because the watch is recursive, a change deeper down arrives as its own event and reconciles its own folder, so editing one file never re-walks a subtree.
- With `backend = "api"`, remote events enter the same folder queue as local changes. The watcher maps node UIDs through the baseline or sidecar and reconciles both parents for moves and trash. Events are hints; the three-way merge still decides every change. The watcher saves and acknowledges each cursor after successful reconcile, so restart replays changes made while stopped without a full walk.
- The API watcher subscribes before its first full walk. Later full walks handle refresh notices, unresolved events and the daily safety net. Set `[options] full_walk_interval = 86400` to change that interval in seconds. Sign-out pauses sync; sidecar authentication recovery resumes from the saved cursor.
- With `backend = "cli"`, startup syncs fresh local changes, recently active ("hot") folders, then everything else. Remote changes arrive on the next hot pass or full walk. Full walks stream folder-by-folder and keep their adaptive pacing: six times the walk duration, floored at `poll_interval` and capped at six hours. `full_walk_interval` does not affect this backend.

When the background tray daemon is doing the work, an open window mirrors its live state, so scanning and per-file transfers show up in Activity in real time. The sync model and its reasoning are written up in [docs/SYNC_MODEL.md](docs/SYNC_MODEL.md).

**Safety model.**

- Deletions propagate only when `propagate_deletes` is on, and recoverably: the remote copy goes to Proton's online trash, the local copy to your desktop trash (`local_delete = "remove"` unlinks permanently instead).
- A missing baseline (first run, or a wiped state dir) unions both sides rather than mirroring, so a lost baseline can't trigger a mass delete.
- If a folder's local root is missing (an unmounted drive) or its remote base folder has vanished, the pair is refused for that run instead of being read as "everything was deleted".
- A failed or interrupted run never poisons the wider sync: the baseline records a file only after its transfer actually completes, so a timeout, an error, or a run cancelled part-way leaves every other file's state untouched and retries the rest next run in the correct direction. This is what stops a half-finished run from later mistaking a not-yet-downloaded file for a deletion.
- Conflicts keep both copies by default (`name (conflict <timestamp>).ext`).

## Selective sync

Each pair can list sub-paths to exclude (in the GUI's "Choose folders to sync", or `exclude = [...]` in the config). Excluded paths are ignored entirely and never touched on Proton. Excluding an already-synced folder freezes both copies in place; when you exclude one, NeutronSync offers to remove the local copy (to the desktop trash) to free space while the cloud copy stays. Re-including a folder later re-downloads it from the cloud, so excluding can never delete remote data.

## Configuration

TOML at `~/.config/neutronsync/neutronsync.toml` (override with `-c PATH` or `$NEUTRONSYNC_CONFIG`). See `neutronsync.example.toml` for the annotated template.

```toml
[cli]
backend = "api"                       # default; this line may be omitted
# sidecar = "/path/to/neutronsync-drive"  # optional; else next to neutronsync, then PATH

[options]
remote_root = "/my-files"     # Proton's per-user root
propagate_deletes = false     # deletes cross over (recoverably); false = never delete
local_delete = "trash"        # trash | remove
conflict = "keep-both"        # keep-both | newer | skip
compare = "size+mtime"        # size | size+mtime | sha1
full_walk_interval = 86400    # API: seconds between safety-net full checks (default 1 day)
update_channel = "stable"     # stable | prerelease
# check_on_launch = false     # GUI: check for updates on start (notify only)

[[pair]]
name = "documents"
local = "~/Documents"
remote = "Documents"          # -> /my-files/Documents
# exclude = ["APPS"]          # sub-paths never synced; the Proton copy is untouched
```

#### Legacy CLI backend

Set `[cli] backend = "cli"` only as a fallback. It goes away in 0.5.0 and needs a separate `proton-drive` install and sign-in. Keys used only on that backend:

```toml
[cli]
backend = "cli"
binary = "proton-drive"
upload_flags = ["--file-conflict-strategy", "replace", "--folder-conflict-strategy", "replace"]
download_flags = ["--file-conflict-strategy", "remove", "--folder-conflict-strategy", "remove"]
fresh_cache = true            # throwaway metadata cache per run (avoids stale listings)
# credentials_store = "keychain"

[options]
poll_interval = 900           # CLI watch: floor for adaptive full rescans (seconds)
```

## Running in the background (systemd --user)

Use one of these, not both (they'd contend for the same pairs):

```sh
# Live sync (recommended) — the watch daemon:
systemctl --user enable --now neutronsync-watch.service

# or a periodic timer instead:
systemctl --user enable --now neutronsync.timer
```

The packaged units target `/usr/bin/neutronsync`. If you installed with `cargo install`, edit `ExecStart` to `%h/.cargo/bin/neutronsync`. Proton Drive is rate-limited, so keep the sync set modest and the rescan interval sane.

## Updates

Releases come from GitHub. Every tag ships as a pre-release; a stable release is one that has been promoted after the build has proven itself, and its title says `(stable)`. The newest promoted release is the one GitHub marks "Latest". Point the updater at the `stable` or `prerelease` channel in Settings (or `update_channel` in the config): `stable` offers only promoted releases, `prerelease` offers every build as it lands. Checking never installs anything. When an update is available, "Download and install" verifies the download against the checksum GitHub publishes and installs it through your package manager (one `pkexec` prompt), then offers to restart.

## Privacy and trust

The API sign-in form passes credentials over the private local socket to the sidecar. The sidecar stores session tokens and the derived key passphrase in your Secret Service keyring. Passwords, TOTP codes and tokens never enter logs, configuration, activity history or process arguments. One process owns and refreshes the session. Existing phase-labelled sessions migrate automatically to `NeutronSync Drive session`. See [SECURITY.md](SECURITY.md) for the threat model.

## Troubleshooting

- **Sign-in service didn't start:** the GUI shows this when `neutronsync-drive` cannot be started or reached. Reinstall the package, or build `sidecar/` and place `neutronsync-drive` beside the Rust binaries or on `PATH`, or set `cli.sidecar`. Check the socket under `$XDG_RUNTIME_DIR/neutronsync-drive/` (or `$HOME/.cache/neutronsync-drive/` when `XDG_RUNTIME_DIR` is unset). The legacy fallback is `[cli] backend = "cli"`.
- **`secret-tool` missing:** install `libsecret-tools` on Debian/Ubuntu or `libsecret` on Fedora/RHEL.
- **Keyring locked / no Secret Service:** start and unlock your desktop keyring. Run NeutronSync in the same user D-Bus session. A headless service needs that session too.
- **Human verification:** complete the shown Proton verification link, then retry sign-in. The CLI `login` command waits for Enter before retrying.
- **Signed out:** sign in from Account or run `neutronsync login`. Refresh the Account page if needed. The watcher resumes from its saved event cursor.
- **Sidecar path differs:** all clients in one user session must select the same executable. Align `cli.sidecar` before reconnecting.
- **Reading sync.log:** under the configured `state_dir` (default `~/.local/state/neutronsync/sync.log`). Timestamps are UTC.

## Known limitations

- The legacy CLI backend notices remote changes on a hot or full pass. The default API backend polls remote events every five seconds.
- Default `compare = "size+mtime"` can miss an in-place edit that keeps the same size and mtime; use `compare = "sha1"` to compare content exactly.
- Renames look like a delete + create.

## Development

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for layout, build, and release notes, and [`docs/GUI_API.md`](docs/GUI_API.md) for the GUI backend API.

## License

MIT. See [`LICENSE`](LICENSE).
