//! Adapter over Proton's official `proton-drive` CLI (verified: cli-drive 0.8.0).
//!
//! This is the ONLY module that shells out to the binary or knows its exact
//! command spellings and JSON shape. Facts baked in here:
//!   * `filesystem list -j PATH` (JSON), global `-j` AFTER the subcommand.
//!   * `filesystem upload [-f STRATEGY] [-d STRATEGY] LOCAL... PARENT` (dest = parent).
//!   * `filesystem download [-f STRATEGY] [-d STRATEGY] PATH... LOCALFOLDER` (dest = folder).
//!     Both upload AND download prompt interactively without a strategy.
//!     cli-drive ≥ 0.8.0 dropped the unified `--conflict-strategy` / `-c` flag;
//!     upload overwrite is `replace`, download overwrite is `remove`.
//!   * `filesystem create-folder PARENT NAME`; `filesystem trash PATH`
//!     (recoverable) vs `filesystem delete` (permanent - never used).
//!   * The CLI caches directory metadata and serves it STALE, so we point it at
//!     a throwaway `PROTON_DRIVE_CACHE_DIR` per run (see `Config::fresh_cache`).
//!   * `list -j` node: {uid, type:"file"|"folder", name:{ok,value},
//!       activeRevision: either the revision object itself (cli-drive ≥ 0.8) or
//!       {ok,value:{claimedSize, claimedModificationTime, claimedDigests:{sha1}}}
//!       (≤ 0.7). totalStorageSize is the ENCRYPTED size and must not be used
//!       as content size.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::datefmt::iso8601_to_epoch;
use crate::models::{DownloadJob, Entry, TreeScan};

/// The remote operations the engine needs. A fake implements this in tests.
pub trait Remote {
    fn list_dir(&self, remote_path: &str) -> Result<Vec<Entry>>;

    /// Like [`list_dir`] but distinguishes a genuine "not found" from an empty
    /// listing (a transport error is still `Err`). The default treats every
    /// successful listing as `Listed` — backends with no not-found signal never
    /// report `NotFound`. `ProtonCli` overrides it. Callers that would delete on
    /// the strength of an empty listing should use this and suppress deletes on
    /// `NotFound`, so a misclassified transient error can't drive a deletion.
    fn list_dir_probe(&self, remote_path: &str) -> Result<ListOutcome> {
        Ok(ListOutcome::Listed(self.list_dir(remote_path)?))
    }

    fn create_folder(&self, parent: &str, name: &str) -> Result<()>;
    fn upload(&self, local_path: &str, remote_parent: &str) -> Result<()>;
    fn download(&self, remote_path: &str, local_dest: &str) -> Result<()>;
    fn trash(&self, remote_path: &str) -> Result<()>;

    /// Rename or move a remote node from absolute `from` to absolute `to`.
    /// Default: unsupported (the engine then falls back to delete + re-upload).
    fn rename(&self, _from: &str, _to: &str) -> Result<()> {
        anyhow::bail!("rename/move not supported by this backend")
    }

    /// List an entire subtree. `progress` is called with each remote path as it
    /// is listed. `exclude` is called with each child's relative path; a subtree
    /// it accepts is skipped entirely — never listed, never descended — so
    /// excluded folders cost nothing and can't contaminate the scan. The default
    /// is a sequential recursive walk; ProtonCli overrides it with a concurrent,
    /// retrying one. A folder that can't be listed is recorded in
    /// `TreeScan::failed` and the walk continues (partial result).
    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        let mut out = Vec::new();
        let mut failed = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let here = crate::config::remote_join(base, &rel);
            progress(&here);
            let entries = match self.list_dir(&here) {
                Ok(e) => e,
                Err(_) => {
                    failed.push(rel);
                    continue;
                }
            };
            for e in entries {
                let child_rel = if rel.is_empty() {
                    e.path.clone()
                } else {
                    format!("{rel}/{}", e.path)
                };
                if exclude(&child_rel) {
                    continue;
                }
                let is_dir = e.is_dir;
                out.push((
                    child_rel.clone(),
                    Entry {
                        path: child_rel.clone(),
                        ..e
                    },
                ));
                if is_dir {
                    stack.push(child_rel);
                }
            }
        }
        Ok(TreeScan {
            entries: out,
            failed,
            // The sequential fallback lists via `list_dir`, which can't tell a
            // missing base from an empty one; only the concurrent walk detects
            // it. Report "present" here.
            root_missing: false,
        })
    }

    /// Download many files. Default = sequential; ProtonCli overrides with a
    /// bounded, retrying worker pool. `on_start`/`on_done` fire per job (their
    /// sinks must be thread-safe). Stops early if `cancel` is set. `on_done`
    /// receives Ok or the error string.
    #[allow(clippy::type_complexity)]
    fn download_many(
        &self,
        jobs: &[DownloadJob],
        _threads: usize,
        cancel: &AtomicBool,
        on_start: &(dyn Fn(&DownloadJob) + Sync),
        on_done: &(dyn Fn(&DownloadJob, std::result::Result<(), String>) + Sync),
    ) {
        for j in jobs {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            on_start(j);
            let r = self
                .download(&j.remote_path, &j.dest_dir)
                .map_err(|e| e.to_string());
            on_done(j, r);
        }
    }
}

pub struct ProtonCli {
    binary: String,
    upload_flags: Vec<String>,
    download_flags: Vec<String>,
    credentials_store: Option<String>,
    cache_dir: Option<PathBuf>,
    scan_threads: usize,
    download_threads: usize,
}

impl Drop for ProtonCli {
    fn drop(&mut self) {
        if let Some(dir) = &self.cache_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Outcome of listing one remote directory: either it listed (possibly empty)
/// or the CLI reported it as not found — distinct from a transport error, which
/// is returned as `Err` so it can be retried/treated as a partial scan.
pub enum ListOutcome {
    Listed(Vec<Entry>),
    NotFound,
}

impl ProtonCli {
    pub fn new(cfg: &Config) -> Self {
        let cache_dir = if cfg.fresh_cache {
            make_cache_dir()
        } else {
            None
        };
        ProtonCli {
            binary: cfg.binary.clone(),
            upload_flags: cfg.upload_flags.clone(),
            download_flags: cfg.download_flags.clone(),
            credentials_store: cfg.credentials_store.clone(),
            cache_dir,
            scan_threads: cfg.scan_threads,
            download_threads: cfg.download_threads,
        }
    }

    fn command_with(&self, args: &[&str], cache: Option<&Path>) -> Command {
        let mut c = Command::new(&self.binary);
        c.args(args);
        if let Some(cs) = &self.credentials_store {
            c.env("PROTON_DRIVE_CREDENTIALS_STORE", cs);
        }
        if let Some(cd) = cache.or(self.cache_dir.as_deref()) {
            c.env("PROTON_DRIVE_CACHE_DIR", cd);
        }
        c
    }

    /// Run and capture. Returns (success, stdout, stderr).
    fn run(&self, args: &[&str]) -> Result<(bool, String, String)> {
        self.run_with(args, None)
    }

    fn run_with(&self, args: &[&str], cache: Option<&Path>) -> Result<(bool, String, String)> {
        let out = self
            .command_with(args, cache)
            .output()
            .with_context(|| format!("failed to run {:?}", self.binary))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// Number of concurrent folder-listers for a tree scan (0 in config = auto).
    fn scan_threads(&self) -> usize {
        if self.scan_threads > 0 {
            self.scan_threads
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(2, 8)
        }
    }

    /// Concurrent file downloads (0 in config = auto). Kept modest because
    /// downloads are bandwidth-bound and too many risks server rate limits.
    fn download_worker_count(&self) -> usize {
        if self.download_threads > 0 {
            self.download_threads
        } else {
            4
        }
    }

    /// Download one file, optionally with an explicit cache dir (so parallel
    /// downloads don't fight over one PROTON_DRIVE_CACHE_DIR).
    fn download_cached(
        &self,
        remote_path: &str,
        local_dest: &str,
        cache: Option<&Path>,
    ) -> Result<()> {
        // The CLI globs the local destination too (and demands exactly one
        // match), so quote it. Callers create the folder before downloading.
        let quoted_dest = glob_quote_local(local_dest);
        let mut args: Vec<&str> = vec!["filesystem", "download"];
        args.extend(self.download_flags.iter().map(|s| s.as_str()));
        args.push("--"); // end of options: never treat a path as a flag
        args.push(remote_path);
        args.push(&quoted_dest);
        let (ok, out, err) = self.run_with(&args, cache)?;
        if !ok {
            bail!(
                "download {remote_path} failed: {}",
                first_nonempty(&err, &out)
            );
        }
        Ok(())
    }

    /// One directory level, optionally with an explicit cache dir (used by the
    /// concurrent scanner so parallel CLI processes don't share one cache). A
    /// missing folder lists as empty — a not-yet-created folder is normal here.
    fn list_dir_cached(&self, remote_path: &str, cache: Option<&Path>) -> Result<Vec<Entry>> {
        match self.probe_dir(remote_path, cache)? {
            ListOutcome::Listed(entries) => Ok(entries),
            ListOutcome::NotFound => Ok(Vec::new()),
        }
    }

    /// Like [`list_dir_cached`] but distinguishes a genuine "not found" from an
    /// empty listing (both collapse to an empty vec in `list_dir_cached`). Real
    /// transport failures are returned as `Err` so callers can retry. (Inherent,
    /// cache-aware; the `Remote::list_dir_probe` trait method wraps it.)
    fn probe_dir(&self, remote_path: &str, cache: Option<&Path>) -> Result<ListOutcome> {
        let (ok, out, err) =
            self.run_with(&["filesystem", "list", "-j", "--", remote_path], cache)?;
        if !ok {
            let low = format!("{err}{out}").to_lowercase();
            // Auth failures FIRST: the keyring message ends in "No such file or
            // directory", which the not-found classifier below would swallow,
            // turning "signed out" into "remote folder is empty".
            if is_not_logged_in(&low) {
                return Err(anyhow!("not signed in: {}", first_nonempty(&err, &out)));
            }
            if ["not found", "no such", "does not exist"]
                .iter()
                .any(|w| low.contains(w))
            {
                return Ok(ListOutcome::NotFound);
            }
            return Err(anyhow!(
                "list failed for {remote_path:?}: {}",
                first_nonempty(&err, &out)
            ));
        }
        Ok(ListOutcome::Listed(ProtonCli::parse_list(
            &out,
            remote_path,
        )?))
    }

    pub fn resolve_binary(&self) -> Option<PathBuf> {
        which(&self.binary)
    }

    pub fn version(&self) -> String {
        match self.run(&["version"]) {
            Ok((_, out, err)) => {
                let s = if out.trim().is_empty() { err } else { out };
                s.trim().to_string()
            }
            Err(e) => format!("error: {e}"),
        }
    }

    /// Browser login. Inherits stdio so the user can interact.
    pub fn login(&self) -> Result<()> {
        let status = self.command_with(&["auth", "login"], None).status()?;
        if !status.success() {
            bail!("auth login failed");
        }
        Ok(())
    }

    pub fn logout(&self) -> Result<()> {
        let (ok, out, err) = self.run(&["auth", "logout"])?;
        if !ok {
            bail!("auth logout failed: {}", first_nonempty(&err, &out));
        }
        Ok(())
    }

    pub fn raw_list(&self, remote_path: &str) -> Result<String> {
        let (_, out, _) = self.run(&["filesystem", "list", "-j", remote_path])?;
        Ok(out)
    }

    // --- parsing (associated fn so tests/doctor can call it directly) --------
    pub fn parse_list(stdout: &str, remote_path: &str) -> Result<Vec<Entry>> {
        let stdout = stdout.trim();
        if stdout.is_empty() {
            return Ok(vec![]);
        }
        let payload: Value = serde_json::from_str(stdout).with_context(|| {
            format!(
                "could not parse JSON from `filesystem list -j {remote_path}`; \
                 run `neutronsync doctor` and adjust protoncli.rs"
            )
        })?;
        let rows = rows(&payload);
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let uid = get_str(row, &["uid", "id", "nodeId", "linkId", "nodeUid"]);
            let name = extract_name(row).or_else(|| uid.clone());
            let name = match name {
                Some(n) => n,
                None => continue,
            };
            // A node name that isn't a single safe path component (contains a
            // separator, is "." / "..", or holds a NUL) can't be mapped to a
            // local file and could escape a pair's local root on download. Names
            // are remote-controlled (shared folders come from other people), so
            // skip anything unsafe rather than trust it.
            if !is_safe_component(&name) {
                continue;
            }
            let is_dir = is_folder(row);
            let (mut size, mut mtime, mut sha1) = (0u64, None, None);
            if !is_dir {
                if let Some(rev) = active_revision(row) {
                    size = num_u64(rev.get("claimedSize")).unwrap_or(0);
                    mtime = parse_time(rev.get("claimedModificationTime"));
                    sha1 = rev
                        .get("claimedDigests")
                        .and_then(|d| d.get("sha1"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                // Never fall back to totalStorageSize: that is the ENCRYPTED
                // size and would make every file look modified vs local content.
                if size == 0 {
                    size = num_u64(get_val(row, &["claimedSize", "size"])).unwrap_or(0);
                }
                if mtime.is_none() {
                    mtime = parse_time(row.get("modificationTime"));
                }
            }
            entries.push(Entry {
                path: name,
                is_dir,
                size,
                mtime,
                sha1,
                remote_id: uid,
            });
        }
        Ok(entries)
    }
}

impl Remote for ProtonCli {
    fn list_dir(&self, remote_path: &str) -> Result<Vec<Entry>> {
        self.list_dir_cached(remote_path, None)
    }

    fn list_dir_probe(&self, remote_path: &str) -> Result<ListOutcome> {
        self.probe_dir(remote_path, None)
    }

    /// Concurrent breadth-first tree walk. The CLI has no recursive list, so we
    /// spawn `scan_threads` workers that list folders in parallel, each with its
    /// own throwaway cache dir (parallel CLI processes must not share a cache).
    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        let threads = self.scan_threads();
        let out: Mutex<Vec<(String, Entry)>> = Mutex::new(Vec::new());
        // Folders that couldn't be listed even after retries. We keep walking the
        // rest of the tree; the engine treats a non-empty list as a PARTIAL scan
        // and suppresses deletions so a transient failure can't lose data.
        let failed: Mutex<Vec<String>> = Mutex::new(Vec::new());
        // Set if the pair's remote base folder itself is not found (as opposed
        // to present-but-empty), so deletions can be suppressed this run.
        let root_missing = AtomicBool::new(false);
        let mut frontier = vec![String::new()];

        while !frontier.is_empty() {
            let queue: Mutex<VecDeque<String>> = Mutex::new(frontier.drain(..).collect());
            let next: Mutex<Vec<String>> = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| {
                        // Isolated cache per worker; removed when the worker ends.
                        let cache = make_cache_dir();
                        loop {
                            let rel = {
                                let mut q = queue.lock().unwrap();
                                q.pop_front()
                            };
                            let Some(rel) = rel else { break };
                            let here = crate::config::remote_join(base, &rel);
                            progress(&here);
                            // Retry transient listing failures (rate limits, blips)
                            // a few times with backoff before giving up on this
                            // folder.
                            let mut attempt = 0u32;
                            let listed = loop {
                                match self.probe_dir(&here, cache.as_deref()) {
                                    Ok(ListOutcome::Listed(entries)) => break Some(entries),
                                    // The pair's remote base folder itself is not
                                    // found. On a first sync that just means an
                                    // empty remote; the engine only suppresses
                                    // deletions on this flag when a baseline exists.
                                    Ok(ListOutcome::NotFound) if rel.is_empty() => {
                                        root_missing.store(true, Ordering::Relaxed);
                                        break Some(Vec::new());
                                    }
                                    // A subfolder we just saw in its parent that now
                                    // reports "not found" is a race/transient, not an
                                    // authoritative empty: treat it as a failed
                                    // listing so deletions are suppressed this run.
                                    Ok(ListOutcome::NotFound) => break None,
                                    Err(_) if attempt < 2 => {
                                        std::thread::sleep(std::time::Duration::from_millis(
                                            400 * (1 << attempt),
                                        ));
                                        attempt += 1;
                                    }
                                    Err(_) => break None,
                                }
                            };
                            match listed {
                                Some(entries) => {
                                    let mut o = out.lock().unwrap();
                                    let mut nx = next.lock().unwrap();
                                    for e in entries {
                                        let child_rel = if rel.is_empty() {
                                            e.path.clone()
                                        } else {
                                            format!("{rel}/{}", e.path)
                                        };
                                        // Excluded subtree: never record it and
                                        // never queue it for descent.
                                        if exclude(&child_rel) {
                                            continue;
                                        }
                                        if e.is_dir {
                                            nx.push(child_rel.clone());
                                        }
                                        o.push((
                                            child_rel.clone(),
                                            Entry {
                                                path: child_rel,
                                                ..e
                                            },
                                        ));
                                    }
                                }
                                None => failed.lock().unwrap().push(rel),
                            }
                        }
                        if let Some(c) = cache {
                            let _ = std::fs::remove_dir_all(c);
                        }
                    });
                }
            });
            frontier = next.into_inner().unwrap();
        }
        Ok(TreeScan {
            entries: out.into_inner().unwrap(),
            failed: failed.into_inner().unwrap(),
            root_missing: root_missing.into_inner(),
        })
    }

    fn download_many(
        &self,
        jobs: &[DownloadJob],
        threads: usize,
        cancel: &AtomicBool,
        on_start: &(dyn Fn(&DownloadJob) + Sync),
        on_done: &(dyn Fn(&DownloadJob, std::result::Result<(), String>) + Sync),
    ) {
        let workers = if threads > 0 {
            threads
        } else {
            self.download_worker_count()
        }
        .clamp(1, 8);
        let queue: Mutex<VecDeque<&DownloadJob>> = Mutex::new(jobs.iter().collect());

        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    // Own cache dir per worker so parallel CLI processes don't
                    // share (and corrupt) one PROTON_DRIVE_CACHE_DIR.
                    let cache = make_cache_dir();
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let job = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };
                        let Some(job) = job else { break };
                        on_start(job);
                        // Retry transient failures (e.g. "socket connection closed")
                        // a few times with backoff before reporting the job failed.
                        let mut attempt = 0u32;
                        let result = loop {
                            match self.download_cached(
                                &job.remote_path,
                                &job.dest_dir,
                                cache.as_deref(),
                            ) {
                                Ok(()) => break Ok(()),
                                Err(_) if attempt < 2 => {
                                    std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
                                    attempt += 1;
                                }
                                Err(e) => break Err(e.to_string()),
                            }
                        };
                        on_done(job, result);
                    }
                    if let Some(c) = cache {
                        let _ = std::fs::remove_dir_all(c);
                    }
                });
            }
        });
    }

    fn create_folder(&self, parent: &str, name: &str) -> Result<()> {
        let (ok, out, err) = self.run(&["filesystem", "create-folder", "--", parent, name])?;
        if !ok {
            let low = format!("{err}{out}").to_lowercase();
            if low.contains("exist") || low.contains("already") {
                return Ok(()); // idempotent from our side
            }
            bail!(
                "create-folder {parent}/{name} failed: {}",
                first_nonempty(&err, &out)
            );
        }
        Ok(())
    }

    fn upload(&self, local_path: &str, remote_parent: &str) -> Result<()> {
        let quoted = glob_quote_local(local_path);
        let mut args: Vec<&str> = vec!["filesystem", "upload"];
        args.extend(self.upload_flags.iter().map(|s| s.as_str()));
        args.push("--"); // end of options: never treat a path as a flag
        args.push(&quoted); // and never as a glob
        args.push(remote_parent);
        let (ok, out, err) = self.run(&args)?;
        if !ok {
            bail!("upload {local_path} failed: {}", first_nonempty(&err, &out));
        }
        Ok(())
    }

    fn download(&self, remote_path: &str, local_dest: &str) -> Result<()> {
        // local_dest is a FOLDER; the item keeps its name inside it.
        self.download_cached(remote_path, local_dest, None)
    }

    fn trash(&self, remote_path: &str) -> Result<()> {
        let (ok, out, err) = self.run(&["filesystem", "trash", "--", remote_path])?;
        if !ok {
            bail!("trash {remote_path} failed: {}", first_nonempty(&err, &out));
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (fp, fname) = split_remote(from);
        let (tp, tname) = split_remote(to);
        if fp == tp {
            // same folder: rename in place (CLI: `rename path newName`)
            let (ok, out, err) = self.run(&["filesystem", "rename", "--", from, tname])?;
            if !ok {
                bail!(
                    "rename {from} -> {tname} failed: {}",
                    first_nonempty(&err, &out)
                );
            }
        } else {
            // different folder: move (keeps name), then rename if the name changed
            let (ok, out, err) = self.run(&["filesystem", "move", "--", from, tp])?;
            if !ok {
                bail!("move {from} -> {tp} failed: {}", first_nonempty(&err, &out));
            }
            if fname != tname {
                let moved = format!("{}/{}", tp.trim_end_matches('/'), fname);
                let (ok2, out2, err2) = self.run(&["filesystem", "rename", "--", &moved, tname])?;
                if !ok2 {
                    bail!(
                        "rename {moved} -> {tname} failed: {}",
                        first_nonempty(&err2, &out2)
                    );
                }
            }
        }
        Ok(())
    }
}

/// Split an absolute remote path into (parent, basename).
fn split_remote(p: &str) -> (&str, &str) {
    match p.trim_end_matches('/').rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", p),
    }
}

// --- JSON helpers -----------------------------------------------------------
fn rows(payload: &Value) -> Vec<&Value> {
    if let Some(arr) = payload.as_array() {
        return arr.iter().filter(|v| v.is_object()).collect();
    }
    if let Some(obj) = payload.as_object() {
        for key in ["items", "entries", "nodes", "children", "files", "results"] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                return arr.iter().filter(|v| v.is_object()).collect();
            }
        }
        return vec![payload];
    }
    vec![]
}

fn get_val<'a>(row: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    for k in keys {
        if let Some(v) = row.get(k) {
            if !v.is_null() {
                return Some(v);
            }
        }
    }
    None
}

fn get_str(row: &Value, keys: &[&str]) -> Option<String> {
    get_val(row, keys)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Unwrap a `{ "ok": true, "value": ... }` envelope; None if absent/failed.
fn ok_value(v: Option<&Value>) -> Option<&Value> {
    let obj = v?.as_object()?;
    if obj.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        obj.get("value")
    } else {
        None
    }
}

/// The revision object for a file node. cli-drive ≤ 0.7 wrapped it as
/// `{ "ok": true, "value": { claimedSize, ... } }`; ≥ 0.8 puts the fields
/// directly on `activeRevision`. Accept both so a CLI upgrade can't make every
/// file look modified (missing claimedSize used to fall through to the
/// encrypted totalStorageSize).
fn active_revision(row: &Value) -> Option<&Value> {
    let v = row.get("activeRevision")?;
    if let Some(inner) = ok_value(Some(v)) {
        return Some(inner);
    }
    if v.as_object()
        .is_some_and(|o| o.contains_key("claimedSize") || o.contains_key("storageSize"))
    {
        return Some(v);
    }
    None
}

fn extract_name(row: &Value) -> Option<String> {
    match row.get("name") {
        Some(Value::Object(_)) => ok_value(row.get("name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn is_folder(row: &Value) -> bool {
    if let Some(t) =
        get_val(row, &["type", "nodeType", "kind", "mimeType"]).and_then(|v| v.as_str())
    {
        if t.to_lowercase().contains("folder") {
            return true;
        }
    }
    for k in ["isFolder", "is_folder"] {
        if row.get(k).and_then(|v| v.as_bool()) == Some(true) {
            return true;
        }
    }
    false
}

fn num_u64(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some(n.max(0) as u64);
    }
    if let Some(f) = v.as_f64() {
        return Some(f.max(0.0) as u64);
    }
    v.as_str().and_then(|s| s.trim().parse::<u64>().ok())
}

fn parse_time(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    if let Some(f) = v.as_f64() {
        let n = f as i64;
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if let Ok(n) = s.parse::<i64>() {
            return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
        }
        return iso8601_to_epoch(s);
    }
    None
}

/// Whether a proton-drive error/output string is the "not authenticated" signal
/// — the session has expired or the user is logged out — as opposed to a genuine
/// transfer failure (a bad path, a network blip, a rate limit). Callers use this
/// to surface a single clear "signed out, sign in to resume" state instead of
/// treating a logout as a pile of per-file errors. Kept conservative: only the
/// phrasings proton-drive actually uses for "log in first", so an unrelated 4xx
/// never trips it.
pub fn is_not_logged_in(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("login first")
        || s.contains("need to login")
        || s.contains("need to log in")
        || s.contains("not logged in")
        || s.contains("please login")
        || s.contains("please log in")
        || s.contains("no active session")
        // The keyring/secret-service is unreachable (locked, headless, no D-Bus):
        // the CLI has no session, which for us means "not signed in", NOT "node
        // not found" (the message ends in "No such file or directory").
        || s.contains("failed to load session")
}

// --- process/env helpers ----------------------------------------------------
fn first_nonempty(a: &str, b: &str) -> String {
    let a = a.trim();
    if !a.is_empty() {
        a.to_string()
    } else {
        b.trim().to_string()
    }
}

/// Whether a remote node name is a single, safe local path component: not
/// empty, not "." or "..", and free of path separators or NUL. Rejecting the
/// rest stops a hostile or shared node name from escaping a pair's local root.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Quote a LOCAL path so proton-drive treats it literally instead of as a glob.
///
/// The CLI expands every local path argument itself: if the path matches
/// `/[*?\[{]/` it is handed to `fs/promises` glob, and a pattern that matches
/// nothing is a hard error ("No paths matched: ..."). A real file called
/// `[RTA06]_Change_of_bond_contributors_....pdf` therefore never uploads — the
/// `[RTA06]` is read as a one-character class, matching nothing. Passing `--`
/// does not help; this is glob expansion, not flag parsing.
///
/// Each metacharacter is wrapped in a single-character class (`[` -> `[[]`), and
/// a literal backslash is doubled inside one (`\` -> `[\\]`) so it isn't read as
/// an escape. The result matches exactly the original path, so the CLI resolves
/// it back to the real name (the remote node keeps the correct name). Verified
/// against cli-drive 0.8.0 for all of `\ * ? [ {`.
///
/// A quoted path is a pattern, so it must EXIST — a path with no metacharacters
/// is returned untouched and stays literal, and every caller here passes a path
/// that is already on disk (download destinations are created first).
fn glob_quote_local(path: &str) -> String {
    if !path.contains(['*', '?', '[', '{', '\\']) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 8);
    for ch in path.chars() {
        match ch {
            '\\' => out.push_str("[\\\\]"),
            '*' | '?' | '[' | '{' => {
                out.push('[');
                out.push(ch);
                out.push(']');
            }
            _ => out.push(ch),
        }
    }
    out
}

fn make_cache_dir() -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "neutronsync-cache-{}-{}",
        std::process::id(),
        nanos
    ));
    // 0700: the cache holds decrypted directory metadata, so keep it readable
    // only by this user (it lives in the shared temp dir).
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()
        .map(|_| dir)
}

/// Minimal `which`: honour an explicit path, else search $PATH.
fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let p = PathBuf::from(binary);
        return if p.exists() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(binary);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{glob_quote_local, is_not_logged_in, is_safe_component, ProtonCli};

    #[test]
    fn glob_quotes_local_metacharacters() {
        // Ordinary paths are passed through byte-for-byte: no metacharacter
        // means the CLI never globs them, and quoting would only add risk.
        let plain = "/home/w/ProtonDrive/Documents/Summary.pdf";
        assert_eq!(glob_quote_local(plain), plain);

        // The real-world case: a bracketed prefix read as a character class.
        assert_eq!(
            glob_quote_local("/d/1. Agreements/[RTA06]_Change_of_bond.pdf"),
            "/d/1. Agreements/[[]RTA06]_Change_of_bond.pdf"
        );
        // A closing bracket on its own is literal to the CLI, so it is left be.
        assert_eq!(glob_quote_local("/d/no] class.txt"), "/d/no] class.txt");

        // Every character the CLI's glob trigger looks for, plus backslash
        // (doubled so it is not read as an escape of the character after it).
        assert_eq!(glob_quote_local("/d/a*b"), "/d/a[*]b");
        assert_eq!(glob_quote_local("/d/a?b"), "/d/a[?]b");
        assert_eq!(glob_quote_local("/d/{a,b}"), "/d/[{]a,b}");
        assert_eq!(glob_quote_local("/d/back\\slash"), "/d/back[\\\\]slash");
        assert_eq!(
            glob_quote_local("/d/x[1]*?{y}\\z"),
            "/d/x[[]1][*][?][{]y}[\\\\]z"
        );
    }

    #[test]
    fn detects_not_logged_in_signal() {
        // The phrasing proton-drive actually returns when the session is gone.
        assert!(is_not_logged_in("You need to login first"));
        assert!(is_not_logged_in("Error: not logged in"));
        assert!(is_not_logged_in("Please log in to continue"));
        assert!(is_not_logged_in("no active session"));
        assert!(is_not_logged_in(
            "Failed to load session from secrets (ensure you have secrets \
             available, read the README for more information): Could not \
             connect: No such file or directory (code: 1)"
        ));
        // Genuine transfer failures must NOT read as a logout.
        assert!(!is_not_logged_in("Node not found"));
        assert!(!is_not_logged_in("upload /x failed: connection reset"));
        assert!(!is_not_logged_in("rate limited, try again"));
        assert!(!is_not_logged_in(""));
    }

    #[test]
    fn rejects_unsafe_node_names() {
        assert!(is_safe_component("file.txt"));
        assert!(is_safe_component("a normal name.pdf"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b")); // embedded separator
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("x\0y")); // NUL
    }

    #[test]
    fn parses_cli_08_inline_active_revision() {
        // cli-drive 0.8 dropped the {ok,value} envelope around activeRevision.
        // If we miss that, claimedSize is lost and totalStorageSize (encrypted)
        // would be used instead — every file then looks modified forever.
        let raw = r#"[
          {
            "uid": "u1",
            "type": "file",
            "name": {"ok": true, "value": "invoice.pdf"},
            "modificationTime": "2026-08-26T06:17:58.000Z",
            "totalStorageSize": 190037,
            "activeRevision": {
              "claimedSize": 189950,
              "claimedModificationTime": "2025-04-26T13:45:48.000Z",
              "claimedDigests": {"sha1": "abc"}
            }
          }
        ]"#;
        let entries = ProtonCli::parse_list(raw, "/my-files").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "invoice.pdf");
        assert_eq!(entries[0].size, 189950);
        assert_eq!(entries[0].mtime, Some(1745675148));
        assert_eq!(entries[0].sha1.as_deref(), Some("abc"));
    }

    #[test]
    fn parses_legacy_wrapped_active_revision() {
        let raw = r#"[
          {
            "uid": "u1",
            "type": "file",
            "name": {"ok": true, "value": "old.pdf"},
            "totalStorageSize": 999,
            "activeRevision": {
              "ok": true,
              "value": {
                "claimedSize": 100,
                "claimedModificationTime": "2025-04-26T13:45:48.000Z"
              }
            }
          }
        ]"#;
        let entries = ProtonCli::parse_list(raw, "/my-files").unwrap();
        assert_eq!(entries[0].size, 100);
        assert_eq!(entries[0].mtime, Some(1745675148));
    }
}
