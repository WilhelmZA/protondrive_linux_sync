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

/// Highest allowed value for `scan_threads` and `download_threads` (0 means auto).
pub const MAX_SYNC_THREADS: usize = 64;

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

/// Which colours the window uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    /// Follow the desktop's light or dark preference; dark when it reports none.
    #[default]
    System,
    Light,
    Dark,
}

/// Where the sidecar keeps the Proton session.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SessionStore {
    /// The desktop's Secret Service keyring (the default).
    #[default]
    Keyring,
    /// An encrypted file under `state_dir`, for machines without a keyring
    /// (a container or a headless server). See `session_key_file`.
    File,
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
    pub sidecar: Option<PathBuf>,
    /// Where the sidecar keeps the Proton session: the keyring or an
    /// encrypted file.
    pub session_store: SessionStore,
    /// File holding the 32-byte key for the session file (base64 or hex).
    /// Only read when `session_store` is `File`.
    pub session_key_file: Option<PathBuf>,
    /// TCP address (`host:port`) the desktop tray also binds for `/api/v1`.
    /// Absent means the tray serves only on its unix socket. `neutronsync serve`
    /// uses `--listen` and does not read this.
    pub api_listen: Option<String>,
    /// API token file for the tray's TCP listener when `NEUTRONSYNC_API_TOKEN`
    /// is unset. `neutronsync serve` uses `--token-file` or the environment.
    pub api_token_file: Option<PathBuf>,
    /// Concurrent folder reconciles during a full walk (0 = auto).
    pub scan_threads: usize,
    /// Concurrent file downloads during a sync (0 = auto). Bandwidth-bound, so
    /// the auto default is modest.
    pub download_threads: usize,
    pub remote_root: String,
    pub propagate_deletes: bool,
    /// Most files one run may delete from one pair (0 = no fixed cap). See
    /// [`crate::engine::delete_allowance`].
    pub max_delete_files: usize,
    /// Most files one run may delete from one pair, as a share of the files it
    /// tracks (0 = no share-based limit). Both at 0 turns the guard off.
    pub max_delete_percent: u32,
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
    /// watch mode: seconds between safety-net full walks.
    pub full_walk_interval: u64,
    /// watch mode: seconds between short scans of "hot" (recently active) pairs.
    pub scan_interval_secs: u64,
    /// watch mode: seconds to let a burst of FS events settle before syncing.
    pub debounce_secs: u64,
    /// Which release channel the in-app updater follows.
    pub update_channel: UpdateChannel,
    /// Check for updates when the GUI launches (notify only; never installs).
    pub check_on_launch: bool,
    /// Upload speed limit in KiB/s for the whole machine (0 = no limit).
    pub upload_limit_kbps: u64,
    /// Download speed limit in KiB/s for the whole machine (0 = no limit).
    pub download_limit_kbps: u64,
    /// Send desktop notifications for conflicts, lasting problems and a sign-out.
    pub notifications: bool,
    /// Light, dark, or follow the desktop.
    pub theme: Theme,
    /// Keep one file manager sidebar bookmark per synced folder.
    pub file_manager_bookmarks: bool,
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
    /// Only ever "api" now. The removed CLI backend is refused with a clear
    /// message rather than silently switched.
    backend: Option<String>,
    sidecar: Option<String>,
    session_store: Option<String>,
    session_key_file: Option<String>,
    api_listen: Option<String>,
    api_token_file: Option<String>,
    scan_threads: Option<u64>,
    download_threads: Option<u64>,
}

#[derive(Deserialize, Default)]
struct RawOptions {
    remote_root: Option<String>,
    propagate_deletes: Option<bool>,
    max_delete_files: Option<u64>,
    max_delete_percent: Option<u64>,
    local_delete: Option<String>,
    conflict: Option<String>,
    compare: Option<String>,
    state_dir: Option<String>,
    full_walk_interval: Option<u64>,
    scan_interval: Option<u64>,
    debounce: Option<u64>,
    auto_sync: Option<bool>,
    run_in_tray: Option<bool>,
    x11_compat: Option<bool>,
    update_channel: Option<String>,
    check_on_launch: Option<bool>,
    upload_limit_kbps: Option<u64>,
    download_limit_kbps: Option<u64>,
    notifications: Option<bool>,
    theme: Option<String>,
    file_manager_bookmarks: Option<bool>,
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

/// A Proton folder path must be absolute and every segment must be a real name.
pub fn validate_proton_path(field: &str, path: &str) -> Result<()> {
    if !path.starts_with('/') {
        bail!("{field} must start with /.");
    }
    let body = path.trim_end_matches('/');
    if body.len() > 1 {
        for seg in body[1..].split('/') {
            if seg.is_empty() {
                bail!("{field} must not contain empty path steps.");
            }
            if seg == "." || seg == ".." {
                bail!("{field} must not contain . or .. steps.");
            }
        }
    }
    Ok(())
}

fn normalize_remote_path(path: &str) -> String {
    let parts: Vec<&str> = path
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn normalize_local_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        if let Ok(c) = std::fs::canonicalize(path) {
            return Ok(c);
        }
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::Prefix(p) => out.push(p.as_os_str()),
            std::path::Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("local folder paths must not contain .. steps");
            }
        }
    }
    Ok(out)
}

fn remote_paths_overlap(a: &str, b: &str) -> bool {
    let a = normalize_remote_path(a);
    let b = normalize_remote_path(b);
    if a == "/" || b == "/" {
        return true;
    }
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn local_paths_overlap(a: &Path, b: &Path) -> Result<bool> {
    let a = normalize_local_path(a)?;
    let b = normalize_local_path(b)?;
    Ok(a == b || a.starts_with(&b) || b.starts_with(&a))
}

/// Rules shared by the config file, the HTTP API and the desktop window.
pub fn validate_config(cfg: &Config) -> Result<()> {
    if cfg.scan_threads > MAX_SYNC_THREADS {
        bail!(
            "scan_threads must be 0 (automatic) to {MAX_SYNC_THREADS}, got {0}.",
            cfg.scan_threads
        );
    }
    if cfg.download_threads > MAX_SYNC_THREADS {
        bail!(
            "download_threads must be 0 (automatic) to {MAX_SYNC_THREADS}, got {0}.",
            cfg.download_threads
        );
    }
    validate_proton_path("remote_root", &cfg.remote_root)?;
    for p in &cfg.pairs {
        validate_proton_path("remote folder", &p.remote)?;
        for other in &cfg.pairs {
            if other.name == p.name {
                continue;
            }
            if local_paths_overlap(&p.local, &other.local)? {
                bail!(
                    "{} already syncs {}; one folder cannot be in two pairs.",
                    other.name,
                    other.local.display()
                );
            }
            if remote_paths_overlap(&p.remote, &other.remote) {
                bail!(
                    "{} already syncs {}; one folder cannot be in two pairs.",
                    other.name,
                    other.remote
                );
            }
        }
    }
    Ok(())
}

/// Check a new or updated pair against the rest of the config.
pub fn validate_pair(cfg: &Config, pair: &Pair, replacing: Option<&str>) -> Result<()> {
    validate_proton_path("remote folder", &pair.remote)?;
    for other in &cfg.pairs {
        if replacing == Some(other.name.as_str()) || other.name == pair.name {
            continue;
        }
        if local_paths_overlap(&pair.local, &other.local)? {
            bail!(
                "{} already syncs {}; one folder cannot be in two pairs.",
                other.name,
                other.local.display()
            );
        }
        if remote_paths_overlap(&pair.remote, &other.remote) {
            bail!(
                "{} already syncs {}; one folder cannot be in two pairs.",
                other.name,
                other.remote
            );
        }
    }
    Ok(())
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
    match cli.backend.as_deref().unwrap_or("api") {
        "api" => {}
        "cli" => bail!(
            "cli.backend = \"cli\": the proton-drive backend was removed in 0.6.0. \
             Remove `backend` from [cli]; NeutronSync syncs through neutronsync-drive."
        ),
        other => bail!("cli.backend must be api, got {other:?}"),
    }

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

    let update_channel = match opts.update_channel.as_deref().unwrap_or("stable") {
        "stable" => UpdateChannel::Stable,
        "prerelease" => UpdateChannel::Prerelease,
        other => bail!("options.update_channel must be stable|prerelease, got {other:?}"),
    };

    let state_dir = match opts.state_dir {
        Some(s) => expand(&s),
        None => state_home().join("neutronsync"),
    };

    let session_store = match cli.session_store.as_deref().unwrap_or("keyring") {
        "keyring" => SessionStore::Keyring,
        "file" => SessionStore::File,
        other => bail!("cli.session_store must be keyring|file, got {other:?}"),
    };

    let max_delete_percent = opts.max_delete_percent.unwrap_or(30);
    if max_delete_percent > 100 {
        bail!("options.max_delete_percent must be 0..=100, got {max_delete_percent}");
    }

    let upload_limit_kbps = speed_limit("upload_limit_kbps", opts.upload_limit_kbps)?;
    let download_limit_kbps = speed_limit("download_limit_kbps", opts.download_limit_kbps)?;

    let theme = match opts.theme.as_deref().unwrap_or("system") {
        "system" => Theme::System,
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        other => bail!("options.theme must be system|light|dark, got {other:?}"),
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

    let scan_threads = cli.scan_threads.map(|n| n as usize).unwrap_or(0);
    let download_threads = cli.download_threads.map(|n| n as usize).unwrap_or(0);
    if scan_threads > MAX_SYNC_THREADS {
        bail!("scan_threads must be 0 (automatic) to {MAX_SYNC_THREADS}, got {scan_threads}");
    }
    if download_threads > MAX_SYNC_THREADS {
        bail!(
            "download_threads must be 0 (automatic) to {MAX_SYNC_THREADS}, got {download_threads}"
        );
    }
    validate_proton_path("remote_root", &remote_root)?;

    let config = Config {
        sidecar: cli.sidecar.map(|p| expand(&p)),
        session_store,
        session_key_file: cli.session_key_file.map(|p| expand(&p)),
        api_listen: cli.api_listen.and_then(|s| {
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        }),
        api_token_file: cli.api_token_file.map(|p| expand(&p)),
        scan_threads,
        download_threads,
        remote_root,
        propagate_deletes: opts.propagate_deletes.unwrap_or(false),
        max_delete_files: opts.max_delete_files.unwrap_or(200) as usize,
        max_delete_percent: max_delete_percent as u32,
        auto_sync: opts.auto_sync.unwrap_or(false),
        run_in_tray: opts.run_in_tray.unwrap_or(false),
        x11_compat: opts.x11_compat.unwrap_or(false),
        local_delete,
        conflict,
        compare,
        full_walk_interval: opts.full_walk_interval.unwrap_or(86400),
        scan_interval_secs: opts.scan_interval.unwrap_or(120),
        debounce_secs: opts.debounce.unwrap_or(2),
        update_channel,
        check_on_launch: opts.check_on_launch.unwrap_or(false),
        upload_limit_kbps,
        download_limit_kbps,
        notifications: opts.notifications.unwrap_or(true),
        theme,
        file_manager_bookmarks: opts.file_manager_bookmarks.unwrap_or(false),
        state_dir,
        pairs,
        source_path: Some(path),
    };
    validate_config(&config)?;
    Ok(config)
}

/// Highest speed limit accepted, in KiB/s: 1 GiB/s. Anything above it is a typo.
pub const MAX_LIMIT_KBPS: u64 = 1_048_576;

fn speed_limit(key: &str, value: Option<u64>) -> Result<u64> {
    let value = value.unwrap_or(0);
    if value > MAX_LIMIT_KBPS {
        bail!("options.{key} must be 0..={MAX_LIMIT_KBPS} (KB/s, 0 = no limit), got {value}");
    }
    Ok(value)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    sidecar: Option<PathBuf>,
    /// Written only when it is not the default, so a desktop config that never
    /// chose a store does not gain the key on its next save.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_store: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_key_file: Option<PathBuf>,
    /// Written only when set, so a desktop config does not gain a TCP listener
    /// on the next save.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_token_file: Option<PathBuf>,
    scan_threads: usize,
    download_threads: usize,
}

#[derive(Serialize)]
struct OutOptions {
    remote_root: String,
    propagate_deletes: bool,
    max_delete_files: usize,
    max_delete_percent: u32,
    auto_sync: bool,
    run_in_tray: bool,
    x11_compat: bool,
    local_delete: String,
    conflict: String,
    compare: String,
    full_walk_interval: u64,
    scan_interval: u64,
    debounce: u64,
    update_channel: String,
    check_on_launch: bool,
    upload_limit_kbps: u64,
    download_limit_kbps: u64,
    notifications: bool,
    theme: String,
    file_manager_bookmarks: bool,
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

/// The config spelling of a local delete choice.
pub fn local_delete_str(v: LocalDelete) -> &'static str {
    match v {
        LocalDelete::Trash => "trash",
        LocalDelete::Remove => "remove",
    }
}

/// The config spelling of a conflict policy.
pub fn conflict_str(v: ConflictPolicy) -> &'static str {
    match v {
        ConflictPolicy::KeepBoth => "keep-both",
        ConflictPolicy::Newer => "newer",
        ConflictPolicy::Skip => "skip",
    }
}

/// The config spelling of a compare mode.
pub fn compare_str(v: Compare) -> &'static str {
    match v {
        Compare::Size => "size",
        Compare::SizeMtime => "size+mtime",
        Compare::Sha1 => "sha1",
    }
}

/// The config spelling of an update channel.
pub fn update_channel_str(v: UpdateChannel) -> &'static str {
    match v {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Prerelease => "prerelease",
    }
}

/// The config spelling of a session store choice.
pub fn session_store_str(v: SessionStore) -> &'static str {
    match v {
        SessionStore::Keyring => "keyring",
        SessionStore::File => "file",
    }
}

/// The config spelling of a theme choice.
pub fn theme_str(v: Theme) -> &'static str {
    match v {
        Theme::System => "system",
        Theme::Light => "light",
        Theme::Dark => "dark",
    }
}

/// Serialise a Config back to TOML text.
pub fn to_toml(cfg: &Config) -> Result<String> {
    let out = OutConfig {
        cli: OutCli {
            sidecar: cfg.sidecar.clone(),
            session_store: (cfg.session_store != SessionStore::Keyring)
                .then(|| session_store_str(cfg.session_store)),
            session_key_file: cfg.session_key_file.clone(),
            api_listen: cfg.api_listen.clone(),
            api_token_file: cfg.api_token_file.clone(),
            scan_threads: cfg.scan_threads,
            download_threads: cfg.download_threads,
        },
        options: OutOptions {
            remote_root: cfg.remote_root.clone(),
            propagate_deletes: cfg.propagate_deletes,
            max_delete_files: cfg.max_delete_files,
            max_delete_percent: cfg.max_delete_percent,
            auto_sync: cfg.auto_sync,
            run_in_tray: cfg.run_in_tray,
            x11_compat: cfg.x11_compat,
            local_delete: local_delete_str(cfg.local_delete).to_string(),
            conflict: conflict_str(cfg.conflict).to_string(),
            compare: compare_str(cfg.compare).to_string(),
            full_walk_interval: cfg.full_walk_interval,
            scan_interval: cfg.scan_interval_secs,
            debounce: cfg.debounce_secs,
            update_channel: update_channel_str(cfg.update_channel).to_string(),
            check_on_launch: cfg.check_on_launch,
            upload_limit_kbps: cfg.upload_limit_kbps,
            download_limit_kbps: cfg.download_limit_kbps,
            notifications: cfg.notifications,
            theme: theme_str(cfg.theme).to_string(),
            file_manager_bookmarks: cfg.file_manager_bookmarks,
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
    use super::{load, save, SessionStore, Theme};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn session_store_defaults_round_trips_and_refuses_unknown_values() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        let base = r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
"#;
        fs::write(&path, base).unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert_eq!(cfg.session_store, SessionStore::Keyring);
        assert!(cfg.session_key_file.is_none());
        // The default is not written back, so a desktop config stays as it was.
        save(&cfg, &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("session_store"), "{text}");
        assert!(!text.contains("session_key_file"), "{text}");

        cfg.session_store = SessionStore::File;
        cfg.session_key_file = Some("/tmp/ns-test-state/session.key".into());
        save(&cfg, &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("session_store = \"file\""), "{text}");
        assert!(
            text.contains("session_key_file = \"/tmp/ns-test-state/session.key\""),
            "{text}"
        );
        let loaded = load(path.to_str()).unwrap();
        assert_eq!(loaded.session_store, SessionStore::File);
        assert_eq!(
            loaded.session_key_file.as_deref(),
            Some(std::path::Path::new("/tmp/ns-test-state/session.key"))
        );

        fs::write(
            &path,
            "[cli]\nsession_store = \"vault\"\n[options]\nstate_dir = \"/tmp/ns-test-state\"\n",
        )
        .unwrap();
        let err = load(path.to_str()).unwrap_err().to_string();
        assert!(err.contains("cli.session_store"), "{err}");
        assert!(err.contains("keyring|file"), "{err}");

        for example in [
            crate::EXAMPLE_CONFIG,
            include_str!("../neutronsync.example.toml"),
        ] {
            assert!(example.contains("# session_store = \"keyring\""));
            assert!(example.contains("# session_key_file ="));
            fs::write(&path, example).unwrap();
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(loaded.session_store, SessionStore::Keyring);
            assert!(loaded.session_key_file.is_none());
        }
    }

    #[test]
    fn full_walk_interval_round_trip_gui_values() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        fs::write(
            &path,
            r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
[[pair]]
name = "x"
local = "/tmp/ns-test-local"
remote = "Documents"
"#,
        )
        .unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert_eq!(cfg.full_walk_interval, 86400);
        for secs in [21600_u64, 43200, 86400, 604800] {
            cfg.full_walk_interval = secs;
            save(&cfg, &path).unwrap();
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(loaded.full_walk_interval, secs);
        }
        // Absent key loads as the one-day default.
        fs::write(
            &path,
            r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
[[pair]]
name = "x"
local = "/tmp/ns-test-local"
remote = "Documents"
"#,
        )
        .unwrap();
        assert_eq!(load(path.to_str()).unwrap().full_walk_interval, 86400);
    }

    #[test]
    fn delete_guard_options_round_trip_and_refuse_bad_percent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        let base = r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
"#;
        fs::write(&path, base).unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert_eq!(cfg.max_delete_files, 200);
        assert_eq!(cfg.max_delete_percent, 30);
        for (files, percent) in [(0usize, 0u32), (500, 0), (0, 50), (10, 100)] {
            cfg.max_delete_files = files;
            cfg.max_delete_percent = percent;
            save(&cfg, &path).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains(&format!("max_delete_files = {files}")));
            assert!(text.contains(&format!("max_delete_percent = {percent}")));
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(loaded.max_delete_files, files);
            assert_eq!(loaded.max_delete_percent, percent);
        }
        fs::write(&path, format!("{base}max_delete_percent = 101\n")).unwrap();
        let err = load(path.to_str()).unwrap_err().to_string();
        assert!(err.contains("max_delete_percent"), "{err}");
    }

    #[test]
    fn speed_limits_default_round_trip_and_refuse_absurd_values() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        let base = r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
"#;
        fs::write(&path, base).unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert_eq!((cfg.upload_limit_kbps, cfg.download_limit_kbps), (0, 0));
        for (up, down) in [
            (256u64, 0u64),
            (0, 1024),
            (750, 10240),
            (1_048_576, 1_048_576),
        ] {
            cfg.upload_limit_kbps = up;
            cfg.download_limit_kbps = down;
            save(&cfg, &path).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                text.contains(&format!("upload_limit_kbps = {up}")),
                "{text}"
            );
            assert!(
                text.contains(&format!("download_limit_kbps = {down}")),
                "{text}"
            );
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(
                (loaded.upload_limit_kbps, loaded.download_limit_kbps),
                (up, down)
            );
        }
        for key in ["upload_limit_kbps", "download_limit_kbps"] {
            fs::write(&path, format!("{base}{key} = 1048577\n")).unwrap();
            let err = load(path.to_str()).unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
            assert!(err.contains("1048576"), "{err}");
        }
        for example in [
            crate::EXAMPLE_CONFIG,
            include_str!("../neutronsync.example.toml"),
        ] {
            assert!(example.contains("# upload_limit_kbps = 0"));
            assert!(example.contains("# download_limit_kbps = 0"));
            fs::write(&path, example).unwrap();
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(
                (loaded.upload_limit_kbps, loaded.download_limit_kbps),
                (0, 0)
            );
        }
    }

    #[test]
    fn desktop_options_default_and_round_trip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        let base = r#"
[cli]
backend = "api"
[options]
state_dir = "/tmp/ns-test-state"
"#;
        fs::write(&path, base).unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert!(cfg.notifications, "notifications default on");
        assert_eq!(cfg.theme, Theme::System);
        assert!(!cfg.file_manager_bookmarks, "bookmarks default off");

        for (notify, theme, bookmarks) in [
            (false, Theme::Light, true),
            (true, Theme::Dark, false),
            (false, Theme::System, true),
        ] {
            cfg.notifications = notify;
            cfg.theme = theme;
            cfg.file_manager_bookmarks = bookmarks;
            save(&cfg, &path).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                text.contains(&format!("notifications = {notify}")),
                "{text}"
            );
            assert!(
                text.contains(&format!("theme = \"{}\"", super::theme_str(theme))),
                "{text}"
            );
            assert!(
                text.contains(&format!("file_manager_bookmarks = {bookmarks}")),
                "{text}"
            );
            let loaded = load(path.to_str()).unwrap();
            assert_eq!(loaded.notifications, notify);
            assert_eq!(loaded.theme, theme);
            assert_eq!(loaded.file_manager_bookmarks, bookmarks);
        }

        fs::write(&path, format!("{base}theme = \"sepia\"\n")).unwrap();
        let err = load(path.to_str()).unwrap_err().to_string();
        assert!(err.contains("options.theme"), "{err}");
        assert!(err.contains("system|light|dark"), "{err}");
        assert!(err.contains("sepia"), "{err}");

        for example in [
            crate::EXAMPLE_CONFIG,
            include_str!("../neutronsync.example.toml"),
        ] {
            assert!(example.contains("# notifications = true"));
            assert!(example.contains("# theme = \"system\""));
            assert!(example.contains("# file_manager_bookmarks = false"));
            fs::write(&path, example).unwrap();
            let loaded = load(path.to_str()).unwrap();
            assert!(loaded.notifications);
            assert_eq!(loaded.theme, Theme::System);
            assert!(!loaded.file_manager_bookmarks);
        }
    }

    #[test]
    fn api_listen_is_kept_only_when_set() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("cfg.toml");
        fs::write(
            &path,
            "[cli]\nbackend = \"api\"\n[options]\nstate_dir = \"/tmp/ns-test-state\"\n",
        )
        .unwrap();
        let mut cfg = load(path.to_str()).unwrap();
        assert!(cfg.api_listen.is_none());
        assert!(cfg.api_token_file.is_none());
        save(&cfg, &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("api_listen"), "{text}");
        assert!(!text.contains("api_token_file"), "{text}");

        cfg.api_listen = Some("127.0.0.1:8091".into());
        cfg.api_token_file = Some("/tmp/ns-test-state/api.token".into());
        save(&cfg, &path).unwrap();
        let loaded = load(path.to_str()).unwrap();
        assert_eq!(loaded.api_listen.as_deref(), Some("127.0.0.1:8091"));
        assert_eq!(
            loaded.api_token_file.as_deref(),
            Some(std::path::Path::new("/tmp/ns-test-state/api.token"))
        );

        for example in [
            crate::EXAMPLE_CONFIG,
            include_str!("../neutronsync.example.toml"),
        ] {
            assert!(example.contains("# api_listen = \"127.0.0.1:8090\""));
            assert!(example.contains("# api_token_file ="));
            fs::write(&path, example).unwrap();
            let loaded = load(path.to_str()).unwrap();
            assert!(loaded.api_listen.is_none());
            assert!(loaded.api_token_file.is_none());
        }
    }
}
