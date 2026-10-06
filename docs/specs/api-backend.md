# Spec: move NeutronSync from the proton-drive CLI to the Drive API

Status: draft, 2026-10-06. Owner: maintainer.

## 1. Problem

NeutronSync drives Proton Drive through the official `proton-drive` CLI. Two faults make it unfit for daily use:

- **Frequent sign-outs.** The session drops and the watcher pauses until a manual sign-in.
- **Slow remote walk.** The CLI has no change feed, so every remote change is found only by re-listing the tree. A full walk of the Documents pair takes about 13 minutes. Each CLI process cold-starts (about 1.4 s per folder in a warm worker, about 13 s cold).

## 2. Why a CLI upgrade does not fix it

CLI 0.9.0 (released 2026-10-05, SDK js@0.22.2) was checked against the installed 0.8.0 (SDK js@0.21.0):

- New commands: `filesystem size` (folder size including trash) and `takeout run` (offline export).
- `filesystem list` and `auth login` help output is identical to 0.8.0.
- No recursive listing, no event or change feed, no long-running or daemon mode, no session-sharing option.

The walk cost and the per-process session model stay the same. The upgrade is still safe to take, but it is not the fix.

## 3. Root-cause hypotheses

### 3.1 Sign-outs

Every CLI invocation is a separate process. Each one loads the session from the keyring and refreshes the access token on its own. NeutronSync runs up to 8 scan workers plus concurrent transfers. Proton rotates refresh tokens on use. Two processes that refresh at the same moment present the same refresh token twice, and the server can treat the reuse as a replay and revoke the session.

This is a hypothesis. Phase 0 must confirm it (see 9.1). If it holds, the fix is one process that owns the session and is the only refresher.

### 3.2 Slow walk

- No change feed in the CLI, so detection is O(tree) per walk.
- A fresh `PROTON_DRIVE_CACHE_DIR` per run (required because the CLI serves stale listings), so no metadata or key cache survives between runs.
- Process start, session load and key unlock repeat on every call.

## 4. Goals

- G1: Zero unexplained sign-outs over a 7-day watch run.
- G2: Remote changes detected from the event feed in under 60 s, with no full walk in steady state.
- G3: A full walk runs only on first sync, on a `TreeRefresh` or `FastForward` event, or on manual resync.
- G4: Keep every existing data-safety invariant (section 7).
- G5: Keep the CLI backend selectable as a fallback until the API backend is proven.

## 5. Non-goals

- Pure-Rust Proton crypto and SRP. Possible later, not in this work.
- Photos, sharing, albums, devices.
- Commercial distribution. The SDK permits personal, non-commercial use only.

## 6. Design

### 6.1 Shape

A long-lived sidecar process, `neutronsync-drive`, written in TypeScript on the official `@protontech/drive-sdk` (MIT, 0.22.2 at time of writing). The Rust app starts it once and talks to it over JSON-RPC on stdin/stdout. One sidecar holds one session, one SDK client and one in-memory entity and crypto cache for its whole life.

Reasons for a JS sidecar over the alternatives:

- It is the same SDK the CLI and the official clients use, so node parsing, crypto and the coming crypto migration are Proton's code, not ours.
- The C# SDK works too but adds a .NET runtime for no gain on Linux.
- A pure-Rust client means writing SRP, the PGP key chain and Drive node crypto by hand, and redoing it at the end-2026 crypto migration.

Packaging: compile the sidecar to a single executable (Node SEA or `bun build --compile`) and ship it in the deb, rpm and tarball next to the Rust binaries. No system Node dependency.

### 6.2 Authentication and session

The SDK does not do login or session management. The sidecar owns both:

- Login: SRP auth against the Proton account API, then TOTP if 2FA is on, then mailbox-password unlock if the account has two-password mode. Use Proton's published SRP implementation; do not hand-roll SRP.
- Storage: session UID, access token, refresh token and the salted key passphrase go in the Secret Service keyring under a NeutronSync-owned item. It is a separate session from the CLI, so the two never refresh each other's tokens.
- Refresh: a single-flight refresh. One in-flight refresh at a time, every request waits on it, and the new token pair is written to the keyring before any request retries. This is the fix for 3.1.
- Header: every request sends `x-pm-appversion: external-drive-neutronsync@<semver>-<channel>`, per the SDK third-party rules. Never spoof a first-party client.
- Sign-out signal: a 401 after a failed refresh, or a refresh that returns invalid-grant, emits `auth.signed_out` over RPC. The Rust watcher already pauses on `SyncEvent::Auth { signed_in: false }`; map it to that.
- Login UI: the GUI Account page calls `auth.login` with username and password, then `auth.submit_2fa` if asked. No more shelling out to `proton-drive auth login`.

### 6.3 RPC contract

Newline-delimited JSON-RPC 2.0. Requests from Rust, responses and notifications from the sidecar. Paths are relative to the pair root; the sidecar also returns node UIDs.

Requests:

- `auth.status` → `{ signed_in, account }`
- `auth.login { username, password }` → `{ ok } | { need_2fa } | { need_mailbox_password }`
- `auth.submit_2fa { code }`, `auth.submit_mailbox_password { password }`
- `auth.logout`
- `node.resolve { path }` → `{ uid, type } | not_found`
- `node.list { uid }` → `[Entry]` (one folder)
- `node.walk { uid, exclude_globs }` → streamed `walk.entry` notifications, then a final `{ folders, failed }`
- `node.create_folder { parent_uid, name }` → `{ uid }`
- `file.upload { parent_uid, name, local_path, mtime, size, replace_uid? }` → `{ uid, revision_uid, sha1 }`
- `file.download { uid, local_path }` → `{ sha1, size }` (writes to a temp file, renames on success)
- `node.rename { uid, new_name }`, `node.move { uid, new_parent_uid }`
- `node.trash { uids }` (never `delete`)
- `events.subscribe { scope_id, since_event_id? }`
- `events.ack { event_id }`

Notifications:

- `events.batch { scope_id, events: [ { type, node_uid, parent_uid, name?, revision? } ], last_event_id }`
- `events.refresh_required { scope_id, reason }` for `TreeRefresh`, `TreeRemove` and `FastForward`
- `auth.signed_out`, `transfer.progress { id, bytes, total }`, `log { level, msg }`

`Entry` keeps today's shape (name, type, size from `claimedSize`, mtime from `claimedModificationTime`, `sha1` from `claimedDigests`) and adds `uid` and `parent_uid`.

Errors carry a typed `code`: `not_found`, `auth`, `conflict`, `rate_limited { retry_after }`, `transient`, `fatal`. The Rust side must never read `not_found` from a failed listing as "empty"; it keeps the `ListOutcome` rule from today.

### 6.4 Change feed

- The SDK exposes `subscribeToTreeEvents(treeEventScopeId, cb)`, `getLatestEventId(scope)` and event types `node_created`, `node_updated`, `node_deleted`, `tree_refresh`, `tree_remove`, `fast_forward`.
- The sidecar subscribes once per pair (the scope of the pair's volume). It forwards batches to Rust.
- Rust persists `last_event_id` per pair in `stats.db` only after the batch is applied. A crash replays the batch, and replay is idempotent because the engine works on state, not on deltas.
- Apply: map each event to the affected folder via `parent_uid` → path, then run the existing shallow reconcile on that folder. Events are hints, exactly as inotify events are on the local side. The three-way merge stays the source of truth.
- `refresh_required` → schedule one full walk for that pair, then resume from the new latest event ID.
- Safety net: keep a periodic full walk, but stretch the default from minutes to once a day. Phase 3 measures whether it ever finds anything the feed missed.

### 6.5 Rust integration

- New module `src/driveapi.rs`: spawns and supervises the sidecar (restart with backoff, kill on exit), owns the RPC client, implements the existing `Remote` trait.
- New trait `RemoteChangeFeed` with `subscribe`, `next_batch` and `ack`. `ProtonCli` does not implement it; the watcher keeps today's walk-only loop for the CLI backend.
- `watcher.rs`: if the backend implements the feed, feed batches go into the same hot-folder queue that inotify uses, and the background full walk becomes the rare safety net.
- Baseline: add a nullable `remote_uid` column to the state DB. It enables mapping events to paths without a walk. It also unblocks UID-based rename detection, which stays deferred.
- Config: `[cli] backend = "cli" | "api"`, default `cli` until Phase 4. `scan_threads` and `fresh_cache` apply to the CLI backend only.

### 6.6 Throughput

- One process, one session, parallelism inside the sidecar: listing concurrency 8 and transfer concurrency 4, both configurable.
- The SDK entity cache lives for the sidecar's life, so a repeat walk of an unchanged tree hits memory for keys and node metadata.
- Honour `rate_limited.retry_after`. Rate limits are per session and user, the same as first-party clients.

## 7. Data-safety invariants (must hold on the API backend)

- A failed or partial listing never produces a delete (`remote_failed` and `local_failed` suppression).
- `not_found` on a listing suppresses deletes; it is not an empty folder.
- Deletes are recoverable: remote goes to Proton trash, local goes to the desktop trash.
- Missing baseline unions both sides and never mass-deletes.
- Path-component validation (`is_safe_component`, `safe_join`) applies to names from the sidecar.
- Descendant baseline purge only for directory deletes in `deleted_ok`.
- An auth error is an error, never an empty listing.
- Downloads write to a temp file and rename; a cancelled download leaves no partial file in the tree.

The existing integration tests run against a fake `Remote`. Add the same suite against a fake sidecar (scripted JSON-RPC) so the invariants are tested on the new transport.

## 8. Phases and acceptance

### Phase 0: spike (go / no-go)

- Sidecar with `auth.login`, `auth.status`, `node.list`, `node.walk` and `events.subscribe` only.
- Measure a full walk of the Documents pair. Target: under 2 minutes cold, under 30 s warm.
- Trigger an edit from the web client and measure feed latency. Target: under 60 s.
- Run 8 concurrent CLI listings against a test account while the access token expires. Confirm or reject hypothesis 3.1.
- Go if the walk and feed targets hold.

### Phase 1: read-only backend

- `Remote` read methods on the API backend. `neutronsync sync --dry-run --backend api` produces the same plan as the CLI backend on the same tree.
- Equivalence test: CLI and API backends return identical `TreeScan` sets for the live tree, ignoring the new UID fields.

### Phase 2: writes

- Upload, download, create folder, rename, move, trash.
- Fake-sidecar suite passes every invariant in section 7.
- Live test on a scratch pair: create, edit, rename, delete both sides, conflict.

### Phase 3: event-driven watcher

- `RemoteChangeFeed` wired into `watcher.rs`. `last_event_id` persisted.
- 7-day run on the real pairs: zero sign-outs, zero safety-net walk findings, no full walk outside first sync and `refresh_required`.

### Phase 4: default and packaging

- `backend = "api"` becomes the default. The CLI backend stays for one release as a fallback.
- Sidecar binary in deb, rpm and tarball. The GUI sign-in uses the sidecar. README and SECURITY.md updated.

## 9. Risks

- **Pre-release SDK.** Proton says the SDK is not ready for third-party production use and its interfaces may change. Mitigation: pin the SDK version, keep the CLI backend as a fallback, keep the RPC contract stable so only the sidecar changes.
- **Crypto migration, end 2026 to early 2027.** Older SDK releases will stop working against the service. Mitigation: track SDK releases and ship a sidecar update with the migration. This hits the CLI equally.
- **Login is ours to maintain.** SRP, 2FA, CAPTCHA or human-verification challenges and two-password mode must all be handled. Mitigation: use Proton's SRP library; on a human-verification challenge, report it in the GUI and fall back to "sign in with the CLI" until supported.
- **Personal use only.** Fine for this project. Blocks any commercial distribution.
- **Sidecar binary size.** A compiled Node or Bun binary is about 60 to 120 MB. Acceptable; the CLI is already 118 MB.

## 10. Open questions

- Q1: Does the SDK's own HTTP client support a single-flight refresh hook, or must the sidecar wrap `fetch`?
- Q2: Can the sidecar reuse an existing CLI session at first start to avoid a second login? Default answer: no, because it shares a refresh token with the CLI and brings back 3.1.
- Q3: Which tree-event scope ID covers `/my-files`? Resolve in Phase 0 from `getMyFilesRootFolder()`.
- Q4: Keep the 0.9.0 CLI upgrade in the meantime? Recommended yes, after the running app is stopped.
