//! Controller heartbeat tests use only a scratch config and a fake sidecar.
use neutronsync::{auth_signal, config, service::Controller};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn status_publisher_recovers_locked_account_and_coalesces_probes() {
    // This test binary has one test, so its transport override cannot race
    // another test or reach the real sidecar broker, session, or keyring.
    std::env::set_var("NEUTRONSYNC_DRIVE_STDIO", "1");
    // Script this test through controls.json, not an inherited screenshot or
    // validation override that can force every auth.status reply signed out.
    std::env::remove_var("NEUTRONSYNC_FAKE_AUTH");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sidecar = root.join("sidecar");
    fs::write(&sidecar, include_str!("fixtures/drive_sidecar.py")).unwrap();
    fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o700)).unwrap();
    let state_dir = root.join("state");
    let config_path = root.join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[cli]\nbackend = \"api\"\nsidecar = {sidecar:?}\n[options]\nstate_dir = {state_dir:?}\nauto_sync = false\n"
        ),
    )
    .unwrap();
    let controls = root.join("controls.json");
    fs::write(
        &controls,
        json!({"keyring_locked": true, "status_delay": 1}).to_string(),
    )
    .unwrap();
    let ctrl = Controller::new(config::load(config_path.to_str()).unwrap());
    for _ in 0..20 {
        ctrl.refresh_account(false);
        ctrl.publish_status();
    }
    wait_for("startup probe", Duration::from_secs(5), || {
        !ctrl.snapshot().account.checking
    });
    let calls = || -> Vec<Value> {
        fs::read_to_string(root.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|call: &Value| call["method"] == "auth.status")
            .collect()
    };
    assert_eq!(calls().len(), 1, "overlapping probes must coalesce");
    assert!(ctrl.snapshot().account.keyring_locked);
    assert!(!ctrl.snapshot().signed_out);
    fs::write(&controls, json!({"keyring_locked": false}).to_string()).unwrap();
    let recovery_deadline = Instant::now() + Duration::from_secs(25);
    loop {
        ctrl.publish_status();
        if Controller::read_status(&state_dir)
            .is_some_and(|s| s.account.signed_in && !s.account.keyring_locked && !s.signed_out)
        {
            break;
        }
        assert!(
            Instant::now() < recovery_deadline,
            "status publisher recovery timed out: state={}, status={:?}, calls={:?}, state_dir={:?}",
            serde_json::to_string(&ctrl.snapshot()).unwrap(),
            fs::read_to_string(state_dir.join("status.json")),
            calls(),
            ctrl.config().state_dir,
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(calls().len(), 2, "publisher must throttle its probes");
    wait_for("watcher recovery signal", Duration::from_secs(1), || {
        auth_signal::take(&state_dir)
    });
    assert!(
        !ctrl.is_watching(),
        "account recovery must not enable auto-sync"
    );
}
