//! Local freedesktop trash, restricted to configured pair roots.
use std::ffi::CString;
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Clone, Serialize)]
#[cfg_attr(feature = "serve", derive(utoipa::ToSchema))]
pub struct TrashItem {
    pub id: String,
    pub pair: String,
    pub name: String,
    pub original_path: String,
    pub size: u64,
    pub deleted_at: String,
    #[serde(skip)]
    root: PathBuf,
    #[serde(skip)]
    key: String,
    #[serde(skip)]
    pair_root: PathBuf,
}

/// Open every directory without following symlinks, including intermediate ones.
fn directory(path: &Path, create: bool) -> Result<File> {
    if !path.is_absolute() {
        bail!("Expected an absolute path");
    }
    let mut dir = File::open("/")?;
    for part in path.components() {
        let Component::Normal(part) = part else {
            if part == Component::RootDir {
                continue;
            }
            bail!("Unsafe path");
        };
        let name = CString::new(part.as_encoded_bytes())?;
        if create {
            // mkdirat cannot follow a symlink; openat below verifies existing entries.
            unsafe {
                libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700);
            }
        }
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok(dir)
}

fn anchored(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn decode(value: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut iter = value.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = (iter.next().context("Invalid escape")? as char)
                .to_digit(16)
                .context("Invalid escape")?;
            let lo = (iter.next().context("Invalid escape")? as char)
                .to_digit(16)
                .context("Invalid escape")?;
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(b);
        }
    }
    if bytes.contains(&0) {
        bail!("NUL in path");
    }
    Ok(String::from_utf8(bytes)?)
}

pub fn roots(pair: &Path) -> Vec<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let mut roots = vec![crate::trash::data_home().join("Trash")];
    for ancestor in pair.ancestors() {
        roots.push(ancestor.join(format!(".Trash-{uid}")));
        roots.push(ancestor.join(".Trash").join(uid.to_string()));
    }
    roots
}

/// Explicit roots keep filesystem tests independent of the user's trash.
pub fn list(pair: &str, pair_root: &Path, roots: &[PathBuf]) -> Result<Vec<TrashItem>> {
    let pair_root = fs::canonicalize(pair_root)?;
    directory(&pair_root, false)?;
    let mut items = Vec::new();
    for root in roots {
        let Ok(info_dir) = directory(&root.join("info"), false) else {
            continue;
        };
        let Ok(files_dir) = directory(&root.join("files"), false) else {
            continue;
        };
        for entry in fs::read_dir(anchored(&info_dir))? {
            let entry = entry?;
            let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(key) = filename.strip_suffix(".trashinfo") else {
                continue;
            };
            if key.is_empty() || key == "." || key == ".." {
                continue;
            }
            let Ok(item) = read_item(pair, &pair_root, root, key, &info_dir, &files_dir) else {
                continue;
            };
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items.dedup_by(|a, b| a.id == b.id);
    Ok(items)
}

fn read_item(
    pair: &str,
    pair_root: &Path,
    root: &Path,
    key: &str,
    info: &File,
    files: &File,
) -> Result<TrashItem> {
    use std::io::Read;
    let name = CString::new(format!("{key}.trashinfo"))?;
    let fd = unsafe {
        libc::openat(
            info.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        bail!("Cannot read trash metadata");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        bail!("Invalid trash metadata");
    }
    let mut text = String::new();
    file.take(65537).read_to_string(&mut text)?;
    if text.len() > 65536 {
        bail!("Trash metadata too large");
    }
    let mut path = None;
    let mut date = None;
    let mut section = false;
    for line in text.lines() {
        if line.starts_with('[') {
            section = line == "[Trash Info]";
            continue;
        }
        if !section {
            continue;
        }
        if let Some(value) = line.strip_prefix("Path=") {
            path = Some(decode(value)?);
        }
        if let Some(value) = line.strip_prefix("DeletionDate=") {
            date = Some(value.to_owned());
        }
    }
    let path = PathBuf::from(path.context("Missing original path")?);
    // Relative freedesktop paths are relative to the volume mount point.
    let original = if path.is_absolute() {
        path
    } else {
        let mount = if root
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == ".Trash"))
        {
            root.parent().and_then(Path::parent)
        } else {
            root.parent()
        }
        .context("Missing mount point")?;
        mount.join(path)
    };
    if original
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || !original.starts_with(pair_root)
        || original == pair_root
    {
        bail!("Path outside folder pair");
    }
    let payload = anchored(files).join(key);
    let meta = fs::symlink_metadata(&payload)?;
    if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
        bail!("Unsupported trash entry");
    }
    let id = sha1_smol::Sha1::from(format!("{}\0{key}", root.display()))
        .digest()
        .to_string();
    let size = if meta.is_dir() {
        let mut total = 0u64;
        for entry in walkdir::WalkDir::new(&payload).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
        total
    } else {
        meta.len()
    };
    Ok(TrashItem {
        id,
        pair: pair.to_owned(),
        name: original
            .file_name()
            .context("Missing name")?
            .to_string_lossy()
            .into_owned(),
        original_path: original.to_string_lossy().into_owned(),
        size,
        deleted_at: date.context("Missing deletion date")?,
        root: root.to_owned(),
        key: key.to_owned(),
        pair_root: pair_root.to_owned(),
    })
}

pub fn act(item: &TrashItem, restore: bool) -> Result<()> {
    let info = directory(&item.root.join("info"), false)?;
    let files = directory(&item.root.join("files"), false)?;
    // Re-read metadata before acting, rather than trusting an earlier listing.
    let current = read_item(
        &item.pair,
        &item.pair_root,
        &item.root,
        &item.key,
        &info,
        &files,
    )?;
    if current.original_path != item.original_path {
        bail!("Trash metadata changed; refresh the list");
    }
    let source = CString::new(item.key.as_str())?;
    if restore {
        let target = Path::new(&current.original_path);
        let parent = directory(target.parent().context("Missing parent")?, true)?;
        let name = CString::new(
            target
                .file_name()
                .context("Missing name")?
                .as_encoded_bytes(),
        )?;
        let result = unsafe {
            libc::renameat2(
                files.as_raw_fd(),
                source.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    } else {
        let path = anchored(&files).join(&item.key);
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    fs::remove_file(anchored(&info).join(format!("{}.trashinfo", item.key)))?;
    Ok(())
}
