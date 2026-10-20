//! Change-frequency stats, backed by SQLite, used to find "hot" folders so the
//! watcher can scan busy subtrees more often than the full tree.
//!
//! Each local/remote change to a file bumps two folders: the file's containing
//! subfolder AND its direct parent (a change deep in a tree keeps both the leaf
//! and one level up warm). `hot_folders` then returns the folders with the most
//! recent activity, which the tiered scanner re-lists on the short interval.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::models::{ConflictOutcome, Entry};

/// The schema this build writes and the newest it reads. Version 1 is every
/// table that existed before the version stamp; version 2 adds `held_deletes`
/// and `conflicts`. A file stamped higher was written by a newer NeutronSync
/// and is refused rather than misread.
pub const SCHEMA_VERSION: u32 = 2;

/// Dismissed conflicts older than this are pruned when the database opens.
const CONFLICT_KEEP_SECS: i64 = 90 * 86_400;

pub struct Stats {
    conn: Mutex<Connection>,
}

/// A pair whose deletions were held back, as stored in `held_deletes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HeldDeletes {
    pub pair: String,
    /// Files that would have left this computer.
    pub local: usize,
    /// Files that would have left Proton Drive.
    pub remote: usize,
    /// Files the pair tracked when the deletions were first held.
    pub tracked: usize,
    pub since: i64,
    pub updated: i64,
}

/// A settled conflict, as stored in `conflicts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConflictRecord {
    pub id: i64,
    pub pair: String,
    pub path: String,
    pub outcome: ConflictOutcome,
    pub copy: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub seen: i64,
    pub dismissed: bool,
}

/// Refuse a file SQLite would open but could not read whole: not a database
/// at all, or cut short of what its pages describe. `PRAGMA quick_check`
/// reads every page, so it runs once per process for each file (keyed by
/// path and inode, so a file swapped in later is checked again). It never
/// opens the file outside SQLite: closing a plain handle on a database would
/// drop every lock this process holds on it.
fn verify_once(conn: &Connection, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    static VERIFIED: OnceLock<Mutex<HashSet<(PathBuf, u64)>>> = OnceLock::new();
    let inode = std::fs::metadata(path).map(|m| m.ino()).unwrap_or(0);
    let key = (path.to_path_buf(), inode);
    let verified = VERIFIED.get_or_init(|| Mutex::new(HashSet::new()));
    if verified.lock().unwrap().contains(&key) {
        return Ok(());
    }
    quick_check(conn, path)?;
    verified.lock().unwrap().insert(key);
    Ok(())
}

fn quick_check(conn: &Connection, path: &Path) -> Result<()> {
    // Severe damage makes the check itself fail; mild damage returns a verdict.
    let verdict = match conn.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0)) {
        Ok(verdict) => verdict,
        Err(e) => e.to_string(),
    };
    if verdict != "ok" {
        bail!(
            "Sync state file {} is damaged ({verdict}). Move it aside to start over.",
            path.display()
        );
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as u32)
}

/// Refuse a database stamped by a newer build. Changes nothing in the file.
fn check_version(conn: &Connection, path: &Path) -> Result<u32> {
    let version = user_version(conn)?;
    if version > SCHEMA_VERSION {
        bail!(
            "Sync state was saved by a newer NeutronSync (format {version}; this version reads up to {SCHEMA_VERSION}). Update NeutronSync, or move {} aside to start over.",
            path.display()
        );
    }
    Ok(version)
}

/// Everything that existed before the version stamp (schema version 1).
const SCHEMA_V1: &str = "
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
    );";

/// What version 2 adds: held deletions and the conflict inbox.
const SCHEMA_V2: &str = "
    CREATE TABLE IF NOT EXISTS held_deletes (
        pair    TEXT PRIMARY KEY,
        local   INTEGER NOT NULL,
        remote  INTEGER NOT NULL,
        tracked INTEGER NOT NULL,
        since   INTEGER NOT NULL,
        updated INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS conflicts (
        id        INTEGER PRIMARY KEY,
        pair      TEXT    NOT NULL,
        path      TEXT    NOT NULL,
        outcome   TEXT    NOT NULL,
        copy      TEXT,
        first_ts  INTEGER NOT NULL,
        last_ts   INTEGER NOT NULL,
        seen      INTEGER NOT NULL,
        dismissed INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_conflicts_open ON conflicts(pair, path, outcome, dismissed);";

/// Bring a database from before the version stamp up to version 1: the
/// schema as it was, plus the column migrations that used to run on every
/// open, then the pending-row cleanup those builds needed.
fn migrate_to_1(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V1)?;
    // Older DBs predate the per-file state column. Existing rows were fully
    // synced, so 'synced' is the right default.
    let _ = conn.execute(
        "ALTER TABLE baseline ADD COLUMN state TEXT NOT NULL DEFAULT 'synced'",
        [],
    );
    // Older ops tables predate the error column. Old failed rows keep a NULL
    // reason: the message was never recorded.
    let _ = conn.execute("ALTER TABLE ops ADD COLUMN error TEXT", []);
    // Rows a previous build left in a transient "pending" state described a
    // transfer that had not completed, so they can't be trusted. Dropping them
    // makes the next run re-detect and re-sync those files (never as a delete).
    conn.execute(
        "DELETE FROM baseline WHERE state IN ('pending_up', 'pending_down')",
        [],
    )?;
    conn.pragma_update(None, "user_version", 1)?;
    Ok(())
}

fn migrate_to_2(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V2)?;
    conn.pragma_update(None, "user_version", 2)?;
    Ok(())
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
        let source = state_dir.join("stats.db");
        let scratch = tempfile::tempdir()?;
        let copy = scratch.path().join("stats.db");
        std::fs::copy(&source, &copy)?;
        let wal = state_dir.join("stats.db-wal");
        match std::fs::copy(&wal, scratch.path().join("stats.db-wal")) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let conn = Connection::open(&copy)?;
        // The same guards as `open`, naming the real file, without migrating.
        check_version(&conn, &source)?;
        quick_check(&conn, &source)?;
        let stats = Self {
            conn: Mutex::new(conn),
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
    ///
    /// The file's `user_version` is read before anything is created or
    /// altered. A new file gets schema [`SCHEMA_VERSION`]. A file from before
    /// the stamp (version 0 with tables) runs the old migrations, is stamped
    /// 1, then migrates on. A version above [`SCHEMA_VERSION`] is refused
    /// untouched. A file SQLite cannot read is an error here, never an empty
    /// baseline.
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir).ok();
        let path = state_dir.join("stats.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("opening stats db {}", path.display()))?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .with_context(|| format!("opening sync state {}", path.display()))?;
        let version = check_version(&conn, &path)
            .with_context(|| format!("reading sync state {}", path.display()))?;
        verify_once(&conn, &path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .with_context(|| format!("opening sync state {}", path.display()))?;
        let stats = Stats {
            conn: Mutex::new(conn),
        };
        stats
            .migrate(version)
            .with_context(|| format!("preparing sync state {}", path.display()))?;
        let now = crate::datefmt::now_epoch();
        let _ = stats.prune_conflicts(now - CONFLICT_KEEP_SECS);
        Ok(stats)
    }

    /// Step the schema from `version` up to [`SCHEMA_VERSION`] in one
    /// transaction, so a crash mid-way leaves the file at the old version.
    fn migrate(&self, version: u32) -> Result<()> {
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        // Take the write lock first. A deferred transaction that reads and
        // then writes cannot wait for another opener doing the same (SQLite
        // reports BUSY at once to avoid a deadlock), and several workers open
        // a fresh database together.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Another connection may have migrated while this one waited.
        let mut at = user_version(&tx)?;
        if at == SCHEMA_VERSION {
            return Ok(());
        }
        if at > SCHEMA_VERSION {
            bail!("sync state is now format {at}; this version reads up to {SCHEMA_VERSION}");
        }
        if at == 0 {
            let tables: i64 = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )?;
            if tables == 0 {
                // A new file: create this build's schema outright.
                tx.execute_batch(SCHEMA_V1)?;
                tx.execute_batch(SCHEMA_V2)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
                return Ok(());
            }
            migrate_to_1(&tx)?;
            at = 1;
        }
        if at == 1 {
            migrate_to_2(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The schema version stamped in the open database.
    pub fn schema_version(&self) -> Result<u32> {
        user_version(&self.conn.lock().unwrap())
    }

    /// In-memory DB, for tests.
    #[cfg(test)]
    fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_V1)?;
        conn.execute_batch(SCHEMA_V2)?;
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

    /// Drop every row for `pair` (baseline, per-file state, hotness, activity,
    /// held deletions, conflicts). Called when a folder is removed so a later
    /// folder of the same name can't inherit stale baseline state.
    pub fn forget_pair(&self, pair: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for tbl in [
            "baseline",
            "pair_state",
            "events",
            "ops",
            "held_deletes",
            "conflicts",
        ] {
            conn.execute(&format!("DELETE FROM {tbl} WHERE pair = ?1"), params![pair])?;
        }
        Ok(())
    }

    /// Wipe all NeutronSync metadata (every table). Local files are untouched.
    pub fn wipe(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM baseline; DELETE FROM pair_state; DELETE FROM events; DELETE FROM ops;
             DELETE FROM held_deletes; DELETE FROM conflicts;",
        )?;
        Ok(())
    }

    // -- held deletions ------------------------------------------------------

    /// Record that `pair`'s deletions were held back this run. A row that
    /// exists keeps its `since`; the counts and `updated` take the new values.
    pub fn record_held_deletes(
        &self,
        pair: &str,
        local: usize,
        remote: usize,
        tracked: usize,
        now: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO held_deletes(pair, local, remote, tracked, since, updated)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(pair) DO UPDATE SET
               local = ?2, remote = ?3, tracked = ?4, updated = ?5",
            params![pair, local as i64, remote as i64, tracked as i64, now],
        )?;
        Ok(())
    }

    /// Forget `pair`'s hold. Returns whether a row was there to clear.
    pub fn clear_held_deletes(&self, pair: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM held_deletes WHERE pair = ?1", params![pair])?;
        Ok(n > 0)
    }

    /// Every pair with deletions held back, oldest hold first.
    pub fn held_deletes(&self) -> Result<Vec<HeldDeletes>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pair, local, remote, tracked, since, updated FROM held_deletes ORDER BY since",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(HeldDeletes {
                pair: r.get(0)?,
                local: r.get::<_, i64>(1)? as usize,
                remote: r.get::<_, i64>(2)? as usize,
                tracked: r.get::<_, i64>(3)? as usize,
                since: r.get(4)?,
                updated: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The hold for one pair, if any.
    pub fn held_for(&self, pair: &str) -> Result<Option<HeldDeletes>> {
        Ok(self.held_deletes()?.into_iter().find(|h| h.pair == pair))
    }

    // -- conflicts -----------------------------------------------------------

    /// Record a settled conflict. The same `(pair, path, outcome)` with an
    /// undismissed row bumps that row's `seen` and `last_ts` instead of adding
    /// another, so a skipped conflict that repeats every run shows once.
    pub fn record_conflict(
        &self,
        pair: &str,
        path: &str,
        outcome: ConflictOutcome,
        copy: Option<&str>,
        ts: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE conflicts SET last_ts = ?4, seen = seen + 1, copy = COALESCE(?5, copy)
             WHERE pair = ?1 AND path = ?2 AND outcome = ?3 AND dismissed = 0",
            params![pair, path, outcome.label(), ts, copy],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO conflicts(pair, path, outcome, copy, first_ts, last_ts, seen, dismissed)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?5, 1, 0)",
                params![pair, path, outcome.label(), copy, ts],
            )?;
        }
        Ok(())
    }

    /// Undismissed conflicts, newest activity first.
    pub fn conflicts(&self) -> Result<Vec<ConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pair, path, outcome, copy, first_ts, last_ts, seen, dismissed
             FROM conflicts WHERE dismissed = 0 ORDER BY last_ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let outcome: String = r.get(3)?;
            Ok(ConflictRecord {
                id: r.get(0)?,
                pair: r.get(1)?,
                path: r.get(2)?,
                outcome: ConflictOutcome::parse(&outcome).unwrap_or(ConflictOutcome::Skipped),
                copy: r.get(4)?,
                first_ts: r.get(5)?,
                last_ts: r.get(6)?,
                seen: r.get(7)?,
                dismissed: r.get::<_, i64>(8)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many conflicts are waiting to be looked at.
    pub fn conflict_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM conflicts WHERE dismissed = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn dismiss_conflict(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conflicts SET dismissed = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn dismiss_all_conflicts(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE conflicts SET dismissed = 1 WHERE dismissed = 0", [])?;
        Ok(())
    }

    /// Drop dismissed conflicts whose last activity is before `before`.
    fn prune_conflicts(&self, before: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM conflicts WHERE dismissed = 1 AND last_ts < ?1",
            params![before],
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
    fn conflicts_dedupe_until_dismissed() {
        let s = Stats::memory().unwrap();
        for ts in [100, 200, 300] {
            s.record_conflict("p", "a.txt", ConflictOutcome::Skipped, None, ts)
                .unwrap();
        }
        let rows = s.conflicts().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].seen, rows[0].first_ts, rows[0].last_ts),
            (3, 100, 300)
        );
        assert_eq!(s.conflict_count().unwrap(), 1);
        s.dismiss_conflict(rows[0].id).unwrap();
        assert!(s.conflicts().unwrap().is_empty());
        // The same conflict after a dismissal is news again: a new row.
        s.record_conflict("p", "a.txt", ConflictOutcome::Skipped, None, 400)
            .unwrap();
        let rows = s.conflicts().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seen, 1);
        // A different outcome for the same path is its own row.
        s.record_conflict(
            "p",
            "a.txt",
            ConflictOutcome::KeptBoth,
            Some("a (conflict 1).txt"),
            500,
        )
        .unwrap();
        assert_eq!(s.conflict_count().unwrap(), 2);
        s.dismiss_all_conflicts().unwrap();
        assert_eq!(s.conflict_count().unwrap(), 0);
        // Pruning removes only dismissed rows that are old enough.
        s.record_conflict("p", "b.txt", ConflictOutcome::KeptLocal, None, 600)
            .unwrap();
        s.prune_conflicts(450).unwrap();
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM conflicts", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(n, 2, "the 500 dismissed row and the 600 open row survive");
    }

    #[test]
    fn held_deletes_keep_since_and_clear_reports_presence() {
        let s = Stats::memory().unwrap();
        assert!(!s.clear_held_deletes("p").unwrap());
        s.record_held_deletes("p", 0, 250, 300, 1000).unwrap();
        s.record_held_deletes("p", 10, 250, 300, 2000).unwrap();
        let h = s.held_for("p").unwrap().unwrap();
        assert_eq!(
            (h.local, h.remote, h.tracked, h.since, h.updated),
            (10, 250, 300, 1000, 2000)
        );
        assert!(s.clear_held_deletes("p").unwrap());
        assert!(s.held_deletes().unwrap().is_empty());
        s.record_held_deletes("q", 1, 2, 3, 5).unwrap();
        s.forget_pair("q").unwrap();
        assert!(s.held_deletes().unwrap().is_empty());
    }

    fn baseline_rows(n: usize) -> BTreeMap<String, Entry> {
        (0..n)
            .map(|i| {
                let rel = format!("f{i}.txt");
                (
                    rel.clone(),
                    Entry {
                        path: rel,
                        size: i as u64,
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn new_file_is_stamped_with_this_schema() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stats::open(dir.path()).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = s.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for want in [
            "baseline",
            "conflicts",
            "events",
            "feed_cursor",
            "held_deletes",
            "ops",
            "pair_state",
        ] {
            assert!(
                tables.iter().any(|t| t == want),
                "missing {want}: {tables:?}"
            );
        }
    }

    /// A `stats.db` as a build before the version stamp wrote it: version 0,
    /// the old tables, a baseline, and no `state` column yet.
    fn legacy_db(dir: &Path) {
        let conn = Connection::open(dir.join("stats.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE events (pair TEXT NOT NULL, folder TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE ops (pair TEXT NOT NULL, action TEXT NOT NULL, path TEXT NOT NULL, ok INTEGER NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE baseline (pair TEXT NOT NULL, rel TEXT NOT NULL, is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER, sha1 TEXT, remote_id TEXT, PRIMARY KEY (pair, rel));
             CREATE TABLE feed_cursor (scope TEXT PRIMARY KEY, event_id TEXT NOT NULL);
             CREATE TABLE pair_state (pair TEXT PRIMARY KEY, last_synced INTEGER);",
        )
        .unwrap();
        for (rel, e) in baseline_rows(40) {
            conn.execute(
                "INSERT INTO baseline(pair, rel, is_dir, size) VALUES('docs', ?1, 0, ?2)",
                params![rel, e.size as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn legacy_file_migrates_and_keeps_every_baseline_row() {
        let dir = tempfile::tempdir().unwrap();
        legacy_db(dir.path());
        let s = Stats::open(dir.path()).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        let loaded = s.load_baseline("docs").unwrap();
        let want = baseline_rows(40);
        assert_eq!(loaded.len(), want.len());
        for (rel, e) in &want {
            let got = loaded.get(rel).expect(rel);
            assert_eq!((got.size, got.is_dir), (e.size, e.is_dir));
        }
        assert!(s.held_deletes().unwrap().is_empty());
        assert!(s.conflicts().unwrap().is_empty());
        // The state column arrived with the migration.
        assert_eq!(s.baseline_states("docs").unwrap().len(), 40);
        drop(s);
        // And it opens cleanly a second time at the stamped version.
        assert_eq!(
            Stats::open(dir.path()).unwrap().schema_version().unwrap(),
            SCHEMA_VERSION
        );
    }

    fn digest_of(path: &Path) -> String {
        // sha1 is enough to prove the bytes did not move; no sha2 in the CLI build.
        let bytes = std::fs::read(path).unwrap();
        let mut h = sha1_smol::Sha1::new();
        h.update(&bytes);
        h.digest().to_string()
    }

    #[test]
    fn newer_file_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Stats::open(dir.path()).unwrap();
            s.save_baseline("docs", &baseline_rows(3)).unwrap();
        }
        let path = dir.path().join("stats.db");
        {
            let conn = Connection::open(&path).unwrap();
            // Fold the WAL into the main file so the hash covers everything.
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let before = digest_of(&path);
        let err = Stats::open(dir.path())
            .err()
            .expect("a newer file is refused");
        let text = format!("{err:#}");
        assert!(
            text.contains(
                "Sync state was saved by a newer NeutronSync (format 3; this version reads up to 2)"
            ),
            "{text}"
        );
        assert!(text.contains("Update NeutronSync, or move"), "{text}");
        assert_eq!(digest_of(&path), before, "a refused file must not change");
        // The read-only path refuses it too, never returning an empty baseline.
        assert!(Stats::copied_baseline(dir.path(), "docs").is_err());
    }

    #[test]
    fn damaged_files_are_errors_not_empty_baselines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        std::fs::write(&path, b"this is not a database at all, just bytes").unwrap();
        assert!(Stats::open(dir.path()).is_err());
        assert!(Stats::copied_baseline(dir.path(), "docs").is_err());

        // A real database cut to half its size (its own directory: a file is
        // checked once per process, and this one is damaged after it was made).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        {
            let s = Stats::open(dir.path()).unwrap();
            s.save_baseline("docs", &baseline_rows(2000)).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        }
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > 8192, "fixture must span several pages, got {len}");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len / 2).unwrap();
        drop(file);
        // A fresh process would see a new (path, inode); here the inode is the
        // same, so check the way a fresh process does.
        let conn = Connection::open(&path).unwrap();
        let Err(err) = quick_check(&conn, &path) else {
            panic!("a truncated file is refused");
        };
        assert!(format!("{err:#}").contains("damaged"), "{err:#}");
        assert!(Stats::copied_baseline(dir.path(), "docs").is_err());
        // Replace the file by a damaged copy under a new inode: `open` refuses it.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let err = Stats::open(dir.path())
            .err()
            .expect("a truncated file is refused");
        let text = format!("{err:#}");
        assert!(
            text.contains("damaged") || text.contains("malformed"),
            "{text}"
        );
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
