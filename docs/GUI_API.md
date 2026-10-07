# GUI backend API (`neutronsync::service`)

The backend for a rich GUI. The frontend stays thin: hold one `Controller`, read a cheap `snapshot()` each frame, and issue non-blocking commands. All slow work runs on background threads; engine events become observable state.

## Lifecycle

```rust
use neutronsync::service::{Controller, Phase, ActivityKind};

// once, in App::new:
let cfg = neutronsync::config::load(None).unwrap_or_else(|_| /* starter */);
let controller = Controller::new(cfg);   // kicks off an account check

// each frame (egui):
let s = controller.snapshot();           // cheap clone of AppState
// ...render s...
if s.busy || s.watching {
    ctx.request_repaint_after(std::time::Duration::from_millis(150));
}
```

## Reading state — `AppState`

```rust
struct AppState {
    pairs: Vec<PairState>,
    account: AccountState,
    activity: VecDeque<ActivityItem>,   // newest at the back, capped at 500
    busy: bool,                          // a sync is running
    watching: bool,                      // watch daemon active
}

struct PairState {
    name, local, remote: String,
    phase: Phase,                        // Idle | Scanning | Syncing | Synced | Error
    progress: Progress,                  // .done / .total, .fraction() -> f32 for a bar
    scanned_folders: usize,              // live count during a remote scan
    current_op: Option<String>,          // e.g. "upload sub/a.txt" — good for a status line
    last_error: Option<String>,
    last_synced: Option<i64>,            // epoch seconds
    tracked: usize,                      // files under management
}

struct AccountState {
    checked: bool,
    backend_ready: bool,   // API: sidecar answered; CLI: binary found on PATH
    signed_in: bool,
    keyring_locked: bool,  // system keyring locked; not a sign-out, no login form
    account: String,       // Proton account name from the API probe (empty on CLI)
    version: String,       // sidecar path/identity (API) or binary version/path (CLI);
                           // on probe failure, the error text for the sign-in page
    checking: bool,        // refresh in flight (Refresh spinner)
}
struct ActivityItem { ts: i64, kind: ActivityKind /*Info|Sync|Error*/, text: String }
```

Rendering hints:
- Per-pair row: name, `phase`, a progress bar from `progress.fraction()`, and `current_op` as a subtitle. Colour by `phase` (Synced=green, Error=red, Scanning/Syncing=accent).
- Activity tab: iterate `activity` (it's already capped), colour by `kind`.
- Account tab: status from `backend_ready` / `signed_in`, show `account` and `version`.

## Commands (all non-blocking)

```rust
controller.sync(vec![], false);              // sync all pairs
controller.sync(vec!["docs".into()], true);  // dry-run just "docs"
controller.cancel();                         // stop the current sync (before next op)
controller.start_watch(vec![]);              // live-sync all pairs
controller.stop_watch();
controller.refresh_account(false);           // re-check login; on API probes the sidecar via auth.status
controller.is_busy();  controller.is_watching();
```

## Editing config

```rust
let mut cfg = controller.config();           // a working copy
cfg.pairs.push(pair);                         // edit it (add/remove pairs, options)
controller.commit_config(cfg);               // store + refresh the pair list
controller.save(&path)?;                      // persist to disk
```

`commit_config` preserves the phase/progress of pairs whose names are unchanged, so the UI doesn't flicker when you tweak options.

## Notes

- `Controller` methods take `&self` (interior mutability), so you can keep it in your `App` by value; no `Arc`/`Mutex` needed on your side.
- Progress `total` is the number of actionable operations for the pair; `done` ticks up per finished op. During a scan, `phase == Scanning` and `scanned_folders` climbs (there's no total for the scan — the CLI can't tell us the tree size up front).
- The watch daemon feeds the same state, so a running watch shows pairs cycling Scanning → Synced with activity entries, and `watching == true`.
- In tray mode the daemon and the window are **separate processes**. The process running the work calls `publish_status()` to write a live snapshot to `status.json`; a window reads it with `Controller::read_status(state_dir)` and renders that instead of its own idle state. This is why an open window reflects what the background daemon is doing. A CLI `neutronsync sync` also records its ops into the shared activity store, so those runs appear in the feed too.
