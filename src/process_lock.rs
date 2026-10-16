//! PID records tied to a process start time and boot, not just a reusable PID.

use std::path::Path;

fn identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces and parentheses. Field 22 follows its final ')'.
    let start = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{pid} {start} {}", boot.trim()))
}

pub fn write(path: &Path) -> std::io::Result<()> {
    let record = identity(std::process::id())
        .ok_or_else(|| std::io::Error::other("cannot read process identity"))?;
    std::fs::write(path, record)
}

pub fn is_alive(path: &Path) -> bool {
    let Ok(record) = std::fs::read_to_string(path) else {
        return false;
    };
    let fields: Vec<_> = record.split_whitespace().collect();
    let Some(pid) = fields.first().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    match fields.len() {
        3 => identity(pid).is_some_and(|current| current == fields.join(" ")),
        // Upgrade compatibility: never mistake an unrelated process for an old daemon.
        1 => std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|exe| {
                exe.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .is_some_and(|name| {
                matches!(
                    name.trim_end_matches(" (deleted)"),
                    "neutronsync" | "neutronsync-gui"
                )
            }),
        _ => false,
    }
}

pub fn remove_owned(path: &Path) {
    if let (Ok(record), Some(current)) =
        (std::fs::read_to_string(path), identity(std::process::id()))
    {
        if record.trim() == current {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_identity_prevents_stale_pid_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        assert!(!is_alive(&path));
        write(&path).unwrap();
        assert!(is_alive(&path));
        let record = std::fs::read_to_string(&path).unwrap();
        let fields: Vec<_> = record.split_whitespace().collect();
        std::fs::write(&path, format!("{} 0 {}", fields[0], fields[2])).unwrap();
        assert!(!is_alive(&path));
        remove_owned(&path);
        assert!(path.exists());
        std::fs::write(&path, format!("{} {} other-boot", fields[0], fields[1])).unwrap();
        assert!(!is_alive(&path));
        // A live non-NeutronSync process must not satisfy a legacy PID lock.
        std::fs::write(&path, std::process::id().to_string()).unwrap();
        assert!(!is_alive(&path));
        std::fs::write(&path, "invalid").unwrap();
        assert!(!is_alive(&path));
        write(&path).unwrap();
        remove_owned(&path);
        assert!(!path.exists());
    }
}
