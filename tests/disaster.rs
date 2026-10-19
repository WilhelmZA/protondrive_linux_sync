//! Crash tests: the real `neutronsync` binary against the mutable fake sidecar,
//! killed by PID at points read from the fixture's `writes` and `transfers`
//! logs, never at a guessed time. Every scenario ends with two clean runs and
//! the same convergence checks (D17). Also the CLI checks for the deletion
//! guard (AC-10, AC-25), the conflict inbox (AC-17), the sync-state guards
//! (AC-20, AC-21) and the status line (AC-1).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_neutronsync");
const PAIR: &str = "test";

/// One scratch world: config, sidecar copy, tree, local folder, state dir.
struct Lab {
    dir: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sidecar = root.join("sidecar");
        fs::write(&sidecar, include_str!("fixtures/drive_sidecar.py")).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(root.join("local")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(
            root.join("config.toml"),
            format!(
                r#"
[cli]
backend = "api"
sidecar = {:?}
download_threads = 4
[options]
state_dir = {:?}
propagate_deletes = true
local_delete = "remove"
auto_sync = false
[[pair]]
name = "test"
local = {:?}
remote = "/my-files/test"
"#,
                sidecar,
                root.join("state"),
                root.join("local")
            ),
        )
        .unwrap();
        fs::write(
            root.join("tree.json"),
            json!({
                "root": {"name":"my-files","type":"folder","parent_uid":null},
                "test": {"name":"test","type":"folder","parent_uid":"root"}
            })
            .to_string(),
        )
        .unwrap();
        Lab { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn local_root(&self) -> PathBuf {
        self.path("local")
    }
    fn state_dir(&self) -> PathBuf {
        self.path("state")
    }

    fn tree(&self) -> Value {
        serde_json::from_slice(&fs::read(self.path("tree.json")).unwrap()).unwrap()
    }
    fn save_tree(&self, value: Value) {
        fs::write(self.path("tree.json"), value.to_string()).unwrap();
    }
    fn controls(&self, value: Value) {
        fs::write(self.path("controls.json"), value.to_string()).unwrap();
    }

    /// A remote file directly under the pair root, uid = name.
    fn remote(&self, name: &str, body: &str) {
        let mut tree = self.tree();
        let bytes: String = body.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        tree[name] = json!({
            "name": name, "type": "file", "parent_uid": "test",
            "size": body.len(), "sha1": sha1_smol::Sha1::from(body).digest().to_string(),
            "data": bytes, "mtime": 1700000000
        });
        self.save_tree(tree);
    }
    fn local(&self, rel: &str, body: &str) {
        let p = self.local_root().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
    }
    /// The same file on both sides with the fixture's mtime, so a run only
    /// records it.
    fn both(&self, name: &str, body: &str) {
        self.remote(name, body);
        self.local(name, body);
        let f = fs::OpenOptions::new()
            .write(true)
            .open(self.local_root().join(name))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
    }

    fn lines(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.path(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn writes(&self) -> Vec<Value> {
        self.lines("writes")
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn write_count(&self, method: &str) -> usize {
        self.writes()
            .iter()
            .filter(|w| w["method"] == method)
            .count()
    }
    fn transfer_starts(&self) -> usize {
        self.lines("transfers")
            .iter()
            .filter(|l| l.contains("\"start\""))
            .count()
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("NEUTRONSYNC_DRIVE_STDIO", "1")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_STATE_HOME", self.path("home/.local/state"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env_remove("NEUTRONSYNC_CONFIG")
            .arg("--config")
            .arg(self.path("config.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// Run `neutronsync <args>` to completion: (exit code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = self.command().args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Start `neutronsync sync` in the background.
    fn start_sync(&self) -> Child {
        self.command().arg("sync").spawn().unwrap()
    }

    /// Wait until `ready` holds, or fail after 20 s.
    fn wait_for(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !ready() {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kill a client we started, by PID, then reap it and wait for every
    /// sidecar it spawned to exit so a late write cannot race the clean runs.
    fn kill(&self, child: &mut Child, signal: i32) {
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "kill {pid}");
        let status = child.wait().unwrap();
        assert!(!status.success(), "the killed run must not exit 0");
        self.wait_for_sidecars();
    }

    fn wait_for_sidecars(&self) {
        let pids: Vec<i32> = self
            .lines("starts")
            .iter()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        self.wait_for("the sidecars to exit", || {
            pids.iter()
                .all(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        });
    }

    fn local_files(&self) -> BTreeMap<String, Vec<u8>> {
        let root = self.local_root();
        let mut out = BTreeMap::new();
        for e in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel = e
                .path()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.insert(rel, fs::read(e.path()).unwrap());
        }
        out
    }

    fn remote_files(&self) -> BTreeMap<String, Vec<u8>> {
        let tree = self.tree();
        let obj = tree.as_object().unwrap();
        let mut out = BTreeMap::new();
        for (uid, node) in obj {
            if node["type"] != "file" {
                continue;
            }
            // Walk up to the pair root; skip nodes outside it.
            let mut parts = vec![node["name"].as_str().unwrap().to_string()];
            let mut cur = node["parent_uid"].as_str().map(str::to_string);
            let mut under = false;
            while let Some(p) = cur {
                if p == "test" {
                    under = true;
                    break;
                }
                let Some(n) = obj.get(&p) else { break };
                parts.push(n["name"].as_str().unwrap().to_string());
                cur = n["parent_uid"].as_str().map(str::to_string);
            }
            if !under {
                continue;
            }
            parts.reverse();
            let hex = node["data"].as_str().unwrap_or("");
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let _ = uid;
            out.insert(parts.join("/"), bytes);
        }
        out
    }

    fn baseline_files(&self) -> BTreeSet<String> {
        neutronsync::state::load_baseline(&self.state_dir(), PAIR)
            .unwrap()
            .into_iter()
            .filter(|(_, e)| !e.is_dir)
            .map(|(k, _)| k)
            .collect()
    }

    /// A clean run must exit 0; returns the parsed `--json` output.
    fn clean_run(&self) -> Value {
        let (code, out, err) = self.run(&["--quiet", "sync", "--json"]);
        assert_eq!(code, 0, "clean run failed:\n{out}\n{err}");
        serde_json::from_str(&out).unwrap()
    }

    /// D17: two clean runs, the second planning nothing, then both sides
    /// hold exactly `expected` and the baseline lists exactly those files.
    fn converge(&self, expected: &BTreeMap<String, Vec<u8>>) {
        self.controls(json!({}));
        self.wait_for_sidecars();
        self.clean_run();
        self.wait_for_sidecars();
        let second = self.clean_run();
        let pair = &second["pairs"][0];
        assert_eq!(
            pair["applied"], 0,
            "second clean run must do nothing: {second}"
        );
        assert_eq!(pair["errors"], 0, "{second}");
        assert_eq!(
            pair["plan"], "",
            "second clean run must plan nothing: {second}"
        );

        let local = self.local_files();
        let remote = self.remote_files();
        assert_eq!(
            &local, expected,
            "local files differ from what should be there"
        );
        assert_eq!(
            &remote, expected,
            "remote files differ from what should be there"
        );
        for name in local.keys() {
            assert!(
                !name.contains("(conflict"),
                "conflict copy left behind: {name}"
            );
            assert!(!name.ends_with(".tmp"), "temp file left behind: {name}");
            assert!(
                !name
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .starts_with(".neutronsync-"),
                "temp file left behind: {name}"
            );
        }
        for e in walkdir::WalkDir::new(self.local_root())
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let n = e.file_name().to_string_lossy();
            assert!(
                !n.ends_with(".tmp") && !n.starts_with(".neutronsync-"),
                "{n}"
            );
        }
        let want: BTreeSet<String> = expected.keys().cloned().collect();
        assert_eq!(
            self.baseline_files(),
            want,
            "baseline must list exactly the files"
        );
    }
}

fn hex_of(path: &Path) -> String {
    sha1_smol::Sha1::from(fs::read(path).unwrap())
        .digest()
        .to_string()
}

// ---- AC-24: crashes mid-run --------------------------------------------------

/// 20 remote files to download; kill the client after 5 transfers started.
fn killed_during_downloads(signal: i32) {
    let lab = Lab::new();
    let mut expected = BTreeMap::new();
    for i in 0..20 {
        let body = format!("remote body {i}");
        lab.remote(&format!("r{i}.txt"), &body);
        expected.insert(format!("r{i}.txt"), body.into_bytes());
    }
    lab.controls(json!({"download_delay": 1.0}));
    let mut child = lab.start_sync();
    lab.wait_for("5 downloads to start", || lab.transfer_starts() >= 5);
    lab.kill(&mut child, signal);
    assert!(lab.local_files().len() < 20, "the kill must land mid-batch");
    lab.converge(&expected);
}

#[test]
fn a_sigterm_during_a_batch_of_downloads() {
    killed_during_downloads(libc::SIGTERM);
}

#[test]
fn b_sigkill_during_a_batch_of_downloads() {
    killed_during_downloads(libc::SIGKILL);
}

#[test]
fn c_sigkill_during_a_batch_of_uploads() {
    let lab = Lab::new();
    let mut expected = BTreeMap::new();
    for i in 0..20 {
        let body = format!("local body {i}");
        lab.local(&format!("l{i}.txt"), &body);
        expected.insert(format!("l{i}.txt"), body.into_bytes());
    }
    lab.controls(json!({"upload_delay": 0.3}));
    let mut child = lab.start_sync();
    lab.wait_for("5 upload requests", || lab.write_count("file.upload") >= 5);
    lab.kill(&mut child, libc::SIGKILL);
    assert!(
        lab.remote_files().len() < 20,
        "the kill must land mid-batch"
    );
    lab.converge(&expected);
}

#[test]
fn d_sigkill_during_propagated_remote_deletes() {
    let lab = Lab::new();
    let mut expected = BTreeMap::new();
    for i in 0..20 {
        lab.both(&format!("gone{i}.txt"), &format!("doomed {i}"));
    }
    for i in 0..5 {
        let body = format!("stays {i}");
        lab.both(&format!("keep{i}.txt"), &body);
        expected.insert(format!("keep{i}.txt"), body.into_bytes());
    }
    lab.clean_run();
    assert_eq!(lab.baseline_files().len(), 25);
    for i in 0..20 {
        fs::remove_file(lab.local_root().join(format!("gone{i}.txt"))).unwrap();
    }
    lab.controls(json!({"trash_delay": 0.3}));
    let mut child = lab.start_sync();
    lab.wait_for("5 trash requests", || lab.write_count("node.trash") >= 5);
    lab.kill(&mut child, libc::SIGKILL);
    let left = lab.remote_files().len();
    assert!(
        left > 5 && left < 25,
        "the kill must land mid-batch, {left} remote files"
    );
    // The intended deletions are the 20; nothing else may go.
    lab.converge(&expected);
}

#[test]
fn e_sidecar_exits_during_a_mixed_run() {
    let lab = Lab::new();
    let mut expected = BTreeMap::new();
    for i in 0..10 {
        let body = format!("up {i}");
        lab.local(&format!("up{i}.txt"), &body);
        expected.insert(format!("up{i}.txt"), body.into_bytes());
        let body = format!("down {i}");
        lab.remote(&format!("down{i}.txt"), &body);
        expected.insert(format!("down{i}.txt"), body.into_bytes());
    }
    lab.controls(json!({"exit_after_writes": 5}));
    let (code, out, err) = lab.run(&["sync"]);
    assert_ne!(code, 0, "a run whose sidecar died must fail:\n{out}\n{err}");
    lab.converge(&expected);
}

#[test]
fn f_sigkill_after_an_upload_the_client_never_heard_about() {
    let lab = Lab::new();
    lab.local("late.txt", "arrives after the client is gone");
    let mut expected = BTreeMap::new();
    expected.insert(
        "late.txt".to_string(),
        b"arrives after the client is gone".to_vec(),
    );
    lab.controls(json!({"upload_delay": 2.0}));
    let mut child = lab.start_sync();
    lab.wait_for("the upload request", || lab.write_count("file.upload") >= 1);
    lab.kill(&mut child, libc::SIGKILL);
    // The sidecar finished the upload on its own: the remote has the file,
    // the baseline does not.
    assert_eq!(
        lab.remote_files().len(),
        1,
        "the sidecar's late upload must land"
    );
    assert!(lab.baseline_files().is_empty());
    lab.converge(&expected);
}

// ---- AC-25 and AC-10: the guard from the CLI ---------------------------------

/// 300 tracked files, then 250 removed locally.
fn held_lab() -> Lab {
    let lab = Lab::new();
    for i in 0..300 {
        lab.both(&format!("f{i}.txt"), &format!("tracked {i}"));
    }
    lab.clean_run();
    assert_eq!(lab.baseline_files().len(), 300);
    for i in 0..250 {
        fs::remove_file(lab.local_root().join(format!("f{i}.txt"))).unwrap();
    }
    lab
}

#[test]
fn held_mass_deletion_from_the_cli() {
    let lab = held_lab();

    // AC-10: a dry run says what it would hold and writes no row.
    let (code, _out, err) = lab.run(&["sync", "--dry-run"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("would hold back 250 deletions"), "{err}");
    assert!(
        neutronsync::stats::Stats::open(&lab.state_dir())
            .unwrap()
            .held_for(PAIR)
            .unwrap()
            .is_none(),
        "a dry run writes no held_deletes row"
    );

    // AC-25: the held run exits 1, trashes nothing and stores the hold.
    let (code, _out, err) = lab.run(&["sync"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("250 deletions held back"), "{err}");
    assert!(err.contains("250 removed on this computer"), "{err}");
    assert!(err.contains("neutronsync sync --resync test"), "{err}");
    assert!(
        err.contains("neutronsync sync --allow-mass-delete test"),
        "{err}"
    );
    assert_eq!(
        lab.write_count("node.trash"),
        0,
        "no trash request may be sent"
    );
    assert_eq!(lab.remote_files().len(), 300);
    let row = neutronsync::stats::Stats::open(&lab.state_dir())
        .unwrap()
        .held_for(PAIR)
        .unwrap()
        .expect("held_deletes row");
    assert_eq!((row.local, row.remote, row.tracked), (0, 250, 300));

    // AC-10: the flag without a pair name exits 2 and syncs nothing.
    let starts_before = lab.lines("starts").len();
    let (code, _out, err) = lab.run(&["sync", "--allow-mass-delete"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("usage: neutronsync sync --allow-mass-delete NAME"),
        "{err}"
    );
    assert!(err.contains("Nothing was synced."), "{err}");
    assert_eq!(
        lab.lines("starts").len(),
        starts_before,
        "no sidecar may start"
    );
    assert_eq!(lab.remote_files().len(), 300);

    // AC-10: with the name, the 250 deletions go through and the row is cleared.
    let (code, _out, err) = lab.run(&["sync", "--allow-mass-delete", PAIR]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lab.write_count("node.trash"), 250);
    assert_eq!(lab.remote_files().len(), 50);
    assert!(neutronsync::stats::Stats::open(&lab.state_dir())
        .unwrap()
        .held_for(PAIR)
        .unwrap()
        .is_none());
    let expected: BTreeMap<String, Vec<u8>> = (250..300)
        .map(|i| (format!("f{i}.txt"), format!("tracked {i}").into_bytes()))
        .collect();
    lab.converge(&expected);
}

#[test]
fn resync_brings_held_files_back_from_the_cli() {
    let lab = held_lab();
    let (code, _, _) = lab.run(&["sync"]);
    assert_eq!(code, 1);
    let (code, _out, err) = lab.run(&["sync", "--resync", PAIR]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lab.write_count("node.trash"), 0);
    assert_eq!(lab.local_files().len(), 300);
    assert!(neutronsync::stats::Stats::open(&lab.state_dir())
        .unwrap()
        .held_for(PAIR)
        .unwrap()
        .is_none());
    let expected: BTreeMap<String, Vec<u8>> = (0..300)
        .map(|i| (format!("f{i}.txt"), format!("tracked {i}").into_bytes()))
        .collect();
    lab.converge(&expected);
}

// ---- AC-17: conflicts recorded by the CLI ------------------------------------

#[test]
fn cli_records_a_kept_both_conflict() {
    let lab = Lab::new();
    lab.both("a.txt", "base");
    lab.clean_run();
    lab.local("a.txt", "changed here");
    lab.remote("a.txt", "changed in Proton Drive");
    let (code, _out, err) = lab.run(&["sync"]);
    assert_eq!(code, 0, "{err}");
    let stats = neutronsync::stats::Stats::open(&lab.state_dir()).unwrap();
    let rows = stats.conflicts().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].pair, PAIR);
    assert_eq!(rows[0].path, "a.txt");
    assert_eq!(
        rows[0].outcome,
        neutronsync::models::ConflictOutcome::KeptBoth
    );
    let copy = rows[0].copy.clone().expect("the copy's path");
    assert!(copy.contains("(conflict"), "{copy}");
    assert!(lab.local_root().join(&copy).exists());
    assert_eq!(stats.conflict_count().unwrap(), 1);
    drop(stats);
    // Both sides settle with the copy present on each.
    let mut expected = BTreeMap::new();
    expected.insert("a.txt".to_string(), b"changed in Proton Drive".to_vec());
    expected.insert(copy, b"changed here".to_vec());
    lab.controls(json!({}));
    lab.wait_for_sidecars();
    let second = lab.clean_run();
    assert_eq!(second["pairs"][0]["applied"], 0, "{second}");
    assert_eq!(lab.local_files(), expected);
    assert_eq!(lab.remote_files(), expected);
}

// ---- AC-20 and AC-21: the sync-state guards ----------------------------------

/// The local files are untouched and the run sent no write request.
fn refused_run(lab: &Lab, what: &str) -> String {
    refused_run_with(lab, what, &["sync"])
}

/// The same for a resync: "Restore them" must stop at the same file.
fn refused_resync(lab: &Lab, what: &str) -> String {
    refused_run_with(lab, what, &["sync", "--resync", PAIR])
}

fn refused_run_with(lab: &Lab, what: &str, args: &[&str]) -> String {
    let before = lab.local_files();
    let (code, out, err) = lab.run(args);
    assert_ne!(code, 0, "{what}: must fail\n{out}\n{err}");
    assert_eq!(lab.write_count("file.upload"), 0, "{what}");
    assert_eq!(lab.write_count("file.download"), 0, "{what}");
    assert_eq!(lab.write_count("node.trash"), 0, "{what}");
    assert!(lab.writes().is_empty(), "{what}: zero write requests");
    assert_eq!(lab.local_files(), before, "{what}: local files must stay");
    err
}

#[test]
fn newer_sync_state_is_refused_untouched() {
    let lab = Lab::new();
    lab.local("keep.txt", "here");
    lab.remote("there.txt", "there");
    fs::create_dir_all(lab.state_dir()).unwrap();
    let db = lab.state_dir().join("stats.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE baseline(pair TEXT, rel TEXT, is_dir INTEGER, size INTEGER, \
             mtime INTEGER, sha1 TEXT, remote_id TEXT, PRIMARY KEY(pair, rel)); \
             INSERT INTO baseline VALUES ('test','keep.txt',0,4,1,NULL,NULL); \
             PRAGMA user_version = 3;",
        )
        .unwrap();
    }
    let before = hex_of(&db);
    let err = neutronsync::stats::Stats::open(&lab.state_dir())
        .err()
        .expect("a version 3 file must be refused");
    let text = format!("{err:#}");
    assert!(
        text.contains(
            "Sync state was saved by a newer NeutronSync (format 3; this version reads up to 2)"
        ),
        "{text}"
    );
    assert!(text.contains("Update NeutronSync, or move"), "{text}");
    assert!(text.contains("aside to start over."), "{text}");
    assert_eq!(hex_of(&db), before, "the refusal must not change the file");
    let err = refused_run(&lab, "version 3");
    assert!(err.contains("saved by a newer NeutronSync"), "{err}");
    assert_eq!(
        hex_of(&db),
        before,
        "the CLI must not change the file either"
    );
    assert!(lab.local_root().join("keep.txt").exists());
    let err = refused_resync(&lab, "version 3, resync");
    assert!(err.contains("saved by a newer NeutronSync"), "{err}");
    assert_eq!(hex_of(&db), before, "a resync must not change the file");
    assert!(
        !lab.remote_files().contains_key("keep.txt"),
        "a refused resync uploads nothing"
    );
}

#[test]
fn random_bytes_as_sync_state_are_refused() {
    let lab = Lab::new();
    lab.local("keep.txt", "here");
    lab.remote("there.txt", "there");
    fs::create_dir_all(lab.state_dir()).unwrap();
    let mut noise = Vec::with_capacity(8192);
    let mut x: u32 = 0x9e37_79b9;
    for _ in 0..8192 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        noise.push(x as u8);
    }
    fs::write(lab.state_dir().join("stats.db"), &noise).unwrap();
    let err = refused_run(&lab, "random bytes");
    assert!(
        err.contains("damaged") || err.contains("malformed") || err.contains("not a database"),
        "{err}"
    );
    assert!(lab.local_root().join("keep.txt").exists());
    assert!(!lab.local_root().join("there.txt").exists());
    let err = refused_resync(&lab, "random bytes, resync");
    assert!(
        err.contains("damaged") || err.contains("malformed") || err.contains("not a database"),
        "{err}"
    );
    assert!(
        !lab.remote_files().contains_key("keep.txt"),
        "a refused resync uploads nothing"
    );
}

#[test]
fn truncated_sync_state_is_refused() {
    let lab = Lab::new();
    for i in 0..40 {
        lab.both(
            &format!("f{i}.txt"),
            &format!("file {i} with enough bytes to fill pages"),
        );
    }
    lab.clean_run();
    lab.wait_for_sidecars();
    let db = lab.state_dir().join("stats.db");
    let bytes = fs::read(&db).unwrap();
    assert!(bytes.len() > 4096);
    fs::write(&db, &bytes[..bytes.len() / 2]).unwrap();
    for side in ["stats.db-wal", "stats.db-shm"] {
        let _ = fs::remove_file(lab.state_dir().join(side));
    }
    lab.remote("new.txt", "a remote file the run must not fetch");
    let err = refused_run(&lab, "truncated");
    assert!(
        err.contains("damaged") || err.contains("malformed"),
        "{err}"
    );
    assert_eq!(lab.local_files().len(), 40);
    refused_resync(&lab, "truncated, resync");
    assert_eq!(lab.local_files().len(), 40);
    assert!(!lab.local_root().join("new.txt").exists());
}

// ---- AC-1: the status line ---------------------------------------------------

#[test]
fn status_shows_the_delete_guard() {
    let lab = Lab::new();
    let (code, out, err) = lab.run(&["status"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains("delete guard    max_delete_files = 200, max_delete_percent = 30"),
        "{out}"
    );
}
