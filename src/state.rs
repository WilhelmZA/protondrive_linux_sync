//! Per-pair baseline snapshot persistence.
//!
//! The baseline records what the two sides looked like the last time they
//! agreed. The engine diffs the current trees against it to tell a creation
//! from a deletion - the crux of a real bidirectional sync.
//!
//! Storage lives in the SQLite `stats.db` (the `baseline` + `pair_state`
//! tables) alongside the activity feed and hot-folder stats. Legacy
//! `baselines/*.json` files are imported once, then set aside.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::models::Entry;
use crate::stats::Stats;

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct EntryRec {
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
    sha1: Option<String>,
    remote_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct BaselineFile {
    version: u32,
    entries: BTreeMap<String, EntryRec>,
}

/// Legacy JSON baseline path (kept only for the one-time import).
pub fn baseline_path(state_dir: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    state_dir.join("baselines").join(format!("{safe}.json"))
}

/// Read a legacy JSON baseline, if present and valid.
fn read_json_baseline(state_dir: &Path, name: &str) -> Option<BTreeMap<String, Entry>> {
    let path = baseline_path(state_dir, name);
    let text = std::fs::read_to_string(&path).ok()?;
    let bf: BaselineFile = serde_json::from_str(&text).ok()?;
    if bf.version != SCHEMA_VERSION {
        return None;
    }
    let mut out = BTreeMap::new();
    for (rel, r) in bf.entries {
        out.insert(
            rel.clone(),
            Entry {
                path: rel,
                is_dir: r.is_dir,
                size: r.size,
                mtime: r.mtime,
                sha1: r.sha1,
                remote_id: r.remote_id,
            },
        );
    }
    Some(out)
}

pub fn load_baseline(state_dir: &Path, name: &str) -> Result<BTreeMap<String, Entry>> {
    let stats = Stats::open(state_dir)?;
    // One-time migration: if the DB has nothing for this pair but a legacy JSON
    // baseline exists, import it (and stamp last_synced from the file's mtime so
    // the pair isn't mislabelled as "never synced"), then set the JSON aside.
    if !stats.has_baseline(name)? {
        if let Some(entries) = read_json_baseline(state_dir, name) {
            if !entries.is_empty() {
                stats.save_baseline(name, &entries)?;
                let path = baseline_path(state_dir, name);
                if let Ok(meta) = std::fs::metadata(&path) {
                    if let Ok(mt) = meta.modified() {
                        if let Ok(dur) = mt.duration_since(std::time::UNIX_EPOCH) {
                            let _ = stats.set_last_synced(name, dur.as_secs() as i64);
                        }
                    }
                }
                let _ = std::fs::rename(&path, path.with_extension("json.imported"));
                return Ok(entries);
            }
        }
    }
    stats.load_baseline(name)
}

/// Read without creating a DB, migrating legacy JSON, or touching WAL/SHM files.
pub fn load_baseline_read_only(state_dir: &Path, name: &str) -> Result<BTreeMap<String, Entry>> {
    let entries = if state_dir.join("stats.db").exists() {
        Stats::copied_baseline(state_dir, name)?
    } else {
        BTreeMap::new()
    };
    if entries.is_empty() {
        Ok(read_json_baseline(state_dir, name).unwrap_or(entries))
    } else {
        Ok(entries)
    }
}

pub fn save_baseline(
    state_dir: &Path,
    name: &str,
    entries: &BTreeMap<String, Entry>,
) -> Result<()> {
    Stats::open(state_dir)?.save_baseline(name, entries)
}

/// Per-file states for `name` (rel -> synced/pending_down/pending_up).
pub fn baseline_states(
    state_dir: &Path,
    name: &str,
) -> Result<std::collections::HashMap<String, String>> {
    Stats::open(state_dir)?.baseline_states(name)
}

/// Additive per-file baseline commit (see [`Stats::commit_baseline`]).
pub fn commit_baseline(
    state_dir: &Path,
    name: &str,
    entries: &BTreeMap<String, Entry>,
    removed: &std::collections::HashSet<String>,
) -> Result<()> {
    Stats::open(state_dir)?.commit_baseline(name, entries, removed)
}

/// Last successful sync time for `name`, if the pair has ever synced.
pub fn last_synced(state_dir: &Path, name: &str) -> Result<Option<i64>> {
    Stats::open(state_dir)?.last_synced(name)
}

/// Record `name`'s last successful sync time.
pub fn set_last_synced(state_dir: &Path, name: &str, ts: i64) -> Result<()> {
    Stats::open(state_dir)?.set_last_synced(name, ts)
}
