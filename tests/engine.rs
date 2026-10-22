//! Engine tests using an in-memory fake `Remote` - the real three-way-merge
//! logic end to end without a sidecar. Mirrors the Python
//! tests/test_engine.py so the two ports can be diffed for behaviour parity.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use std::sync::atomic::AtomicBool;

use neutronsync::config::{Config, ConflictPolicy, LocalDelete, Pair, Theme, UpdateChannel};
use neutronsync::engine::Engine;
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::models::{Compare, ConflictOutcome, Entry, Uploaded};
use neutronsync::remote::{ListOutcome, Remote};
use neutronsync::stats::Stats;

// --- in-memory fake remote --------------------------------------------------
#[derive(Default)]
struct Store {
    files: BTreeMap<String, Vec<u8>>, // absolute proton path -> bytes
    mtimes: BTreeMap<String, i64>,    // absolute proton path -> claimed mtime, when known
    dirs: BTreeSet<String>,
    moves: usize,                               // count of rename/move calls
    fail_dir: Option<String>,                   // absolute path whose listing should error
    notfound_dir: Option<String>,               // absolute path whose probe reports NotFound
    fail_download: Option<String>,              // absolute file path whose download should error
    fail_upload: Option<String>,                // absolute file path whose upload should error
    edit_on_upload: Option<(PathBuf, Vec<u8>)>, // write this file when the next upload starts
    // Typed sidecar-style failures, per absolute path: the codes to fail with
    // on successive attempts ("transient", "rate_limited:2", "conflict", ...).
    // An exhausted script succeeds.
    fail_script: BTreeMap<String, Vec<String>>,
    // Every transfer fails with this code (the dead-network case).
    fail_all: Option<String>,
    // Transfer attempts per absolute path, with when each started.
    attempts: BTreeMap<String, Vec<std::time::Instant>>,
    // Set this flag when the given path reaches the given attempt.
    cancel_on_attempt: Option<(String, usize, std::sync::Arc<AtomicBool>)>,
}

/// A failure as `DriveApi` would return it: "code" or "code:retry_after".
fn rpc_failure(spec: &str) -> anyhow::Error {
    let (code, retry_after) = spec.split_once(':').unwrap_or((spec, ""));
    neutronsync::driveapi::RpcError {
        code: code.to_string(),
        retry_after: retry_after.parse().ok(),
    }
    .into()
}

impl Store {
    /// Count an attempt on `key` and return the scripted failure for it, if any.
    fn attempt(&mut self, key: &str) -> Option<anyhow::Error> {
        self.attempts
            .entry(key.to_string())
            .or_default()
            .push(std::time::Instant::now());
        if let Some((path, n, flag)) = &self.cancel_on_attempt {
            if path == key && self.attempts[key].len() == *n {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(code) = &self.fail_all {
            return Some(rpc_failure(code));
        }
        let script = self.fail_script.get_mut(key)?;
        if script.is_empty() {
            return None;
        }
        Some(rpc_failure(&script.remove(0)))
    }
    fn attempts_on(&self, key: &str) -> usize {
        self.attempts.get(key).map_or(0, Vec::len)
    }
}

struct FakeRemote {
    store: Rc<RefCell<Store>>,
}

impl FakeRemote {
    fn new() -> (Self, Rc<RefCell<Store>>) {
        let mut s = Store::default();
        s.dirs.insert("/my-files".to_string());
        let rc = Rc::new(RefCell::new(s));
        (FakeRemote { store: rc.clone() }, rc)
    }
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

impl Remote for FakeRemote {
    fn list_dir_probe(&self, remote_path: &str) -> anyhow::Result<ListOutcome> {
        if self.store.borrow().notfound_dir.as_deref() == Some(remote_path.trim_end_matches('/')) {
            return Ok(ListOutcome::NotFound);
        }
        Ok(ListOutcome::Listed(self.list_dir(remote_path)?))
    }

    fn list_dir(&self, remote_path: &str) -> anyhow::Result<Vec<Entry>> {
        let prefix = format!("{}/", remote_path.trim_end_matches('/'));
        let s = self.store.borrow();
        if s.fail_dir.as_deref() == Some(remote_path.trim_end_matches('/')) {
            anyhow::bail!("simulated listing failure for {remote_path}");
        }
        let mut out = Vec::new();
        for d in &s.dirs {
            if let Some(rest) = d.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(Entry {
                        path: rest.to_string(),
                        is_dir: true,
                        ..Default::default()
                    });
                }
            }
        }
        for (f, data) in &s.files {
            if let Some(rest) = f.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.push(Entry {
                        path: rest.to_string(),
                        is_dir: false,
                        size: data.len() as u64,
                        mtime: s.mtimes.get(f).copied(),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(out)
    }

    fn create_folder(&self, parent: &str, name: &str) -> anyhow::Result<()> {
        self.store
            .borrow_mut()
            .dirs
            .insert(format!("{}/{}", parent.trim_end_matches('/'), name));
        Ok(())
    }

    fn upload(&self, local_path: &str, remote_parent: &str) -> anyhow::Result<Option<Uploaded>> {
        if let Some((path, bytes)) = self.store.borrow_mut().edit_on_upload.take() {
            std::fs::write(path, bytes)?;
        }
        let data = std::fs::read(local_path)?;
        let key = format!(
            "{}/{}",
            remote_parent.trim_end_matches('/'),
            basename(local_path)
        );
        if self.store.borrow().fail_upload.as_deref() == Some(key.as_str()) {
            anyhow::bail!("simulated upload failure for {key}");
        }
        if let Some(error) = self.store.borrow_mut().attempt(&key) {
            return Err(error);
        }
        let mtime = std::fs::metadata(local_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let mut s = self.store.borrow_mut();
        if let Some(m) = mtime {
            s.mtimes.insert(key.clone(), m);
        }
        s.files.insert(key, data);
        Ok(None)
    }

    fn download(&self, remote_path: &str, local_dest: &str) -> anyhow::Result<()> {
        if self.store.borrow().fail_download.as_deref() == Some(remote_path) {
            anyhow::bail!("simulated download failure for {remote_path}");
        }
        if let Some(error) = self.store.borrow_mut().attempt(remote_path) {
            return Err(error);
        }
        let data = self
            .store
            .borrow()
            .files
            .get(remote_path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such remote file: {remote_path}"))?;
        std::fs::create_dir_all(local_dest)?;
        std::fs::write(Path::new(local_dest).join(basename(remote_path)), data)?;
        Ok(())
    }

    fn trash(&self, remote_path: &str) -> anyhow::Result<()> {
        let p = remote_path.trim_end_matches('/').to_string();
        let mut s = self.store.borrow_mut();
        s.files.remove(&p);
        s.mtimes.remove(&p);
        s.dirs.remove(&p);
        let pref = format!("{p}/");
        s.files.retain(|k, _| !k.starts_with(&pref));
        s.mtimes.retain(|k, _| !k.starts_with(&pref));
        s.dirs.retain(|k| !k.starts_with(&pref));
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let mut s = self.store.borrow_mut();
        let data = s
            .files
            .remove(from)
            .ok_or_else(|| anyhow::anyhow!("no such remote file: {from}"))?;
        s.files.insert(to.to_string(), data);
        s.moves += 1;
        Ok(())
    }
}

// --- helpers ----------------------------------------------------------------
fn cfg(root: &Path) -> Config {
    Config {
        sidecar: None,
        scan_threads: 0,
        download_threads: 0,
        remote_root: "/my-files".into(),
        propagate_deletes: false,
        max_delete_files: 200,
        max_delete_percent: 30,
        auto_sync: false,
        run_in_tray: false,
        x11_compat: false,
        local_delete: LocalDelete::Trash,
        conflict: ConflictPolicy::KeepBoth,
        compare: Compare::SizeMtime,
        full_walk_interval: 86400,
        scan_interval_secs: 120,
        debounce_secs: 2,
        update_channel: UpdateChannel::Stable,
        check_on_launch: false,
        upload_limit_kbps: 0,
        download_limit_kbps: 0,
        notifications: true,
        theme: Theme::System,
        file_manager_bookmarks: false,
        session_store: Default::default(),
        session_key_file: None,
        api_listen: None,
        api_token_file: None,
        state_dir: root.join("state"),
        pairs: vec![Pair {
            name: "docs".into(),
            local: root.join("local"),
            remote: "/my-files/docs".into(),
            auto: true,
            exclude: vec![],
        }],
        source_path: None,
    }
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn run(cfg: &Config, remote: FakeRemote, resync: bool) {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).unwrap();
}

// Like `run`, but surfaces the pair result instead of unwrapping — used to
// assert that a pair is REFUSED (e.g. a vanished local root).
fn try_run(cfg: &Config, remote: FakeRemote, resync: bool) -> anyhow::Result<()> {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).map(|_| ())
}

fn remote_files(store: &Rc<RefCell<Store>>) -> BTreeMap<String, String> {
    store
        .borrow()
        .files
        .iter()
        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
        .collect()
}

// A fresh temp dir under the system temp, cleaned at drop.
struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("neutronsync-test-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- tests ------------------------------------------------------------------
#[test]
fn initial_upload() {
    let t = Tmp::new("init");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hello");
    write(&t.path().join("local/sub/b.txt"), "world");
    run(&c, fake, false);
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/a.txt").map(String::as_str),
        Some("hello")
    );
    assert_eq!(
        rf.get("/my-files/docs/sub/b.txt").map(String::as_str),
        Some("world")
    );
}

#[test]
fn download_new_remote() {
    let t = Tmp::new("dl");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // establish empty baseline
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/remote.txt".into(), b"from-cloud".to_vec());
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let got = std::fs::read_to_string(t.path().join("local/remote.txt")).unwrap();
    assert_eq!(got, "from-cloud");
}

#[test]
fn modify_local_uploads() {
    let t = Tmp::new("mod");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "v1");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "v2-longer");
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/a.txt")
            .map(String::as_str),
        Some("v2-longer")
    );
}

#[test]
fn delete_local_no_propagate() {
    let t = Tmp::new("delnop");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "keep");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        store.borrow().files.contains_key("/my-files/docs/a.txt"),
        "remote copy preserved"
    );
    assert!(
        !t.path().join("local/a.txt").exists(),
        "not resurrected locally"
    );
}

#[test]
fn delete_local_propagate_trashes_remote() {
    let t = Tmp::new("delprop");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "bye");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !store.borrow().files.contains_key("/my-files/docs/a.txt"),
        "remote copy trashed"
    );
}

#[test]
fn delete_remote_propagate_removes_local() {
    let t = Tmp::new("delrem");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = LocalDelete::Remove; // deterministic (no real trash)
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/gone.txt"), "data");
    run(&c, fake, false);
    store.borrow_mut().files.remove("/my-files/docs/gone.txt");
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !t.path().join("local/gone.txt").exists(),
        "local copy removed"
    );
}

#[test]
fn conflict_keep_both() {
    let t = Tmp::new("conf");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "local-change");
    store.borrow_mut().files.insert(
        "/my-files/docs/a.txt".into(),
        b"remote-change-different".to_vec(),
    );
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let names: Vec<String> = std::fs::read_dir(t.path().join("local"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("conflict")),
        "conflict copy created: {names:?}"
    );
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "remote-change-different"
    );
    let contents: BTreeSet<String> = remote_files(&store).into_values().collect();
    assert!(
        contents.contains("remote-change-different") && contents.contains("local-change"),
        "both versions on remote"
    );
}

#[test]
fn idempotent_second_run() {
    let t = Tmp::new("idem");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "x");
    write(&t.path().join("local/d/e.txt"), "y");
    run(&c, fake, false);
    // second run: nothing should change
    let (fake2, _s2) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake2, &log, false);
    let res = eng.sync_pair(&c.pairs[0], false).unwrap();
    assert_eq!(res.applied, 0, "second run is a no-op");
}

#[test]
fn manual_xdg_trash_fallback() {
    // trash_local with gio=None must move the file into $XDG_DATA_HOME/Trash.
    let t = Tmp::new("trash");
    let data_home = t.path().join("xdgdata");
    std::env::set_var("XDG_DATA_HOME", &data_home);
    let victim = t.path().join("victim.txt");
    std::fs::write(&victim, "bye").unwrap();
    neutronsync::trash::trash_local(&victim, None).unwrap();
    std::env::remove_var("XDG_DATA_HOME");
    assert!(!victim.exists(), "victim moved out");
    let files = std::fs::read_dir(data_home.join("Trash/files"))
        .unwrap()
        .count();
    assert!(files >= 1, "file landed in XDG trash");
}

#[test]
fn sha1_mode_idempotent() {
    // Exercises the (size,mtime)->sha1 cache path: a second run in sha1 mode,
    // with the file unchanged, must reuse the cached hash and do nothing.
    let t = Tmp::new("sha1");
    let mut c = cfg(t.path());
    c.compare = Compare::Sha1;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "content");
    run(&c, fake, false); // upload; baseline stores the local sha1
    let (fake2, _s2) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake2, &log, false);
    let res = eng.sync_pair(&c.pairs[0], false).unwrap();
    assert_eq!(res.applied, 0, "sha1 second run is a no-op");
}

struct CapSink(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
impl EventSink for CapSink {
    fn emit(&self, ev: &SyncEvent) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}

#[test]
fn emits_structured_events() {
    let t = Tmp::new("events");
    let c = cfg(t.path());
    let (fake, _store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hi");
    let evs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = CapSink(evs.clone());
    let sink_ref: &dyn EventSink = &sink;
    let cancel = AtomicBool::new(false);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake, &log, false);
    eng.set_observer(Some(sink_ref), Some(&cancel));
    eng.sync_pair(&c.pairs[0], false).unwrap();
    let joined = evs.lock().unwrap().join("\n");
    for want in [
        "PairStarted",
        "ScanStarted",
        "Planned",
        "OpFinished",
        "PairFinished",
    ] {
        assert!(joined.contains(want), "missing {want} in:\n{joined}");
    }
}

// Rebuild a FakeRemote sharing an existing store (a run consumes the remote).
fn reuse(store: &Rc<RefCell<Store>>) -> (FakeRemote, Rc<RefCell<Store>>) {
    (
        FakeRemote {
            store: store.clone(),
        },
        store.clone(),
    )
}

#[test]
fn local_rename_becomes_remote_move() {
    let t = Tmp::new("ren");
    let mut c = cfg(t.path());
    c.propagate_deletes = true; // a rename surfaces as delete-old + create-new
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hello world");
    run(&c, fake, false); // initial upload + baseline
    std::fs::rename(t.path().join("local/a.txt"), t.path().join("local/b.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/b.txt").map(String::as_str),
        Some("hello world")
    );
    assert!(
        !rf.contains_key("/my-files/docs/a.txt"),
        "old name should be gone"
    );
    assert_eq!(
        store.borrow().moves,
        1,
        "rename should move, not delete + re-upload"
    );
}

#[test]
fn incomplete_remote_scan_suppresses_delete() {
    // A folder that can't be listed must NOT be read as "remote deleted" —
    // deletions are suppressed and the baseline is left untouched, so a
    // transient listing failure can never destroy local data.
    let t = Tmp::new("incomplete");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs".into());
        s.dirs.insert("/my-files/docs/sub".into());
        s.files
            .insert("/my-files/docs/sub/a.txt".into(), b"hi".to_vec());
    }
    // First sync: pulls the file down and records a baseline.
    run(&c, fake, false);
    let local_file = t.path().join("local/sub/a.txt");
    assert!(local_file.exists(), "file should download on first sync");

    // Now the subfolder listing fails; a.txt vanishes from the remote view.
    store.borrow_mut().fail_dir = Some("/my-files/docs/sub".into());
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);

    // The local copy must survive (no delete propagated), and the remote file
    // is untouched too.
    assert!(
        local_file.exists(),
        "local file must survive an incomplete remote scan"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/sub/a.txt"),
        "remote file must be untouched"
    );
}

#[test]
fn downloads_many_new_remote_files() {
    // Several new remote files should all be pulled down (exercises the batch
    // download path; FakeRemote uses the sequential default of download_many).
    let t = Tmp::new("dlmany");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // establish empty baseline
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs/sub".into());
        for i in 0..5 {
            s.files.insert(
                format!("/my-files/docs/f{i}.txt"),
                format!("body {i}").into_bytes(),
            );
        }
        s.files
            .insert("/my-files/docs/sub/deep.txt".into(), b"deep".to_vec());
    }
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    for i in 0..5 {
        let p = t.path().join(format!("local/f{i}.txt"));
        assert!(p.exists(), "f{i}.txt should have downloaded");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("body {i}"));
    }
    assert!(
        t.path().join("local/sub/deep.txt").exists(),
        "nested file should download too"
    );
}

#[test]
fn failed_download_is_pending_not_deleted() {
    // A download that fails is recorded pending (not synced). On the next run
    // the still-missing local file must be RE-DOWNLOADED, never mistaken for a
    // local deletion and trashed remotely — even with propagate_deletes on.
    let t = Tmp::new("pending");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // empty baseline
    {
        let mut s = store.borrow_mut();
        s.files
            .insert("/my-files/docs/keep.txt".into(), b"payload".to_vec());
        s.fail_download = Some("/my-files/docs/keep.txt".into());
    }
    // Run 1: the download fails -> file recorded pending_down, not synced.
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !t.path().join("local/keep.txt").exists(),
        "download was supposed to fail"
    );
    assert!(
        store.borrow().files.contains_key("/my-files/docs/keep.txt"),
        "remote file must NOT be trashed after a failed download"
    );

    // Run 2: downloads work again -> the pending file resumes, not deleted.
    store.borrow_mut().fail_download = None;
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/keep.txt")).unwrap(),
        "payload",
        "pending file should download on the next run"
    );
    assert!(
        store.borrow().files.contains_key("/my-files/docs/keep.txt"),
        "remote file still present"
    );
}

#[test]
fn failed_upload_keeps_local_edit() {
    // Regression: a file present on BOTH sides, edited locally, whose upload
    // then fails must NOT be recorded as synced. On the next run it must
    // re-upload the LOCAL edit — never pull the stale remote copy back over it.
    let t = Tmp::new("failup");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    let local = t.path().join("local/x.txt");
    write(&local, "v1");
    run(&c, fake, false); // both sides now hold "v1"

    // Edit locally, then make the upload fail.
    write(&local, "v2-local-edit");
    store.borrow_mut().fail_upload = Some("/my-files/docs/x.txt".into());
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("v1"),
        "upload was supposed to fail; remote keeps the old copy"
    );
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "v2-local-edit",
        "local edit must be untouched after a failed upload"
    );

    // Uploads work again: the local edit must win (re-uploaded), and the stale
    // remote copy must never be pulled back over it.
    store.borrow_mut().fail_upload = None;
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "v2-local-edit",
        "local edit must survive — never overwritten by the old remote copy"
    );
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("v2-local-edit"),
        "the local edit should have uploaded on the retry"
    );
}

#[test]
fn edit_after_plan_does_not_raise_false_conflict() {
    // Regression: a file edited between the scan and its upload uploads the
    // NEW content. The baseline must record that content, not the plan-time
    // size, or the next local edit makes both sides look changed and raises a
    // keep-both conflict.
    let t = Tmp::new("editplan");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    let local = t.path().join("local/x.txt");
    let other = t.path().join("local/a.txt");
    write(&local, "v1");
    write(&other, "a1");
    run(&c, fake, false);

    // Both change; while a.txt (sorted first) uploads, x.txt is edited again,
    // so x.txt uploads content newer than the scan saw.
    write(&local, "v2-edit");
    write(&other, "a2-edit");
    store.borrow_mut().edit_on_upload = Some((local.clone(), b"v3-before-its-upload".to_vec()));
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);

    write(&local, "v4-after-the-upload-finished");
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("v4-after-the-upload-finished"),
        "the later local edit must upload as a plain change"
    );
    let conflicts = std::fs::read_dir(t.path().join("local"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("(conflict")
        })
        .count();
    assert_eq!(conflicts, 0, "no conflict copy may be created");
}

#[test]
fn file_replaced_by_dir_is_surfaced_not_merged() {
    // A path that is a file on one side and a directory on the other is a type
    // clash: it must be surfaced and skipped, never silently reconciled or
    // allowed to advance the baseline to the wrong type (which used to leave
    // the two sides permanently diverged).
    let t = Tmp::new("typeswap");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/x.txt"), "file");
    run(&c, fake, false); // both sides hold the file x.txt

    // Locally replace the file with a directory of the same name.
    std::fs::remove_file(t.path().join("local/x.txt")).unwrap();
    std::fs::create_dir(t.path().join("local/x.txt")).unwrap();
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("file"),
        "remote file must survive a local file->dir type clash"
    );
    assert!(t.path().join("local/x.txt").is_dir(), "local dir stays");

    // It must not silently converge: a second run still leaves the remote file
    // intact (the baseline was never advanced to the wrong type).
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("file"),
    );
}

#[test]
fn missing_local_root_refuses_delete() {
    // A vanished local root (e.g. an unmounted drive) must NOT be read as a
    // mass deletion: the pair is refused and the remote copy is left intact.
    let t = Tmp::new("missroot");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "keep");
    write(&t.path().join("local/b.txt"), "keep too");
    run(&c, fake, false);

    // The whole local root disappears (unmounted); the baseline is still populated.
    std::fs::remove_dir_all(t.path().join("local")).unwrap();
    let (fake2, _s) = reuse(&store);
    let res = try_run(&c, fake2, false);
    assert!(
        res.is_err(),
        "a missing local root must be refused, not synced"
    );
    let rf = remote_files(&store);
    assert!(
        rf.contains_key("/my-files/docs/a.txt") && rf.contains_key("/my-files/docs/b.txt"),
        "remote files must survive a vanished local root"
    );
}

#[test]
fn exclude_skips_subtree() {
    // An excluded sub-path is never uploaded/downloaded; everything else syncs.
    let t = Tmp::new("excl");
    let mut c = cfg(t.path());
    c.pairs[0].exclude = vec!["APPS".into()];
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);
    let rf = remote_files(&store);
    assert!(
        rf.contains_key("/my-files/docs/keep.txt"),
        "non-excluded file should upload"
    );
    assert!(
        !rf.keys().any(|k| k.contains("/APPS")),
        "excluded APPS must never reach the remote"
    );
}

#[test]
fn excluding_after_sync_touches_neither_side() {
    // Exclude = freeze: an already-synced subtree stays put on BOTH sides, with
    // deletes on — the feature must never issue a delete for excluded paths.
    let t = Tmp::new("exclafter");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "APPS should be synced before we exclude it"
    );

    // Now exclude APPS and sync again.
    c.pairs[0].exclude = vec!["APPS".into()];
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "remote APPS must survive being excluded (never a Proton-side op)"
    );
    assert!(
        t.path().join("local/APPS/app.bin").exists(),
        "local APPS must survive being excluded (freeze, not delete)"
    );
}

#[test]
fn reinclude_after_local_removal_redownloads_never_deletes_remote() {
    // The Dropbox "free up space" lifecycle: exclude, remove the local copy,
    // then re-include -> it re-downloads from the cloud and NEVER deletes remote.
    let t = Tmp::new("reincl");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);

    // Exclude APPS (prunes its baseline), then the user frees local space.
    c.pairs[0].exclude = vec!["APPS".into()];
    let (f2, _s) = reuse(&store);
    run(&c, f2, false);
    std::fs::remove_dir_all(t.path().join("local/APPS")).unwrap();
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "remote still holds APPS while it's excluded"
    );

    // Re-include: must re-download from the cloud, remote untouched.
    c.pairs[0].exclude = vec![];
    let (f3, _s) = reuse(&store);
    run(&c, f3, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/APPS/app.bin")).unwrap(),
        "app",
        "re-include should restore the local copy from the cloud"
    );
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "re-include must never delete the remote copy"
    );
}

#[test]
fn cancelled_run_does_not_record_untransferred_as_synced() {
    // Data-loss regression. A run cancelled before its transfers run must NOT
    // record the never-transferred files in the baseline. Otherwise the next
    // run sees them in the baseline but absent locally and "propagates" a bogus
    // remote deletion. With the positive-confirmation commit, a cancelled
    // download leaves the baseline untouched, so the next run RE-DOWNLOADS it
    // and the remote copy survives — even with propagate_deletes on.
    let t = Tmp::new("cancel-dl");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    std::fs::create_dir_all(t.path().join("local")).unwrap();

    let (fake, store) = FakeRemote::new();
    run(&c, fake, false); // establish an empty baseline

    // A new remote file appears after the baseline was taken.
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/report.txt".into(), b"payload".to_vec());

    // Run 1: cancelled up front, so the planned download never executes.
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let cancel = AtomicBool::new(true);
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.set_observer(None, Some(&cancel));
        let _ = eng.sync_pair(&c.pairs[0], false);
    }
    assert!(
        !t.path().join("local/report.txt").exists(),
        "cancelled run should not have downloaded anything"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/report.txt"),
        "cancelled run must never trash the remote file"
    );

    // Run 2: not cancelled. The file must DOWNLOAD (be treated as new-remote),
    // never mistaken for a local deletion and trashed.
    {
        let (fake3, _s) = reuse(&store);
        run(&c, fake3, false);
    }
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/report.txt")).unwrap(),
        "payload",
        "file must download on the recovery run, not be deleted"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/report.txt"),
        "remote file must still exist after the recovery run"
    );
}

#[test]
fn excluded_subtree_is_not_synced() {
    // An excluded sub-path must be invisible to the engine: never downloaded,
    // never created locally, never recorded — while its siblings sync normally.
    let t = Tmp::new("excl-walk");
    let mut c = cfg(t.path());
    c.pairs[0].exclude = vec!["sub".into()];
    std::fs::create_dir_all(t.path().join("local")).unwrap();

    let (fake, store) = FakeRemote::new();
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs".into());
        s.dirs.insert("/my-files/docs/sub".into());
        s.files
            .insert("/my-files/docs/top.txt".into(), b"top".to_vec());
        s.files
            .insert("/my-files/docs/sub/inner.txt".into(), b"inner".to_vec());
    }
    run(&c, fake, false);

    assert!(
        t.path().join("local/top.txt").exists(),
        "non-excluded file should download"
    );
    assert!(
        !t.path().join("local/sub/inner.txt").exists(),
        "excluded file must NOT download"
    );
    assert!(
        !t.path().join("local/sub").exists(),
        "excluded folder must not even be created locally"
    );
}

#[test]
fn scoped_sync_only_touches_its_subtree() {
    // A scoped sync reconciles ONLY files under the scope. Anything outside it
    // is never scanned, so even a file that has gone missing locally must not be
    // seen as a deletion and trashed remotely — proving scope containment.
    let t = Tmp::new("scoped");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "a1");
    write(&t.path().join("local/B/other.txt"), "b1");
    run(&c, fake, false); // full sync: A/keep.txt and B/other.txt on both sides

    // Add a file in A, and delete B/other.txt locally.
    write(&t.path().join("local/A/new.txt"), "a2");
    std::fs::remove_file(t.path().join("local/B/other.txt")).unwrap();

    // Sync ONLY sub-folder A.
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.sync_pair_scoped(&c.pairs[0], false, Some("A")).unwrap();
    }

    let remote = remote_files(&store);
    assert_eq!(
        remote.get("/my-files/docs/A/new.txt").map(String::as_str),
        Some("a2"),
        "new file inside the scope must upload"
    );
    assert!(
        remote.contains_key("/my-files/docs/B/other.txt"),
        "file OUTSIDE the scope must be untouched even though it's gone locally"
    );
    assert_eq!(
        remote.get("/my-files/docs/A/keep.txt").map(String::as_str),
        Some("a1"),
        "unchanged in-scope file stays"
    );
}

#[test]
fn empty_scope_is_whole_pair_not_a_slash_folder() {
    // Regression: a scoped sync with an empty scope ("") must behave as a normal
    // whole-pair sync, NOT prefix paths with a stray "/" (which used to make
    // every entry look new and mass-recreate folders on the remote).
    let t = Tmp::new("emptyscope");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "a1");
    write(&t.path().join("local/top.txt"), "t1");
    run(&c, fake, false); // full sync: both on remote + baseline

    // A no-op scoped sync with empty scope must NOT re-create/re-upload anything.
    let before = remote_files(&store);
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        let r = eng.sync_pair_scoped(&c.pairs[0], false, Some("")).unwrap();
        assert_eq!(
            r.applied, 0,
            "empty-scope sync of an in-sync pair must be a no-op"
        );
    }
    let after = remote_files(&store);
    assert_eq!(before, after, "empty-scope sync must not change the remote");
    // No stray leading-slash keys were created.
    assert!(
        !store
            .borrow()
            .dirs
            .iter()
            .any(|d| d.contains("//") || d.ends_with('/')),
        "no malformed remote dir paths"
    );
}

#[test]
fn shallow_sync_reconciles_only_direct_children() {
    // A shallow sync of folder A uploads a new file placed directly in A, but
    // does NOT descend into A/sub — a deeper local edit is left for that folder's
    // own (recursive-inotify) event, so touching A never re-walks its subtree.
    let t = Tmp::new("shallow");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/file1.txt"), "one");
    write(&t.path().join("local/A/sub/file2.txt"), "two");
    run(&c, fake, false); // full sync: both files + A + A/sub on remote & baseline

    // Add a file directly in A, and (separately) change the deep file.
    write(&t.path().join("local/A/new.txt"), "new");
    write(&t.path().join("local/A/sub/file2.txt"), "two-EDITED");

    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.sync_pair_shallow(&c.pairs[0], "A").unwrap();
    }

    let remote = remote_files(&store);
    assert_eq!(
        remote.get("/my-files/docs/A/new.txt").map(String::as_str),
        Some("new"),
        "new direct file must upload"
    );
    assert_eq!(
        remote
            .get("/my-files/docs/A/sub/file2.txt")
            .map(String::as_str),
        Some("two"),
        "deep file must be UNTOUCHED by a shallow sync of A (no recursion)"
    );
    assert_eq!(
        remote.get("/my-files/docs/A/file1.txt").map(String::as_str),
        Some("one"),
        "unchanged direct file stays"
    );
}

// ---- streaming full walk -------------------------------------------------

// Run the sequential streaming walk (the tested reference for run_sync_streaming).
fn run_streaming(cfg: &Config, remote: FakeRemote) {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair_streaming(&cfg.pairs[0]).unwrap();
}

// Seed a remote directory + a file under /my-files/docs/<rel>.
fn seed_remote_file(store: &Rc<RefCell<Store>>, rel: &str, body: &str) {
    let mut s = store.borrow_mut();
    // ensure every ancestor dir exists
    let full = format!("/my-files/docs/{rel}");
    let parent = &full[..full.rfind('/').unwrap()];
    let mut acc = String::from("/my-files");
    for comp in parent.trim_start_matches("/my-files/").split('/') {
        if comp.is_empty() {
            continue;
        }
        acc.push('/');
        acc.push_str(comp);
        s.dirs.insert(acc.clone());
    }
    s.files.insert(full, body.as_bytes().to_vec());
}

#[test]
fn streaming_downloads_full_remote_tree() {
    // Empty local, a nested remote tree -> streaming downloads every file at
    // every depth, creating the intermediate folders.
    let t = Tmp::new("stream-dl");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    seed_remote_file(&store, "top.txt", "T");
    seed_remote_file(&store, "A/a.txt", "A");
    seed_remote_file(&store, "A/B/deep.txt", "D");
    seed_remote_file(&store, "A/B/C/deeper.txt", "DD");

    run_streaming(&c, fake);

    for (rel, body) in [
        ("top.txt", "T"),
        ("A/a.txt", "A"),
        ("A/B/deep.txt", "D"),
        ("A/B/C/deeper.txt", "DD"),
    ] {
        assert_eq!(
            std::fs::read_to_string(t.path().join("local").join(rel)).ok(),
            Some(body.to_string()),
            "streaming must download {rel} (walking into every subfolder)"
        );
    }
}

#[test]
fn streaming_uploads_full_local_tree() {
    // A nested local tree, empty remote -> streaming uploads every file.
    let t = Tmp::new("stream-up");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/top.txt"), "T");
    write(&t.path().join("local/A/a.txt"), "A");
    write(&t.path().join("local/A/B/deep.txt"), "D");
    write(&t.path().join("local/A/B/C/deeper.txt"), "DD");

    run_streaming(&c, fake);

    let remote = remote_files(&store);
    for (rel, body) in [
        ("top.txt", "T"),
        ("A/a.txt", "A"),
        ("A/B/deep.txt", "D"),
        ("A/B/C/deeper.txt", "DD"),
    ] {
        assert_eq!(
            remote
                .get(&format!("/my-files/docs/{rel}"))
                .map(String::as_str),
            Some(body),
            "streaming must upload {rel}"
        );
    }
}

#[test]
fn streaming_is_idempotent() {
    // A second streaming run over an already-synced tree does nothing.
    let t = Tmp::new("stream-idem");
    let c = cfg(t.path());
    write(&t.path().join("local/A/a.txt"), "A");
    write(&t.path().join("local/A/B/deep.txt"), "D");
    let (fake, store) = FakeRemote::new();
    run_streaming(&c, fake); // first run uploads

    let before = remote_files(&store);
    let (fake2, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake2, &log, false);
    let r = eng.sync_pair_streaming(&c.pairs[0]).unwrap();
    assert_eq!(r.applied, 0, "second streaming run must be a no-op");
    assert_eq!(
        before,
        remote_files(&store),
        "remote unchanged on the second run"
    );
}

#[test]
fn streaming_propagates_nested_delete() {
    // With deletes on, removing a nested local file trashes the remote copy;
    // untouched siblings survive.
    let t = Tmp::new("stream-del");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "K");
    write(&t.path().join("local/A/gone.txt"), "G");
    run_streaming(&c, fake); // both on remote + baseline

    std::fs::remove_file(t.path().join("local/A/gone.txt")).unwrap();
    let (fake2, _s) = reuse(&store);
    run_streaming(&c, fake2);

    let remote = remote_files(&store);
    assert!(
        !remote.contains_key("/my-files/docs/A/gone.txt"),
        "deleted nested file must be trashed on the remote"
    );
    assert_eq!(
        remote.get("/my-files/docs/A/keep.txt").map(String::as_str),
        Some("K"),
        "sibling must survive"
    );
}

#[test]
fn streaming_respects_excludes() {
    // An excluded subtree is never walked or downloaded.
    let t = Tmp::new("stream-excl");
    let mut c = cfg(t.path());
    c.pairs[0].exclude = vec!["Secret".into()];
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    seed_remote_file(&store, "Public/ok.txt", "OK");
    seed_remote_file(&store, "Secret/private.txt", "NO");

    run_streaming(&c, fake);

    assert!(
        t.path().join("local/Public/ok.txt").exists(),
        "non-excluded file downloads"
    );
    assert!(
        !t.path().join("local/Secret").exists(),
        "excluded subtree must not be walked or created"
    );
}

#[test]
fn streaming_matches_batch_walk() {
    // Equivalence: the streaming walk reaches the SAME end state as the batch
    // full walk for a mixed tree (local-only, remote-only, identical-both).
    let scenario = |root: &std::path::Path, store: &Rc<RefCell<Store>>| {
        write(&root.join("local/both.txt"), "same");
        write(&root.join("local/L/localonly.txt"), "L");
        seed_remote_file(store, "both.txt", "same");
        seed_remote_file(store, "R/remoteonly.txt", "R");
    };

    // Batch.
    let tb = Tmp::new("equiv-batch");
    let cb = cfg(tb.path());
    let (fb, sb) = FakeRemote::new();
    scenario(tb.path(), &sb);
    run(&cb, fb, false);

    // Streaming.
    let ts = Tmp::new("equiv-stream");
    let cs = cfg(ts.path());
    let (fs, ss) = FakeRemote::new();
    scenario(ts.path(), &ss);
    run_streaming(&cs, fs);

    // Same remote file set + contents.
    assert_eq!(
        remote_files(&sb),
        remote_files(&ss),
        "streaming and batch must produce the same remote state"
    );
    // Same local file set.
    let local_set = |root: &std::path::Path| -> std::collections::BTreeMap<String, String> {
        walkdir::WalkDir::new(root.join("local"))
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let rel = e
                    .path()
                    .strip_prefix(root.join("local"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (rel, std::fs::read_to_string(e.path()).unwrap())
            })
            .collect()
    };
    assert_eq!(
        local_set(tb.path()),
        local_set(ts.path()),
        "streaming and batch must produce the same local state"
    );
}

#[test]
fn shallow_root_reconciles_top_level_only() {
    // A shallow sync of the root ("") handles top-level entries only, not deep
    // ones — deep files stay for their own folder's reconcile.
    let t = Tmp::new("shallow-root");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/toplevel.txt"), "T");
    write(&t.path().join("local/Sub/deep.txt"), "D");

    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        eng.sync_pair_shallow(&c.pairs[0], "").unwrap();
    }
    let _ = fake; // original fake unused; we drove via reuse
    let remote = remote_files(&store);
    assert_eq!(
        remote
            .get("/my-files/docs/toplevel.txt")
            .map(String::as_str),
        Some("T"),
        "top-level file uploads on a root shallow sync"
    );
    assert!(
        !remote.contains_key("/my-files/docs/Sub/deep.txt"),
        "deep file is NOT handled by a root shallow sync (its own folder does it)"
    );
}

#[test]
fn streaming_keeps_both_on_conflict() {
    // Both sides have the same path with different content -> keep-both, and the
    // streaming walk must not silently overwrite either.
    let t = Tmp::new("stream-conflict");
    let c = cfg(t.path()); // default conflict = keep-both
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/x.txt"), "LOCAL");
    seed_remote_file(&store, "A/x.txt", "REMOTE");

    let (f, _s) = reuse(&store);
    run_streaming(&c, f);
    let _ = fake;

    let remote = remote_files(&store);
    // The original name holds the remote copy; a conflict copy holds local.
    assert_eq!(
        remote.get("/my-files/docs/A/x.txt").map(String::as_str),
        Some("REMOTE"),
        "remote copy preserved under the original name"
    );
    let has_conflict_copy = remote
        .keys()
        .any(|k| k.starts_with("/my-files/docs/A/x") && k.contains("conflict"));
    assert!(
        has_conflict_copy,
        "local copy preserved as a keep-both conflict file; remote keys: {:?}",
        remote.keys().collect::<Vec<_>>()
    );
}

#[test]
fn shallow_remote_list_failure_suppresses_delete() {
    // If a folder's remote listing fails, a locally-missing file must NOT be
    // trashed on the remote (never act on a blind listing).
    let t = Tmp::new("shallow-failsafe");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "K");
    write(&t.path().join("local/A/gone.txt"), "G");
    run(&c, fake, false); // both on remote + baseline

    std::fs::remove_file(t.path().join("local/A/gone.txt")).unwrap();
    store.borrow_mut().fail_dir = Some("/my-files/docs/A".into());
    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        eng.sync_pair_shallow(&c.pairs[0], "A").unwrap();
    }
    let remote = remote_files(&store);
    assert!(
        remote.contains_key("/my-files/docs/A/gone.txt"),
        "a failed remote listing must not trash the locally-missing file"
    );
    assert!(
        remote.contains_key("/my-files/docs/A/keep.txt"),
        "and must not trash the surviving file either"
    );
}

// ---- regressions for the streaming/shallow data-safety review ------------

#[test]
fn shallow_local_read_dir_failure_suppresses_delete() {
    // If the local read_dir of the folder fails (here the path is a FILE, not a
    // dir), the reconcile must NOT read it as "all children deleted" and trash
    // the remote copies. Regression for a swallowed local read_dir error.
    let t = Tmp::new("shallow-localfail");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "K");
    run(&c, fake, false); // A/keep.txt on remote + baseline

    // Replace local folder A with a FILE named A -> read_dir(local/A) errors.
    std::fs::remove_dir_all(t.path().join("local/A")).unwrap();
    write(&t.path().join("local/A"), "now a file");

    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        let _ = eng.sync_pair_shallow(&c.pairs[0], "A");
    }
    assert!(
        remote_files(&store).contains_key("/my-files/docs/A/keep.txt"),
        "a failed local read_dir must not trash the remote copies"
    );
}

#[test]
fn shallow_cancelled_dir_delete_keeps_descendant_baseline() {
    // A directory delete that is CANCELLED (never executed) must not purge the
    // folder's descendant baseline rows. Regression: the descendant purge used
    // to fire on a merely-planned (not confirmed) delete.
    let t = Tmp::new("shallow-canceldel");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/sub/y.txt"), "Y");
    run(&c, fake, false); // baseline: A, A/sub, A/sub/y.txt

    std::fs::remove_dir_all(t.path().join("local/A")).unwrap();
    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let cancel = AtomicBool::new(true); // pre-cancelled: no op runs
        let mut eng = Engine::new(&c, f, &log, false);
        eng.set_observer(None, Some(&cancel));
        let _ = eng.sync_pair_shallow(&c.pairs[0], "");
    }
    let base = neutronsync::state::load_baseline(&c.state_dir, &c.pairs[0].name).unwrap();
    assert!(
        base.contains_key("A/sub/y.txt"),
        "a cancelled dir-delete must leave descendant baseline rows intact"
    );
    assert!(
        remote_files(&store).contains_key("/my-files/docs/A/sub/y.txt"),
        "and must not have trashed the remote subtree"
    );
}

#[test]
fn shallow_remote_notfound_suppresses_delete() {
    // A remote NotFound (a race, or a transient error misread as not-found) must
    // not delete the still-present local children. Regression for NotFound
    // collapsing to an empty listing without the delete-suppression guard.
    let t = Tmp::new("shallow-notfound");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/x.txt"), "X");
    run(&c, fake, false); // A/x.txt on both + baseline

    store.borrow_mut().notfound_dir = Some("/my-files/docs/A".into());
    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        let _ = eng.sync_pair_shallow(&c.pairs[0], "A");
    }
    assert!(
        t.path().join("local/A/x.txt").exists(),
        "a remote NotFound must not delete the still-present local file"
    );
}

// ---- deletion guard --------------------------------------------------------

/// Captures every event as a value, so tests can match on fields.
struct EventLog(std::sync::Arc<std::sync::Mutex<Vec<SyncEvent>>>);
impl EventSink for EventLog {
    fn emit(&self, ev: &SyncEvent) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

struct Run {
    result: neutronsync::engine::SyncResult,
    events: Vec<SyncEvent>,
}

/// Run one full pair sync and return its result and every event it emitted.
fn run_logged(cfg: &Config, remote: FakeRemote, resync: bool, dry_run: bool) -> Run {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = EventLog(events.clone());
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, dry_run);
    eng.set_observer(Some(&sink), None);
    let result = eng.sync_pair(&cfg.pairs[0], resync).unwrap();
    let events = events.lock().unwrap().clone();
    Run { result, events }
}

fn held_events(events: &[SyncEvent]) -> Vec<(usize, usize, usize)> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::DeletesHeld {
                local,
                remote,
                tracked,
                ..
            } => Some((*local, *remote, *tracked)),
            _ => None,
        })
        .collect()
}

fn released(events: &[SyncEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, SyncEvent::DeletesReleased { .. }))
}

fn held_row(cfg: &Config) -> Option<neutronsync::stats::HeldDeletes> {
    Stats::open(&cfg.state_dir)
        .unwrap()
        .held_for(&cfg.pairs[0].name)
        .unwrap()
}

fn baseline(cfg: &Config) -> BTreeMap<String, Entry> {
    neutronsync::state::load_baseline(&cfg.state_dir, &cfg.pairs[0].name).unwrap()
}

/// `n` files named `<dir>/f<i>.txt` (or `f<i>.txt` at the root) holding `body`.
fn seed_files(root: &Path, dir: &str, n: usize) {
    for i in 0..n {
        let rel = if dir.is_empty() {
            format!("f{i}.txt")
        } else {
            format!("{dir}/f{i}.txt")
        };
        write(&root.join("local").join(&rel), &format!("body {dir} {i}"));
    }
}

fn remove_files(root: &Path, dir: &str, from: usize, to: usize) {
    for i in from..to {
        let rel = if dir.is_empty() {
            format!("f{i}.txt")
        } else {
            format!("{dir}/f{i}.txt")
        };
        std::fs::remove_file(root.join("local").join(rel)).unwrap();
    }
}

/// 300 tracked files on both sides (250 flat, 50 under `sub`), deletes on.
fn tracked_300(tag: &str) -> (Tmp, Config, Rc<RefCell<Store>>) {
    let t = Tmp::new(tag);
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = LocalDelete::Remove;
    let (fake, store) = FakeRemote::new();
    seed_files(t.path(), "", 250);
    seed_files(t.path(), "sub", 50);
    run(&c, fake, false);
    assert_eq!(remote_files(&store).len(), 300);
    assert_eq!(baseline(&c).values().filter(|e| !e.is_dir).count(), 300);
    (t, c, store)
}

#[test]
fn mass_local_deletion_is_held_on_the_batch_path() {
    // AC-3: 250 of 300 deleted locally is far over the allowance (200). The
    // run trashes nothing, still uploads a new file, reports the hold once,
    // stores it, and keeps every baseline row so the next run sees it again.
    let (t, c, store) = tracked_300("hold-batch");
    remove_files(t.path(), "", 0, 250);
    write(&t.path().join("local/new.txt"), "created in the same run");

    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    let remote = remote_files(&store);
    assert_eq!(
        remote.keys().filter(|k| k.contains("/f")).count(),
        300,
        "nothing may be trashed on Proton"
    );
    assert_eq!(
        remote.get("/my-files/docs/new.txt").map(String::as_str),
        Some("created in the same run"),
        "the upload in the same run still happens"
    );
    assert_eq!(held_events(&r.events), vec![(0, 250, 300)]);
    assert_eq!(
        r.result.held.map(|h| (h.local, h.remote, h.tracked)),
        Some((0, 250, 300))
    );
    let row = held_row(&c).expect("a held_deletes row");
    assert_eq!((row.local, row.remote, row.tracked), (0, 250, 300));
    let base = baseline(&c);
    for i in 0..250 {
        assert!(
            base.contains_key(&format!("f{i}.txt")),
            "row f{i} must stay"
        );
    }

    // A second run holds the same 250 again (new.txt is tracked by now).
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert_eq!(held_events(&r.events), vec![(0, 250, 301)]);
    assert_eq!(
        remote_files(&store)
            .keys()
            .filter(|k| k.contains("/f"))
            .count(),
        300
    );
    assert!(held_row(&c).is_some());
}

#[test]
fn small_deletion_propagates_without_a_hold() {
    // AC-4: 20 of 300 is under the allowance; all 20 go and no row is written.
    let (t, c, store) = tracked_300("hold-small");
    remove_files(t.path(), "", 0, 20);
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert!(held_events(&r.events).is_empty());
    assert!(r.result.held.is_none());
    let remote = remote_files(&store);
    for i in 0..20 {
        assert!(!remote.contains_key(&format!("/my-files/docs/f{i}.txt")));
    }
    assert_eq!(remote.keys().filter(|k| k.contains("/f")).count(), 280);
    assert!(held_row(&c).is_none());
}

#[test]
fn folder_delete_counts_the_files_under_it() {
    // AC-5: one folder holding 260 of 300 files is one delete op but counts
    // 260, on the batch path and on a shallow reconcile of its parent.
    for shallow in [false, true] {
        let t = Tmp::new("hold-folder");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        let (fake, store) = FakeRemote::new();
        seed_files(t.path(), "big", 260);
        seed_files(t.path(), "", 40);
        run(&c, fake, false);
        std::fs::remove_dir_all(t.path().join("local/big")).unwrap();

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = EventLog(events.clone());
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        eng.set_observer(Some(&sink), None);
        if shallow {
            eng.sync_pair_shallow(&c.pairs[0], "").unwrap();
        } else {
            let r = eng.sync_pair(&c.pairs[0], false).unwrap();
            assert_eq!(r.held.map(|h| (h.local, h.remote)), Some((0, 260)));
            assert_eq!(held_events(&events.lock().unwrap()), vec![(0, 260, 300)]);
        }
        let remote = remote_files(&store);
        assert_eq!(
            remote.keys().filter(|k| k.contains("/big/")).count(),
            260,
            "shallow={shallow}: the folder's files must survive on Proton"
        );
        assert!(remote.contains_key("/my-files/docs/big/f0.txt"));
        assert_eq!(baseline(&c).values().filter(|e| !e.is_dir).count(), 300);
    }
}

#[test]
fn streaming_walk_holds_a_mass_deletion_before_the_first_folder() {
    // AC-6 (reference walk): 250 of 300 gone across many sub-folders. The
    // up-front local count holds the run, so no folder deletes anything.
    let t = Tmp::new("hold-stream");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    for d in 0..10 {
        seed_files(t.path(), &format!("d{d}"), 30);
    }
    run_streaming(&c, fake);
    assert_eq!(remote_files(&store).len(), 300);
    for d in 0..8 {
        std::fs::remove_dir_all(t.path().join(format!("local/d{d}"))).unwrap();
    }
    remove_files(t.path(), "d8", 0, 10);

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = EventLog(events.clone());
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.set_observer(Some(&sink), None);
    let r = eng.sync_pair_streaming(&c.pairs[0]).unwrap();
    assert_eq!(remote_files(&store).len(), 300, "nothing may be trashed");
    assert_eq!(
        r.held.map(|h| (h.local, h.remote, h.tracked)),
        Some((0, 250, 300))
    );
    assert_eq!(held_events(&events.lock().unwrap()), vec![(0, 250, 300)]);
    assert_eq!(baseline(&c).values().filter(|e| !e.is_dir).count(), 300);
    assert!(held_row(&c).is_some());
}

#[test]
fn remote_folder_trash_is_held_on_the_streaming_walk() {
    // AC-7: a remote top-level folder with 260 of 300 files vanishes. The walk
    // deletes nothing locally and reports 260 that would have left this computer.
    let t = Tmp::new("hold-remote");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = LocalDelete::Remove;
    let (fake, store) = FakeRemote::new();
    seed_files(t.path(), "big", 260);
    seed_files(t.path(), "", 40);
    run_streaming(&c, fake);
    {
        let (f, _s) = reuse(&store);
        f.trash("/my-files/docs/big").unwrap();
    }
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = EventLog(events.clone());
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.set_observer(Some(&sink), None);
    let r = eng.sync_pair_streaming(&c.pairs[0]).unwrap();
    assert!(t.path().join("local/big/f0.txt").exists());
    assert_eq!(
        walkdir::WalkDir::new(t.path().join("local/big"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count(),
        260,
        "no local file may be removed"
    );
    assert_eq!(
        r.held.map(|h| (h.local, h.remote, h.tracked)),
        Some((260, 0, 300))
    );
    assert_eq!(held_events(&events.lock().unwrap()), vec![(260, 0, 300)]);
    assert_eq!(baseline(&c).values().filter(|e| !e.is_dir).count(), 300);
}

/// A pair whose deletions are held, with the 250 files then put back locally
/// so a full run would have nothing to hold.
fn held_then_restored(tag: &str) -> (Tmp, Config, Rc<RefCell<Store>>) {
    let (t, c, store) = tracked_300(tag);
    remove_files(t.path(), "", 0, 250);
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert_eq!(held_events(&r.events).len(), 1);
    assert!(held_row(&c).is_some());
    seed_files(t.path(), "", 250);
    (t, c, store)
}

#[test]
fn full_run_with_nothing_to_hold_releases_the_hold() {
    // AC-9: a later full run with nothing to hold clears the row and says so.
    let (_t, c, store) = held_then_restored("release-full");
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert!(held_events(&r.events).is_empty());
    assert!(
        released(&r.events),
        "DeletesReleased expected: {:?}",
        r.events
    );
    assert!(held_row(&c).is_none());
    // A clean follow-up does not announce a release again.
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert!(!released(&r.events));
}

#[test]
fn restore_them_is_a_resync_that_releases_the_hold() {
    // D8 "Restore them": a resync merges both sides afresh, deleting nothing,
    // brings the 250 files back and clears the hold.
    let (t, c, store) = tracked_300("release-resync");
    remove_files(t.path(), "", 0, 250);
    let (f, _s) = reuse(&store);
    run_logged(&c, f, false, false);
    assert!(held_row(&c).is_some());
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, true, false);
    assert!(released(&r.events));
    assert!(held_row(&c).is_none());
    for i in 0..250 {
        assert!(
            t.path().join(format!("local/f{i}.txt")).exists(),
            "f{i} restored"
        );
    }
    assert_eq!(remote_files(&store).len(), 300);
}

#[test]
fn hot_pass_never_releases_the_hold() {
    let (_t, c, store) = held_then_restored("release-hot");
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.sync_pair_shallow(&c.pairs[0], "").unwrap();
    assert!(
        held_row(&c).is_some(),
        "a shallow pass must not clear the hold"
    );
}

#[test]
fn scoped_run_never_releases_the_hold() {
    let (_t, c, store) = held_then_restored("release-scoped");
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.sync_pair_scoped(&c.pairs[0], false, Some("sub"))
        .unwrap();
    assert!(
        held_row(&c).is_some(),
        "a scoped run must not clear the hold"
    );
}

#[test]
fn dry_run_never_releases_the_hold() {
    let (_t, c, store) = held_then_restored("release-dry");
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, true);
    assert!(!released(&r.events));
    assert!(held_row(&c).is_some(), "a dry run must not clear the hold");
}

#[test]
fn cancelled_run_never_releases_the_hold() {
    let (_t, c, store) = held_then_restored("release-cancel");
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let cancel = AtomicBool::new(true);
    let mut eng = Engine::new(&c, f, &log, false);
    eng.set_observer(None, Some(&cancel));
    let _ = eng.sync_pair(&c.pairs[0], false);
    assert!(
        held_row(&c).is_some(),
        "a cancelled run must not clear the hold"
    );
}

#[test]
fn incomplete_remote_scan_never_releases_the_hold() {
    let (_t, c, store) = held_then_restored("release-incomplete");
    store.borrow_mut().fail_dir = Some("/my-files/docs/sub".into());
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert!(!released(&r.events));
    assert!(
        held_row(&c).is_some(),
        "an incomplete scan must not clear the hold"
    );
}

#[test]
fn delete_them_switches_the_guard_off_for_one_run() {
    // D8 "Delete them": the allowance is off for that run only; the 250
    // deletions go through and the hold is cleared.
    let (t, c, store) = tracked_300("allow-mass");
    remove_files(t.path(), "", 0, 250);
    let (f, _s) = reuse(&store);
    run_logged(&c, f, false, false);
    assert!(held_row(&c).is_some());
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = EventLog(events.clone());
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.set_observer(Some(&sink), None);
    eng.set_allow_mass_delete(true);
    let r = eng.sync_pair(&c.pairs[0], false).unwrap();
    assert!(r.held.is_none());
    assert_eq!(remote_files(&store).len(), 50);
    assert!(released(&events.lock().unwrap()));
    assert!(held_row(&c).is_none());
    assert_eq!(baseline(&c).values().filter(|e| !e.is_dir).count(), 50);
}

#[test]
fn guard_off_in_config_deletes_everything() {
    // Both options at 0: the guard is off and a mass deletion is mirrored.
    let (t, mut c, store) = tracked_300("guard-off");
    c.max_delete_files = 0;
    c.max_delete_percent = 0;
    remove_files(t.path(), "", 0, 250);
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert!(r.result.held.is_none());
    assert_eq!(remote_files(&store).len(), 50);
}

#[test]
fn rows_gone_on_both_sides_are_forgotten() {
    // A delete that reached Proton but whose run never committed leaves a
    // baseline row behind. With both sides in view that row is dropped, so
    // the next run plans nothing instead of carrying it forever.
    let t = Tmp::new("gone-both");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "a");
    write(&t.path().join("local/d/b.txt"), "b");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    std::fs::remove_dir_all(t.path().join("local/d")).unwrap();
    {
        let (f, _s) = reuse(&store);
        f.trash("/my-files/docs/a.txt").unwrap();
        f.trash("/my-files/docs/d").unwrap();
    }
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    assert_eq!(r.result.applied, 0);
    assert!(
        baseline(&c).is_empty(),
        "stale rows must be pruned: {:?}",
        baseline(&c)
    );
    // The shallow path prunes a gone folder's descendants too.
    let t2 = Tmp::new("gone-both-shallow");
    let mut c2 = cfg(t2.path());
    c2.propagate_deletes = true;
    let (fake2, store2) = FakeRemote::new();
    write(&t2.path().join("local/d/b.txt"), "b");
    write(&t2.path().join("local/d/e/c.txt"), "c");
    run(&c2, fake2, false);
    std::fs::remove_dir_all(t2.path().join("local/d")).unwrap();
    {
        let (f, _s) = reuse(&store2);
        f.trash("/my-files/docs/d").unwrap();
    }
    let (f, _s) = reuse(&store2);
    let log = Logger::silent();
    let mut eng = Engine::new(&c2, f, &log, false);
    eng.sync_pair_shallow(&c2.pairs[0], "").unwrap();
    assert!(baseline(&c2).is_empty(), "{:?}", baseline(&c2));
}

// ---- conflict inbox ----------------------------------------------------------

fn settled(events: &[SyncEvent]) -> Vec<(String, ConflictOutcome, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::ConflictSettled {
                path,
                outcome,
                copy,
                ..
            } => Some((path.clone(), *outcome, copy.clone())),
            _ => None,
        })
        .collect()
}

fn set_mtime(p: &Path, secs: i64) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
        .unwrap();
}

#[test]
fn keep_both_reports_the_copy_once_it_exists_on_both_sides() {
    // AC-13
    let t = Tmp::new("inbox-both");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "local-change");
    store.borrow_mut().files.insert(
        "/my-files/docs/a.txt".into(),
        b"remote-change-different".to_vec(),
    );
    let (f, _s) = reuse(&store);
    let r = run_logged(&c, f, false, false);
    let got = settled(&r.events);
    assert_eq!(got.len(), 1, "{got:?}");
    let (path, outcome, copy) = &got[0];
    assert_eq!(path, "a.txt");
    assert_eq!(*outcome, ConflictOutcome::KeptBoth);
    let copy = copy.as_ref().expect("the copy's relative path");
    assert!(copy.contains("(conflict"), "{copy}");
    assert_eq!(
        std::fs::read_to_string(t.path().join("local").join(copy)).unwrap(),
        "local-change"
    );
    assert_eq!(
        remote_files(&store)
            .get(&format!("/my-files/docs/{copy}"))
            .map(String::as_str),
        Some("local-change")
    );
}

#[test]
fn newer_policy_reports_the_winner_only_after_the_transfer() {
    // AC-14: local newer -> KeptLocal after the upload; remote newer ->
    // KeptRemote after the download; a failed transfer reports nothing.
    for (local_newer, fail) in [(true, false), (false, false), (true, true), (false, true)] {
        let t = Tmp::new("inbox-newer");
        let mut c = cfg(t.path());
        c.conflict = ConflictPolicy::Newer;
        let (fake, store) = FakeRemote::new();
        let local = t.path().join("local/a.txt");
        write(&local, "base");
        set_mtime(&local, 1_700_000_000);
        run(&c, fake, false);
        write(&local, "local-edit");
        let (lm, rm) = if local_newer {
            (1_700_000_900, 1_700_000_500)
        } else {
            (1_700_000_500, 1_700_000_900)
        };
        set_mtime(&local, lm);
        {
            let mut s = store.borrow_mut();
            s.files.insert(
                "/my-files/docs/a.txt".into(),
                b"remote-edit-longer".to_vec(),
            );
            s.mtimes.insert("/my-files/docs/a.txt".into(), rm);
            if fail && local_newer {
                s.fail_upload = Some("/my-files/docs/a.txt".into());
            }
            if fail && !local_newer {
                s.fail_download = Some("/my-files/docs/a.txt".into());
            }
        }
        let (f, _s) = reuse(&store);
        let r = run_logged(&c, f, false, false);
        let got = settled(&r.events);
        if fail {
            assert!(got.is_empty(), "local_newer={local_newer}: {got:?}");
            assert!(!r.result.errors.is_empty());
            continue;
        }
        let want = if local_newer {
            ConflictOutcome::KeptLocal
        } else {
            ConflictOutcome::KeptRemote
        };
        assert_eq!(got, vec![("a.txt".to_string(), want, None)]);
        let body = if local_newer {
            "local-edit"
        } else {
            "remote-edit-longer"
        };
        assert_eq!(std::fs::read_to_string(&local).unwrap(), body);
        assert_eq!(
            remote_files(&store)
                .get("/my-files/docs/a.txt")
                .map(String::as_str),
            Some(body)
        );
    }
}

/// Records settled conflicts into stats.db the way the CLI and GUI sinks do.
struct InboxSink(std::sync::Arc<Stats>);
impl EventSink for InboxSink {
    fn emit(&self, ev: &SyncEvent) {
        if let SyncEvent::ConflictSettled {
            pair,
            path,
            outcome,
            copy,
        } = ev
        {
            self.0
                .record_conflict(
                    pair,
                    path,
                    *outcome,
                    copy.as_deref(),
                    neutronsync::datefmt::now_epoch(),
                )
                .unwrap();
        }
    }
}

#[test]
fn skipped_conflict_shows_once_until_dismissed() {
    // AC-15 and AC-16: three runs over one skipped conflict leave one row
    // seen three times; dismissing hides it; the same conflict again is new.
    let t = Tmp::new("inbox-skip");
    let mut c = cfg(t.path());
    c.conflict = ConflictPolicy::Skip;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "local-change");
    store.borrow_mut().files.insert(
        "/my-files/docs/a.txt".into(),
        b"remote-change-different".to_vec(),
    );
    let stats = std::sync::Arc::new(Stats::open(&c.state_dir).unwrap());
    let sink = InboxSink(stats.clone());
    for _ in 0..3 {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        eng.set_observer(Some(&sink), None);
        eng.sync_pair(&c.pairs[0], false).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "local-change",
        "skip leaves the local file alone"
    );
    let rows = stats.conflicts().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].outcome, ConflictOutcome::Skipped);
    assert_eq!(rows[0].seen, 3);
    assert_eq!(stats.conflict_count().unwrap(), 1);
    stats.dismiss_conflict(rows[0].id).unwrap();
    assert_eq!(stats.conflict_count().unwrap(), 0);
    {
        let (f, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, f, &log, false);
        eng.set_observer(Some(&sink), None);
        eng.sync_pair(&c.pairs[0], false).unwrap();
    }
    let rows = stats.conflicts().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].seen, 1,
        "after a dismissal the conflict is news again"
    );
}

// ---- in-run retry ------------------------------------------------------------

use neutronsync::retry::Policy;
use std::time::{Duration, Instant};

/// One pair sync with a shortened retry schedule (`unit` per scheduled
/// second), an optional cancel flag, and every event it emitted.
fn run_retrying(
    cfg: &Config,
    remote: FakeRemote,
    unit: Duration,
    cancel: Option<&AtomicBool>,
) -> Run {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = EventLog(events.clone());
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.set_observer(Some(&sink), cancel);
    eng.set_retry_policy(Policy::scaled(unit));
    let result = eng.sync_pair(&cfg.pairs[0], false).unwrap();
    let events = events.lock().unwrap().clone();
    Run { result, events }
}

fn op_events(events: &[SyncEvent], path: &str) -> (usize, Vec<(bool, Option<String>)>) {
    let started = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::OpStarted { path: p, .. } if p == path))
        .count();
    let finished = events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::OpFinished {
                path: p, ok, error, ..
            } if p == path => Some((*ok, error.clone())),
            _ => None,
        })
        .collect();
    (started, finished)
}

fn script(store: &Rc<RefCell<Store>>, key: &str, codes: &[&str]) {
    store.borrow_mut().fail_script.insert(
        key.to_string(),
        codes.iter().map(|c| c.to_string()).collect(),
    );
}

const FAST: Duration = Duration::from_millis(10);

#[test]
fn upload_that_fails_twice_then_succeeds_lands_in_the_same_run() {
    let t = Tmp::new("retry-up");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "payload");
    script(&store, "/my-files/docs/a.txt", &["transient", "transient"]);
    let run = run_retrying(&c, fake, FAST, None);
    assert!(run.result.errors.is_empty(), "{:?}", run.result.errors);
    assert_eq!(store.borrow().attempts_on("/my-files/docs/a.txt"), 3);
    assert_eq!(
        remote_files(&store).get("/my-files/docs/a.txt").unwrap(),
        "payload"
    );
    assert!(baseline(&c).contains_key("a.txt"));
    assert_eq!(op_events(&run.events, "a.txt"), (1, vec![(true, None)]));
}

#[test]
fn download_that_fails_twice_then_succeeds_does_not_repeat_the_rest_of_the_batch() {
    let t = Tmp::new("retry-down");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    seed_remote_file(&store, "a.txt", "alpha");
    seed_remote_file(&store, "b.txt", "bravo");
    seed_remote_file(&store, "c.txt", "charlie");
    script(&store, "/my-files/docs/b.txt", &["transient", "transient"]);
    let run = run_retrying(&c, fake, FAST, None);
    assert!(run.result.errors.is_empty(), "{:?}", run.result.errors);
    let s = store.borrow();
    assert_eq!(s.attempts_on("/my-files/docs/a.txt"), 1);
    assert_eq!(s.attempts_on("/my-files/docs/b.txt"), 3);
    assert_eq!(s.attempts_on("/my-files/docs/c.txt"), 1);
    drop(s);
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(t.path().join("local").join(name).exists(), "{name}");
        assert!(baseline(&c).contains_key(name), "{name}");
        assert_eq!(
            op_events(&run.events, name),
            (1, vec![(true, None)]),
            "{name}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/b.txt")).unwrap(),
        "bravo"
    );
}

#[test]
fn permanent_failures_are_attempted_once_in_both_directions() {
    for code in [
        "not_found",
        "conflict",
        "auth",
        "fatal",
        "keyring_locked",
        "unknown_code",
    ] {
        let t = Tmp::new("retry-perm");
        let c = cfg(t.path());
        let (fake, store) = FakeRemote::new();
        write(&t.path().join("local/up.txt"), "local");
        seed_remote_file(&store, "down.txt", "remote");
        script(&store, "/my-files/docs/up.txt", &[code; 4]);
        script(&store, "/my-files/docs/down.txt", &[code; 4]);
        let run = run_retrying(&c, fake, FAST, None);
        assert_eq!(
            run.result.errors.len(),
            2,
            "{code}: {:?}",
            run.result.errors
        );
        assert!(
            run.result.errors.iter().all(|e| !e.contains("gave up")),
            "{code}: {:?}",
            run.result.errors
        );
        let s = store.borrow();
        assert_eq!(s.attempts_on("/my-files/docs/up.txt"), 1, "{code}");
        assert_eq!(s.attempts_on("/my-files/docs/down.txt"), 1, "{code}");
        drop(s);
        let base = baseline(&c);
        assert!(!base.contains_key("up.txt"), "{code}");
        assert!(!base.contains_key("down.txt"), "{code}");
        assert!(!t.path().join("local/down.txt").exists(), "{code}");
        assert_eq!(op_events(&run.events, "up.txt").0, 1, "{code}");
        assert_eq!(op_events(&run.events, "down.txt").0, 1, "{code}");
    }
}

#[test]
fn transient_on_every_attempt_gives_up_after_four() {
    let t = Tmp::new("retry-exhaust");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/up.txt"), "local");
    seed_remote_file(&store, "down.txt", "remote");
    script(&store, "/my-files/docs/up.txt", &["transient"; 10]);
    script(&store, "/my-files/docs/down.txt", &["transient"; 10]);
    let run = run_retrying(&c, fake, FAST, None);
    assert_eq!(run.result.errors.len(), 2, "{:?}", run.result.errors);
    for (action, key, rel) in [
        ("upload", "/my-files/docs/up.txt", "up.txt"),
        ("download", "/my-files/docs/down.txt", "down.txt"),
    ] {
        assert_eq!(store.borrow().attempts_on(key), 4, "{rel}");
        let line = run
            .result
            .errors
            .iter()
            .find(|e| e.starts_with(&format!("{action} {rel}: ")))
            .unwrap_or_else(|| panic!("{rel}: {:?}", run.result.errors));
        assert!(line.contains("api backend: transient"), "{line}");
        assert!(line.ends_with("(gave up after 4 attempts)"), "{line}");
        assert!(!baseline(&c).contains_key(rel), "{rel}");
        let (started, finished) = op_events(&run.events, rel);
        assert_eq!(started, 1, "{rel}");
        assert_eq!(finished.len(), 1, "{rel}");
        assert!(!finished[0].0, "{rel}");
        assert!(
            finished[0]
                .1
                .as_deref()
                .unwrap()
                .contains("gave up after 4 attempts"),
            "{rel}: {:?}",
            finished[0]
        );
    }
}

#[test]
fn breaker_stops_upload_retries_after_three_exhausted_files_and_a_success_resets_it() {
    let t = Tmp::new("retry-breaker-up");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    for i in 1..=7 {
        write(&t.path().join(format!("local/f{i}.txt")), "payload");
        if i != 6 {
            script(
                &store,
                &format!("/my-files/docs/f{i}.txt"),
                &["transient"; 10],
            );
        }
    }
    let run = run_retrying(&c, fake, FAST, None);
    let s = store.borrow();
    // First three: every attempt. Then the breaker is open: one attempt each.
    assert_eq!(s.attempts_on("/my-files/docs/f1.txt"), 4);
    assert_eq!(s.attempts_on("/my-files/docs/f2.txt"), 4);
    assert_eq!(s.attempts_on("/my-files/docs/f3.txt"), 4);
    assert_eq!(s.attempts_on("/my-files/docs/f4.txt"), 1);
    assert_eq!(s.attempts_on("/my-files/docs/f5.txt"), 1);
    // f6 succeeds, which closes the breaker: f7 is retried in full again.
    assert_eq!(s.attempts_on("/my-files/docs/f6.txt"), 1);
    assert_eq!(s.attempts_on("/my-files/docs/f7.txt"), 4);
    drop(s);
    assert_eq!(run.result.errors.len(), 6, "{:?}", run.result.errors);
    assert!(run
        .result
        .errors
        .iter()
        .any(|e| e.starts_with("upload f4.txt: ") && !e.contains("gave up")));
    let base = baseline(&c);
    assert!(base.contains_key("f6.txt"));
    assert!(!base.contains_key("f1.txt") && !base.contains_key("f7.txt"));
}

#[test]
fn breaker_tripped_by_uploads_leaves_downloads_one_attempt() {
    let t = Tmp::new("retry-breaker-down");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    for i in 1..=3 {
        write(&t.path().join(format!("local/up{i}.txt")), "payload");
    }
    for i in 1..=5 {
        seed_remote_file(&store, &format!("down{i}.txt"), "payload");
    }
    store.borrow_mut().fail_all = Some("transient".into());
    let run = run_retrying(&c, fake, FAST, None);
    let s = store.borrow();
    // Three uploads in a row use up their retries and trip the breaker; the
    // downloads that follow run once each and are left for the next run.
    for i in 1..=3 {
        assert_eq!(
            s.attempts_on(&format!("/my-files/docs/up{i}.txt")),
            4,
            "up{i}"
        );
    }
    for i in 1..=5 {
        assert_eq!(
            s.attempts_on(&format!("/my-files/docs/down{i}.txt")),
            1,
            "down{i}"
        );
        assert_eq!(op_events(&run.events, &format!("down{i}.txt")).0, 1);
    }
    drop(s);
    assert_eq!(run.result.errors.len(), 8, "{:?}", run.result.errors);
    assert!(
        run.result
            .errors
            .iter()
            .filter(|e| e.starts_with("download "))
            .all(|e| !e.contains("gave up")),
        "{:?}",
        run.result.errors
    );
    assert!(baseline(&c).is_empty());
}

#[test]
fn downloads_on_a_dead_network_probe_with_three_files_and_leave_the_rest() {
    let t = Tmp::new("retry-dead-down");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    for i in 1..=5 {
        seed_remote_file(&store, &format!("f{i}.txt"), "payload");
    }
    store.borrow_mut().fail_all = Some("transient".into());
    let started = Instant::now();
    let run = run_retrying(&c, fake, Duration::from_millis(50), None);
    let elapsed = started.elapsed();
    // AC-14: the first three files use all four attempts, every later file
    // exactly one. The three probes wait together: three delays of at most
    // 2, 4 and 8 units, not one schedule per file.
    let attempts: Vec<usize> = (1..=5)
        .map(|i| {
            store
                .borrow()
                .attempts_on(&format!("/my-files/docs/f{i}.txt"))
        })
        .collect();
    assert_eq!(attempts, [4, 4, 4, 1, 1]);
    assert!(
        elapsed < Duration::from_millis(50 * 14 + 500),
        "{elapsed:?}"
    );
    assert_eq!(run.result.errors.len(), 5, "{:?}", run.result.errors);
    for i in 1..=5 {
        let line = run
            .result
            .errors
            .iter()
            .find(|e| e.starts_with(&format!("download f{i}.txt: ")))
            .unwrap();
        assert_eq!(
            line.contains("(gave up after 4 attempts)"),
            i <= 3,
            "{line}"
        );
        assert_eq!(op_events(&run.events, &format!("f{i}.txt")).0, 1);
        assert_eq!(op_events(&run.events, &format!("f{i}.txt")).1.len(), 1);
    }
    assert!(baseline(&c).is_empty());
}

#[test]
fn download_retry_rounds_resume_for_everyone_once_a_probe_succeeds() {
    let t = Tmp::new("retry-rounds");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    for i in 1..=4 {
        seed_remote_file(&store, &format!("f{i}.txt"), "payload");
    }
    script(&store, "/my-files/docs/f1.txt", &["transient"]);
    script(&store, "/my-files/docs/f2.txt", &["transient"; 10]);
    script(&store, "/my-files/docs/f3.txt", &["transient"; 10]);
    script(
        &store,
        "/my-files/docs/f4.txt",
        &["transient", "transient", "transient"],
    );
    let run = run_retrying(&c, fake, FAST, None);
    // Round 1: all four fail, so round 2 probes f1 to f3 while f4 waits. f1
    // succeeds, so every held job is retried again from round 3 on: f2 and
    // f3 use up their attempts, f4 gets its full schedule and lands.
    let attempts: Vec<usize> = (1..=4)
        .map(|i| {
            store
                .borrow()
                .attempts_on(&format!("/my-files/docs/f{i}.txt"))
        })
        .collect();
    assert_eq!(attempts, [2, 4, 4, 4]);
    assert_eq!(run.result.errors.len(), 2, "{:?}", run.result.errors);
    let base = baseline(&c);
    assert!(base.contains_key("f1.txt") && base.contains_key("f4.txt"));
    assert!(!base.contains_key("f2.txt") && !base.contains_key("f3.txt"));
    assert_eq!(op_events(&run.events, "f4.txt"), (1, vec![(true, None)]));
    assert_eq!(op_events(&run.events, "f1.txt"), (1, vec![(true, None)]));
}

#[test]
fn cancel_during_a_retry_round_finishes_the_jobs_the_pool_skipped() {
    let t = Tmp::new("retry-cancel-round");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    seed_remote_file(&store, "a.txt", "payload");
    seed_remote_file(&store, "b.txt", "payload");
    script(&store, "/my-files/docs/a.txt", &["transient"; 10]);
    script(&store, "/my-files/docs/b.txt", &["transient"; 10]);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    // Both fail in round 1. In round 2 a.txt's attempt sets the cancel flag,
    // so the (sequential) pool never dequeues b.txt again.
    store.borrow_mut().cancel_on_attempt = Some(("/my-files/docs/a.txt".into(), 2, cancel.clone()));
    let run = run_retrying(&c, fake, FAST, Some(&cancel));
    let s = store.borrow();
    assert_eq!(s.attempts_on("/my-files/docs/a.txt"), 2);
    assert_eq!(s.attempts_on("/my-files/docs/b.txt"), 1);
    drop(s);
    // Once per file, both ways: started in round 1, finished with the final result.
    let (a_started, a_finished) = op_events(&run.events, "a.txt");
    let (b_started, b_finished) = op_events(&run.events, "b.txt");
    assert_eq!((a_started, b_started), (1, 1));
    assert_eq!(a_finished.len(), 1);
    assert_eq!(
        b_finished,
        vec![(
            false,
            Some("cancelled before the retry could run".to_string())
        )]
    );
    assert!(!a_finished[0].0);
    assert_eq!(run.result.errors.len(), 2, "{:?}", run.result.errors);
    assert!(baseline(&c).is_empty());
}

#[test]
fn cancel_during_a_retry_wait_ends_the_run_within_a_second() {
    let t = Tmp::new("retry-cancel");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "payload");
    script(&store, "/my-files/docs/a.txt", &["transient"; 10]);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let setter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = Instant::now();
    // Full-length schedule: the first wait alone is at least 1 s.
    let run = run_retrying(&c, fake, Duration::from_secs(1), Some(&cancel));
    let elapsed = started.elapsed();
    setter.join().unwrap();
    assert!(elapsed < Duration::from_millis(1300), "{elapsed:?}");
    assert_eq!(store.borrow().attempts_on("/my-files/docs/a.txt"), 1);
    assert!(!baseline(&c).contains_key("a.txt"));
    let (started_events, finished) = op_events(&run.events, "a.txt");
    assert_eq!(started_events, 1);
    assert_eq!(finished.len(), 1);
    assert!(!finished[0].0);
    assert!(
        run.result
            .errors
            .iter()
            .any(|e| e.starts_with("upload a.txt: ") && e.contains("cancelled")),
        "{:?}",
        run.result.errors
    );
}

#[test]
fn cancel_during_a_download_retry_wait_ends_the_run_within_a_second() {
    let t = Tmp::new("retry-cancel-down");
    let c = cfg(t.path());
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    let (fake, store) = FakeRemote::new();
    seed_remote_file(&store, "a.txt", "payload");
    script(&store, "/my-files/docs/a.txt", &["transient"; 10]);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let setter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = Instant::now();
    let run = run_retrying(&c, fake, Duration::from_secs(1), Some(&cancel));
    let elapsed = started.elapsed();
    setter.join().unwrap();
    assert!(elapsed < Duration::from_millis(1300), "{elapsed:?}");
    assert_eq!(store.borrow().attempts_on("/my-files/docs/a.txt"), 1);
    assert!(!baseline(&c).contains_key("a.txt"));
    assert!(!t.path().join("local/a.txt").exists());
    let (started_events, finished) = op_events(&run.events, "a.txt");
    assert_eq!(started_events, 1);
    assert_eq!(
        finished,
        vec![(false, Some("cancelled while waiting to retry".to_string()))]
    );
}

#[test]
fn rate_limited_retry_after_raises_the_wait() {
    let t = Tmp::new("retry-after");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "payload");
    // retry_after = 2 at a 100 ms unit: at least 200 ms before the next
    // attempt, where plain backoff for the first retry would allow 100 ms.
    script(&store, "/my-files/docs/a.txt", &["rate_limited:2"]);
    let run = run_retrying(&c, fake, Duration::from_millis(100), None);
    assert!(run.result.errors.is_empty(), "{:?}", run.result.errors);
    let s = store.borrow();
    let times = &s.attempts["/my-files/docs/a.txt"];
    assert_eq!(times.len(), 2);
    let gap = times[1].duration_since(times[0]);
    assert!(gap >= Duration::from_millis(200), "{gap:?}");
    assert!(gap < Duration::from_millis(1000), "{gap:?}");
    drop(s);
    assert!(baseline(&c).contains_key("a.txt"));
}

#[test]
fn local_and_remote_without_baseline_keeps_both() {
    let t = Tmp::new("no-base-conflict");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/readme.txt"), "local-B");
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/readme.txt".into(), b"remote-A".to_vec());
    run(&c, fake, false);
    let on_disk: Vec<String> = std::fs::read_dir(t.path().join("local"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    assert!(
        on_disk.iter().any(|s| s.contains("local-B")),
        "local bytes must survive on disk: {on_disk:?}"
    );
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/readme.txt")
            .map(String::as_str),
        Some("remote-A")
    );
}

#[test]
fn newer_policy_keeps_local_bytes_when_sync_state_forgot_the_file() {
    let t = Tmp::new("no-base-newer");
    let mut c = cfg(t.path());
    c.conflict = ConflictPolicy::Newer;
    let (_fake, store) = FakeRemote::new();
    write(&t.path().join("local/readme.txt"), "local-B");
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/readme.txt".into(), b"remote-A".to_vec());
    set_mtime(&t.path().join("local/readme.txt"), 100);
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/readme.txt".into(), 200);
    let mut gone = std::collections::HashSet::new();
    gone.insert("readme.txt".to_string());
    neutronsync::state::commit_baseline(&c.state_dir, "docs", &BTreeMap::new(), &gone).unwrap();
    let (fake, _s) = reuse(&store);
    run(&c, fake, false);
    let on_disk: Vec<String> = std::fs::read_dir(t.path().join("local"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    assert!(
        on_disk.iter().any(|s| s.contains("local-B")),
        "newer policy must not overwrite a local file with no remembered row: {on_disk:?}"
    );
}

#[test]
fn explicit_sync_picks_up_local_change_without_watcher() {
    let t = Tmp::new("explicit-local");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/note.txt"), "v1");
    run(&c, fake, false);
    write(&t.path().join("local/note.txt"), "v2-edited");
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/note.txt")
            .map(String::as_str),
        Some("v2-edited")
    );
}

#[test]
fn incomplete_remote_listing_does_not_prune_stale_rows() {
    let t = Tmp::new("no-prune-blind");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "a");
    write(&t.path().join("local/sub/b.txt"), "b");
    run(&c, fake, false);
    store.borrow_mut().fail_dir = Some("/my-files/docs/sub".to_string());
    std::fs::remove_file(t.path().join("local/sub/b.txt")).unwrap();
    {
        let mut s = store.borrow_mut();
        s.files.remove("/my-files/docs/sub/b.txt");
    }
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        baseline(&c).contains_key("sub/b.txt"),
        "blind listing must not prune rows: {:?}",
        baseline(&c)
    );
}

#[test]
fn two_pairs_one_remote_local_edit_survives() {
    // Reproduces #35: a stale baseline row is dropped while the local file still
    // exists, then a sync must not treat the local copy as missing and download
    // over it.
    let t = Tmp::new("issue-35");
    let mut c = cfg(t.path());
    c.pairs[0].name = "primary".into();
    c.pairs[0].local = t.path().join("primary");
    c.pairs[0].remote = "/my-files/shared".into();
    c.pairs.push(Pair {
        name: "mirror".into(),
        local: t.path().join("mirror"),
        remote: "/my-files/shared".into(),
        auto: false,
        exclude: vec![],
    });
    let (fake, store) = FakeRemote::new();
    store
        .borrow_mut()
        .dirs
        .insert("/my-files/shared".to_string());
    std::fs::create_dir_all(t.path().join("primary/Mirror")).unwrap();
    write(&t.path().join("primary/Mirror/readme.txt"), "A");
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake, &log, false);
    eng.sync_pair(&c.pairs[0], false).unwrap();
    write(&t.path().join("primary/Mirror/readme.txt"), "B");
    store.borrow_mut().files.insert(
        "/my-files/shared/Mirror/readme.txt".into(),
        b"A-from-remote".to_vec(),
    );
    let mut gone = std::collections::HashSet::new();
    gone.insert("Mirror/readme.txt".to_string());
    neutronsync::state::commit_baseline(&c.state_dir, "primary", &BTreeMap::new(), &gone).unwrap();
    let (fake2, _s) = reuse(&store);
    let mut eng = Engine::new(&c, fake2, &log, false);
    eng.sync_pair(&c.pairs[0], false).unwrap();
    let primary = t.path().join("primary/Mirror");
    let on_disk: Vec<String> = std::fs::read_dir(&primary)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    assert!(
        on_disk.iter().any(|s| s.contains('B')),
        "local edit must survive as the file or a conflict copy: {on_disk:?}"
    );
}

fn download_with_umask(mask: u32, want: u32) {
    use std::os::unix::fs::PermissionsExt;
    let old = unsafe { libc::umask(mask) };
    let t = Tmp::new(&format!("umask-{mask:o}"));
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/f.txt".into(), b"data".to_vec());
    run(&c, fake, false);
    let mode = std::fs::metadata(t.path().join("local/f.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, want,
        "umask {mask:o} should yield {want:o}, got {mode:o}"
    );
    unsafe { libc::umask(old) };
}

#[test]
fn download_mode_respects_umask_022_and_002() {
    download_with_umask(0o022, 0o644);
    download_with_umask(0o002, 0o664);
}

#[test]
fn rate_limited_pause_above_five_minutes_is_left_for_the_next_run() {
    let t = Tmp::new("retry-after-long");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "payload");
    script(&store, "/my-files/docs/a.txt", &["rate_limited:301"]);
    let run = run_retrying(&c, fake, FAST, None);
    assert_eq!(store.borrow().attempts_on("/my-files/docs/a.txt"), 1);
    assert_eq!(run.result.errors.len(), 1);
    assert!(
        !run.result.errors[0].contains("gave up"),
        "{:?}",
        run.result.errors
    );
    assert!(!baseline(&c).contains_key("a.txt"));
}
