# Sync model

How NeutronSync decides what to sync, when, and why. This is the design behind `src/engine.rs`, `src/watcher.rs` and `src/watcher_feed.rs`. It is written for contributors; users only need the "How it works" section of the [README](../README.md).

## API backend (default)

The default backend talks to Proton Drive through `neutronsync-drive`, a long-lived sidecar on Proton's Drive SDK. That design has three properties that shape the watcher:

1. **A change feed.** The sidecar polls remote events every five seconds and delivers batches the watcher can acknowledge. Remote edits on other devices arrive within seconds without a full re-walk.
2. **One shared session.** GUI, tray, CLI and watch processes share one sidecar over a user-only socket. Sign-in and token refresh happen once per user session.
3. **Full walks as a safety net.** A full walk still runs on first sync, when Proton asks for a refresh (`events.refresh_required`), when events cannot be mapped, and once per `[options] full_walk_interval` (default one day).

Local changes stay cheap: a recursive inotify watch triggers a shallow reconcile of the one folder whose direct contents changed. Remote events enter the same shallow-folder queue. Events are hints; the three-way merge still decides every change.

### Remote change feed

With `[cli] backend = "api"`, the sidecar's acknowledged delivery mode repeats each batch until the watcher acknowledges it. The watcher maps parent UIDs to pair-relative folders using cached root UIDs, baseline `remote_id` values, then depth-bounded `node.path` resolution. Moves and trash also queue the node's old baseline parent. A changed baseline folder queues its own contents. An unresolved event schedules a full walk; a resolved path outside every pair is logged as `out_of_scope`.

Remote events and local inotify changes share the shallow-folder queue. Every event records one disposition (`mapped`, `out_of_scope`, `walk` or `deferred`), and each batch records counts. Events never issue filesystem operations directly.

First start subscribes before walking and stores the starting cursor only after that walk succeeds. Subsequent batches persist their cursor in `stats.db` before acknowledgement, after all their reconciles succeed. Failed batches remain unacknowledged for replay. Three consecutive failed deliveries trigger one `apply_failed` recovery walk and acknowledgement. Restart and authentication recovery subscribe with the stored cursor. A refused cursor requires one `resume_gap` walk.

`[options] full_walk_interval` sets the API safety-net interval in seconds, default `86400`. The GUI Settings page offers 6 h, 12 h, 1 day and 7 days. Sign-out pauses sync; sidecar authentication recovery resumes from the saved cursor.

## Baseline and the three-way merge

For each folder pair NeutronSync keeps a **baseline**: the last state the two sides agreed on, stored per file in SQLite (`stats.db`, table `baseline`, keyed by `(pair, rel)`). A sync classifies every path on each side against the baseline as Created, Modified, Deleted, Unchanged, or Absent, then combines the two verdicts to pick an action (`decide` / `decide_dir` in `engine.rs`).

The baseline commit is **additive**: `commit_baseline` writes the rows that synced this run and deletes the rows explicitly marked removed. It never rewrites the whole pair. This is what makes a partial or scoped sync safe: it only ever touches the rows it actually reconciled.

## Detecting changes: shallow, folder-scoped syncs

The local side is watched live with a recursive filesystem watch (inotify via `notify-debouncer-full`). A change does **not** trigger a whole-pair sync. It triggers a **shallow reconcile of the one folder whose direct contents changed** (`sync_pair_shallow`): list just that folder (one non-recursive listing on each side), reconcile its direct children, create or remove immediate sub-folders, but do **not** descend.

Why shallow is correct and not lossy:

- The watch is recursive, so a change deeper in the tree arrives as its **own** event and reconciles its **own** folder. Walking the subtree on every change would redo work that the deeper events already cover.
- A directory event reconciles that directory; a file event (or a delete, where the path can no longer be stat-ed) reconciles the file's parent. Both converge on "the folder whose direct listing changed."
- With the API backend, the remote feed queues remote-only changes directly. With the legacy CLI backend, the hot pass and full walk catch them.

Consequences that are intentional:

- Deleting a whole sub-folder locally trashes the remote folder (recoverably) and cleans that sub-folder's descendant rows out of the baseline (the shallow commit only removed the folder's own row, so descendants are pruned explicitly).
- A brand-new remote sub-folder initially appears as an empty local folder. API events for its children queue their parent for shallow reconcile; the CLI full walk fills it.

## Legacy CLI backend

The `proton-drive` CLI backend remains available until release 0.5.0 as a fallback (`[cli] backend = "cli"`). It has no change feed: the only way to learn about a remote-side change is to list the folder again and compare. The CLI also caches directory metadata and can serve it stale, so every listing runs with a throwaway `PROTON_DRIVE_CACHE_DIR`. Everything below follows from "local changes are cheap to detect, remote changes cost a re-walk."

### CLI startup order: quick wins first, full walk last

When the watcher starts it does not lead with the expensive full walk. It attaches the file watch **first** (so changes made during startup are captured, not lost in a blind window), then:

1. **Fresh local changes.** A fast local-only scan (`local_change_folders`, no network) finds folders that hold a new or changed file and shallow-syncs each. New local work uploads within seconds.
2. **Hot folders.** Folders with recent local activity (`hot_folders`) are shallow-synced next.
3. **Full walk.** Only then does the full reconcile run.

So syncing starts almost immediately and the full walk stops being a gate.

### CLI full walk: an adaptively paced safety net

The full walk scans both trees end to end. It is the only thing that catches remote-only changes across the whole tree on this backend, so it must run, but on a large tree it is expensive and pointless to run constantly. So it is **paced to its own cost**: after each full walk NeutronSync measures how long it took and schedules the next one at roughly `duration * 6` (`FULL_WALK_MULTIPLIER`), floored at the configured `poll_interval` and capped so it always eventually runs. A 15-minute walk reruns about every 90 minutes; small, active folders stay fresh in between through the shallow and hot syncs. `full_walk_interval` does not affect this backend.

The full walk **streams**: rather than scan the whole tree and only then apply, it walks folder-by-folder (a concurrent breadth-first walk, `run_sync_streaming`) and reconciles each folder as it is discovered, transferring immediately. So uploads and downloads overlap the walk instead of waiting for a full scan. Each folder is handled by the same shallow, direct-children primitive (`sync_pair_shallow_with_base`), and the walk descends into the child folders that reconcile reports (skipping any it just deleted or that are excluded).

Concurrency is a pool of workers (`scan_threads`, auto = CPU cores clamped to 2..8), each with its own Engine and cache dir. They share one read-only baseline snapshot and commit disjoint rows (SQLite serialises the writes), and the breadth-first, level-by-level structure means a parent folder is always created before its children are processed. A sequential reference implementation (`Engine::sync_pair_streaming`) is the tested oracle for the walk's semantics; an equivalence test asserts it reaches the same end state as the batch walk.

## Data-safety invariants

These are the rules that must hold. Most exist because breaking one caused a real incident.

- **Positive-confirmation commit.** A file advances the baseline only after its transfer actually completes. A failed transfer, or an op a cancelled/killed run never reached, is dropped from the prospective baseline, leaving its previous row intact. Without this, a run cancelled mid-way recorded files it never transferred as "synced", and the next run then read those still-missing local files as deletions and propagated them to the remote.
- **Deletes are opt-in and recoverable.** They propagate only with `propagate_deletes`; remote deletes go to Proton trash, local deletes to the desktop trash (`local_delete = "remove"` unlinks permanently instead).
- **Incomplete views never delete.** If a folder listing fails, that run syncs without deletions rather than acting on a partial picture.
- **Missing roots are refused.** A vanished local root (unmounted drive) or a vanished remote base folder is refused for that run, not read as "everything was deleted".
- **Missing baseline unions.** A first run or a wiped state dir unions both sides rather than mirroring, so a lost baseline cannot trigger a mass delete.
- **Scope containment.** A scoped or shallow sync only ever looks at paths inside its folder, so nothing outside can be seen as missing and deleted. An empty scope means the pair root, not a folder literally named "".
- **Excludes are invisible.** Excluded sub-paths are pruned from the scan itself (local and remote), never listed, downloaded, or deleted. Re-including a folder re-downloads it, so excluding can never delete remote data.
- **Conflicts keep both.** Both sides changed and differ, keep both (`name (conflict <timestamp>).ext`), never silently overwrite.

## The GUI is two processes

In tray mode the background tray daemon owns the watcher and does the syncing in one process; the window is a separate process. The window cannot see the daemon's in-memory state directly, so the daemon publishes a compact live snapshot to `status.json` a few times a second and the window reads it. That is how an open window shows the daemon scanning, syncing, and per-file transfers in real time. A CLI `neutronsync sync` also records its operations into the same activity store the window reads, so command-line runs show up there too.

## Config knobs

| Key | Meaning |
| --- | --- |
| `full_walk_interval` | API safety-net full-walk interval (seconds; default 86400). Ignored by the CLI backend. |
| `poll_interval` | CLI only: floor for the adaptive full-walk interval (seconds). |
| `scan_interval` | How often the hot-folder pass runs (seconds). |
| `debounce` | Filesystem-event debounce window (seconds). |
| `scan_threads` | Concurrent remote-scan workers (0 = auto, capped at 8). |
| `download_threads` | Concurrent downloads (0 = auto, kept modest). |
| `propagate_deletes` | Whether deletions cross to the other side. |
| `local_delete` | `trash` (recoverable) or `remove` (permanent) for local deletes. |
| `conflict` | `keep-both` (default), `newer`, or `skip`. |
| `compare` | `size+mtime` (default), `size`, or `sha1`. |
