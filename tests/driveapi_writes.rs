use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use neutronsync::config::{self, Config};
use neutronsync::driveapi::{DriveApi, RpcError};
use neutronsync::engine::{self, Engine};
use neutronsync::logger::Logger;
use neutronsync::models::{DownloadJob, Entry};
use neutronsync::protoncli::{is_not_logged_in, Remote};
use serde_json::{json, Value};

static SERIAL: Mutex<()> = Mutex::new(());
struct Harness {
    cfg: Config,
    api: DriveApi,
    dir: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}
impl Harness {
    fn new() -> Self {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sidecar = root.join("sidecar");
        fs::write(&sidecar, include_str!("fixtures/drive_sidecar.py")).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.join("local")).unwrap();
        let path = root.join("config.toml");
        fs::write(
            &path,
            format!(
                r#"
[cli]
backend = "api"
sidecar = {:?}
download_threads = 2
[options]
state_dir = {:?}
propagate_deletes = true
local_delete = "remove"
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
        let cfg = config::load(path.to_str()).unwrap();
        fs::write(
            root.join("tree.json"),
            json!({
                "root": {"name":"my-files","type":"folder","parent_uid":null},
                "test": {"name":"test","type":"folder","parent_uid":"root"}
            })
            .to_string(),
        )
        .unwrap();
        let api = DriveApi::new(&cfg).unwrap();
        Self {
            cfg,
            api,
            dir,
            _guard: guard,
        }
    }
    fn tree(&self) -> Value {
        serde_json::from_slice(&fs::read(self.dir.path().join("tree.json")).unwrap()).unwrap()
    }
    fn save_tree(&self, value: Value) {
        fs::write(self.dir.path().join("tree.json"), value.to_string()).unwrap();
    }
    fn controls(&self, value: Value) {
        fs::write(self.dir.path().join("controls.json"), value.to_string()).unwrap();
    }
    fn remote(&self, name: &str, parent: &str, data: Option<&str>) {
        let mut tree = self.tree();
        let (size, digest, bytes) = data
            .map(|s| {
                (
                    s.len(),
                    sha1_smol::Sha1::from(s).digest().to_string(),
                    s.as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                )
            })
            .unwrap_or_default();
        tree[name] = json!({"name":name,"type":if data.is_some(){"file"}else{"folder"},"parent_uid":parent,"size":size,"sha1":digest,"data":bytes,"mtime":1700000000});
        self.save_tree(tree);
    }
    fn local(&self, path: &str, data: &str) {
        let path = self.cfg.pairs[0].local.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fn run(&self) -> neutronsync::engine::SyncResult {
        let log = Logger::silent();
        Engine::new(&self.cfg, DriveApi::new(&self.cfg).unwrap(), &log, false)
            .sync_pair(&self.cfg.pairs[0], false)
            .unwrap()
    }
    fn settled(&self) {
        let result = self.run();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.applied, 0, "second sync must do nothing");
        assert!(
            result.plan_summary.is_empty(),
            "second sync must plan nothing"
        );
    }
    fn writes(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("writes"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn baseline(&self) -> BTreeMap<String, Entry> {
        neutronsync::state::load_baseline(&self.cfg.state_dir, "test").unwrap()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.api.list_dir("/my-files/die");
    }
}

#[test]
fn writes_round_trip_revisions_rename_move_and_trash_reresolve() {
    let h = Harness::new();
    h.api.create_folder("/my-files/test", "folder").unwrap();
    h.api.create_folder("/my-files/test", "folder").unwrap();
    h.local("file", "original");
    let path = h.cfg.pairs[0].local.join("file");
    h.api
        .upload(path.to_str().unwrap(), "/my-files/test")
        .unwrap();
    let uid = h
        .api
        .list_dir("/my-files/test")
        .unwrap()
        .into_iter()
        .find(|n| n.path == "file")
        .unwrap()
        .remote_id
        .unwrap();
    h.local("file", "a replacement revision");
    h.api
        .upload(path.to_str().unwrap(), "/my-files/test")
        .unwrap();
    assert_eq!(h.tree()[&uid]["size"], 22);
    h.api
        .rename("/my-files/test/file", "/my-files/test/renamed")
        .unwrap();
    h.api
        .rename("/my-files/test/renamed", "/my-files/test/folder/renamed")
        .unwrap();
    h.api
        .rename("/my-files/test/folder/renamed", "/my-files/test/final")
        .unwrap();
    let methods: Vec<_> = h
        .writes()
        .into_iter()
        .filter_map(|v| v["method"].as_str().map(String::from))
        .collect();
    assert_eq!(
        &methods[4..],
        ["node.rename", "node.move", "node.move", "node.rename"]
    );
    assert!(h
        .api
        .download(
            "/my-files/test/file",
            h.cfg.pairs[0].local.to_str().unwrap()
        )
        .is_err());
    h.api
        .download(
            "/my-files/test/final",
            h.cfg.pairs[0].local.to_str().unwrap(),
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(h.cfg.pairs[0].local.join("final")).unwrap(),
        "a replacement revision"
    );
    h.api
        .rename("/my-files/test/folder", "/my-files/test/new-folder")
        .unwrap();
    h.api
        .upload(path.to_str().unwrap(), "/my-files/test/new-folder")
        .unwrap();
    assert!(h
        .api
        .upload(path.to_str().unwrap(), "/my-files/test/folder")
        .is_err());
    h.api.trash("/my-files/test/new-folder").unwrap();
    assert!(h
        .api
        .upload(path.to_str().unwrap(), "/my-files/test/new-folder")
        .is_err());
    h.api.trash("/my-files/test/final").unwrap();
    assert!(h.tree().get(&uid).is_none());
}

#[test]
fn missing_baseline_unions_both_sides_and_second_sync_is_empty() {
    let h = Harness::new();
    h.local("local.txt", "local");
    h.remote("remote.txt", "test", Some("remote"));
    let result = h.run();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(h.cfg.pairs[0].local.join("remote.txt").exists());
    assert_eq!(h.api.list_dir("/my-files/test").unwrap().len(), 2);
    assert!(!h.writes().iter().any(|r| r["method"] == "node.trash"));
    h.settled();
    h.local("local.txt", "edited local bytes");
    assert_eq!(h.run().applied, 1);
    h.settled();
}

#[test]
fn propagated_delete_uses_only_trash() {
    let h = Harness::new();
    h.local("file", "content");
    h.run();
    fs::remove_file(h.cfg.pairs[0].local.join("file")).unwrap();
    assert_eq!(h.run().applied, 1);
    assert!(h.writes().iter().any(|r| r["method"] == "node.trash"));
    assert!(h
        .writes()
        .iter()
        .all(|r| !r["method"].as_str().unwrap().contains("delete")));
    h.settled();
}

#[test]
fn partial_listing_never_produces_a_delete() {
    let h = Harness::new();
    h.remote("folder", "test", None);
    h.remote("file", "folder", Some("content"));
    h.run();
    h.controls(json!({"list_errors":{"folder":"transient"}}));
    let before = h.writes().len();
    h.run();
    assert!(h.cfg.pairs[0].local.join("folder/file").exists());
    assert!(!h.writes()[before..]
        .iter()
        .any(|r| r["method"] == "node.trash"));
}

#[test]
fn failed_listing_never_produces_a_delete() {
    let h = Harness::new();
    h.remote("file", "test", Some("content"));
    h.run();
    h.controls(json!({"list_errors":{"test":"transient"}}));
    let before = h.writes().len();
    let log = Logger::silent();
    assert!(
        Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false)
            .sync_pair(&h.cfg.pairs[0], false)
            .is_err()
    );
    assert!(h.cfg.pairs[0].local.join("file").exists());
    assert_eq!(h.writes().len(), before);
}

#[test]
fn not_found_listing_suppresses_deletes_and_is_not_empty() {
    let h = Harness::new();
    h.remote("file", "test", Some("content"));
    h.run();
    h.controls(json!({"list_errors":{"test":"not_found"}}));
    let before = h.writes().len();
    engine::run_sync_shallow(&h.cfg, &h.cfg.pairs[0], "", &Logger::silent(), None, None);
    assert!(h.cfg.pairs[0].local.join("file").exists());
    assert!(!h.writes()[before..]
        .iter()
        .any(|r| r["method"] == "node.trash"));
}

#[test]
fn auth_listing_is_an_error_never_empty() {
    let h = Harness::new();
    h.controls(json!({"list_errors":{"test":"auth"}}));
    assert!(is_not_logged_in(
        &h.api.list_dir("/my-files/test").unwrap_err().to_string()
    ));
    assert!(h
        .api
        .list_tree("/my-files/test", &|_| false, &|_| {})
        .is_err());
    assert!(h.writes().is_empty());
}

#[test]
fn unsafe_sidecar_names_never_reach_the_filesystem() {
    let h = Harness::new();
    h.controls(json!({"extra_entries":{"test":[
        {"uid":"bad","name":"../escape","type":"file","size":1},
        {"uid":"bad2","name":"bad\\name","type":"file","size":1}
    ]}}));
    h.run();
    assert!(h
        .api
        .download("/my-files/test/..", h.cfg.pairs[0].local.to_str().unwrap())
        .is_err());
    assert!(h
        .api
        .download(
            "/my-files/test/bad\\name",
            h.cfg.pairs[0].local.to_str().unwrap()
        )
        .is_err());
    assert_eq!(fs::read_dir(&h.cfg.pairs[0].local).unwrap().count(), 0);
    assert!(!h.dir.path().join("escape").exists());
    assert!(!h.writes().iter().any(|r| r["method"] == "file.download"));
}

#[test]
fn cancelled_downloads_are_bounded_and_leave_no_partial_or_temp_files() {
    let h = Harness::new();
    for i in 0..20 {
        h.remote(&format!("f{i}"), "test", Some("complete content"));
    }
    h.controls(json!({"download_delay":0.1}));
    let jobs: Vec<_> = (0..20)
        .map(|i| DownloadJob {
            rel: format!("f{i}"),
            remote_path: format!("/my-files/test/f{i}"),
            dest_dir: h.cfg.pairs[0].local.to_string_lossy().into_owned(),
            mtime: Some(1700000000),
        })
        .collect();
    let cancel = AtomicBool::new(false);
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    h.api.download_many(
        &jobs,
        0,
        &cancel,
        &|_| {
            let n = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(n, Ordering::SeqCst);
        },
        &|_, result| {
            assert!(result.is_ok());
            cancel.store(true, Ordering::SeqCst);
            done.fetch_add(1, Ordering::SeqCst);
            active.fetch_sub(1, Ordering::SeqCst);
        },
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert_eq!(done.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.writes()
            .iter()
            .filter(|r| r["method"] == "file.download")
            .count(),
        2
    );
    for entry in fs::read_dir(&h.cfg.pairs[0].local).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert_eq!(
            fs::read_to_string(entry.path()).unwrap(),
            "complete content"
        );
    }
}

#[test]
fn failed_download_is_pending_and_recovers_without_trash() {
    let h = Harness::new();
    h.remote("file", "test", Some("content"));
    h.controls(json!({"download_error":"fatal"}));
    assert!(!h.run().errors.is_empty());
    assert_eq!(fs::read_dir(&h.cfg.pairs[0].local).unwrap().count(), 0);
    h.controls(json!({}));
    h.run();
    assert!(h.cfg.pairs[0].local.join("file").exists());
    assert!(!h.writes().iter().any(|r| r["method"] == "node.trash"));
    h.settled();
}

#[test]
fn descendant_baseline_purge_requires_successful_directory_delete() {
    let h = Harness::new();
    h.remote("folder", "test", None);
    h.remote("file", "folder", Some("content"));
    h.run();
    fs::remove_dir_all(h.cfg.pairs[0].local.join("folder")).unwrap();
    h.controls(json!({"write_error":"fatal"}));
    engine::run_sync_shallow(&h.cfg, &h.cfg.pairs[0], "", &Logger::silent(), None, None);
    assert!(h.baseline().contains_key("folder/file"));
    h.controls(json!({}));
    let cancelled = AtomicBool::new(true);
    engine::run_sync_shallow(
        &h.cfg,
        &h.cfg.pairs[0],
        "",
        &Logger::silent(),
        None,
        Some(&cancelled),
    );
    assert!(h.baseline().contains_key("folder/file"));
    engine::run_sync_shallow(&h.cfg, &h.cfg.pairs[0], "", &Logger::silent(), None, None);
    assert!(!h.baseline().contains_key("folder/file"));
}

#[test]
fn missing_local_root_refuses_remote_deletes() {
    let h = Harness::new();
    h.remote("file", "test", Some("content"));
    h.run();
    fs::remove_dir_all(&h.cfg.pairs[0].local).unwrap();
    let log = Logger::silent();
    assert!(
        Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false)
            .sync_pair(&h.cfg.pairs[0], false)
            .is_err()
    );
    assert!(!h.writes().iter().any(|r| r["method"] == "node.trash"));
}

#[test]
fn writes_preserve_auth_classification_and_retry_after() {
    let h = Harness::new();
    h.controls(json!({"write_error":"auth"}));
    let error = h.api.create_folder("/my-files/test", "folder").unwrap_err();
    assert!(is_not_logged_in(&error.to_string()));
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, "auth");
    h.controls(json!({"write_error":"rate_limited","retry_after":1}));
    let error = h.api.create_folder("/my-files/test", "folder").unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().unwrap().retry_after,
        Some(1)
    );
    h.controls(json!({}));
    let start = Instant::now();
    h.api.create_folder("/my-files/test", "folder").unwrap();
    assert!(start.elapsed() >= Duration::from_millis(900));
}

#[test]
fn api_sync_writes_sync_log_and_shallow_batch_has_no_forced_warning() {
    let h = Harness::new();
    h.local("file", "content");
    let log = Logger::new(&h.cfg.log_dir(), false, false);
    let result = engine::run_sync(&h.cfg, &h.cfg.pairs, false, false, &log);
    assert_eq!(result.errors, 0);
    let text = fs::read_to_string(h.cfg.log_dir().join("sync.log")).unwrap();
    assert!(!text.is_empty());
    let (tx, rx) = std::sync::mpsc::channel();
    let log = Logger::channel(tx, false);
    let result = engine::run_sync_shallow_many(
        &h.cfg,
        &h.cfg.pairs[0],
        &["".into(), "a".into(), "b".into()],
        &log,
        None,
        None,
    );
    assert_eq!(result.errors, 0);
    assert_eq!(
        rx.try_iter()
            .filter(|line| line.contains("forcing a dry run"))
            .count(),
        0
    );
}

#[test]
fn conflict_keeps_both_copies_and_then_settles() {
    let h = Harness::new();
    h.remote("file", "test", Some("original"));
    h.run();
    h.local("file", "local edit");
    h.remote("file", "test", Some("remote edit longer"));
    let result = h.run();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let files: Vec<_> = fs::read_dir(&h.cfg.pairs[0].local)
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert!(files.contains(&"local edit".into()));
    assert!(files.contains(&"remote edit longer".into()));
    h.settled();
}

#[test]
fn remote_delete_reaches_desktop_trash_with_restore_metadata() {
    if std::env::var_os("PHASE2_TRASH_CHILD").is_none() {
        let data = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote_delete_reaches_desktop_trash_with_restore_metadata",
                "--nocapture",
            ])
            .env("PHASE2_TRASH_CHILD", "1")
            .env("XDG_DATA_HOME", data.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut h = Harness::new();
    h.cfg.local_delete = config::LocalDelete::Trash;
    h.remote("recoverable.txt", "test", Some("restore these bytes"));
    h.run();
    h.api.trash("/my-files/test/recoverable.txt").unwrap();
    assert_eq!(h.run().applied, 1);
    let trash = std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("Trash");
    assert_eq!(
        fs::read_to_string(trash.join("files/recoverable.txt")).unwrap(),
        "restore these bytes"
    );
    let receipt = fs::read_to_string(trash.join("info/recoverable.txt.trashinfo")).unwrap();
    assert!(receipt.contains("recoverable.txt") && receipt.contains("DeletionDate="));
    assert!(!h.cfg.pairs[0].local.join("recoverable.txt").exists());
    h.settled();
}

#[test]
fn shallow_local_listing_failure_suppresses_remote_delete() {
    let h = Harness::new();
    h.local("folder/file", "content");
    h.run();
    let folder = h.cfg.pairs[0].local.join("folder");
    let before = h.writes().len();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
    engine::run_sync_shallow(
        &h.cfg,
        &h.cfg.pairs[0],
        "folder",
        &Logger::silent(),
        None,
        None,
    );
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read_to_string(folder.join("file")).unwrap(), "content");
    assert!(!h.writes()[before..]
        .iter()
        .any(|r| r["method"] == "node.trash"));
}

#[test]
fn cancelled_engine_run_does_not_confirm_untransferred_files() {
    let h = Harness::new();
    h.remote("file", "test", Some("content"));
    let cancel = AtomicBool::new(true);
    let log = Logger::silent();
    let mut engine = Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false);
    engine.set_observer(None, Some(&cancel));
    engine.sync_pair(&h.cfg.pairs[0], false).unwrap();
    assert!(!h.cfg.pairs[0].local.join("file").exists());
    assert!(!h.baseline().contains_key("file"));
    assert!(!h.writes().iter().any(|r| r["method"] == "file.download"));
    h.run();
    assert!(h.cfg.pairs[0].local.join("file").exists());
    assert!(!h.writes().iter().any(|r| r["method"] == "node.trash"));
    h.settled();
}

#[test]
fn missing_remote_hint_is_complete_and_keeps_local_descendants() {
    let h = Harness::new();
    h.remote("folder", "test", None);
    h.remote("child", "folder", Some("keep until parent confirmation"));
    h.run();
    h.api.trash("/my-files/test/folder").unwrap();
    let before = h.writes().len();
    let log = Logger::silent();
    let (result, _) = Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false)
        .sync_pair_shallow(&h.cfg.pairs[0], "folder")
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.applied, 0);
    assert_eq!(
        fs::read_to_string(h.cfg.pairs[0].local.join("folder/child")).unwrap(),
        "keep until parent confirmation"
    );
    assert!(h.baseline().contains_key("folder/child"));
    assert_eq!(h.writes().len(), before);
}

#[test]
fn missing_remote_hint_with_unreadable_local_folder_still_fails() {
    let h = Harness::new();
    h.remote("folder", "test", None);
    h.remote("child", "folder", Some("keep unreadable contents"));
    h.run();
    h.api.trash("/my-files/test/folder").unwrap();
    let folder = h.cfg.pairs[0].local.join("folder");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
    let log = Logger::silent();
    let result = Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false)
        .sync_pair_shallow(&h.cfg.pairs[0], "folder");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    let (result, _) = result.unwrap();
    assert!(!result.errors.is_empty());
    assert_eq!(result.applied, 0);
    assert_eq!(
        fs::read_to_string(folder.join("child")).unwrap(),
        "keep unreadable contents"
    );
}

#[test]
fn remote_transport_error_in_hinted_folder_still_fails() {
    let h = Harness::new();
    h.remote("folder", "test", None);
    h.remote("child", "folder", Some("keep on transport error"));
    h.run();
    h.controls(json!({"list_errors":{"folder":"transient"}}));
    let before = h.writes().len();
    let log = Logger::silent();
    let (result, _) = Engine::new(&h.cfg, DriveApi::new(&h.cfg).unwrap(), &log, false)
        .sync_pair_shallow(&h.cfg.pairs[0], "folder")
        .unwrap();
    assert!(!result.errors.is_empty());
    assert_eq!(result.applied, 0);
    assert_eq!(
        fs::read_to_string(h.cfg.pairs[0].local.join("folder/child")).unwrap(),
        "keep on transport error"
    );
    assert_eq!(h.writes().len(), before);
}

#[test]
fn transfer_thread_overrides_fallback_clamp_and_error_callbacks() {
    let h = Harness::new();
    for i in 0..10 {
        h.remote(&format!("f{i}"), "test", Some("content"));
    }
    let jobs: Vec<_> = (0..10)
        .map(|i| DownloadJob {
            rel: format!("f{i}"),
            remote_path: format!("/my-files/test/f{i}"),
            dest_dir: h.cfg.pairs[0].local.to_string_lossy().into_owned(),
            mtime: None,
        })
        .collect();
    h.controls(json!({"download_delay":0.05}));
    for (configured, requested, expected) in [(0, 0, 4), (2, 1, 1), (2, 99, 8), (99, 0, 8)] {
        let mut cfg = h.cfg.clone();
        cfg.download_threads = configured;
        let api = DriveApi::new(&cfg).unwrap();
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        api.download_many(
            &jobs,
            requested,
            &AtomicBool::new(false),
            &|_| {
                maximum.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            },
            &|_, result| {
                assert!(result.is_ok());
                active.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(maximum.load(Ordering::SeqCst), expected);
        assert_eq!(done.load(Ordering::SeqCst), jobs.len());
    }
    h.controls(json!({"write_error":"auth"}));
    let failed = AtomicUsize::new(0);
    h.api
        .download_many(&jobs, 2, &AtomicBool::new(false), &|_| {}, &|_, result| {
            assert!(is_not_logged_in(&result.unwrap_err()));
            failed.fetch_add(1, Ordering::SeqCst);
        });
    assert_eq!(failed.load(Ordering::SeqCst), jobs.len());
}
