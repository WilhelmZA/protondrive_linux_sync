use neutronsync::trash_browser::{act, list};
use std::{fs, os::unix::fs::symlink, path::PathBuf};

struct Lab {
    _tmp: tempfile::TempDir,
    pair: PathBuf,
    trash: PathBuf,
}
impl Lab {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let pair = tmp.path().join("pair");
        let trash = tmp.path().join(".Trash-test");
        for p in [&pair, &trash.join("files"), &trash.join("info")] {
            fs::create_dir_all(p).unwrap();
        }
        Self {
            _tmp: tmp,
            pair,
            trash,
        }
    }
    fn entry(&self, key: &str, path: &str) {
        fs::write(self.trash.join("files").join(key), "saved").unwrap();
        fs::write(
            self.trash.join("info").join(format!("{key}.trashinfo")),
            format!("[Trash Info]\nPath={path}\nDeletionDate=2026-10-23T12:34:56\n"),
        )
        .unwrap();
    }
    fn items(&self) -> Vec<neutronsync::trash_browser::TrashItem> {
        list("test", &self.pair, std::slice::from_ref(&self.trash)).unwrap()
    }
}

#[test]
fn list_restore_decodes_path_and_preserves_collision() {
    let l = Lab::new();
    l.entry(
        "a.1",
        &format!("{}/sub/a%20%23%C3%A9.txt", l.pair.display()),
    );
    let items = l.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "a #é.txt");
    assert_eq!(items[0].size, 5);
    assert_eq!(items[0].deleted_at, "2026-10-23T12:34:56");
    act(&items[0], true).unwrap();
    assert_eq!(
        fs::read_to_string(l.pair.join("sub/a #é.txt")).unwrap(),
        "saved"
    );
    assert!(l.items().is_empty());
    l.entry("b", &l.pair.join("sub/a #é.txt").to_string_lossy());
    assert!(act(&l.items()[0], true).is_err());
    assert_eq!(
        fs::read_to_string(l.pair.join("sub/a #é.txt")).unwrap(),
        "saved"
    );
    assert!(l.trash.join("files/b").exists());
    assert!(l.trash.join("info/b.trashinfo").exists());
}

#[test]
fn rejects_outside_traversal_metadata_and_payload_symlinks() {
    let l = Lab::new();
    l.entry("outside", "/etc/passwd");
    l.entry("traversal", &format!("{}/../secret", l.pair.display()));
    l.entry("encoded", &format!("{}/%2e%2e/secret", l.pair.display()));
    l.entry("nul", &format!("{}/bad%00", l.pair.display()));
    l.entry("bad-escape", &format!("{}/bad%xx", l.pair.display()));
    l.entry("link", &l.pair.join("ok").to_string_lossy());
    fs::remove_file(l.trash.join("files/link")).unwrap();
    symlink("/etc/passwd", l.trash.join("files/link")).unwrap();
    l.entry("info-link", &l.pair.join("ok").to_string_lossy());
    fs::remove_file(l.trash.join("info/info-link.trashinfo")).unwrap();
    symlink("/etc/passwd", l.trash.join("info/info-link.trashinfo")).unwrap();
    assert!(l.items().is_empty());
}

#[test]
fn refuses_symlink_restore_parent_and_existing_dangling_link() {
    let l = Lab::new();
    l.entry("a", &l.pair.join("sub/file").to_string_lossy());
    symlink(l._tmp.path(), l.pair.join("sub")).unwrap();
    assert!(act(&l.items()[0], true).is_err());
    assert!(!l._tmp.path().join("file").exists());
    fs::remove_file(l.pair.join("sub")).unwrap();
    fs::create_dir(l.pair.join("sub")).unwrap();
    symlink("missing", l.pair.join("sub/file")).unwrap();
    assert!(act(&l.items()[0], true).is_err());
}

#[test]
fn delete_directory_does_not_follow_nested_symlinks() {
    let l = Lab::new();
    l.entry("dir", &l.pair.join("dir").to_string_lossy());
    fs::remove_file(l.trash.join("files/dir")).unwrap();
    fs::create_dir(l.trash.join("files/dir")).unwrap();
    let protected = l.pair.join("protected");
    fs::write(&protected, "keep").unwrap();
    symlink(&protected, l.trash.join("files/dir/link")).unwrap();
    act(&l.items()[0], false).unwrap();
    assert_eq!(fs::read_to_string(protected).unwrap(), "keep");
    assert!(l.items().is_empty());
}

#[test]
fn relative_paths_are_mount_relative_and_other_pairs_are_hidden() {
    let l = Lab::new();
    l.entry("relative", "pair/file.txt");
    l.entry("other", "other/file.txt");
    assert_eq!(l.items().len(), 1);
    act(&l.items()[0], true).unwrap();
    assert!(l.pair.join("file.txt").exists());
}

#[test]
fn refuses_symlink_trash_directories_and_changed_metadata() {
    let l = Lab::new();
    l.entry("a", &l.pair.join("a").to_string_lossy());
    let item = l.items().remove(0);
    l.entry("a", &l.pair.join("b").to_string_lossy());
    assert!(act(&item, true).is_err());
    fs::rename(l.trash.join("files"), l.trash.join("elsewhere")).unwrap();
    symlink(l.trash.join("elsewhere"), l.trash.join("files")).unwrap();
    assert!(l.items().is_empty());
}

#[test]
fn directory_restore_reports_content_size_and_preserves_tree() {
    let l = Lab::new();
    l.entry("dir", &l.pair.join("nested/dir").to_string_lossy());
    let payload = l.trash.join("files/dir");
    fs::remove_file(&payload).unwrap();
    fs::create_dir_all(payload.join("child")).unwrap();
    fs::write(payload.join("a"), "123").unwrap();
    fs::write(payload.join("child/b"), "4567").unwrap();
    let items = l.items();
    assert_eq!(items[0].size, 7);
    act(&items[0], true).unwrap();
    assert_eq!(
        fs::read_to_string(l.pair.join("nested/dir/child/b")).unwrap(),
        "4567"
    );
    assert!(l.items().is_empty());
}

#[test]
fn trash_collision_names_remain_independent_entries() {
    let l = Lab::new();
    l.entry("a", &l.pair.join("same.txt").to_string_lossy());
    l.entry("a.1", &l.pair.join("same.txt").to_string_lossy());
    let items = l.items();
    assert_eq!(items.len(), 2);
    assert_ne!(items[0].id, items[1].id);
    act(&items[0], true).unwrap();
    assert!(act(&items[1], true).is_err());
    assert_eq!(l.items().len(), 1);
}
