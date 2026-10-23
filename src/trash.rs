//! Recoverable local deletion: freedesktop trash via `gio`, with a manual
//! XDG-trash fallback. We never silently fall through to a permanent delete.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::datefmt::epoch_to_iso;

/// Move `path` to the freedesktop trash. Prefers `gio` (handles cross-device
/// moves + restore metadata); falls back to a manual XDG-trash move on the home
/// filesystem.
pub fn trash_local(path: &Path, gio: Option<&Path>) -> Result<()> {
    if !path.exists() {
        return Ok(()); // already gone (e.g. a parent dir was trashed first)
    }
    if let Some(gio) = gio {
        let status = std::process::Command::new(gio)
            .arg("trash")
            .arg("--")
            .arg(path)
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Ok(());
            }
        }
        // fall through to manual on any gio failure
    }
    xdg_trash(path)
}

pub(crate) fn data_home() -> PathBuf {
    if let Ok(v) = std::env::var("XDG_DATA_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local").join("share")
}

/// Manual freedesktop trash on the home filesystem (fallback for `gio`).
fn xdg_trash(path: &Path) -> Result<()> {
    let trash = data_home().join("Trash");
    let files_dir = trash.join("files");
    let info_dir = trash.join("info");
    std::fs::create_dir_all(&files_dir)?;
    std::fs::create_dir_all(&info_dir)?;

    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("path has no file name")?
        .to_string();

    let mut dest = files_dir.join(&base);
    let mut info = info_dir.join(format!("{base}.trashinfo"));
    let mut n = 1u32;
    while dest.exists() || info.exists() {
        dest = files_dir.join(format!("{base}.{n}"));
        info = info_dir.join(format!("{base}.{n}.trashinfo"));
        n += 1;
    }

    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let contents = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        uri_escape(&abs.to_string_lossy()),
        epoch_to_iso(now)
    );
    std::fs::write(&info, contents)?;
    if let Err(e) = std::fs::rename(path, &dest) {
        let _ = std::fs::remove_file(&info);
        bail!(
            "could not trash {} (cross-filesystem?). Install gio/trash-cli or set \
             options.local_delete = \"remove\". Error: {e}",
            path.display()
        );
    }
    Ok(())
}

/// Percent-encode everything except unreserved + '/' (enough for a trash path).
fn uri_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
