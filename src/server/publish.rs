//! The state envelope, its revision, and the one task that publishes it.
//!
//! [`Revisions`] is the single source of every envelope: the event stream and
//! `GET /api/v1/state` both take their content and their `rev` from it, so
//! the same `rev` always describes the same state. One background task asks
//! it for the current envelope every 250 ms and hands each new one to every
//! connected event stream through a `watch` channel, so a slow client only
//! ever sees the latest value and no client polls the controller itself.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;

use crate::service::{AppState, Controller};
use crate::stats::{ConflictRecord, HeldDeletes};

/// Activity items the event stream carries (the same number the desktop
/// window's status file keeps).
pub const STREAM_ACTIVITY: usize = 100;
/// Most activity items `GET /api/v1/state` hands out; the feed holds no more.
pub const MAX_ACTIVITY: usize = 500;
const TICK: Duration = Duration::from_millis(250);
/// How often held deletions and conflicts are re-read from the sync state.
const DB_EVERY: Duration = Duration::from_secs(2);

/// Everything a client needs in one object. `rev` grows only when the rest
/// changed, so a client can tell a repeat from an update.
#[derive(Serialize)]
pub struct Envelope {
    pub rev: u64,
    pub state: AppState,
    pub held: Vec<HeldDeletes>,
    pub conflicts: Vec<ConflictRecord>,
    pub auto_sync: bool,
}

/// The envelope without its revision, serialised for comparison. The
/// activity feed is compared whole, so `rev` names one state of the engine
/// whatever share of the feed a response carries.
#[derive(Serialize)]
struct Body<'a> {
    state: &'a AppState,
    held: &'a [HeldDeletes],
    conflicts: &'a [ConflictRecord],
    auto_sync: bool,
}

struct Inner {
    rev: u64,
    last: Vec<u8>,
    held: Vec<HeldDeletes>,
    conflicts: Vec<ConflictRecord>,
    db_read: Option<Instant>,
}

/// The current envelope and its revision, shared by the publisher and the
/// state endpoint. Held deletions and conflicts are read from the sync state
/// at most every two seconds, by whichever caller comes first; both callers
/// see the same copy, so their envelopes never disagree about them.
pub struct Revisions {
    ctrl: Arc<Controller>,
    inner: Mutex<Inner>,
}

impl Revisions {
    pub fn new(ctrl: Arc<Controller>) -> Self {
        Revisions {
            ctrl,
            inner: Mutex::new(Inner {
                rev: 0,
                last: Vec::new(),
                held: Vec::new(),
                conflicts: Vec::new(),
                db_read: None,
            }),
        }
    }

    /// The envelope as it is now, with the full activity feed. Its `rev` is
    /// bumped here when the content differs from the last envelope either
    /// caller saw. Blocking: call it off the async threads.
    pub fn current(&self) -> Envelope {
        let mut inner = self.inner.lock().unwrap();
        if inner.db_read.is_none_or(|t| t.elapsed() >= DB_EVERY) {
            inner.held = self.ctrl.held_deletes();
            inner.conflicts = self.ctrl.conflicts();
            inner.db_read = Some(Instant::now());
        }
        let state = self.ctrl.snapshot();
        let auto_sync = self.ctrl.config().auto_sync;
        let bytes = serde_json::to_vec(&Body {
            state: &state,
            held: &inner.held,
            conflicts: &inner.conflicts,
            auto_sync,
        })
        .unwrap_or_default();
        if inner.rev == 0 || bytes != inner.last {
            inner.rev += 1;
            inner.last = bytes;
        }
        Envelope {
            rev: inner.rev,
            state,
            held: inner.held.clone(),
            conflicts: inner.conflicts.clone(),
            auto_sync,
        }
    }
}

/// One published envelope, already serialised.
pub struct Published {
    pub rev: u64,
    pub json: String,
}

/// Trim the activity feed from the oldest end to at most `keep` items.
pub fn trim_activity(state: &mut AppState, keep: usize) {
    if state.activity.len() > keep {
        let drop = state.activity.len() - keep;
        state.activity.drain(0..drop);
    }
}

/// Start the publisher. The returned receiver hands out the latest envelope;
/// the task stops, and so does every stream fed by it, when `shutdown`
/// becomes true.
pub fn start(
    revisions: Arc<Revisions>,
    mut shutdown: watch::Receiver<bool>,
) -> (watch::Receiver<Arc<Published>>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = watch::channel(Arc::new(Published {
        rev: 0,
        json: String::from("{}"),
    }));
    let task = tokio::spawn(async move {
        let mut sent: u64 = 0;
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
            let r = revisions.clone();
            let Ok(mut envelope) = tokio::task::spawn_blocking(move || r.current()).await else {
                continue;
            };
            if envelope.rev == sent {
                continue;
            }
            sent = envelope.rev;
            trim_activity(&mut envelope.state, STREAM_ACTIVITY);
            if let Ok(json) = serde_json::to_string(&envelope) {
                let _ = tx.send(Arc::new(Published {
                    rev: envelope.rev,
                    json,
                }));
            }
        }
        // Dropping the sender ends every event stream.
        drop(tx);
    });
    (rx, task)
}
