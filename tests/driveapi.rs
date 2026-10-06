use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use neutronsync::config::{self, Backend, Config};
use neutronsync::driveapi::DriveApi;
use neutronsync::engine::{self, Engine};
use neutronsync::logger::Logger;
use neutronsync::models::Entry;
use neutronsync::protoncli::{is_not_logged_in, ListOutcome, Remote};
use neutronsync::stats::Stats;

fn config_at(root: &Path) -> Config {
    let fixture = root.join("fake-sidecar");
    fs::write(&fixture, include_str!("fixtures/drive_sidecar.py")).unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(root.join("local")).unwrap();
    let path = root.join("config.toml");
    fs::write(
        &path,
        format!(
            r#"
[cli]
backend = "api"
sidecar = {:?}
binary = "/does/not/exist/proton-drive"
scan_threads = 3
[options]
state_dir = {:?}
propagate_deletes = true
local_delete = "remove"
[[pair]]
name = "test"
local = {:?}
remote = "/my-files/Documents"
"#,
            fixture,
            root.join("state"),
            root.join("local")
        ),
    )
    .unwrap();
    config::load(path.to_str()).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    if !root.exists() {
        return BTreeMap::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (
                path,
                if entry.file_type().is_file() {
                    fs::read(entry.path()).unwrap()
                } else {
                    Vec::new()
                },
            )
        })
        .collect()
}

#[test]
fn scripted_sidecar_contract_and_all_read_only_entry_points() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config_at(temp.path());
    let api = DriveApi::new(&cfg).unwrap();
    let other = DriveApi::new(&cfg).unwrap();
    assert!(!temp.path().join("starts").exists(), "construction is lazy");
    let status = api.status().unwrap();
    assert_eq!(
        status,
        other.status().unwrap(),
        "instances share one process"
    );
    std::thread::scope(|s| {
        let jobs: Vec<_> = (0..8)
            .map(|_| s.spawn(|| DriveApi::new(&cfg).unwrap().status().unwrap()))
            .collect();
        for job in jobs {
            assert_eq!(job.join().unwrap(), status);
        }
    });
    assert_eq!(
        fs::read_to_string(temp.path().join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    let plain = api.list_dir("/my-files/Documents").unwrap();
    assert_eq!(plain.len(), 3, "plain list");
    assert_eq!(plain[0].path, "remote.txt");
    assert_eq!(plain[0].size, 42);
    assert!(matches!(
        api.list_dir_probe("/my-files/missing").unwrap(),
        ListOutcome::NotFound
    ));
    assert!(
        matches!(api.list_dir_probe("/my-files/empty").unwrap(), ListOutcome::Listed(v) if v.is_empty())
    );
    assert!(api.list_dir("/my-files/missing").unwrap().is_empty());
    assert!(matches!(
        api.list_dir_probe("/my-files/list-missing").unwrap(),
        ListOutcome::NotFound
    ));
    for path in ["missing", "walk-missing"] {
        assert!(
            api.list_tree(&format!("/my-files/{path}"), &|_| false, &|_| {})
                .unwrap()
                .root_missing
        );
    }
    let progress = Mutex::new(Vec::new());
    let tree = api
        .list_tree("/my-files/partial", &|p| p == "excluded", &|p| {
            progress.lock().unwrap().push(p.to_string())
        })
        .unwrap();
    assert_eq!(
        tree.failed,
        ["unreadable"],
        "failed folders stay pair-relative"
    );
    assert!(
        !tree
            .entries
            .iter()
            .any(|(p, _)| p == "excluded" || p.starts_with("excluded/")),
        "exclude hides descendants even if closure only matches parent"
    );
    let paths: Vec<_> = tree.entries.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.contains(&"folder/nested/child.txt"),
        "parent-before-child path reconstruction"
    );
    assert!(
        paths.iter().position(|p| *p == "folder").unwrap()
            < paths.iter().position(|p| *p == "folder/nested").unwrap()
    );
    assert!(
        paths.iter().position(|p| *p == "folder/nested").unwrap()
            < paths
                .iter()
                .position(|p| *p == "folder/nested/child.txt")
                .unwrap()
    );
    for folder in [
        "/my-files/partial",
        "/my-files/partial/folder",
        "/my-files/partial/folder/nested",
        "/my-files/partial/excluded",
    ] {
        assert!(
            progress.lock().unwrap().iter().any(|p| p == folder),
            "progress missing {folder}"
        );
    }
    let empty_progress = Mutex::new(0);
    assert!(
        !api.list_tree("/my-files/empty", &|_| false, &|_| *empty_progress
            .lock()
            .unwrap() += 1)
            .unwrap()
            .root_missing
    );
    assert_eq!(*empty_progress.lock().unwrap(), 1);
    for path in ["auth", "list-auth"] {
        let path = format!("/my-files/{path}");
        assert!(is_not_logged_in(
            &api.list_dir(&path).unwrap_err().to_string()
        ));
        let error = match api.list_dir_probe(&path) {
            Err(e) => e,
            _ => panic!("auth returned a listing"),
        };
        assert!(is_not_logged_in(&error.to_string()));
    }
    assert!(is_not_logged_in(
        &api.list_tree("/my-files/walk-auth", &|_| false, &|_| {})
            .err()
            .unwrap()
            .to_string()
    ));
    for code in ["transient", "fatal", "conflict", "rate_limited"] {
        assert!(api.list_dir(&format!("/my-files/{code}")).is_err());
        assert!(api.list_dir_probe(&format!("/my-files/{code}")).is_err());
    }
    for (op, result) in [
        ("create_folder", api.create_folder("a", "b")),
        ("upload", api.upload("a", "b")),
        ("download", api.download("a", "b")),
        ("trash", api.trash("a")),
        ("rename", api.rename("a", "b")),
    ] {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("read-only until Phase 2") && error.contains(op));
    }

    // Force a real local deletion and remote download into the plan. Leave the
    // DB open so WAL/SHM bytes, pending rows and >1000 history rows are protected.
    let pair = &cfg.pairs[0];
    fs::write(pair.local.join("delete-local.txt"), "keep me").unwrap();
    fs::write(pair.local.join("upload.txt"), "never upload").unwrap();
    let stats = Stats::open(&cfg.state_dir).unwrap();
    let baseline = BTreeMap::from([(
        "delete-local.txt".into(),
        Entry {
            path: "delete-local.txt".into(),
            is_dir: false,
            size: 7,
            mtime: None,
            sha1: None,
            remote_id: None,
        },
    )]);
    stats.save_baseline(&pair.name, &baseline).unwrap();
    stats.set_last_synced(&pair.name, 123).unwrap();
    for n in 0..1002 {
        stats
            .record_op(&pair.name, "upload", "old.txt", true, None, n)
            .unwrap();
    }
    let db = rusqlite::Connection::open(cfg.state_dir.join("stats.db")).unwrap();
    db.execute("UPDATE baseline SET state='pending_down'", [])
        .unwrap();
    let log = Logger::new(&cfg.log_dir(), false, true);
    let before_state = snapshot(&cfg.state_dir);
    let before_local = snapshot(&pair.local);
    assert_eq!(
        neutronsync::state::load_baseline_read_only(&cfg.state_dir, &pair.name)
            .unwrap()
            .len(),
        1,
        "committed WAL rows are read"
    );
    for which in 0..7 {
        let result = match which {
            0 => engine::run_sync(&cfg, &cfg.pairs, false, false, &log),
            1 => engine::run_sync_with(&cfg, &cfg.pairs, false, false, &log, None, None),
            2 => engine::run_sync_scoped(&cfg, pair, "", &log, None, None),
            3 => engine::run_sync_shallow(&cfg, pair, "", &log, None, None),
            4 => engine::run_sync_streaming(&cfg, pair, &log, None, None),
            5 => engine::run_sync_shallow_many(
                &cfg,
                pair,
                &[String::new(), "folder".into()],
                &log,
                None,
                None,
            ),
            _ => {
                let (tx, rx) = mpsc::channel();
                let logger = Logger::channel(tx, false);
                let mut engine = Engine::new(&cfg, DriveApi::new(&cfg).unwrap(), &logger, false);
                let result = engine.sync_pair(pair, false).unwrap();
                assert!(
                    rx.try_iter().any(|line| line.contains("forcing a dry run")),
                    "direct Engine backstop warns"
                );
                engine::RunSummary {
                    applied: result.applied,
                    errors: result.errors.len(),
                    pairs: 1,
                }
            }
        };
        assert_eq!(
            (result.applied, result.errors),
            (0, 0),
            "entry point {which}"
        );
        assert_eq!(
            snapshot(&pair.local),
            before_local,
            "local bytes: entry point {which}"
        );
        assert_eq!(
            snapshot(&cfg.state_dir),
            before_state,
            "state bytes: entry point {which}"
        );
    }
    let mut cli_config = cfg.clone();
    cli_config.backend = Backend::Cli;
    fs::write(
        cfg.source_path.as_ref().unwrap(),
        config::to_toml(&cli_config).unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_neutronsync"))
        .args([
            "--config",
            cfg.source_path.as_ref().unwrap().to_str().unwrap(),
            "sync",
            "--backend",
            "api",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("forcing a dry run"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"dry_run\": true"));
    assert_eq!(snapshot(&pair.local), before_local, "cmd_sync local bytes");
    assert_eq!(
        snapshot(&cfg.state_dir),
        before_state,
        "cmd_sync state bytes including logs and WAL"
    );
    fs::write(
        cfg.source_path.as_ref().unwrap(),
        config::to_toml(&cfg).unwrap(),
    )
    .unwrap();
    let cli_override = Command::new(env!("CARGO_BIN_EXE_neutronsync"))
        .args([
            "--config",
            cfg.source_path.as_ref().unwrap().to_str().unwrap(),
            "sync",
            "--backend",
            "cli",
        ])
        .output()
        .unwrap();
    assert!(!cli_override.status.success());
    assert!(String::from_utf8_lossy(&cli_override.stderr).contains("cli.binary"));

    // Missing state and legacy JSON must not trigger database creation/import.
    let mut fresh = cfg.clone();
    fresh.state_dir = temp.path().join("absent-state");
    assert_eq!(
        engine::run_sync(&fresh, &fresh.pairs, false, false, &log).errors,
        0
    );
    assert!(!fresh.state_dir.exists());
    fs::create_dir_all(fresh.state_dir.join("baselines")).unwrap();
    fs::write(fresh.state_dir.join("baselines/test.json"), r#"{"version":1,"entries":{"old.txt":{"is_dir":false,"size":1,"mtime":null,"sha1":null,"remote_id":null}}}"#).unwrap();
    let legacy = snapshot(&fresh.state_dir);
    assert_eq!(
        engine::run_sync(&fresh, &fresh.pairs, false, false, &log).errors,
        0
    );
    assert_eq!(snapshot(&fresh.state_dir), legacy);

    // All blocked requests fail on process death; existing DriveApi instances
    // restart lazily on their next read, sharing the replacement too.
    let started = Instant::now();
    std::thread::scope(|s| {
        let jobs: Vec<_> = (0..3)
            .map(|_| s.spawn(|| api.list_dir("/my-files/hold")))
            .collect();
        while fs::read_to_string(temp.path().join("held"))
            .unwrap_or_default()
            .lines()
            .count()
            < 3
        {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(other.list_dir("/my-files/die").is_err());
        for job in jobs {
            assert!(job.join().unwrap().is_err());
        }
    });
    assert!(started.elapsed() < Duration::from_secs(5));
    let restarted = api.status().unwrap();
    assert_ne!(status, restarted);
    assert_eq!(other.status().unwrap(), restarted);
    for line in fs::read_to_string(temp.path().join("calls"))
        .unwrap()
        .lines()
    {
        let call: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(["auth.status", "node.resolve", "node.list", "node.walk"]
            .contains(&call["method"].as_str().unwrap()));
    }
}

#[test]
fn backend_config_roundtrip_and_argument_validation() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config_at(temp.path());
    let path = cfg.source_path.clone().unwrap();
    for backend in [Backend::Cli, Backend::Api] {
        cfg.backend = backend;
        fs::write(&path, config::to_toml(&cfg).unwrap()).unwrap();
        let loaded = config::load(path.to_str()).unwrap();
        assert_eq!(loaded.backend, backend);
        assert_eq!(loaded.sidecar, cfg.sidecar);
    }
    fs::write(&path, "[cli]\nbackend = 'invalid'\n").unwrap();
    assert!(config::load(path.to_str())
        .unwrap_err()
        .to_string()
        .contains("cli.backend"));
    fs::write(&path, "").unwrap();
    assert_eq!(config::load(path.to_str()).unwrap().backend, Backend::Cli);
    let output = Command::new(env!("CARGO_BIN_EXE_neutronsync"))
        .args(["sync", "--backend", "invalid"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value"));
    cfg.sidecar = Some(temp.path().join("missing"));
    assert!(neutronsync::backend::select(&cfg)
        .err()
        .unwrap()
        .to_string()
        .contains("cli.sidecar"));
    cfg.backend = Backend::Cli;
    assert!(neutronsync::backend::select(&cfg)
        .err()
        .unwrap()
        .to_string()
        .contains("cli.binary"));
}
