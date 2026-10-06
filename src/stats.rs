//! Change-frequency stats, backed by SQLite, used to find "hot" folders so the
//! watcher can scan busy subtrees more often than the full tree.
//!
//! Each local/remote change to a file bumps two folders: the file's containing
//! subfolder AND its direct parent (a change deep in a tree keeps both the leaf
//! and one level up warm). `hot_folders` then returns the folders with the most
//! recent activity, which the tiered scanner re-lists on the short interval.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::Entry;

pub struct Stats {
    conn: Mutex<Connection>,
}

/// A recorded file operation, for restoring the activity feed on launch.
#[derive(Clone, Debug)]
pub struct OpRecord {
    pub ts: i64,
    pub pair: String,
    pub action: String,
    pub path: String,
    pub ok: bool,
    /// Why it failed, when it did. `None` for successful ops.
    pub error: Option<String>,
}

impl Stats {
    /// Read a disposable copy, including committed WAL rows. Opening the source
    /// even with SQLITE_OPEN_READ_ONLY can create/update its shared-memory file.
    /// Never initialise/migrate schema or remove pending rows on this path.
    pub(crate) fn copied_baseline(state_dir: &Path, pair: &str) -> Result<BTreeMap<String, Entry>> {
        let scratch = tempfile::tempdir()?;
        let copy = scratch.path().join("stats.db");
        std::fs::copy(state_dir.join("stats.db"), &copy)?;
        let wal = state_dir.join("stats.db-wal");
        match std::fs::copy(&wal, scratch.path().join("stats.db-wal")) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let stats = Self {
            conn: Mutex::new(Connection::open(&copy)?),
        };
        stats.load_baseline(pair)
    }

    /// Load the last successfully applied event cursor for a scope.
    pub fn feed_cursor(&self, scope: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT event_id FROM feed_cursor WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn save_feed_cursor(&self, scope: &str, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO feed_cursor(scope,event_id) VALUES (?1,?2) ON CONFLICT(scope) DO UPDATE SET event_id=excluded.event_id", [scope,id])?;
        Ok(())
    }

    /// Open (creating if needed) the stats DB under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir).ok();
        let path = state_dir.join("stats.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("opening stats db {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS events (
                 pair   TEXT    NOT NULL,
                 folder TEXT    NOT NULL,
                 ts     INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_pair_ts ON events(pair, ts);
             CREATE TABLE IF NOT EXISTS ops (
                 pair   TEXT    NOT NULL,
                 action TEXT    NOT NULL,
                 path   TEXT    NOT NULL,
                 ok     INTEGER NOT NULL,
                 ts     INTEGER NOT NULL,
                 error  TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_ops_ts ON ops(ts);
             CREATE TABLE IF NOT EXISTS baseline (
                 pair      TEXT    NOT NULL,
                 rel       TEXT    NOT NULL,
                 is_dir    INTEGER NOT NULL,
                 size      INTEGER NOT NULL,
                 mtime     INTEGER,
                 sha1      TEXT,
                 remote_id TEXT,
                 state     TEXT    NOT NULL DEFAULT 'synced',
                 PRIMARY KEY (pair, rel)
             );
             CREATE TABLE IF NOT EXISTS feed_cursor (
                 scope TEXT PRIMARY KEY,
                 event_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pair_state (
                 pair        TEXT PRIMARY KEY,
                 last_synced INTEGER
             );",
        )
        .context("initialising stats schema")?;
        // Migrate older DBs that predate the per-file state column. Existing
        // rows were fully synced, so 'synced' is the right default.
        let _ = conn.execute(
            "ALTER TABLE baseline ADD COLUMN state TEXT NOT NULL DEFAULT 'synced'",
            [],
        );
        // Migrate DBs whose ops table predates the error column. Older failed
        // rows keep a NULL reason: the message was never recorded, and the only
        // copy is the text log.
        let _ = conn.execute("ALTER TABLE ops ADD COLUMN error TEXT", []);
        // Drop any rows a previous build left in a transient "pending" state:
        // their stored metadata reflected a transfer that had not completed, so
        // it can't be trusted. Deleting them makes the next run re-detect and
        // re-sync those files safely (never as a deletion).
        let _ = conn.execute(
            "DELETE FROM baseline WHERE state IN ('pending_up', 'pending_down')",
            [],
        );
        Ok(Stats {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory DB, for tests.
    #[cfg(test)]
    fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE events (pair TEXT NOT NULL, folder TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE ops (pair TEXT NOT NULL, action TEXT NOT NULL, path TEXT NOT NULL, ok INTEGER NOT NULL, ts INTEGER NOT NULL, error TEXT);",
        )?;
        Ok(Stats {
            conn: Mutex::new(conn),
        })
    }

    /// Record a change to `rel_file` (POSIX path relative to the pair root) at
    /// epoch `ts`. Bumps the containing subfolder and its direct parent.
    pub fn record_change(&self, pair: &str, rel_file: &str, ts: i64) -> Result<()> {
        let folder = folder_of(rel_file);
        let conn = self.conn.lock().unwrap();
        insert(&conn, pair, folder, ts)?;
        if let Some(parent) = parent_of(folder) {
            insert(&conn, pair, parent, ts)?;
        }
        Ok(())
    }

    /// Folders for `pair` with at least `threshold` changes since `now - window`,
    /// hottest first. Folder "" is the pair root.
    pub fn hot_folders(
        &self,
        pair: &str,
        window_secs: i64,
        threshold: i64,
        now: i64,
    ) -> Result<Vec<String>> {
        let since = now - window_secs;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT folder, COUNT(*) c FROM events
             WHERE pair = ?1 AND ts >= ?2
             GROUP BY folder HAVING c >= ?3
             ORDER BY c DESC, folder ASC",
        )?;
        let rows = stmt.query_map(params![pair, since, threshold], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Drop events older than `now - keep_secs` so the DB doesn't grow forever.
    pub fn prune(&self, keep_secs: i64, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM events WHERE ts < ?1", params![now - keep_secs])?;
        Ok(())
    }

    /// Persist a completed file operation for the activity history. `error` is
    /// the reason a failed op failed, so "what broke and why" is one query.
    pub fn record_op(
        &self,
        pair: &str,
        action: &str,
        path: &str,
        ok: bool,
        error: Option<&str>,
        ts: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ops(pair, action, path, ok, error, ts) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![pair, action, path, ok as i64, error, ts],
        )?;
        Ok(())
    }

    /// The most recent `limit` operations, newest first.
    pub fn recent_ops(&self, limit: usize) -> Result<Vec<OpRecord>> {
        self.query_ops("SELECT ts, pair, action, path, ok, error FROM ops", limit)
    }

    /// The most recent `limit` FAILED operations, newest first. This is the
    /// "what is broken right now" query.
    pub fn recent_failures(&self, limit: usize) -> Result<Vec<OpRecord>> {
        self.query_ops(
            "SELECT ts, pair, action, path, ok, error FROM ops WHERE ok = 0",
            limit,
        )
    }

    fn query_ops(&self, select: &str, limit: usize) -> Result<Vec<OpRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{select} ORDER BY rowid DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(OpRecord {
                ts: r.get(0)?,
                pair: r.get(1)?,
                action: r.get(2)?,
                path: r.get(3)?,
                ok: r.get::<_, i64>(4)? != 0,
                error: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Keep only the newest `keep` operation rows.
    pub fn prune_ops(&self, keep: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ops WHERE rowid NOT IN (SELECT rowid FROM ops ORDER BY rowid DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        Ok(())
    }

    // -- sync baseline (per-pair three-way-merge snapshot) ------------------

    /// The stored baseline for `pair` (empty map if none), keyed by POSIX rel path.
    pub fn load_baseline(&self, pair: &str) -> Result<BTreeMap<String, Entry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rel, is_dir, size, mtime, sha1, remote_id FROM baseline WHERE pair = ?1",
        )?;
        let rows = stmt.query_map(params![pair], |r| {
            let rel: String = r.get(0)?;
            Ok(Entry {
                path: rel,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get::<_, Option<i64>>(3)?,
                sha1: r.get::<_, Option<String>>(4)?,
                remote_id: r.get::<_, Option<String>>(5)?,
            })
        })?;
        let mut out = BTreeMap::new();
        for e in rows {
            let e = e?;
            out.insert(e.path.clone(), e);
        }
        Ok(out)
    }

    /// Replace the whole baseline for `pair` atomically.
    pub fn save_baseline(&self, pair: &str, entries: &BTreeMap<String, Entry>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM baseline WHERE pair = ?1", params![pair])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO baseline(pair, rel, is_dir, size, mtime, sha1, remote_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (rel, e) in entries {
                ins.execute(params![
                    pair,
                    rel,
                    e.is_dir as i64,
                    e.size as i64,
                    e.mtime,
                    e.sha1,
                    e.remote_id
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Per-file state for `pair` (rel -> "synced" | "pending_down" | "pending_up").
    pub fn baseline_states(&self, pair: &str) -> Result<HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT rel, state FROM baseline WHERE pair = ?1")?;
        let rows = stmt.query_map(params![pair], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut m = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            m.insert(k, v);
        }
        Ok(m)
    }

    /// Incremental, additive baseline commit: upsert every entry as `synced`
    /// and drop the rows for files that were genuinely deleted/renamed away.
    /// Only files whose transfer SUCCEEDED this run are present in `entries`
    /// (the caller omits failed ones), so a failed transfer leaves its previous
    /// row intact and the file is simply retried next run. Unlisted/unseen rows
    /// are never touched — never a wholesale replace — so a partial scan is safe.
    pub fn commit_baseline(
        &self,
        pair: &str,
        entries: &BTreeMap<String, Entry>,
        removed: &HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO baseline(pair, rel, is_dir, size, mtime, sha1, remote_id, state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'synced')
                 ON CONFLICT(pair, rel) DO UPDATE SET
                   is_dir=?3, size=?4, mtime=?5, sha1=?6, remote_id=?7, state='synced'",
            )?;
            for (rel, e) in entries {
                ins.execute(params![
                    pair,
                    rel,
                    e.is_dir as i64,
                    e.size as i64,
                    e.mtime,
                    e.sha1,
                    e.remote_id,
                ])?;
            }
            let mut del = tx.prepare("DELETE FROM baseline WHERE pair = ?1 AND rel = ?2")?;
            for rel in removed {
                del.execute(params![pair, rel])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether `pair` has any baseline rows (i.e. it has synced before).
    pub fn has_baseline(&self, pair: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM baseline WHERE pair = ?1",
            params![pair],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Last successful sync time for `pair`, if any.
    pub fn last_synced(&self, pair: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT last_synced FROM pair_state WHERE pair = ?1",
                params![pair],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Drop every row for `pair` (baseline, per-file state, hotness, activity).
    /// Called when a folder is removed so a later folder of the same name can't
    /// inherit stale baseline state.
    pub fn forget_pair(&self, pair: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for tbl in ["baseline", "pair_state", "events", "ops"] {
            conn.execute(&format!("DELETE FROM {tbl} WHERE pair = ?1"), params![pair])?;
        }
        Ok(())
    }

    /// Wipe all NeutronSync metadata (every table). Local files are untouched.
    pub fn wipe(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM baseline; DELETE FROM pair_state; DELETE FROM events; DELETE FROM ops;",
        )?;
        Ok(())
    }

    /// Record `pair`'s last successful sync time.
    pub fn set_last_synced(&self, pair: &str, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pair_state(pair, last_synced) VALUES(?1, ?2)
             ON CONFLICT(pair) DO UPDATE SET last_synced = ?2",
            params![pair, ts],
        )?;
        Ok(())
    }
}

fn insert(conn: &Connection, pair: &str, folder: &str, ts: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO events(pair, folder, ts) VALUES(?1, ?2, ?3)",
        params![pair, folder, ts],
    )?;
    Ok(())
}

/// The folder containing `rel_file` ("" for a file at the pair root).
fn folder_of(rel_file: &str) -> &str {
    match rel_file.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// The direct parent of `folder`, or None if `folder` is already the root.
fn parent_of(folder: &str) -> Option<&str> {
    if folder.is_empty() {
        None
    } else {
        Some(match folder.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_and_parent() {
        assert_eq!(folder_of("a/b/c.txt"), "a/b");
        assert_eq!(folder_of("c.txt"), "");
        assert_eq!(parent_of("a/b"), Some("a"));
        assert_eq!(parent_of("a"), Some(""));
        assert_eq!(parent_of(""), None);
    }

    #[test]
    fn records_subfolder_and_parent() {
        let s = Stats::memory().unwrap();
        // one change deep in a/b -> bumps "a/b" and "a"
        s.record_change("p", "a/b/c.txt", 100).unwrap();
        let hot = s.hot_folders("p", 1000, 1, 200).unwrap();
        assert!(hot.contains(&"a/b".to_string()));
        assert!(hot.contains(&"a".to_string()));
        assert!(!hot.contains(&"".to_string()));
    }

    #[test]
    fn hotness_threshold_and_window() {
        let s = Stats::memory().unwrap();
        for _ in 0..3 {
            s.record_change("p", "docs/x.txt", 100).unwrap(); // folder "docs" x3, root x3
        }
        s.record_change("p", "misc/y.txt", 100).unwrap(); // folder "misc" x1
                                                          // threshold 3 in a wide window -> only "docs" and "" (root, 4 events) qualify
        let hot = s.hot_folders("p", 10_000, 3, 200).unwrap();
        assert!(hot.contains(&"docs".to_string()));
        assert!(hot.contains(&"".to_string())); // root accumulated 4
        assert!(!hot.contains(&"misc".to_string()));
        // outside the window -> nothing
        let old = s.hot_folders("p", 10, 1, 1_000_000).unwrap();
        assert!(old.is_empty());
    }
}
