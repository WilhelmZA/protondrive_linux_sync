//! Minimal logger: console (level-gated), an optional file log, and an optional
//! channel sink (used by the GUI to stream lines into its log pane).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Rotate `sync.log` once it passes this size, keeping one previous generation
/// (`sync.log.1`). The log records every operation, so an unrotated file grows
/// without bound: a long-running daemon reached 19 MB in five days.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// The open log file plus a running size, so rotation needs no `stat` per line.
struct LogFile {
    fh: File,
    path: PathBuf,
    size: u64,
}

#[derive(Clone)]
pub struct Logger {
    verbose: bool,
    quiet: bool,
    file: Option<Arc<Mutex<LogFile>>>,
    tx: Option<Sender<String>>,
    console: bool,
}

impl Logger {
    /// Console output without creating a state directory or log file.
    pub fn console(verbose: bool, quiet: bool) -> Self {
        Self {
            verbose,
            quiet,
            file: None,
            tx: None,
            console: true,
        }
    }

    /// Preserve console/channel reporting but suppress disk writes for planning.
    pub fn without_file(&self) -> Self {
        Self {
            file: None,
            ..self.clone()
        }
    }

    pub fn new(log_dir: &Path, verbose: bool, quiet: bool) -> Self {
        // Logs record decrypted file paths, so keep them private to the user:
        // the directory is created 0700 and the log file 0600 (modes apply when
        // this process creates them).
        let path = log_dir.join("sync.log");
        let file = DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(log_dir)
            .ok()
            .and_then(|_| open_log(&path))
            .map(|f| Arc::new(Mutex::new(f)));
        Logger {
            verbose,
            quiet,
            file,
            tx: None,
            console: true,
        }
    }

    /// A logger that writes nowhere (used in tests).
    pub fn silent() -> Self {
        Logger {
            verbose: false,
            quiet: true,
            file: None,
            tx: None,
            console: false,
        }
    }

    /// A logger that streams lines to a channel (used by the GUI). No console.
    pub fn channel(tx: Sender<String>, verbose: bool) -> Self {
        Logger {
            verbose,
            quiet: false,
            file: None,
            tx: Some(tx),
            console: false,
        }
    }

    fn to_file(&self, level: &str, msg: &str) {
        let Some(f) = &self.file else { return };
        let Ok(mut lf) = f.lock() else { return };
        let line = format!(
            "{} {level:<7} {msg}\n",
            crate::datefmt::epoch_to_log_stamp(crate::datefmt::now_epoch())
        );
        if lf.fh.write_all(line.as_bytes()).is_ok() {
            lf.size += line.len() as u64;
        }
        if lf.size >= MAX_LOG_BYTES {
            rotate(&mut lf);
        }
    }

    fn sink(&self, msg: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg.to_string());
        }
    }

    pub fn info(&self, msg: &str) {
        if self.console && !self.quiet {
            println!("{msg}");
        }
        self.to_file("INFO", msg);
        self.sink(msg);
    }

    pub fn debug(&self, msg: &str) {
        if self.console && self.verbose {
            println!("{msg}");
        }
        self.to_file("DEBUG", msg);
        if self.verbose {
            self.sink(msg);
        }
    }

    pub fn warn(&self, msg: &str) {
        if self.console {
            eprintln!("{msg}");
        }
        self.to_file("WARNING", msg);
        self.sink(msg);
    }

    pub fn error(&self, msg: &str) {
        if self.console {
            eprintln!("{msg}");
        }
        self.to_file("ERROR", msg);
        self.sink(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "neutronsync-log-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn rotates_at_the_size_cap_and_keeps_one_generation() {
        let dir = tmpdir("rotate");
        let log = Logger::new(&dir, false, true);
        let path = dir.join("sync.log");
        let prev = dir.join("sync.log.1");

        // One line is far under the cap, so nothing rotates yet.
        log.info("first line");
        assert!(path.exists());
        assert!(!prev.exists());

        // Push past the cap. Each line is ~1 KB, so this crosses 8 MiB.
        let filler = "x".repeat(1000);
        let lines = (MAX_LOG_BYTES / 1000) + 8;
        for _ in 0..lines {
            log.info(&filler);
        }
        assert!(
            prev.exists(),
            "the old generation should be kept as sync.log.1"
        );
        assert!(
            std::fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES,
            "the live log should have been truncated by the rollover"
        );
        assert!(
            std::fs::metadata(&prev).unwrap().len() >= MAX_LOG_BYTES,
            "the rolled-over file should hold the bulk of the writes"
        );

        // Both generations stay private: they contain decrypted file paths.
        use std::os::unix::fs::PermissionsExt;
        for p in [&path, &prev] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} should be 0600, was {mode:o}", p.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_carry_a_readable_timestamp_and_level() {
        let dir = tmpdir("stamp");
        let log = Logger::new(&dir, false, true);
        log.error("upload x: No paths matched");
        let body = std::fs::read_to_string(dir.join("sync.log")).unwrap();
        let line = body.lines().next().unwrap();
        // "2026-07-28 07:43:17Z ERROR   upload x: ..." - not a raw epoch.
        assert!(line.contains(" ERROR   "), "{line}");
        assert!(line.contains("upload x: No paths matched"), "{line}");
        let stamp = &line[..20];
        assert!(
            stamp.starts_with("20") && stamp.contains('-') && stamp.contains(':'),
            "expected a readable date, got {stamp:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Open (creating if needed) the log file 0600, recording its current size so
/// rotation can be decided without stat-ing on every line.
fn open_log(path: &Path) -> Option<LogFile> {
    let fh = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .ok()?;
    let size = fh.metadata().map(|m| m.len()).unwrap_or(0);
    Some(LogFile {
        fh,
        path: path.to_path_buf(),
        size,
    })
}

/// Move the current log aside to `<name>.1` and start a fresh one. One previous
/// generation is kept, so the logs cost at most twice [`MAX_LOG_BYTES`]. A
/// failure to rotate is not fatal: keep writing to the file already open.
fn rotate(lf: &mut LogFile) {
    let mut prev = lf.path.clone().into_os_string();
    prev.push(".1");
    let prev = PathBuf::from(prev);
    let _ = std::fs::remove_file(&prev);
    if std::fs::rename(&lf.path, &prev).is_err() {
        // Couldn't roll it over: reset the counter so we don't spin on this
        // check for every subsequent line.
        lf.size = 0;
        return;
    }
    if let Some(fresh) = open_log(&lf.path) {
        *lf = fresh;
    } else {
        lf.size = 0;
    }
}
