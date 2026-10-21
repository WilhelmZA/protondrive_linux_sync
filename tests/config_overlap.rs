//! Config validation for overlapping pairs and Proton path rules.

use neutronsync::config::load;
use std::fs;
use tempfile::TempDir;

fn write_pair_config(dir: &TempDir, body: &str) {
    let path = dir.path().join("neutronsync.toml");
    fs::write(
        &path,
        format!(
            r#"
[cli]
backend = "api"
[options]
state_dir = {:?}
{body}
"#,
            dir.path().join("state")
        ),
    )
    .unwrap();
}

#[test]
fn overlapping_local_folders_are_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("docs/sub")).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "docs"
local = {:?}
remote = "Docs"
[[pair]]
name = "sub"
local = {:?}
remote = "Other"
"#,
            root.join("docs"),
            root.join("docs/sub")
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("docs"), "{err}");
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn overlapping_proton_folders_are_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "a"
local = {:?}
remote = "/my-files/Shared"
[[pair]]
name = "b"
local = {:?}
remote = "/my-files/Shared/nested"
"#,
            root.join("a"),
            root.join("b")
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn sibling_folders_still_load() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "a"
local = {:?}
remote = "A"
[[pair]]
name = "b"
local = {:?}
remote = "B"
"#,
            root.join("a"),
            root.join("b")
        ),
    );
    assert_eq!(
        load(dir.path().join("neutronsync.toml").to_str())
            .unwrap()
            .pairs
            .len(),
        2
    );
}

#[test]
fn scan_threads_above_64_are_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("neutronsync.toml");
    fs::write(
        &path,
        format!(
            r#"
[cli]
backend = "api"
scan_threads = 9999
[options]
state_dir = {:?}
"#,
            dir.path().join("state")
        ),
    )
    .unwrap();
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("scan_threads"), "{err}");
}

#[test]
fn remote_root_must_be_absolute() {
    let dir = TempDir::new().unwrap();
    write_pair_config(&dir, "remote_root = \"relative\"\n");
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("remote_root"), "{err}");
}

#[test]
fn same_local_folder_is_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "a"
local = {:?}
remote = "A"
[[pair]]
name = "b"
local = {:?}
remote = "B"
"#,
            root, root
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn parent_local_folder_is_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(root.join("child")).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "parent"
local = {:?}
remote = "Parent"
[[pair]]
name = "child"
local = {:?}
remote = "Child"
"#,
            root,
            root.join("child")
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn same_proton_folder_is_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "a"
local = {:?}
remote = "/my-files/Same"
[[pair]]
name = "b"
local = {:?}
remote = "/my-files/Same"
"#,
            root.join("a"),
            root.join("b")
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn proton_root_overlaps_every_other_pair() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "root"
local = {:?}
remote = "/"
[[pair]]
name = "docs"
local = {:?}
remote = "/my-files/Docs"
"#,
            root.join("a"),
            root.join("b")
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("one folder cannot be in two pairs"), "{err}");
}

#[test]
fn empty_proton_path_segment_is_refused() {
    let dir = TempDir::new().unwrap();
    write_pair_config(&dir, "remote_root = \"/my-files//Shared\"\n");
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("remote_root"), "{err}");
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn pair_remote_with_dotdot_is_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    write_pair_config(
        &dir,
        &format!(
            r#"
[[pair]]
name = "x"
local = {:?}
remote = "/my-files/../x"
"#,
            root
        ),
    );
    let err = load(dir.path().join("neutronsync.toml").to_str())
        .unwrap_err()
        .to_string();
    assert!(err.contains("remote folder"), "{err}");
}
