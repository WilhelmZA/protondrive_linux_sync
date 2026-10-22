//! Local folder listing and log tailing, with the checks that keep the API
//! inside the folders it was given.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use super::error::ApiError;

#[derive(Serialize, Debug, PartialEq, Eq, utoipa::ToSchema)]
pub struct LocalEntry {
    pub name: String,
    pub path: String,
    pub hidden: bool,
}

/// `path` with `.` and `..` steps resolved as text, without following links.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Where `path` really leads once it exists: the deepest part of it that
/// exists now, resolved through every link, with the rest appended. A folder
/// that does not exist yet is created at exactly that place, so this is what
/// must lie inside a root. A name in the missing part that is itself a link
/// (one leading nowhere) is refused, since creating through it would follow
/// it.
fn resolve_existing(path: &Path) -> Result<PathBuf, ApiError> {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut base = path;
    loop {
        match std::fs::canonicalize(base) {
            Ok(real) => {
                let mut out = real;
                for name in missing.iter().rev() {
                    out.push(name);
                }
                return Ok(out);
            }
            Err(_) => {
                if std::fs::symlink_metadata(base).is_ok() {
                    return Err(ApiError::invalid_config(
                        "The local path goes through a link that leads nowhere.",
                    ));
                }
                let (Some(name), Some(parent)) = (base.file_name(), base.parent()) else {
                    return Ok(path.to_path_buf());
                };
                missing.push(name);
                base = parent;
            }
        }
    }
}

/// `path` lies under one of `roots` (both already canonical).
pub fn inside(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

/// A folder path a client may name for a pair: absolute, without `.` or `..`
/// steps, and under one of `roots` when any were given. When the folder
/// exists its real location is checked too, so a link cannot point outside.
pub fn check_pair_local(roots: &[PathBuf], local: &str) -> Result<PathBuf, ApiError> {
    let path = PathBuf::from(local);
    if !path.is_absolute() {
        return Err(ApiError::invalid_config("The local path must be absolute."));
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ApiError::invalid_config(
            "The local path must not contain . or .. steps.",
        ));
    }
    if roots.is_empty() {
        return Ok(path);
    }
    let real = resolve_existing(&path)?;
    if !inside(roots, &path) || !inside(roots, &real) {
        return Err(ApiError::invalid_config(format!(
            "The local path must be inside one of the folders this server may use: {}.",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(path)
}

/// Folders directly under `path` (no files), or the roots themselves when
/// `path` is absent. The path is resolved to its real location first and
/// must stay inside a root, so the API cannot be used to read the server's
/// file system. Hidden folders are included and flagged.
pub fn list_local(roots: &[PathBuf], path: Option<&str>) -> Result<Vec<LocalEntry>, ApiError> {
    let Some(path) = path else {
        return Ok(roots
            .iter()
            .map(|r| LocalEntry {
                name: r.display().to_string(),
                path: r.display().to_string(),
                hidden: false,
            })
            .collect());
    };
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(ApiError::bad_request("path must be absolute."));
    }
    // Refuse by the path as written first, so a name outside the roots is
    // 403 whether or not it exists; then by where it really leads.
    if !inside(roots, &lexical(path)) {
        return Err(ApiError::forbidden(
            "That folder is outside the folders this server may browse.",
        ));
    }
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found("No such folder."))
        }
        Err(e) => {
            return Err(ApiError::internal(format!(
                "cannot read {}: {e}",
                path.display()
            )))
        }
    };
    if !inside(roots, &real) {
        return Err(ApiError::forbidden(
            "That folder is outside the folders this server may browse.",
        ));
    }
    if !real.is_dir() {
        return Err(ApiError::not_found("No such folder."));
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&real)
        .map_err(|e| ApiError::internal(format!("cannot read {}: {e}", real.display())))?;
    for entry in entries.flatten() {
        let child = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&child) else {
            continue;
        };
        let is_dir = if meta.file_type().is_symlink() {
            // A link is listed only when it leads to a folder inside a root;
            // anything else would be refused when a client follows it.
            std::fs::canonicalize(&child)
                .map(|t| t.is_dir() && inside(roots, &t))
                .unwrap_or(false)
        } else {
            meta.is_dir()
        };
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push(LocalEntry {
            hidden: name.starts_with('.'),
            path: child.display().to_string(),
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The last `n` lines of `path`, read from the end without loading the whole
/// file. A missing file is an empty list.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut pos = file.metadata()?.len();
    let mut buf: Vec<u8> = Vec::new();
    const CHUNK: u64 = 8192;
    // Stop once more than n line ends are in hand: the last n complete lines
    // are then fully inside the buffer, whatever partial line precedes them.
    while pos > 0 && buf.iter().filter(|b| **b == b'\n').count() <= n {
        let read = pos.min(CHUNK);
        pos -= read;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].iter().map(|l| l.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn tail_reads_the_last_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.log");
        assert!(tail_lines(&path, 3).unwrap().is_empty());
        let mut text = String::new();
        for i in 0..5000 {
            text.push_str(&format!("line {i} {}\n", "x".repeat(40)));
        }
        fs::write(&path, &text).unwrap();
        let last = tail_lines(&path, 3).unwrap();
        assert_eq!(last.len(), 3);
        assert!(last[0].starts_with("line 4997 "));
        assert!(last[2].starts_with("line 4999 "));
        assert_eq!(tail_lines(&path, 0).unwrap().len(), 0);
        assert_eq!(tail_lines(&path, 10_000).unwrap().len(), 5000);
        // No trailing newline: the last partial line still counts.
        fs::write(&path, "a\nb\nc").unwrap();
        assert_eq!(tail_lines(&path, 2).unwrap(), vec!["b", "c"]);
    }

    #[test]
    fn local_listing_stays_inside_the_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("x/sub/.hidden")).unwrap();
        fs::create_dir_all(root.join("x/sub/visible")).unwrap();
        fs::write(root.join("x/sub/file.txt"), "f").unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("x/sub/etc-link")).unwrap();
        std::os::unix::fs::symlink(root.join("x/sub/visible"), root.join("x/sub/inner-link"))
            .unwrap();
        let roots = vec![root.join("x")];

        let top = list_local(&roots, None).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].path, root.join("x").display().to_string());

        let names: Vec<(String, bool)> =
            list_local(&roots, Some(root.join("x/sub").to_str().unwrap()))
                .unwrap()
                .into_iter()
                .map(|e| (e.name, e.hidden))
                .collect();
        assert_eq!(
            names,
            vec![
                (".hidden".to_string(), true),
                ("inner-link".to_string(), false),
                ("visible".to_string(), false)
            ]
        );

        let dotdot = format!("{}/x/../outside", root.display());
        assert_eq!(list_local(&roots, Some(&dotdot)).unwrap_err().status, 403);
        let missing = format!("{}/x/../../no-such", root.display());
        assert_eq!(list_local(&roots, Some(&missing)).unwrap_err().status, 403);
        let link = root.join("x/sub/etc-link");
        assert_eq!(
            list_local(&roots, Some(link.to_str().unwrap()))
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            list_local(&roots, Some(root.join("x/none").to_str().unwrap()))
                .unwrap_err()
                .status,
            404
        );
        assert_eq!(
            list_local(&roots, Some("relative")).unwrap_err().status,
            400
        );

        assert!(check_pair_local(&roots, root.join("x/new").to_str().unwrap()).is_ok());
        assert_eq!(
            check_pair_local(&roots, "relative/path")
                .unwrap_err()
                .status,
            422
        );
        assert_eq!(check_pair_local(&roots, &dotdot).unwrap_err().status, 422);
        assert_eq!(
            check_pair_local(&roots, root.join("outside").to_str().unwrap())
                .unwrap_err()
                .status,
            422
        );
        assert_eq!(
            check_pair_local(&roots, link.to_str().unwrap())
                .unwrap_err()
                .status,
            422
        );
        assert!(check_pair_local(&[], "/anywhere/at/all").is_ok());
        // A folder that does not exist yet, below a link that leads outside.
        let through_link = link.join("new/deeper");
        assert_eq!(
            check_pair_local(&roots, through_link.to_str().unwrap())
                .unwrap_err()
                .status,
            422
        );
        // Below a link that stays inside, it is fine.
        let inner = root.join("x/sub/inner-link/new");
        assert!(check_pair_local(&roots, inner.to_str().unwrap()).is_ok());
        // A dangling link as the new folder's name is refused.
        std::os::unix::fs::symlink(root.join("outside/gone"), root.join("x/sub/dangling")).unwrap();
        assert_eq!(
            check_pair_local(&roots, root.join("x/sub/dangling").to_str().unwrap())
                .unwrap_err()
                .status,
            422
        );
    }
}
