//! AC-10: browser and picker listing go through backend::list_remote.
use neutronsync::config;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn api_cfg(root: &std::path::Path) -> neutronsync::config::Config {
    std::env::set_var("NEUTRONSYNC_DRIVE_STDIO", "1");
    let fixture = root.join("fake-sidecar");
    fs::write(&fixture, include_str!("fixtures/drive_sidecar.py")).unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("config.toml");
    fs::write(
        &path,
        format!(
            r#"
[cli]
backend = "api"
sidecar = {:?}
[options]
state_dir = {:?}
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
    fs::create_dir_all(root.join("local")).unwrap();
    config::load(path.to_str()).unwrap()
}

#[test]
fn list_remote_api_lists_my_files_without_proton_drive() {
    let temp = TempDir::new().unwrap();
    let cfg = api_cfg(temp.path());

    // System bins only — no proton-drive, but python3 still resolves for the fixture shebang.
    let old_path = std::env::var_os("PATH");
    std::env::set_var("PATH", "/usr/bin:/bin");

    let root = neutronsync::backend::list_remote(&cfg, "/my-files").unwrap();
    assert!(
        root.iter()
            .any(|e| e.path == "folder" || e.path == "remote.txt"),
        "expected /my-files listing, got {root:?}"
    );
    let sub = neutronsync::backend::list_remote(&cfg, "/my-files/folder").unwrap();
    assert!(
        sub.iter().any(|e| e.path == "nested"),
        "expected subfolder listing, got {sub:?}"
    );

    if let Some(p) = old_path {
        std::env::set_var("PATH", p);
    }
}
