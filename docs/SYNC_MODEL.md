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

## Deletions held back

Deleting is the one action a sync cannot undo on its own, so each run of a pair may delete only so many files before it stops and asks. The allowance for a run is `min(max_delete_files, max(25, tracked * max_delete_percent / 100))`, where `tracked` is the number of files in the pair's baseline when the run starts. The defaults are `max_delete_files = 200` and `max_delete_percent = 30`, so a folder of 1,000 files may lose 200 in one run and a folder of 100 files may lose 30; the floor of 25 lets a small folder be emptied without a prompt. Setting one option to 0 drops only its own term; both at 0 switch the guard off. (`delete_allowance` in `engine.rs`.)

What counts: a file delete counts one, a folder delete counts every baseline file under it, and a move or rename counts nothing. Both directions count against the same allowance, files that would leave this computer and files that would leave Proton Drive. The whole plan is judged at once: either every delete fits or none of them runs. On the folder-by-folder walks (the hot pass, a batch of folders, the streaming full walk) every worker shares one allowance for the run, each folder reserves all of its deletes or none, and the first refusal holds every later folder too. The streaming walk also counts what is missing locally before its first folder, so a vanished sub-tree holds the run up front instead of after the first 200 files.

When a run holds deletions back it still does everything else it planned (uploads, downloads, new folders), leaves every baseline row in place so the next run sees the same deletions again, stores the hold in `stats.db` (table `held_deletes`), logs an error naming the pair, the counts and the side, and `neutronsync sync` exits 1. The window shows a banner on the Activity page and the folder card says "N deletions held back". The two answers:

- **Restore them** (`neutronsync sync --resync NAME`): merge both sides afresh, deleting nothing, which brings the missing files back from the side that still has them.
- **Delete them** (`neutronsync sync --allow-mass-delete NAME`): switch the guard off for that one run of that pair and let the deletions through. The flag needs a pair name; it never applies to every pair at once, and it is never a config value.

The stored hold is cleared only by a run that covers the whole pair, runs to the end, is not cancelled, saw both sides in full, and held nothing back. Hot passes, scoped runs, shallow batches and dry runs never clear it. A dry run reports what it would hold and writes nothing.

## Conflicts

A conflict (changed on both sides since the baseline) is settled by the `conflict` policy and then recorded in `stats.db` (table `conflicts`) once the settling transfer has actually completed: `keep-both` after the copy exists on both sides, `newer` after the upload or download of the winner, `skip` at plan time. A failed transfer records nothing and the conflict comes back next run. A `skip` conflict that repeats every run keeps one row with a count of how often it was seen, until it is dismissed. The window lists them on the Conflicts page. Note that `newer` replaces the losing copy without keeping a local backup of it.

## Sync state file

`stats.db` carries `PRAGMA user_version` (2 in this build). A file saved by a newer NeutronSync is refused with a message saying so, unchanged. A file SQLite cannot read (random bytes, truncated, a damaged page) is an error, never an empty baseline, so it can never be mistaken for "nothing was ever synced" and turn into a mass change.

## Config knobs

| Key | Meaning |
| --- | --- |
| `full_walk_interval` | Safety-net full-walk interval (seconds; default 86400). |
| `scan_interval` | How often the hot-folder pass runs (seconds). |
| `debounce` | Filesystem-event debounce window (seconds). |
| `scan_threads` | Concurrent remote-scan workers (0 = auto, capped at 8). |
| `download_threads` | Concurrent downloads (0 = auto, kept modest). |
| `propagate_deletes` | Whether deletions cross to the other side. |
| `max_delete_files` | Most files one run may delete from one pair (default 200; 0 = no cap). |
| `max_delete_percent` | Share of a pair's tracked files one run may delete, never below 25 files (default 30; 0 = no share limit). |
| `local_delete` | `trash` (recoverable) or `remove` (permanent) for local deletes. |
| `conflict` | `keep-both` (default), `newer`, or `skip`. |
| `compare` | `size+mtime` (default), `size`, or `sha1`. |
