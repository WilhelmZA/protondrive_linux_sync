//! Feed-driven watcher. The CLI loop remains in the parent module.
use super::*;
use crate::changefeed::{EventBatch, FeedMessage, RemoteChangeFeed, RemoteEvent};
use crate::models::Entry;
use std::collections::{BTreeMap, VecDeque};

const SCOPE: &str = "/my-files";

#[derive(Default, Debug)]
struct Mapping {
    folders: BTreeSet<(usize, String)>,
    walks: BTreeSet<usize>,
}

fn relative(root: &str, path: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    if path == root {
        return Some(String::new());
    }
    path.strip_prefix(&format!("{root}/")).map(String::from)
}

fn map_event(
    event: &RemoteEvent,
    pairs: &[Pair],
    roots: &[String],
    baselines: &[BTreeMap<String, Entry>],
    feed: &dyn RemoteChangeFeed,
) -> Mapping {
    let mut result = Mapping::default();
    for (i, pair) in pairs.iter().enumerate() {
        let baseline = &baselines[i];
        let old = baseline
            .iter()
            .find(|(_, e)| e.remote_id.as_deref() == Some(&event.node_uid));
        if let Some((path, entry)) = old {
            result.folders.insert((i, folder_of(path)));
            if entry.is_dir {
                result.folders.insert((i, path.clone()));
            }
        }
        let uid = event.parent_uid.as_deref().unwrap_or(&event.node_uid);
        if uid == roots[i] {
            result.folders.insert((i, String::new()));
        } else if let Some((path, entry)) = baseline
            .iter()
            .find(|(_, e)| e.remote_id.as_deref() == Some(uid))
        {
            result.folders.insert((
                i,
                if entry.is_dir {
                    path.clone()
                } else {
                    folder_of(path)
                },
            ));
        } else {
            match feed.node_path(uid) {
                Ok(path) => {
                    if let Some(rel) = relative(&pair.remote, &path) {
                        result.folders.insert((
                            i,
                            if event.parent_uid.is_some() {
                                rel
                            } else {
                                folder_of(&rel)
                            },
                        ));
                    }
                }
                Err(_) => {
                    result.walks.insert(i);
                }
            }
        }
    }
    result
}

fn save_ack(
    stats: &Stats,
    feed: &dyn RemoteChangeFeed,
    scope: &str,
    id: Option<&str>,
) -> Result<()> {
    if let Some(id) = id {
        stats.save_feed_cursor(SCOPE, id)?;
        feed.ack(scope, id)?;
    }
    Ok(())
}

fn deferred(batch: &EventBatch, log: &Logger) {
    for event in &batch.events {
        log.info(&format!(
            "watch: event uid={} disposition=deferred",
            event.node_uid
        ));
    }
    log.info(&format!(
        "watch: batch id={:?} dispositions={{mapped:0,out_of_scope:0,walk:0,deferred:{}}}",
        batch.last_event_id,
        batch.events.len()
    ));
}

fn full_walk(
    cfg: &Config,
    pairs: &[Pair],
    indices: &BTreeSet<usize>,
    cause: &str,
    log: &Logger,
    stop: &AtomicBool,
    events: Option<&dyn EventSink>,
) -> bool {
    let names: Vec<_> = indices.iter().map(|&i| pairs[i].name.as_str()).collect();
    log.info(&format!("watch: full walk (cause={cause}) pairs={names:?}"));
    let live = reload_cfg(cfg, log);
    let mut success = true;
    for &i in indices {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let Some(pair) = live.pairs.iter().find(|p| p.name == pairs[i].name) else {
            return false;
        };
        let result = run_sync_streaming(&live, pair, log, events, Some(stop));
        success &= result.errors == 0;
    }
    log.info(&format!("watch: full walk done (success={success})"));
    success && !stop.load(Ordering::Relaxed)
}

pub(super) fn watch(
    cfg: &Config,
    pairs: &[Pair],
    log: &Logger,
    stop: &AtomicBool,
    events: Option<&dyn EventSink>,
    feed: &dyn RemoteChangeFeed,
    rx: &mpsc::Receiver<Msg>,
) -> Result<()> {
    let stats = Stats::open(&cfg.state_dir)?;
    let all: BTreeSet<usize> = (0..pairs.len()).collect();
    let mut roots = Vec::new();
    let mut scope = String::new();
    let mut connected = false;
    let mut signed_out = false;
    let mut probe_at = 0;
    let mut next_full = now_epoch() + cfg.full_walk_interval as i64;
    let mut batches = VecDeque::<EventBatch>::new();
    let mut acked: Option<String> = None;
    let mut pending = BTreeSet::new();
    let mut retry = (String::new(), 0usize);
    let mut refresh_failed = false;
    let mut next_hot = now_epoch() + cfg.scan_interval_secs.max(15) as i64;
    log.info("watch: remote change feed enabled");
    while !stop.load(Ordering::Relaxed) {
        if !connected {
            if now_epoch() < probe_at && !crate::auth_signal::take(&cfg.state_dir) {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            probe_at = now_epoch() + AUTH_REPROBE_SECS;
            match feed.signed_in() {
                Ok(true) => {}
                Ok(false) => {
                    if !signed_out {
                        note_auth(events, log, false);
                        signed_out = true;
                    }
                    continue;
                }
                Err(e) => {
                    log.warn(&format!("watch: auth.status failed: {e}"));
                    continue;
                }
            }
            let stored = stats.feed_cursor(SCOPE)?;
            // A reconnect replays from the durable cursor; account for queued old deliveries.
            while let Some(batch) = batches.pop_front() {
                deferred(&batch, log);
            }
            while let Ok(Some(message)) = feed.next_batch() {
                if let FeedMessage::Batch(batch) = message {
                    deferred(&batch, log);
                }
            }
            let (subscription, cause) = match feed.subscribe(SCOPE, stored.as_deref()) {
                Ok(s) => (
                    s,
                    if stored.is_none() {
                        Some("first_start")
                    } else {
                        None
                    },
                ),
                Err(e) => {
                    // Only a refused cursor justifies a gap walk, not a network or auth failure.
                    let refused = e
                        .downcast_ref::<crate::driveapi::RpcError>()
                        .is_some_and(|e| {
                            matches!(e.code.as_str(), "not_found" | "conflict" | "fatal")
                        });
                    if stored.is_some() && refused {
                        match feed.subscribe(SCOPE, None) {
                            Ok(s) => (s, Some("resume_gap")),
                            Err(e) => {
                                log.warn(&format!("watch: subscribe failed: {e}"));
                                continue;
                            }
                        }
                    } else {
                        log.warn(&format!("watch: subscribe failed: {e}"));
                        continue;
                    }
                }
            };
            // A pair whose remote folder does not exist yet (never synced) must
            // not block the other pairs. Its empty root never matches a uid, so
            // its events map through node_path, and a walk creates the folder.
            let mut missing_roots = BTreeSet::new();
            let mut resolved = Vec::new();
            let mut root_error = None;
            for (i, p) in pairs.iter().enumerate() {
                match feed.resolve_root(&p.remote) {
                    Ok(uid) => resolved.push(uid),
                    Err(e)
                        if e.downcast_ref::<crate::driveapi::RpcError>()
                            .is_some_and(|e| e.code == "not_found") =>
                    {
                        log.info(&format!(
                            "watch: remote folder for {:?} does not exist yet; a walk will create it",
                            p.name
                        ));
                        missing_roots.insert(i);
                        resolved.push(String::new());
                    }
                    Err(e) => {
                        root_error = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = root_error {
                log.warn(&format!("watch: root resolution failed: {e}"));
                continue;
            }
            roots = resolved;
            scope = subscription.scope_id;
            refresh_failed = false;
            if let Some(cause) = cause {
                if !full_walk(cfg, pairs, &all, cause, log, stop, events) {
                    continue;
                }
                if let Err(e) =
                    save_ack(&stats, feed, &scope, subscription.last_event_id.as_deref())
                {
                    log.warn(&format!("watch: cursor acknowledgement failed: {e}"));
                    continue;
                }
            }
            if cause.is_none()
                && !missing_roots.is_empty()
                && !full_walk(
                    cfg,
                    pairs,
                    &missing_roots,
                    "missing_root",
                    log,
                    stop,
                    events,
                )
            {
                continue;
            }
            if signed_out {
                note_auth(events, log, true);
                signed_out = false;
            }
            connected = true;
            next_full = now_epoch() + cfg.full_walk_interval as i64;
        }

        let mut refresh = false;
        loop {
            match feed.next_batch() {
                Ok(Some(FeedMessage::SignedOut)) => {
                    if !signed_out {
                        note_auth(events, log, false);
                    }
                    signed_out = true;
                    connected = false;
                    probe_at = now_epoch() + AUTH_REPROBE_SECS;
                    continue;
                }
                Ok(Some(FeedMessage::Refresh { scope_id, reason })) => {
                    if scope_id == scope
                        && matches!(
                            reason.as_str(),
                            "tree_refresh" | "tree_remove" | "fast_forward"
                        )
                    {
                        refresh = true;
                    }
                }
                Ok(Some(FeedMessage::Batch(batch))) => {
                    // The sidecar re-sends an unacknowledged batch on every poll, so
                    // a long walk queues many copies. Keep one, and skip any batch
                    // already acknowledged.
                    let duplicate = batches
                        .iter()
                        .any(|b| b.last_event_id == batch.last_event_id)
                        || (batch.last_event_id.is_some() && batch.last_event_id == acked);
                    if batch.scope_id == scope && !duplicate {
                        batches.push_back(batch);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log.warn(&format!("watch: feed disconnected: {e}"));
                    connected = false;
                    probe_at = now_epoch() + 1;
                    break;
                }
            }
        }
        if !connected {
            while let Some(batch) = batches.pop_front() {
                deferred(&batch, log);
            }
            continue;
        }
        let mut walk_ok = !refresh_failed;
        if refresh || now_epoch() >= next_full {
            walk_ok = full_walk(
                cfg,
                pairs,
                &all,
                if refresh {
                    "refresh_required"
                } else {
                    "safety_net"
                },
                log,
                stop,
                events,
            );
            next_full = now_epoch() + cfg.full_walk_interval as i64;
            refresh_failed = !walk_ok;
        }
        while let Ok(msg) = rx.try_recv() {
            if let Msg::Sub { pair, sub } = msg {
                pending.insert((pair, sub));
            }
        }
        if now_epoch() >= next_hot {
            for (i, pair) in pairs.iter().enumerate() {
                for folder in
                    stats.hot_folders(&pair.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now_epoch())?
                {
                    pending.insert((i, folder));
                }
            }
            next_hot = now_epoch() + cfg.scan_interval_secs.max(15) as i64;
        }
        let batch = batches.pop_front();
        let mut walks = BTreeSet::new();
        let mut dispositions = Vec::new();
        if let Some(batch) = &batch {
            let baselines = pairs
                .iter()
                .map(|p| stats.load_baseline(&p.name))
                .collect::<Result<Vec<_>>>()?;
            for event in &batch.events {
                let mapping = map_event(event, pairs, &roots, &baselines, feed);
                let disposition = if !mapping.walks.is_empty() {
                    "walk"
                } else if !mapping.folders.is_empty() {
                    "mapped"
                } else {
                    "out_of_scope"
                };
                dispositions.push((event.node_uid.clone(), disposition));
                pending.extend(mapping.folders);
                walks.extend(mapping.walks);
            }
        }
        if !walks.is_empty() && !refresh {
            walk_ok &= full_walk(cfg, pairs, &walks, "refresh_required", log, stop, events);
            pending.retain(|(i, _)| !walks.contains(i));
        }
        let live = reload_cfg(cfg, log);
        let mut success = walk_ok;
        for (i, original) in pairs.iter().enumerate() {
            let folders: Vec<_> = pending
                .iter()
                .filter(|(pair, _)| *pair == i)
                .map(|(_, folder)| folder.clone())
                .collect();
            if folders.is_empty() {
                continue;
            }
            let Some(pair) = live.pairs.iter().find(|p| p.name == original.name) else {
                success = false;
                continue;
            };
            log.info(&format!(
                "watch: change -> shallow sync {:?} {folders:?}",
                pair.name
            ));
            let result = run_sync_shallow_many(&live, pair, &folders, log, events, Some(stop));
            success &= result.errors == 0;
        }
        pending.clear();
        success &= !stop.load(Ordering::Relaxed);
        if let Some(batch) = batch {
            let id = batch.last_event_id.as_deref().unwrap_or("");
            if !success {
                if retry.0 == id {
                    retry.1 += 1;
                } else {
                    retry = (id.into(), 1);
                }
                if retry.1 == 3 {
                    // The bounded recovery walk is the final attempt for this delivery.
                    full_walk(cfg, pairs, &all, "apply_failed", log, stop, events);
                    success = !stop.load(Ordering::Relaxed);
                    refresh_failed = false;
                }
            }
            if success {
                match save_ack(&stats, feed, &scope, batch.last_event_id.as_deref()) {
                    Ok(()) => {
                        retry = (String::new(), 0);
                        acked = batch.last_event_id.clone();
                        // Drop queued copies of the batch that was just acknowledged.
                        batches.retain(|b| b.last_event_id != acked);
                    }
                    Err(e) => {
                        log.warn(&format!("watch: cursor acknowledgement failed: {e}"));
                        success = false;
                        connected = false;
                        probe_at = now_epoch() + 1;
                    }
                }
            }
            let mut counts = BTreeMap::from([
                ("mapped", 0usize),
                ("out_of_scope", 0),
                ("walk", 0),
                ("deferred", 0),
            ]);
            for (uid, disposition) in dispositions {
                let disposition = if success { disposition } else { "deferred" };
                *counts.entry(disposition).or_default() += 1;
                log.info(&format!("watch: event uid={uid} disposition={disposition}"));
            }
            log.info(&format!("watch: batch id={id} dispositions={counts:?}"));
        }
        thread::sleep(Duration::from_millis(500));
    }
    log.info("watch: stopped");
    Ok(())
}
