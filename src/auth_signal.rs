//! Cross-process nudge when the GUI signs in to Proton so the tray daemon's
//! watcher re-probes auth immediately instead of waiting for its 20s throttle.

use std::path::Path;

const FILE: &str = "auth-refresh";

/// Tell the watch daemon to re-check the proton-drive session now.
pub fn signal(state_dir: &Path) {
    let _ = std::fs::write(state_dir.join(FILE), b"1");
}

/// True once if a refresh was requested since the last call.
pub fn take(state_dir: &Path) -> bool {
    let path = state_dir.join(FILE);
    if path.is_file() {
        let _ = std::fs::remove_file(&path);
        true
    } else {
        false
    }
}
