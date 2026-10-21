//! Permissions for files NeutronSync creates for the user (downloads, conflict copies).

#[cfg(unix)]
fn read_umask() -> u32 {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Umask:") {
            return u32::from_str_radix(rest.trim(), 8).unwrap_or(0o022);
        }
    }
    0o022
}

#[cfg(unix)]
pub fn process_umask() -> u32 {
    read_umask()
}

/// Mode for a synced file the user should be able to read: `0666` masked by the process umask.
#[cfg(unix)]
pub fn sync_file_mode() -> u32 {
    0o666 & !read_umask()
}

#[cfg(unix)]
pub fn set_sync_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.is_file() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(sync_file_mode()));
    }
}

#[cfg(not(unix))]
pub fn process_umask() -> u32 {
    0o022
}

#[cfg(not(unix))]
pub fn sync_file_mode() -> u32 {
    0o666 & !0o022
}

#[cfg(not(unix))]
pub fn set_sync_file_permissions(_path: &std::path::Path) {}

#[cfg(unix)]
pub fn set_private_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn set_private_file_permissions(_path: &std::path::Path) {}
