use neutronsync::{config, stats::Stats};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    dir: tempfile::TempDir,
    cfg: config::Config,
    child: Option<Child>,
    _guard: MutexGuard<'static, ()>,
}
impl Harness {
    fn new() -> Self {
        // Keep the suite within the shared user's inotify instance limit.
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sidecar = root.join("sidecar");
        fs::write(&sidecar, include_str!("fixtures/drive_sidecar.py")).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.join("local")).unwrap();
        fs::write(
            root.join("config.toml"),
            format!(
                r#"
[cli]
backend = "api"
sidecar = {sidecar:?}
binary = "/no-cli-may-run"
[options]
state_dir = {:?}
log_dir = {:?}
propagate_deletes = true
local_delete = "remove"
debounce = 1
full_walk_interval = 86400
[[pair]]
name = "test"
local = {:?}
remote = "/my-files/test"
"#,
                root.join("state"),
                root.join("logs"),
                root.join("local")
            ),
        )
        .unwrap();
        let cfg = config::load(root.join("config.toml").to_str()).unwrap();
        let h = Self {
            dir,
            cfg,
            child: None,
            _guard: guard,
        };
        h.write(
            "tree.json",
            json!({
                "root":{"name":"my-files","type":"folder","parent_uid":null},
                "test":{"name":"test","type":"folder","parent_uid":"root"},
                "a":{"name":"a","type":"folder","parent_uid":"test"},
                "b":{"name":"b","type":"folder","parent_uid":"test"}
            }),
        );
        h
    }
    fn write(&self, name: &str, value: Value) {
        let temp = self.dir.path().join(format!("{name}.next"));
        fs::write(&temp, value.to_string()).unwrap();
        fs::rename(temp, self.dir.path().join(name)).unwrap();
    }
    fn tree(&self) -> Value {
        serde_json::from_slice(&fs::read(self.dir.path().join("tree.json")).unwrap()).unwrap()
    }
    fn file(&self, parent: &str, name: &str, data: &str) {
        let mut tree = self.tree();
        tree["file"] = json!({"name":name,"type":"file","parent_uid":parent,"size":data.len(),"mtime":1700000000,"sha1":sha1_smol::Sha1::from(data).digest().to_string(),"data":data.as_bytes().iter().map(|b|format!("{b:02x}")).collect::<String>()});
        self.write("tree.json", tree);
    }
    fn event(&self, kind: &str, parent: &str) {
        self.write("feed.json", json!([{"last_event_id":"1","events":[{"type":kind,"node_uid":"file","parent_uid":parent}]}]));
    }
    fn record_cli_calls(&self) {
        let cli = self.dir.path().join("cli");
        fs::write(&cli, "#!/usr/bin/env python3\nimport json, sys\nfrom pathlib import Path\nwith Path(__file__).with_name('cli-calls').open('a') as out:\n    out.write(json.dumps(sys.argv[1:]) + '\\n')\nprint('[]')\n").unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let path = self.dir.path().join("config.toml");
        let config = fs::read_to_string(&path)
            .unwrap()
            .replace("\"/no-cli-may-run\"", &format!("{cli:?}"));
        fs::write(path, config).unwrap();
    }
    fn start(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path().join("watch.log"))
            .unwrap();
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_neutronsync"))
                .arg("--config")
                .arg(self.dir.path().join("config.toml"))
                .arg("watch")
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
    }
    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }
    fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("watch.log")).unwrap_or_default()
    }
    fn records(&self, file: &str) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    fn cursor(&self) -> Option<String> {
        Stats::open(&self.cfg.state_dir)
            .unwrap()
            .feed_cursor("/my-files")
            .unwrap()
    }
    fn wait(&self, f: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        while !f() {
            assert!(Instant::now() < until, "timed out: {}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn ready(&mut self) {
        self.start();
        self.wait(|| self.cursor().as_deref() == Some("0"));
    }
    fn applied(&self) {
        self.wait(|| self.cursor().as_deref() == Some("1"));
    }
    fn local(&self, path: &str) -> String {
        fs::read_to_string(self.cfg.pairs[0].local.join(path)).unwrap()
    }
    fn walks(&self) -> usize {
        self.log().matches("watch: full walk (cause=").count()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn remote_create_reconciles_event_parent() {
    let mut h = Harness::new();
    h.ready();
    h.file("a", "file", "created");
    h.event("node_created", "a");
    h.applied();
    assert_eq!(h.local("a/file"), "created");
    assert_eq!(h.walks(), 1);
    assert!(h.log().contains("uid=file disposition=mapped"));
}
#[test]
fn remote_update_reconciles_event_parent() {
    let mut h = Harness::new();
    h.file("a", "file", "old");
    h.ready();
    h.file("a", "file", "updated-longer");
    h.event("node_updated", "a");
    h.applied();
    assert_eq!(h.local("a/file"), "updated-longer");
    assert_eq!(h.walks(), 1);
}
#[test]
fn remote_delete_reconciles_event_parent_and_baseline_parent() {
    let mut h = Harness::new();
    h.file("a", "file", "old");
    h.ready();
    let mut tree = h.tree();
    tree.as_object_mut().unwrap().remove("file");
    h.write("tree.json", tree);
    h.event("node_deleted", "b");
    h.applied();
    assert!(!h.cfg.pairs[0].local.join("a/file").exists());
    assert!(h.log().contains("[\"a\", \"b\"]"));
    assert_eq!(h.walks(), 1);
}
#[test]
fn remote_move_reconciles_old_and_new_parent() {
    let mut h = Harness::new();
    h.file("a", "file", "moved");
    h.ready();
    h.file("b", "file", "moved");
    h.event("node_updated", "b");
    h.applied();
    assert!(!h.cfg.pairs[0].local.join("a/file").exists());
    assert_eq!(h.local("b/file"), "moved");
    assert_eq!(h.walks(), 1);
}
#[test]
fn remote_rename_reconciles_holding_folder() {
    let mut h = Harness::new();
    h.file("a", "old", "renamed");
    h.ready();
    h.file("a", "new", "renamed");
    h.event("node_updated", "a");
    h.applied();
    assert!(!h.cfg.pairs[0].local.join("a/old").exists());
    assert_eq!(h.local("a/new"), "renamed");
    assert_eq!(h.walks(), 1);
}
#[test]
fn unknown_uids_schedule_one_walk_and_record_walk_disposition() {
    let mut h = Harness::new();
    h.ready();
    h.event("node_updated", "unknown");
    h.applied();
    assert_eq!(h.walks(), 2);
    assert!(h.log().contains("uid=file disposition=walk"));
}
#[test]
fn resolved_out_of_scope_event_queues_no_reconcile_or_walk() {
    let mut h = Harness::new();
    h.ready();
    h.event("node_updated", "root");
    h.applied();
    assert_eq!(h.walks(), 1);
    assert!(h.log().contains("uid=file disposition=out_of_scope"));
    assert!(!h.log().contains("change -> shallow"));
}
#[test]
fn refresh_required_triggers_exactly_one_full_walk() {
    let mut h = Harness::new();
    h.ready();
    h.write(
        "feed.json",
        json!([{"last_event_id":"1","refresh":"tree_refresh"}]),
    );
    h.applied();
    assert_eq!(h.walks(), 2);
    assert_eq!(h.log().matches("cause=refresh_required").count(), 1);
}
#[test]
fn failed_reconcile_neither_saves_cursor_nor_acks() {
    let mut h = Harness::new();
    h.ready();
    h.write("controls.json", json!({"list_errors":{"a":"transient"}}));
    h.event("node_updated", "a");
    h.wait(|| h.log().contains("disposition=deferred"));
    h.stop();
    assert_eq!(h.cursor().as_deref(), Some("0"));
    assert!(!h.records("acks").iter().any(|a| a["event_id"] == "1"));
}
#[test]
fn restart_resumes_stored_cursor_without_full_walk() {
    let mut h = Harness::new();
    h.ready();
    h.stop();
    h.file("a", "file", "offline");
    h.event("node_created", "a");
    h.start();
    h.applied();
    assert_eq!(h.local("a/file"), "offline");
    assert_eq!(h.walks(), 1);
    let calls = h.records("calls");
    let subscribes: Vec<_> = calls
        .iter()
        .filter(|c| c["method"] == "events.subscribe")
        .collect();
    assert_eq!(subscribes[1]["params"]["since_event_id"], "0");
}
#[test]
fn crash_between_apply_and_save_replays_without_transfer_or_baseline_change() {
    let mut h = Harness::new();
    h.ready();
    h.file("a", "file", "replayed");
    h.event("node_created", "a");
    h.applied();
    h.stop();
    let stats = Stats::open(&h.cfg.state_dir).unwrap();
    let before = format!("{:?}", stats.load_baseline("test").unwrap()).into_bytes();
    stats.save_feed_cursor("/my-files", "0").unwrap();
    let transfers = h.records("transfers").len();
    h.start();
    h.applied();
    h.stop();
    assert_eq!(h.local("a/file"), "replayed");
    assert_eq!(
        format!("{:?}", stats.load_baseline("test").unwrap()).into_bytes(),
        before
    );
    assert_eq!(h.records("transfers").len(), transfers);
    assert_eq!(h.walks(), 1);
}
#[test]
fn signed_out_pauses_and_auth_status_resumes_without_cli() {
    let mut h = Harness::new();
    h.record_cli_calls();
    h.ready();
    h.write("controls.json", json!({"signed_in":false}));
    h.write(
        "feed.json",
        json!([{"last_event_id":"1","signed_out":true}]),
    );
    h.wait(|| h.log().contains("pausing sync"));
    h.file("a", "file", "after-login");
    h.event("node_created", "a");
    std::thread::sleep(Duration::from_millis(800));
    assert!(!h.cfg.pairs[0].local.join("a/file").exists());
    h.write("controls.json", json!({"signed_in":true}));
    neutronsync::auth_signal::signal(&h.cfg.state_dir);
    h.applied();
    assert_eq!(h.local("a/file"), "after-login");
    assert_eq!(h.walks(), 1);
    assert!(h.log().contains("signed back in"));
    h.stop();
    assert!(
        h.records("cli-calls").is_empty(),
        "API watch invoked the CLI"
    );
}
#[test]
fn cli_has_no_feed_and_keeps_adaptive_pacing() {
    use neutronsync::protoncli::{ProtonCli, Remote};
    let mut h = Harness::new();
    assert!(ProtonCli::new(&h.cfg).change_feed().is_none());
    h.record_cli_calls();
    let path = h.dir.path().join("config.toml");
    let config = fs::read_to_string(&path)
        .unwrap()
        .replace("backend = \"api\"", "backend = \"cli\"")
        .replace("full_walk_interval = 86400", "full_walk_interval = 1");
    fs::write(path, config).unwrap();
    h.start();
    h.wait(|| h.log().contains("next full walk in ~900s"));
    std::thread::sleep(Duration::from_secs(2));
    h.stop();
    assert_eq!(h.log().matches("watch: full walk done").count(), 1);
    assert!(h.records("starts").is_empty());
    assert!(h.records("calls").is_empty());
    assert!(
        !h.records("cli-calls").is_empty(),
        "CLI recorder did not run"
    );
}

fn folder_departure(destination: Option<(&str, &str)>, parent: &str) {
    let mut h = Harness::new();
    let path = h.dir.path().join("config.toml");
    let config = fs::read_to_string(&path)
        .unwrap()
        .replace("backend = \"api\"", "backend = \"api\"\nscan_threads = 1");
    fs::write(path, config).unwrap();
    let mut tree = h.tree();
    tree["sub"] = json!({"name":"sub","type":"folder","parent_uid":"a"});
    h.write("tree.json", tree);
    h.file("sub", "child", "folder contents");
    h.ready();
    assert_eq!(h.local("a/sub/child"), "folder contents");
    // Restart after seeding to exclude startup inotify echoes from the event test.
    h.stop();
    h.start();
    h.wait(|| {
        h.records("calls")
            .iter()
            .filter(|c| c["method"] == "events.subscribe")
            .count()
            == 2
    });
    let call_start = h.records("calls").len();
    let mut tree = h.tree();
    if let Some((parent, name)) = destination {
        tree["sub"]["parent_uid"] = json!(parent);
        tree["sub"]["name"] = json!(name);
    } else {
        tree.as_object_mut().unwrap().remove("sub");
        tree.as_object_mut().unwrap().remove("file");
    }
    h.write("tree.json", tree);
    h.write("feed.json", json!([{"last_event_id":"1","events":[{"type":if destination.is_some(){"node_updated"}else{"node_deleted"},"node_uid":"sub","parent_uid":parent}]}]));
    h.applied();
    h.wait(|| h.records("acks").iter().any(|a| a["event_id"] == "1"));
    h.wait(|| h.log().contains("uid=sub disposition=mapped"));
    assert!(!h.cfg.pairs[0].local.join("a/sub").exists());
    if let Some((parent, name)) = destination {
        let target = format!("{parent}/{name}/child");
        h.wait(|| h.cfg.pairs[0].local.join(&target).is_file());
        assert_eq!(h.local(&target), "folder contents");
    }
    h.stop();
    assert_eq!(h.walks(), 1, "{}", h.log());
    assert!(
        !h.log().contains("uid=sub disposition=deferred"),
        "{}",
        h.log()
    );
    let calls = h.records("calls");
    assert!(calls[call_start..]
        .iter()
        .any(|c| c["method"] == "node.resolve" && c["params"]["path"] == "/my-files/test/a/sub"));
    assert!(!h.records("writes").iter().any(|c| matches!(
        c["method"].as_str(),
        Some("node.trash" | "file.upload" | "node.create_folder")
    )));
}

#[test]
fn remote_folder_move_acks_first_delivery_without_full_walk() {
    folder_departure(Some(("b", "sub")), "b");
}

#[test]
fn remote_folder_rename_acks_first_delivery_without_full_walk() {
    folder_departure(Some(("a", "renamed")), "a");
}

#[test]
fn remote_folder_trash_acks_first_delivery_without_full_walk() {
    folder_departure(None, "a");
}

#[test]
fn cursor_waits_for_download_completion() {
    let mut h = Harness::new();
    h.ready();
    h.write("controls.json", json!({"download_delay":2}));
    h.file("a", "file", "delayed");
    h.event("node_created", "a");
    h.wait(|| h.records("transfers").iter().any(|t| t["event"] == "start"));
    assert_eq!(h.cursor().as_deref(), Some("0"));
    assert!(!h.records("acks").iter().any(|a| a["event_id"] == "1"));
    h.applied();
    assert_eq!(h.local("a/file"), "delayed");
}

#[test]
fn three_failed_deliveries_run_one_apply_failed_walk_then_ack() {
    let mut h = Harness::new();
    h.ready();
    h.write("controls.json", json!({"list_errors":{"a":"transient"}}));
    h.event("node_updated", "a");
    h.applied();
    h.stop();
    assert_eq!(h.log().matches("cause=apply_failed").count(), 1);
    assert_eq!(h.log().matches("uid=file disposition=deferred").count(), 2);
}

#[test]
fn refused_resume_cursor_runs_one_resume_gap_walk() {
    let mut h = Harness::new();
    h.ready();
    h.stop();
    h.write("controls.json", json!({"refuse_cursor":true}));
    h.file("a", "file", "cursor-gap");
    h.event("node_created", "a");
    h.start();
    h.applied();
    h.stop();
    assert_eq!(h.local("a/file"), "cursor-gap");
    assert_eq!(h.log().matches("cause=resume_gap").count(), 1);
}
