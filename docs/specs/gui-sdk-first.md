# Spec: make the GUI and the docs SDK-first

Status: draft, 2026-10-06. Owner: maintainer. Follows `docs/specs/api-backend.md` (Phase 4, release 0.4.0).

## 1. Problem

Release 0.4.0 makes the API backend (the `neutronsync-drive` sidecar on Proton's Drive SDK) the default. The sync engine, the watcher and sign-in use it. The GUI and the docs still treat the `proton-drive` CLI as the primary path in several places:

- Two GUI features call the CLI directly, whatever the configured backend. They fail on an API-only install, and they fail on this machine because the CLI session has been signed out since 2026-10-01.
- The API sign-in and Account pages are plain default egui widgets. The CLI pages they replace use the app's own design.
- The About page and the docs describe the CLI design: "NeutronSync never sees your password", "Proton has no change feed", "remote changes wait for the next walk". All three are false on the API backend.
- Settings show CLI-only options that do nothing on the API backend.

## 2. Goal

The API backend is the primary path in every GUI page and every doc. The CLI backend is described once, as a legacy fallback that goes away in 0.5.0. A user with only the 0.4.x package installed, and no `proton-drive` binary, can use every GUI feature.

## 3. Scope

In scope: `src/bin/gui.rs`, the account probe in `src/service.rs`, `README.md`, `SECURITY.md`, `CONTRIBUTING.md`, `docs/SYNC_MODEL.md`, `docs/GUI_API.md`, `neutronsync.example.toml`, `CHANGELOG.md`.

Out of scope:

- Removing the CLI backend, `protoncli.rs`, the `[cli] backend` switch or the CLI-only config keys. That is release 0.5.0, after the 7-day soak ends on 2026-10-13.
- Changes to the sync engine, the watcher, the sidecar or the RPC contract.
- New GUI features other than those listed in section 4.

## 4. GUI changes

### 4.1 Remote folder browser and folder picker (defect)

- `load_browser` (`gui.rs`, "add a pair from Proton Drive") and `open_exclude_editor` ("Folders to sync") build `ProtonCli::new(&cfg)` and call `list_dir`.
- Replace both with `neutronsync::backend::select(&cfg)` and the `Remote::list_dir` it returns. The result type stays `Result<Vec<Entry>, String>`.
- On the API backend, both use the shared sidecar session. Neither may start a second session owner.
- The CLI backend keeps working through the same `select` call.

### 4.2 Sign-in page (API backend)

Replace the plain `page_signin` API branch with a page in the same design as the CLI sign-in page: centred column, logo, 24 pt title, subtitle, a `card_frame` for the form, `button()` for actions.

- Title: "Sign in to Proton Drive".
- Subtitle: your password goes only to Proton; NeutronSync keeps the session in the system keyring.
- Form steps, one at a time in the same card: username and password; two-factor code; mailbox password; human verification. Human verification shows the `verify.proton.me` link as a secondary button "Open verification page", then "Retry sign-in".
- Inputs use a fixed width that matches the card, with padding. Enter submits the current step.
- While a request runs: inputs and the button are disabled and a spinner shows beside the button.
- Errors show in the card, in `DANGER` colour, in plain words. Never show or log the password, the code or tokens.
- Sidecar not usable (`DriveApi::new` or `auth.status` fails): title "Sign-in service didn't start", the error text in a card, a short fix ("reinstall NeutronSync, or set `[cli] sidecar`"), and a primary "Check again" button. No CLI install guidance on this backend.

### 4.3 Account page (API backend)

Replace the plain `page_account` API branch with the CLI page's design:

- Status dot and bold label: "Checking…", "Signed in", "Not signed in" or "Sign-in service unavailable".
- Under it: "Proton account" and the account name. The sidecar's version or path goes in small dim text.
- Buttons: "Sign out" (ghost, enabled only when signed in, keeps the existing confirm dialog) and "Refresh".
- When signed out, the sign-in form from 4.2 shows in a card below.
- In `service.rs`, the API account probe stores the account name and the sidecar identity as separate fields, not one preformatted `"neutronsync-drive — <account>"` string.

### 4.4 Sign-in gate

- The gate rule stays: show the sign-in page until the probe confirms a usable backend that is signed in.
- Rename `AccountState::binary_found` to `backend_ready` (or equivalent) and update the comments. On the API backend it means "the sidecar answered", not "a binary is on PATH".
- `pubd.signed_out` in the daemon snapshot merge uses the renamed field.

### 4.5 Settings

- Show "proton-drive binary" and "Fresh metadata each run" only when `backend = "cli"`, under a group headed "Legacy CLI backend".
- Add a read-only "Backend" row in Advanced. It shows "Proton Drive SDK (neutronsync-drive)" or "Legacy CLI (proton-drive)". The backend stays a config-file choice; no switch in the GUI.
- On the API backend, add "Full check every" in Advanced, bound to `[options] full_walk_interval` (the daily safety-net walk). Offer 6 h, 12 h, 1 day (default) and 7 days. On the CLI backend this row is hidden; `poll_interval` keeps its current handling.

### 4.6 About page

Rewrite two sections for the API backend. Keep the current text only when `backend = "cli"`.

- **How it works:** NeutronSync is independent and unofficial. It talks to Proton Drive through `neutronsync-drive`, a helper built on Proton's official Drive SDK. You sign in inside NeutronSync. Your password goes only to Proton. The session is kept in your system keyring. Proton's SDK code does all encryption and decryption, on this computer. Keep the "not affiliated with Proton AG" paragraph unchanged.
- **Detecting changes:** local folders are watched live, and only the changed folder is reconciled. Proton Drive changes arrive through Proton's change feed within seconds, and only the affected folders are re-checked. A full walk runs on first sync, when Proton asks for a refresh, and once a day as a safety net.

### 4.7 Copy and visual rules

- Use the existing widgets and colours (`button`, `card_frame`, `status_dot`, `settings_group`, `setting_row`, `DIM`, `DANGER` and so on). No new colours or fonts.
- Plain words in all user-facing text. Say "Proton Drive", "sign-in service" and "full check". Do not say "sidecar", "RPC" or "SDK" except in the Advanced "Backend" row and the About page.

## 5. Documentation changes

Rule for every doc: the API backend is described first and as the default. The CLI backend gets one short "Legacy CLI backend" section or paragraph that says it is a fallback and goes away in 0.5.0. Markdown is never hard-wrapped.

- **README.md**
  - Lead and features: the sidecar on Proton's Drive SDK, sign-in inside the app, change feed.
  - Configuration example: show `[cli] backend` and `sidecar` as the main keys. Move `binary`, `upload_flags`, `download_flags`, `fresh_cache` and `credentials_store` into a "Legacy CLI backend" block. Add `full_walk_interval`. Mark `poll_interval` as CLI-only.
  - Remove the `fresh_cache` paragraph from the main flow; keep it in the legacy block.
  - Prerequisites: `secret-tool` (libsecret) and a running Secret Service. `proton-drive` only for the legacy backend.
  - Troubleshooting: sidecar not starting, socket in `$XDG_RUNTIME_DIR/neutronsync-drive/`, human verification, keyring locked, how to read `sync.log`.
  - GUI section: the sign-in page, the Account page, the "Full check every" setting.
- **docs/SYNC_MODEL.md:** open with the API model: change feed, cursor, acknowledgement, the three full-walk causes. Move "local changes are cheap, remote changes cost a re-walk" and the CLI startup order and adaptive walk sections under one "Legacy CLI backend" heading.
- **SECURITY.md:** add that the GUI sign-in form sends credentials only to the sidecar over the user-only socket, never writes them to disk or logs, and clears the password field after each attempt.
- **CONTRIBUTING.md:** module list gains `driveapi.rs`, `backend.rs`, `changefeed.rs`, `watcher_feed.rs` and `sidecar/`. Build steps include Bun and `sidecar/build.sh`, and `bun test`. Change "keep all proton-drive CLI specifics inside protoncli.rs" to "keep backend specifics behind the `Remote` trait".
- **docs/GUI_API.md:** document the account fields after 4.3 and 4.4, and that `refresh_account` probes the sidecar on the API backend.
- **neutronsync.example.toml:** API keys first. CLI keys in a block headed "Legacy CLI backend (removed in 0.5.0)". Add `full_walk_interval`.
- **CHANGELOG.md:** `[Unreleased]` entries for the folder browser and picker fix, the new sign-in and Account pages, the "Full check every" setting and the doc rewrite.

## 6. Acceptance

Automated:

- `cargo fmt`, `cargo build --release --features gui`, `cargo test` and `bun test` in `sidecar/` pass. No new clippy warnings in changed code.
- A test proves the folder browser and the folder picker list through `backend::select` with the fake sidecar (`tests/fixtures/drive_sidecar.py`) and with no `proton-drive` on `PATH`.
- A config round-trip test covers `full_walk_interval` set from the GUI value.
- `grep -n "ProtonCli::new" src/bin/gui.rs` returns no line outside a `Backend::Cli` branch.

Manual, against the installed session (the maintainer's account), with `PATH` set so `proton-drive` is not found:

- Add pair: the remote browser lists `/my-files` and opens subfolders.
- "Folders to sync" lists the subfolders of the Documents pair.
- Sign out, then sign in again from the sign-in page, including the two-factor step. The watcher resumes without a restart.
- Stop the sidecar binary from starting (point `[cli] sidecar` at a missing path in a scratch config): the "Sign-in service didn't start" page shows with the error and "Check again".
- Settings: CLI-only rows hidden; "Backend" row and "Full check every" show; changing "Full check every" writes `full_walk_interval`.
- About page text matches 4.6.
- Screenshots of the sign-in page (each step), the Account page (signed in and signed out) and Settings, in light and dark themes, go in the evidence folder.

Docs: a reviewer reads README, SYNC_MODEL, SECURITY, CONTRIBUTING and the example config, and finds no statement that is false for the API backend, and no CLI step presented as required.

## 7. Constraints

- Do not touch the running NeutronSync, `/usr/bin`, `~/.config/neutronsync`, `~/.local/state/neutronsync` or `~/ProtonDrive`. Use a scratch config and state dir for manual tests.
- No remote writes are needed. If a test needs one, it goes in `/my-files/NeutronSync-GUI-Test`, which moves to Proton trash at the end.
- No `deleteNodes`, `emptyTrash` or `deleteRevision`.
- No Claude, Anthropic or AI attribution in any commit, file or comment.
- Markdown is never hard-wrapped.
