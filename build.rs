//! Records which commit a binary was built from, as `NEUTRONSYNC_BUILD_SHA`,
//! so `neutronsync --version` and the About page can name it. Uses only std.
//! The selection order lives in `build/sha.rs` so it can be unit-tested.

use std::path::{Path, PathBuf};
use std::process::Command;

include!("build/sha.rs");

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    println!("cargo:rerun-if-env-changed=NEUTRONSYNC_BUILD_SHA");
    println!(
        "cargo:rerun-if-changed={}",
        root.join(".build-sha").display()
    );
    if let Some(git_dir) = git_dir(&root) {
        for f in ["HEAD", "index"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(f).display());
        }
        // A symbolic HEAD ("ref: refs/heads/x") changes when that ref moves,
        // which does not touch HEAD itself.
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
            if let Some(r) = head.trim().strip_prefix("ref: ") {
                println!(
                    "cargo:rerun-if-changed={}",
                    git_dir.join(r.trim()).display()
                );
            }
        }
    }

    let env_override = std::env::var("NEUTRONSYNC_BUILD_SHA").ok();
    let (git_head, git_dirty) = git_state(&root);

    // The `-dirty` marker depends on every tracked file, and editing one
    // touches neither HEAD nor the index. Watching the tracked files keeps the
    // marker honest; cargo only stats them, and git lists them once here.
    if git_head.is_some() {
        for f in tracked_files(&root) {
            println!("cargo:rerun-if-changed={}", root.join(f).display());
        }
    }
    let file = std::fs::read_to_string(root.join(".build-sha")).ok();

    let sha = choose_build_sha(
        env_override.as_deref(),
        git_head.as_deref(),
        git_dirty,
        file.as_deref(),
    );
    println!("cargo:rustc-env=NEUTRONSYNC_BUILD_SHA={sha}");
}

/// The repository's `.git` directory, following a worktree's `.git` file.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    let target = PathBuf::from(target);
    Some(if target.is_absolute() {
        target
    } else {
        root.join(target)
    })
}

/// Every tracked file, relative to `root`; empty when git is unavailable.
fn tracked_files(root: &Path) -> Vec<String> {
    Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split('\0')
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Short hash of HEAD and whether tracked files differ from it. `None` when git
/// is missing, the tree is not a checkout, or git refuses (for example a
/// container checkout owned by another user).
fn git_state(root: &Path) -> (Option<String>, bool) {
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let Some(head) = run(&["rev-parse", "--short=12", "HEAD"]).filter(|h| !h.is_empty()) else {
        return (None, false);
    };
    // Untracked files are not a change to what was built; modified or staged
    // tracked files are.
    let dirty = run(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    (Some(head), dirty)
}
