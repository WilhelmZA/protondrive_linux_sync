# Sync model

How NeutronSync decides what to sync, when, and why. This is the design behind `src/engine.rs`, `src/watcher.rs` and `src/watcher_feed.rs`. It is written for contributors; users only need the "How it works" section of the [README](../README.md).

## API backend (default)

The default backend talks to Proton Drive through `neutronsync-drive`, a long-lived sidecar on Proton's Drive SDK. That design has three properties that shape the watcher:

1. **A change feed.** The sidecar polls remote events every five seconds and delivers batches the watcher can acknowledge. Remote edits on other devices arrive within seconds without a full re-walk.
2. **One shared session.** GUI, tray, CLI and watch processes share one sidecar over a user-only socket. Sign-in and token refresh happen once per user session.
3. **Full walks as a safety net.** A full walk still runs on first sync, when Proton asks for a refresh (`events.refresh_required`), when events cannot be mapped, and once per `[options] full_walk_interval` (default one day).

Local changes stay cheap: a recursive inotify watch triggers a shallow reconcile of the one folder whose direct contents changed. Remote events enter the same shallow-folder queue. Events are hints; the three-way merge still decides every change.

### Remote change feed

The sidecar's acknowledged delivery mode repeats each batch until the watcher acknowledges it. The watcher maps parent UIDs to pair-relative folders using cached root UIDs, baseline `remote_id` values, then depth-bounded `node.path` resolution. Moves and trash also queue the node's old baseline parent. A changed baseline folder queues its own contents. An unresolved event schedules a full walk; a resolved path outside every pair is logged as `out_of_scope`.

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
- The remote feed queues remote-only changes directly; the safety-net full walk catches anything the feed missed.

Consequences that are intentional:

- Deleting a whole sub-folder locally trashes the remote folder (recoverably) and cleans that sub-folder's descendant rows out of the baseline (the shallow commit only removed the folder's own row, so descendants are pruned explicitly).
- A brand-new remote sub-folder initially appears as an empty local folder. Events for its children queue their parent for shallow reconcile.

## Config knobs

| Key | Meaning |
| --- | --- |
| `full_walk_interval` | Safety-net full-walk interval (seconds; default 86400). |
| `scan_interval` | How often the hot-folder pass runs (seconds). |
| `debounce` | Filesystem-event debounce window (seconds). |
| `scan_threads` | Concurrent remote-scan workers (0 = auto, capped at 8). |
| `download_threads` | Concurrent downloads (0 = auto, kept modest). |
| `propagate_deletes` | Whether deletions cross to the other side. |
| `local_delete` | `trash` (recoverable) or `remove` (permanent) for local deletes. |
| `conflict` | `keep-both` (default), `newer`, or `skip`. |
| `compare` | `size+mtime` (default), `size`, or `sha1`. |
