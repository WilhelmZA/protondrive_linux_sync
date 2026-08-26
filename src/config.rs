//! Configuration loading and validation (TOML).
//!
//! Search order: `--config PATH`, `$NEUTRONSYNC_CONFIG`,
//! `$XDG_CONFIG_HOME/neutronsync/neutronsync.toml` (`~/.config/...`),
//! then `./neutronsync.toml`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::models::Compare;

pub const DEFAULT_REMOTE_ROOT: &str = "/my-files";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalDelete {
    Trash,
    Remove,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictPolicy {
    KeepBoth,
    Newer,
    Skip,
}

/// Release channel the in-app updater follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpdateChannel {
    /// Only releases you've promoted (unticked "pre-release").
    Stable,
    /// The newest published release, including pre-releases.
    Prerelease,
}

#[derive(Clone, Debug)]
pub struct Pair {
    pub name: String,
    pub local: PathBuf,
    pub remote: String, // absolute Proton path, e.g. /my-files/Documents
    /// Include this pair when "Auto sync" (the watch daemon) is on. Defaults to
    /// true; set false to keep a pair manual-only.
    pub auto: bool,
    /// Sub-paths (relative to the pair root, POSIX-separated) excluded from sync.
    /// Excluded paths are ignored by the engine entirely and never touched on
    /// either side. See [`Pair::is_excluded`].
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub binary: String,
    pub upload_flags: Vec<String>,
    pub download_flags: Vec<String>,
    pub credentials_store: Option<String>,
    pub fresh_cache: bool,
    pub scan_threads: usize,
    /// Concurrent file downloads during a sync (0 = auto). Bandwidth-bound, so
    /// the auto default is modest.
    pub download_threads: usize,
    pub remote_root: String,
    pub propagate_deletes: bool,
    /// Start the watch daemon on launch (the GUI's global "Auto sync").
    pub auto_sync: bool,
    /// Keep running in the system tray when the window is closed.
    pub run_in_tray: bool,
    /// Force the X11 (XWayland) backend so the window can hide/restore/focus
    /// itself in-process (Wayland forbids those). Off = Wayland + tray daemon.
    pub x11_compat: bool,
    pub local_delete: LocalDelete,
    pub conflict: ConflictPolicy,
    pub compare: Compare,
    /// watch mode: seconds between periodic full rescans (the safety net).
    pub poll_interval_secs: u64,
    /// watch mode: seconds between short scans of "hot" (recently active) pairs.
    pub scan_interval_secs: u64,
    /// watch mode: seconds to let a burst of FS events settle before syncing.
    pub debounce_secs: u64,
    /// Which release channel the in-app updater follows.
    pub update_channel: UpdateChannel,
    /// Check for updates when the GUI launches (notify only; never installs).
    pub check_on_launch: bool,
    pub state_dir: PathBuf,
    pub pairs: Vec<Pair>,
    pub source_path: Option<PathBuf>,
}

impl Config {
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }
}

impl Pair {
    /// Whether `rel` (a POSIX path relative to the pair root) must be ignored by
    /// the engine — either a built-in junk pattern (an office/editor lock or temp
    /// file, OS metadata, another sync client's state folder; see
    /// [`crate::ignore`]) or one of this pair's user-configured excluded
    /// sub-paths. Ignored paths are never touched on either side.
    pub fn is_excluded(&self, rel: &str) -> bool {
        crate::ignore::is_ignored_junk(rel)
            || self
                .exclude
                .iter()
                .any(|e| rel == e || rel.starts_with(&format!("{e}/")))
    }
}

/// Normalise a raw exclude list: trim, drop surrounding slashes, POSIX-ify,
/// drop empties and duplicates.
fn normalize_excludes(v: Option<Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in v.unwrap_or_default() {
        let e = e.trim().replace('\\', "/");
        let e = e.trim_matches('/').to_string();
        if !e.is_empty() && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

// --- raw TOML mirror --------------------------------------------------------
#[derive(Deserialize, Default)]
struct RawConfig {
    cli: Option<RawCli>,
    options: Option<RawOptions>,
    pair: Option<Vec<RawPair>>,
}

#[derive(Deserialize, Default)]
struct RawCli {
    binary: Option<String>,
    upload_flags: Option<Vec<String>>,
    download_flags: Option<Vec<String>>,
    credentials_store: Option<String>,
    fresh_cache: Option<bool>,
    scan_threads: Option<u64>,
    download_threads: Option<u64>,
}

#[derive(Deserialize, Default)]
struct RawOptions {
    remote_root: Option<String>,
    propagate_deletes: Option<bool>,
    local_delete: Option<String>,
    conflict: Option<String>,
    compare: Option<String>,
    state_dir: Option<String>,
    poll_interval: Option<u64>,
    scan_interval: Option<u64>,
    debounce: Option<u64>,
    auto_sync: Option<bool>,
    run_in_tray: Option<bool>,
    x11_compat: Option<bool>,
    update_channel: Option<String>,
    check_on_launch: Option<bool>,
}

#[derive(Deserialize)]
struct RawPair {
    name: String,
    local: String,
    remote: String,
    auto: Option<bool>,
    exclude: Option<Vec<String>>,
}

// --- XDG helpers ------------------------------------------------------------
fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn xdg(var: &str, default_rel: &[&str]) -> PathBuf {
    if let Ok(v) = std::env::var(var) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    let mut p = home();
    for part in default_rel {
        p.push(part);
    }
    p
}

pub fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"])
}

fn state_home() -> PathBuf {
    xdg("XDG_STATE_HOME", &[".local", "state"])
}

pub fn default_config_path() -> PathBuf {
    config_home().join("neutronsync").join("neutronsync.toml")
}

/// Expand a leading `~` and `$VAR` / `${VAR}` references.
pub fn expand(s: &str) -> PathBuf {
    let s = if s == "~" {
        home().to_string_lossy().into_owned()
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{}", home().to_string_lossy(), rest)
    } else {
        s.to_string()
    };
    PathBuf::from(expand_vars(&s))
}

fn expand_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let (name, next) = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                let end = s[i + 2..].find('}').map(|e| i + 2 + e);
                match end {
                    Some(e) => (&s[i + 2..e], e + 1),
                    None => (&s[i + 1..i + 1], i + 1),
                }
            } else {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                (&s[i + 1..j], j)
            };
            if name.is_empty() {
                out.push('$');
                i += 1;
            } else {
                out.push_str(&std::env::var(name).unwrap_or_default());
                i = next;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn join_remote(root: &str, remote: &str) -> String {
    let remote = remote.trim();
    if remote.starts_with('/') {
        let t = remote.trim_end_matches('/');
        return if t.is_empty() {
            "/".to_string()
        } else {
            t.to_string()
        };
    }
    let root = root.trim_end_matches('/');
    let remote = remote.trim_matches('/');
    if remote.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{remote}")
    }
}

pub fn find_config(explicit: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(e) = explicit {
        candidates.push(expand(e));
    }
    if let Ok(e) = std::env::var("NEUTRONSYNC_CONFIG") {
        if !e.is_empty() {
            candidates.push(expand(&e));
        }
    }
    candidates.push(default_config_path());
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("neutronsync.toml"));
    }
    candidates.into_iter().find(|c| c.is_file())
}

pub fn load(explicit: Option<&str>) -> Result<Config> {
    let path = find_config(explicit).with_context(|| {
        format!(
            "No config found. Run `neutronsync init` to create one at {}.",
            default_config_path().display()
        )
    })?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let raw: RawConfig =
        toml::from_str(&text).with_context(|| format!("{}: invalid TOML", path.display()))?;

    let cli = raw.cli.unwrap_or_default();
    let opts = raw.options.unwrap_or_default();

    let remote_root = {
        let r = opts
            .remote_root
            .unwrap_or_else(|| DEFAULT_REMOTE_ROOT.to_string());
        let t = r.trim_end_matches('/').to_string();
        if t.is_empty() {
            "/".to_string()
        } else {
            t
        }
    };

    let local_delete = match opts.local_delete.as_deref().unwrap_or("trash") {
        "trash" => LocalDelete::Trash,
        "remove" => LocalDelete::Remove,
        other => bail!("options.local_delete must be trash|remove, got {other:?}"),
    };
    let conflict = match opts.conflict.as_deref().unwrap_or("keep-both") {
        "keep-both" => ConflictPolicy::KeepBoth,
        "newer" => ConflictPolicy::Newer,
        "skip" => ConflictPolicy::Skip,
        other => bail!("options.conflict must be keep-both|newer|skip, got {other:?}"),
    };
    let compare = match opts.compare.as_deref().unwrap_or("size+mtime") {
        "size" => Compare::Size,
        "size+mtime" => Compare::SizeMtime,
        "sha1" => Compare::Sha1,
        other => bail!("options.compare must be size|size+mtime|sha1, got {other:?}"),
    };

    // The credential store is exported into the child CLI's environment, so it
    // must be one of the values the CLI actually supports — never an arbitrary
    // string from the config.
    let credentials_store = match cli.credentials_store.as_deref() {
        None => None,
        Some(s @ ("keychain" | "pass" | "unsafe_file")) => Some(s.to_string()),
        Some(other) => {
            bail!("cli.credentials_store must be keychain|pass|unsafe_file, got {other:?}")
        }
    };

    let update_channel = match opts.update_channel.as_deref().unwrap_or("stable") {
        "stable" => UpdateChannel::Stable,
        "prerelease" => UpdateChannel::Prerelease,
        other => bail!("options.update_channel must be stable|prerelease, got {other:?}"),
    };

    let state_dir = match opts.state_dir {
        Some(s) => expand(&s),
        None => state_home().join("neutronsync"),
    };

    // Zero pairs is allowed: the app can come up clean and folders get added
    // from the GUI. The engine/watcher simply have nothing to do until then.
    let raw_pairs = raw.pair.unwrap_or_default();
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in raw_pairs {
        let name = p.name.trim().to_string();
        if name.is_empty() {
            bail!("a [[pair]] is missing 'name'");
        }
        if !seen.insert(name.clone()) {
            bail!("duplicate pair name: {name:?}");
        }
        pairs.push(Pair {
            local: expand(&p.local),
            remote: join_remote(&remote_root, &p.remote),
            name,
            auto: p.auto.unwrap_or(true),
            exclude: normalize_excludes(p.exclude),
        });
    }

    Ok(Config {
        binary: cli.binary.unwrap_or_else(|| "proton-drive".to_string()),
        upload_flags: migrate_transfer_flags(
            cli.upload_flags.unwrap_or_else(default_upload_flags),
            TransferKind::Upload,
        ),
        download_flags: migrate_transfer_flags(
            cli.download_flags.unwrap_or_else(default_download_flags),
            TransferKind::Download,
        ),
        credentials_store,
        fresh_cache: cli.fresh_cache.unwrap_or(true),
        scan_threads: cli.scan_threads.map(|n| n as usize).unwrap_or(0),
        download_threads: cli.download_threads.map(|n| n as usize).unwrap_or(0),
        remote_root,
        propagate_deletes: opts.propagate_deletes.unwrap_or(false),
        auto_sync: opts.auto_sync.unwrap_or(false),
        run_in_tray: opts.run_in_tray.unwrap_or(false),
        x11_compat: opts.x11_compat.unwrap_or(false),
        local_delete,
        conflict,
        compare,
        poll_interval_secs: opts.poll_interval.unwrap_or(900),
        scan_interval_secs: opts.scan_interval.unwrap_or(120),
        debounce_secs: opts.debounce.unwrap_or(2),
        update_channel,
        check_on_launch: opts.check_on_launch.unwrap_or(false),
        state_dir,
        pairs,
        source_path: Some(path),
    })
}

/// Defaults for cli-drive ≥ 0.8.0: separate file/folder strategies. Upload
/// `replace` trashes the remote and writes the local copy; download uses
/// `remove` (the 0.8 name for the same overwrite behaviour — `replace` is gone).
fn default_upload_flags() -> Vec<String> {
    vec![
        "--file-conflict-strategy".into(),
        "replace".into(),
        "--folder-conflict-strategy".into(),
        "replace".into(),
    ]
}

fn default_download_flags() -> Vec<String> {
    vec![
        "--file-conflict-strategy".into(),
        "remove".into(),
        "--folder-conflict-strategy".into(),
        "remove".into(),
    ]
}

enum TransferKind {
    Upload,
    Download,
}

/// Rewrite pre-0.8 `--conflict-strategy` / `-c` flags into the split
/// `--file-conflict-strategy` / `--folder-conflict-strategy` form. Configs that
/// already use the new flags (or omit the legacy ones) are left alone.
fn migrate_transfer_flags(flags: Vec<String>, kind: TransferKind) -> Vec<String> {
    let mut out = Vec::with_capacity(flags.len().max(4));
    let mut legacy: Option<String> = None;
    let mut i = 0;
    while i < flags.len() {
        let f = &flags[i];
        if f == "--conflict-strategy" || f == "-c" {
            if let Some(v) = flags.get(i + 1) {
                legacy = Some(v.clone());
                i += 2;
                continue;
            }
        }
        out.push(f.clone());
        i += 1;
    }
    let Some(old) = legacy else {
        return if out.is_empty() {
            match kind {
                TransferKind::Upload => default_upload_flags(),
                TransferKind::Download => default_download_flags(),
            }
        } else {
            out
        };
    };
    // Already has explicit -f/-d: drop the obsolete -c only.
    let has_split = out.iter().any(|f| {
        f == "--file-conflict-strategy"
            || f == "-f"
            || f == "--folder-conflict-strategy"
            || f == "-d"
    });
    if has_split {
        return out;
    }
    let (file, folder) = match (&kind, old.as_str()) {
        (TransferKind::Upload, "replace") => ("replace", "replace"),
        (TransferKind::Upload, "keep-both") => ("rename", "rename"),
        (TransferKind::Upload, "skip") => ("skip", "skip"),
        (TransferKind::Upload, "merge") => ("create-new-revision", "merge"),
        (TransferKind::Download, "replace") => ("remove", "remove"),
        (TransferKind::Download, "keep-both") => ("rename", "rename"),
        (TransferKind::Download, "skip") => ("skip", "skip"),
        (TransferKind::Download, "merge") => ("rename", "merge"),
        (TransferKind::Upload, _) => ("replace", "replace"),
        (TransferKind::Download, _) => ("remove", "remove"),
    };
    out.extend([
        "--file-conflict-strategy".into(),
        file.into(),
        "--folder-conflict-strategy".into(),
        folder.into(),
    ]);
    out
}

/// Path helper used by the engine to build absolute remote paths.
pub fn remote_join(base: &str, rel: &str) -> String {
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        base.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), rel)
    }
}

/// True when `p` lives under `root` (for computing a pair's base sub-path).
pub fn strip_root<'a>(root: &str, p: &'a str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    let prefix = format!("{root}/");
    p.strip_prefix(&prefix)
}

// --- writing (used by the GUI) ---------------------------------------------
#[derive(Serialize)]
struct OutCli {
    binary: String,
    upload_flags: Vec<String>,
    download_flags: Vec<String>,
    fresh_cache: bool,
    scan_threads: usize,
    download_threads: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials_store: Option<String>,
}

#[derive(Serialize)]
struct OutOptions {
    remote_root: String,
    propagate_deletes: bool,
    auto_sync: bool,
    run_in_tray: bool,
    x11_compat: bool,
    local_delete: String,
    conflict: String,
    compare: String,
    poll_interval: u64,
    scan_interval: u64,
    debounce: u64,
    update_channel: String,
    check_on_launch: bool,
    state_dir: String,
}

#[derive(Serialize)]
struct OutPair {
    name: String,
    local: String,
    remote: String,
    auto: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
}

#[derive(Serialize)]
struct OutConfig {
    cli: OutCli,
    options: OutOptions,
    pair: Vec<OutPair>,
}

fn local_delete_str(v: LocalDelete) -> &'static str {
    match v {
        LocalDelete::Trash => "trash",
        LocalDelete::Remove => "remove",
    }
}

fn conflict_str(v: ConflictPolicy) -> &'static str {
    match v {
        ConflictPolicy::KeepBoth => "keep-both",
        ConflictPolicy::Newer => "newer",
        ConflictPolicy::Skip => "skip",
    }
}

fn compare_str(v: Compare) -> &'static str {
    match v {
        Compare::Size => "size",
        Compare::SizeMtime => "size+mtime",
        Compare::Sha1 => "sha1",
    }
}

fn update_channel_str(v: UpdateChannel) -> &'static str {
    match v {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Prerelease => "prerelease",
    }
}

/// Serialise a Config back to TOML text.
pub fn to_toml(cfg: &Config) -> Result<String> {
    let out = OutConfig {
        cli: OutCli {
            binary: cfg.binary.clone(),
            upload_flags: cfg.upload_flags.clone(),
            download_flags: cfg.download_flags.clone(),
            fresh_cache: cfg.fresh_cache,
            scan_threads: cfg.scan_threads,
            download_threads: cfg.download_threads,
            credentials_store: cfg.credentials_store.clone(),
        },
        options: OutOptions {
            remote_root: cfg.remote_root.clone(),
            propagate_deletes: cfg.propagate_deletes,
            auto_sync: cfg.auto_sync,
            run_in_tray: cfg.run_in_tray,
            x11_compat: cfg.x11_compat,
            local_delete: local_delete_str(cfg.local_delete).to_string(),
            conflict: conflict_str(cfg.conflict).to_string(),
            compare: compare_str(cfg.compare).to_string(),
            poll_interval: cfg.poll_interval_secs,
            scan_interval: cfg.scan_interval_secs,
            debounce: cfg.debounce_secs,
            update_channel: update_channel_str(cfg.update_channel).to_string(),
            check_on_launch: cfg.check_on_launch,
            state_dir: cfg.state_dir.to_string_lossy().into_owned(),
        },
        pair: cfg
            .pairs
            .iter()
            .map(|p| OutPair {
                name: p.name.clone(),
                local: p.local.to_string_lossy().into_owned(),
                // Store the sub-path under the root when possible; keeps configs tidy.
                remote: strip_root(&cfg.remote_root, &p.remote)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| p.remote.clone()),
                auto: p.auto,
                exclude: p.exclude.clone(),
            })
            .collect(),
    };
    toml::to_string(&out).context("serialising config to TOML")
}

/// Write a Config to `path` (creating parent dirs).
pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_toml(cfg)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{migrate_transfer_flags, TransferKind};

    #[test]
    fn migrates_legacy_conflict_strategy_upload() {
        let got = migrate_transfer_flags(
            vec!["--conflict-strategy".into(), "replace".into()],
            TransferKind::Upload,
        );
        assert_eq!(
            got,
            vec![
                "--file-conflict-strategy",
                "replace",
                "--folder-conflict-strategy",
                "replace",
            ]
        );
    }

    #[test]
    fn migrates_legacy_conflict_strategy_download() {
        let got = migrate_transfer_flags(
            vec!["-c".into(), "replace".into()],
            TransferKind::Download,
        );
        assert_eq!(
            got,
            vec![
                "--file-conflict-strategy",
                "remove",
                "--folder-conflict-strategy",
                "remove",
            ]
        );
    }

    #[test]
    fn leaves_split_flags_alone() {
        let flags = vec![
            "--file-conflict-strategy".into(),
            "replace".into(),
            "--folder-conflict-strategy".into(),
            "merge".into(),
        ];
        assert_eq!(
            migrate_transfer_flags(flags.clone(), TransferKind::Upload),
            flags
        );
    }
}
