//! In-app update against GitHub Releases: check, download, install, restart.
//!
//! Behind the `gui` feature. [`check`] reads the configured channel's latest
//! release and compares its version to the running build. [`download`] then
//! fetches the asset matching how this copy was installed, [`install`] hands it
//! to the system package manager through `pkexec` (one polkit prompt), and
//! [`restart`] replaces the running processes with the new build.
//!
//! Installing through the package manager is deliberate. Overwriting
//! `/usr/bin/neutronsync*` directly is fewer steps, but it leaves dpkg still
//! reporting the version of whatever `.deb` was last installed, which is how a
//! machine ends up showing 0.1.0-1 in App Center while running 0.3.1.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::UpdateChannel;

/// owner/repo the releases are published under.
const REPO: &str = "WilhelmZA/protondrive_linux_sync";

/// The running build's version (from Cargo). Bare semver, never the build
/// commit: this is what version comparison runs on.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The first version whose release ships a signed checksum list. A target at or
/// above it must carry `SHA256SUMS` and `SHA256SUMS.asc`, signed by the release
/// key below, or it is refused. Older targets keep the digest-only check.
/// `scripts/release.sh` reads this constant and refuses to tag a lower version.
pub const SIGNED_SINCE: &str = "0.7.0";

/// Name of the checksum list a signed release carries.
pub const SUMS_NAME: &str = "SHA256SUMS";
/// Name of the detached signature over [`SUMS_NAME`].
pub const SIG_NAME: &str = "SHA256SUMS.asc";

/// The release signing public keys, embedded at build time. The file may hold
/// more than one key (during rotation it holds the old and the new one), and a
/// signature by any of them is accepted. Until a key exists the file is a
/// marked placeholder that does not parse, and every signed-era release is
/// refused rather than installed unverified.
const RELEASE_KEY: &str = include_str!("../packaging/release-signing-key.asc");

/// Largest release asset the updater will download, applied to the package and
/// to the checksum list and its signature alike.
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

/// Outcome of an update check.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    /// True when `latest` is newer than the running version.
    pub newer: bool,
    /// The release's GitHub page (for an "Open release" action).
    pub html_url: String,
    /// What changed since the running version: the release body of every
    /// published release newer than `current` on this channel, newest first,
    /// each under a `## vX.Y.Z` heading (may be empty).
    pub notes: String,
    /// Whether the found release is flagged pre-release.
    pub prerelease: bool,
    /// Downloadable files attached to the release.
    pub assets: Vec<Asset>,
}

/// One file attached to a release.
#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Size in bytes, as GitHub reports it (0 when absent).
    pub size: u64,
    /// Lower-case hex SHA-256 of the file, from the release API's `digest`
    /// field. `None` on releases old enough to predate it, in which case the
    /// download cannot be verified and is refused rather than installed.
    pub digest: Option<String>,
}

/// How this copy of the app was installed, which decides both the asset to
/// fetch and the command that installs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed from the `.deb` (dpkg knows about it).
    Deb,
    /// Installed from the `.rpm`.
    Rpm,
    /// Neither package manager owns it: a tarball, `cargo install`, or a
    /// hand-copied binary. Updating in place would be guesswork, so the app
    /// downloads the file and leaves installing to the user.
    Unmanaged,
}

impl InstallKind {
    /// The suffix of the release asset this install kind wants.
    fn asset_suffix(self) -> &'static str {
        match self {
            InstallKind::Deb => "_amd64.deb",
            InstallKind::Rpm => ".x86_64.rpm",
            InstallKind::Unmanaged => "-x86_64-linux.tar.gz",
        }
    }

    /// Whether the app can install this itself.
    pub fn can_install(self) -> bool {
        !matches!(self, InstallKind::Unmanaged)
    }
}

/// Detect how this copy was installed by asking each package manager whether it
/// owns the `neutronsync` package. Neither owning it means unmanaged.
pub fn install_kind() -> InstallKind {
    if package_installed("dpkg-query", &["-W", "neutronsync"]) {
        return InstallKind::Deb;
    }
    if package_installed("rpm", &["-q", "neutronsync"]) {
        return InstallKind::Rpm;
    }
    InstallKind::Unmanaged
}

fn package_installed(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The asset matching `kind`, if the release carries one.
pub fn pick_asset(info: &UpdateInfo, kind: InstallKind) -> Option<&Asset> {
    let suffix = kind.asset_suffix();
    info.assets.iter().find(|a| a.name.ends_with(suffix))
}

/// Check the given channel for the latest release.
///
/// Uses `GITHUB_TOKEN`/`GH_TOKEN` from the environment when set (needed while
/// the repo is private); works tokenless once the repo is public.
pub fn check(channel: UpdateChannel) -> Result<UpdateInfo> {
    // The recent releases, newest first. They choose the pre-release target and
    // supply the notes for every version the running copy has missed.
    let body = fetch(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page=20"
    ))?;
    let recent: Vec<serde_json::Value> =
        serde_json::from_str(&body).context("parsing releases JSON")?;
    let release = match channel {
        // The stable channel follows only promoted releases; `/releases/latest`
        // returns the newest non-draft, non-prerelease one.
        UpdateChannel::Stable => {
            let body = fetch(&format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))?;
            serde_json::from_str::<serde_json::Value>(&body).context("parsing release JSON")?
        }
        // The pre-release channel follows the newest published release of any
        // kind: take the first non-draft entry.
        UpdateChannel::Prerelease => recent
            .iter()
            .find(|r| !r["draft"].as_bool().unwrap_or(false))
            .cloned()
            .ok_or_else(|| anyhow!("no published releases yet"))?,
    };

    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("release has no tag_name"))?
        .to_string();
    let html_url = release["html_url"].as_str().unwrap_or("").to_string();
    let prerelease = release["prerelease"].as_bool().unwrap_or(false);

    let assets = release["assets"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a["name"].as_str()?.to_string(),
                        url: a["browser_download_url"].as_str()?.to_string(),
                        size: a["size"].as_u64().unwrap_or(0),
                        // "sha256:<hex>"; anything else is treated as absent.
                        digest: a["digest"]
                            .as_str()
                            .and_then(|d| d.strip_prefix("sha256:"))
                            .map(|h| h.trim().to_ascii_lowercase()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let current = current_version().to_string();
    let latest = tag.trim_start_matches('v').to_string();
    let newer = is_newer(&latest, &current);
    let notes = notes_since(&recent, channel, &current, &latest, &release);

    Ok(UpdateInfo {
        current,
        latest,
        newer,
        html_url,
        notes,
        prerelease,
        assets,
    })
}

/// Download `asset` into a private directory and return the file's path.
///
/// `progress` is called with (bytes so far, total bytes) as the body streams, so
/// the caller can drive a progress bar; `total` is 0 when the size is unknown.
/// `cancel` is polled between chunks and aborts the download when it returns
/// true, so quitting the app doesn't leave a thread pulling 6 MB.
pub fn download(
    asset: &Asset,
    progress: &(dyn Fn(u64, u64) + Sync),
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<PathBuf> {
    let dir = download_dir()?;
    let dest = dir.join(&asset.name);

    let mut req = ureq::get(&asset.url).set("User-Agent", "neutronsync-updater");
    if let Some(tok) = token() {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    let resp = req.call().map_err(|e| anyhow!("download failed: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(asset.size);
    if total > MAX_ASSET_BYTES {
        bail!(
            "{} is {total} bytes, more than the updater will download",
            asset.name
        );
    }

    // Write to a partial file and rename on success, so an interrupted download
    // can never be mistaken for a complete package.
    let part = dir.join(format!("{}.part", asset.name));
    let mut out =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        if cancel() {
            let _ = std::fs::remove_file(&part);
            bail!("download cancelled");
        }
        let n = reader.read(&mut buf).context("reading the download")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("writing the download")?;
        done += n as u64;
        if done > MAX_ASSET_BYTES {
            let _ = std::fs::remove_file(&part);
            bail!("{} grew past {MAX_ASSET_BYTES} bytes", asset.name);
        }
        progress(done, total);
    }
    out.flush().ok();
    drop(out);

    if total > 0 && done != total {
        let _ = std::fs::remove_file(&part);
        bail!("download truncated: got {done} of {total} bytes");
    }
    std::fs::rename(&part, &dest).with_context(|| format!("finishing {}", dest.display()))?;
    Ok(dest)
}

/// A private (0700) directory for downloaded packages, reused across attempts.
fn download_dir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("neutronsync-update-{}", std::process::id()));
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

/// SHA-256 of a file as lower-case hex, streamed so a large package is not read
/// into memory.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut fh = std::fs::File::open(path)
        .with_context(|| format!("opening {} to hash it", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = fh
            .read(&mut buf)
            .context("reading the package to hash it")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Check a downloaded file against the digest the release API published for it.
///
/// Fails closed: an asset with no digest is refused, not installed on trust. The
/// digest arrives over TLS in the same API response as the version number, so it
/// is not something the download itself can influence.
pub fn verify(path: &Path, asset: &Asset) -> Result<()> {
    let actual = sha256_file(path)?;
    check_digest(&asset.name, &actual, asset.digest.as_deref())
}

/// The digest-only check: `actual` must equal the API's digest, which must exist.
fn check_digest(name: &str, actual: &str, expected: Option<&str>) -> Result<()> {
    let expected = expected.ok_or_else(|| {
        anyhow!(
            "{name} has no published checksum, so it can't be verified; \
             install it by hand if you trust it"
        )
    })?;
    if actual != expected {
        bail!(
            "checksum mismatch on {name}: expected {expected}, got {actual}. \
             The download was discarded and nothing was installed."
        );
    }
    Ok(())
}

/// Whether a release at `version` must be signed: true from [`SIGNED_SINCE`]
/// on. A version that does not parse is treated as signed-era, so a strange
/// tag can never slip past the check.
pub fn signing_required(version: &str) -> bool {
    match (
        semver::Version::parse(version),
        semver::Version::parse(SIGNED_SINCE),
    ) {
        (Ok(v), Ok(since)) => v >= since,
        _ => true,
    }
}

/// The checksum list and its signature as downloaded from a release. Either is
/// `None` when the release carries no such asset.
#[derive(Clone, Debug, Default)]
pub struct Proof {
    pub sums: Option<Vec<u8>>,
    pub sig: Option<Vec<u8>>,
}

/// Fetch `SHA256SUMS` and `SHA256SUMS.asc` from the release, through the same
/// download path (private directory, size cap) as the package. A release
/// without them yields `None` for the missing file; whether that is acceptable
/// is decided by [`verify_release`], not here.
pub fn fetch_proof(info: &UpdateInfo) -> Result<Proof> {
    let mut proof = Proof::default();
    for (name, slot) in [(SUMS_NAME, &mut proof.sums), (SIG_NAME, &mut proof.sig)] {
        let Some(asset) = info.assets.iter().find(|a| a.name == name) else {
            continue;
        };
        let path = download(asset, &|_, _| {}, &|| false)?;
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let _ = std::fs::remove_file(&path);
        *slot = Some(bytes);
    }
    Ok(proof)
}

/// Check a downloaded package against the release's signed checksum list and
/// the API digest, with the embedded release key. Any failure means the file
/// must be discarded; the caller deletes it.
pub fn verify_signed(path: &Path, asset: &Asset, info: &UpdateInfo, proof: &Proof) -> Result<()> {
    let actual = sha256_file(path)?;
    verify_release(
        &info.latest,
        RELEASE_KEY,
        &asset.name,
        &actual,
        asset.digest.as_deref(),
        proof.sums.as_deref(),
        proof.sig.as_deref(),
    )
}

/// The whole decision, over bytes, so it can be tested without a network.
///
/// For a target below [`SIGNED_SINCE`], this is the digest-only check that
/// older copies also make. From [`SIGNED_SINCE`] on: the embedded key must be
/// real, both files must be present, the signature must verify against that
/// key, the list must name `asset_name` with the hash `actual_sha256`, and the
/// API digest, when present, must agree too. Every message is one plain
/// sentence, because the GUI shows it as the failure line.
pub fn verify_release(
    target_version: &str,
    key_armor: &str,
    asset_name: &str,
    actual_sha256: &str,
    api_digest: Option<&str>,
    sums: Option<&[u8]>,
    sig: Option<&[u8]>,
) -> Result<()> {
    if !signing_required(target_version) {
        return check_digest(asset_name, actual_sha256, api_digest);
    }
    let keys = parse_release_keys(key_armor);
    if keys.is_empty() {
        bail!(
            "This build has no release key, so updates can't be verified. \
             Download the release by hand."
        );
    }
    let sums = sums.ok_or_else(|| {
        anyhow!(
            "This release has no checksum list, so it can't be verified and nothing was installed."
        )
    })?;
    let sig = sig.ok_or_else(|| {
        anyhow!("This release's checksum list isn't signed, so it can't be verified and nothing was installed.")
    })?;
    if !keys.iter().any(|key| signature_matches(key, sums, sig)) {
        bail!("This release's signature doesn't match NeutronSync's release key, so the download was discarded and nothing was installed.");
    }
    let expected = sum_for(sums, asset_name).ok_or_else(|| {
        anyhow!("The signed checksum list doesn't mention {asset_name}, so the download was discarded and nothing was installed.")
    })?;
    if expected != actual_sha256 {
        bail!("{asset_name} doesn't match its signed checksum, so the download was discarded and nothing was installed.");
    }
    if let Some(digest) = api_digest {
        if digest != actual_sha256 {
            bail!("{asset_name} doesn't match the checksum GitHub published, so the download was discarded and nothing was installed.");
        }
    }
    Ok(())
}

/// Every public key in the embedded file: one or more armored blocks, each of
/// which may itself carry several keys (`gpg --export k1 k2` writes one
/// block). Empty when the file is still the placeholder or holds no key that
/// parses; anything unparsable is skipped, never trusted.
fn parse_release_keys(armor: &str) -> Vec<pgp::composed::SignedPublicKey> {
    use pgp::composed::Deserializable;
    const BEGIN: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
    let mut keys = Vec::new();
    for block in armor.split(BEGIN).skip(1) {
        let block = format!("{BEGIN}{block}");
        let found: Vec<pgp::composed::SignedPublicKey> =
            match pgp::composed::SignedPublicKey::from_string_many(&block) {
                Ok((parsed, _)) => parsed.filter_map(Result::ok).collect(),
                Err(_) => Vec::new(),
            };
        keys.extend(found);
    }
    keys
}

/// Whether the armored detached signature `sig` over `data` was made by `key`
/// or one of its subkeys.
fn signature_matches(key: &pgp::composed::SignedPublicKey, data: &[u8], sig: &[u8]) -> bool {
    use pgp::composed::{Deserializable, DetachedSignature};
    let Ok((sig, _)) = DetachedSignature::from_armor_single(std::io::Cursor::new(sig)) else {
        return false;
    };
    sig.verify(key, data).is_ok()
        || key
            .public_subkeys
            .iter()
            .any(|sub| sig.verify(sub, data).is_ok())
}

/// The lower-case hex hash recorded for `name` in a `sha256sum`-format list
/// (`<hex>  <name>`, or `<hex> *<name>` for binary mode).
fn sum_for(sums: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(sums);
    text.lines().find_map(|line| {
        let (hash, rest) = line.trim().split_once(' ')?;
        let file = rest.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Install a downloaded package through the system package manager, elevating
/// with `pkexec` (the user gets one polkit prompt).
///
/// The package manager is what keeps dpkg's or rpm's record of the installed
/// version honest, which is why this doesn't just overwrite the binaries.
pub fn install(pkg: &Path, kind: InstallKind) -> Result<()> {
    let pkg = pkg
        .to_str()
        .ok_or_else(|| anyhow!("package path is not valid UTF-8"))?;
    let (bin, args): (&str, Vec<&str>) = match kind {
        // apt resolves the package's dependencies; plain `dpkg -i` would fail on
        // a missing library instead of pulling it in.
        InstallKind::Deb => ("apt-get", vec!["install", "-y", pkg]),
        InstallKind::Rpm if which("dnf").is_some() => ("dnf", vec!["install", "-y", pkg]),
        InstallKind::Rpm => ("rpm", vec!["-U", pkg]),
        InstallKind::Unmanaged => {
            bail!("this copy wasn't installed by a package manager; install the download by hand")
        }
    };
    // Absolute paths on both sides: pkexec clears the environment, so the PATH
    // it would resolve against is not the one this process was launched with.
    let pkexec = which("pkexec")
        .ok_or_else(|| anyhow!("pkexec is not available; install the download by hand"))?;
    let bin = which(bin).ok_or_else(|| anyhow!("{bin} is not installed"))?;
    let out = Command::new(pkexec)
        .arg(&bin)
        .args(&args)
        .output()
        .with_context(|| format!("running pkexec {}", bin.display()))?;
    if out.status.success() {
        return Ok(());
    }
    // 126/127 are pkexec's own codes for "not authorised" and "couldn't run".
    let msg = match out.status.code() {
        Some(126) => "authorisation was declined".to_string(),
        Some(127) => format!("pkexec couldn't run {}", bin.display()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            if err.is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                err.to_string()
            }
        }
    };
    bail!("install failed: {msg}");
}

/// Resolve `bin` to an absolute path on `PATH`, falling back to the standard
/// system directories: a desktop launcher's PATH can be minimal.
fn which(bin: &str) -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(bin))
            .find(|c| c.is_file() && is_executable(c))
    });
    from_path.or_else(|| {
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin"]
            .iter()
            .map(|d| Path::new(d).join(bin))
            .find(|c| c.is_file() && is_executable(c))
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Replace the running app with the freshly installed build.
///
/// The window and the tray daemon are both `neutronsync-gui` processes holding
/// PID-keyed locks, and there is no single shutdown path between them, so a
/// clean handover has to happen from outside: a detached shell waits for this
/// process to go away, stops any remaining ones, and starts the pair again.
///
/// The helper runs in its own systemd scope when a user manager is available.
/// Otherwise it shares this process's cgroup, and if the tray runs as a systemd
/// service, stopping the tray stops that service and kills the helper with it.
pub fn restart() -> Result<()> {
    let script = "sleep 1; pkill -x neutronsync-gui; sleep 1; \
                  setsid neutronsync-gui --tray >/dev/null 2>&1 < /dev/null & \
                  sleep 1; setsid neutronsync-gui >/dev/null 2>&1 < /dev/null &";
    let scoped = Command::new("systemd-run")
        .args(scoped_helper_args(std::process::id(), script))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if scoped {
        return Ok(());
    }
    Command::new("setsid")
        .args(["-f", "/bin/sh", "-c", script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning the restart helper")?;
    Ok(())
}

/// `systemd-run` arguments that start the restart helper in a new user scope.
/// `setsid -f` returns at once, so `systemd-run` exits as soon as the scope exists.
fn scoped_helper_args(pid: u32, script: &str) -> Vec<String> {
    [
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        &format!("--unit=neutronsync-restart-{pid}"),
        "setsid",
        "-f",
        "/bin/sh",
        "-c",
        script,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn fetch(url: &str) -> Result<String> {
    let mut req = ureq::get(url)
        .set("User-Agent", "neutronsync-updater")
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token() {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => resp.into_string().context("reading the GitHub response"),
        Err(ureq::Error::Status(404, _)) => Err(anyhow!(
            "no matching release found yet (or the repo is private and GITHUB_TOKEN isn't set)"
        )),
        Err(ureq::Error::Status(code, _)) => Err(anyhow!("GitHub returned HTTP {code}")),
        Err(e) => Err(anyhow!("update check failed: {e}")),
    }
}

fn token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// Whether `latest` is a newer version than `current`. Prefers a semver
/// comparison; falls back to "any difference counts as newer" if either string
/// doesn't parse as semver.
/// Everything the running copy has missed: the body of each published release
/// on `channel` that is newer than `current` and not newer than `latest`,
/// newest first, each under a `## vX.Y.Z` heading. A copy several versions
/// behind therefore sees every change, not just the newest release's. Falls
/// back to `chosen`'s own body when the list has nothing to say.
fn notes_since(
    recent: &[serde_json::Value],
    channel: UpdateChannel,
    current: &str,
    latest: &str,
    chosen: &serde_json::Value,
) -> String {
    let mut out = String::new();
    for r in recent {
        if r["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        if channel == UpdateChannel::Stable && r["prerelease"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(tag) = r["tag_name"].as_str() else {
            continue;
        };
        let version = tag.trim_start_matches('v');
        if !is_newer(version, current) || is_newer(version, latest) {
            continue;
        }
        let body = r["body"].as_str().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        out.push_str(&format!("## v{version}\n{body}\n\n"));
    }
    if out.is_empty() {
        out = chosen["body"].as_str().unwrap_or("").trim().to_string();
    }
    out.trim_end().to_string()
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => !latest.is_empty() && latest != current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_cover_every_missed_release_newest_first() {
        let rel = |tag: &str, pre: bool, body: &str| serde_json::json!({"tag_name": tag, "prerelease": pre, "draft": false, "body": body});
        let recent = vec![
            rel("v0.6.0", true, "### Removed\n- the CLI backend"),
            rel("v0.5.3", true, "### Fixed\n- false conflicts"),
            rel("v0.5.2", false, "### Fixed\n- restart"),
            rel("v0.5.1", true, ""),
            rel("v0.5.0", false, "### Added\n- screenshots"),
        ];
        // Pre-release channel, two versions behind: both newer bodies, newest
        // first, and nothing at or below the running version.
        let notes = notes_since(
            &recent,
            UpdateChannel::Prerelease,
            "0.5.2",
            "0.6.0",
            &recent[0],
        );
        assert_eq!(
            notes,
            "## v0.6.0\n### Removed\n- the CLI backend\n\n## v0.5.3\n### Fixed\n- false conflicts"
        );
        // Stable channel skips pre-releases and stops at the chosen release.
        let notes = notes_since(&recent, UpdateChannel::Stable, "0.4.9", "0.5.2", &recent[2]);
        assert_eq!(
            notes,
            "## v0.5.2\n### Fixed\n- restart\n\n## v0.5.0\n### Added\n- screenshots"
        );
        // Nothing newer in the list: the chosen release's own body.
        let notes = notes_since(
            &recent[..0],
            UpdateChannel::Stable,
            "0.5.0",
            "0.5.2",
            &recent[2],
        );
        assert_eq!(notes, "### Fixed\n- restart");
    }

    #[test]
    fn semver_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // A final release outranks its own pre-releases, and vice versa.
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn restart_helper_gets_its_own_scope() {
        let args = scoped_helper_args(42, "true");
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit=neutronsync-restart-42",
                "setsid",
                "-f",
                "/bin/sh",
                "-c",
                "true"
            ]
        );
    }

    /// The asset names here are the ones the release workflow actually produces.
    fn release_with_assets() -> UpdateInfo {
        let names = [
            "neutronsync-0.3.3-1.x86_64.rpm",
            "neutronsync-v0.3.3-src.tar.gz",
            "neutronsync-v0.3.3-x86_64-linux.tar.gz",
            "neutronsync_0.3.3-1_amd64.deb",
        ];
        UpdateInfo {
            current: "0.3.2".into(),
            latest: "0.3.3".into(),
            newer: true,
            html_url: String::new(),
            notes: String::new(),
            prerelease: false,
            assets: names
                .iter()
                .map(|n| Asset {
                    name: (*n).to_string(),
                    url: format!("https://example.invalid/{n}"),
                    size: 1,
                    digest: Some("00".repeat(32)),
                })
                .collect(),
        }
    }

    fn tmpfile(name: &str, body: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "neutronsync-updater-test-{name}-{}",
            std::process::id()
        ));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        // The canonical SHA-256 of "abc".
        let p = tmpfile("sha", b"abc");
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn verify_accepts_a_matching_digest_and_rejects_a_wrong_one() {
        let p = tmpfile("verify", b"abc");
        let good = Asset {
            name: "pkg.deb".into(),
            url: String::new(),
            size: 3,
            digest: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()),
        };
        verify(&p, &good).expect("a matching digest must pass");

        let tampered = Asset {
            digest: Some("00".repeat(32)),
            ..good.clone()
        };
        let err = verify(&p, &tampered).expect_err("a mismatch must fail");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn verify_fails_closed_when_the_release_published_no_digest() {
        let p = tmpfile("nodigest", b"abc");
        let no_digest = Asset {
            name: "pkg.deb".into(),
            url: String::new(),
            size: 3,
            digest: None,
        };
        let err = verify(&p, &no_digest).expect_err("an unverifiable asset must not pass");
        assert!(err.to_string().contains("can't be verified"), "{err}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn picks_the_asset_matching_the_install_kind() {
        let info = release_with_assets();
        assert_eq!(
            pick_asset(&info, InstallKind::Deb).map(|a| a.name.as_str()),
            Some("neutronsync_0.3.3-1_amd64.deb")
        );
        assert_eq!(
            pick_asset(&info, InstallKind::Rpm).map(|a| a.name.as_str()),
            Some("neutronsync-0.3.3-1.x86_64.rpm")
        );
        // The binary tarball, NOT the source tarball: both end in .tar.gz, so a
        // looser match would hand the user a source archive to run.
        assert_eq!(
            pick_asset(&info, InstallKind::Unmanaged).map(|a| a.name.as_str()),
            Some("neutronsync-v0.3.3-x86_64-linux.tar.gz")
        );
    }

    #[test]
    fn missing_asset_is_reported_not_guessed() {
        let mut info = release_with_assets();
        info.assets.retain(|a| !a.name.ends_with(".deb"));
        assert!(pick_asset(&info, InstallKind::Deb).is_none());
    }

    #[test]
    fn only_package_managed_installs_can_self_install() {
        assert!(InstallKind::Deb.can_install());
        assert!(InstallKind::Rpm.can_install());
        assert!(!InstallKind::Unmanaged.can_install());
    }

    /// Hits the network and the real releases API, so it is not part of the
    /// normal run. Exercise it with:
    ///   cargo test --features gui -- --ignored --nocapture
    #[test]
    #[ignore = "network: downloads a real release asset"]
    fn live_check_and_download() {
        let info = check(UpdateChannel::Prerelease).expect("check");
        println!("latest {} ({} assets)", info.latest, info.assets.len());
        let kind = install_kind();
        let asset = pick_asset(&info, kind).expect("an asset for this system");
        println!("picked {} ({} bytes) for {kind:?}", asset.name, asset.size);

        let seen = std::sync::Mutex::new(0u64);
        let path = download(asset, &|done, _total| *seen.lock().unwrap() = done, &|| {
            false
        })
        .expect("download");
        let len = std::fs::metadata(&path).expect("stat").len();
        println!("downloaded {} -> {len} bytes", path.display());
        assert_eq!(len, asset.size, "size should match what GitHub reported");
        assert!(
            *seen.lock().unwrap() > 0,
            "progress should have been reported"
        );

        // The whole point: the real asset verifies against the real digest.
        println!("digest {:?}", asset.digest);
        verify(&path, asset).expect("the published checksum should match");
        let _ = std::fs::remove_file(&path);
    }

    // --- release signing (fixtures under tests/fixtures/signing) -----------

    const TEST_KEY: &str = include_str!("../tests/fixtures/signing/test-release-key.asc");
    /// Both test keys, as two armored blocks (the rotation layout).
    const TWO_KEYS: &str = include_str!("../tests/fixtures/signing/test-release-keys-both.asc");
    /// Both test keys in one armored block (`gpg --export k1 k2`).
    const TWO_KEYS_ONE_BLOCK: &str =
        include_str!("../tests/fixtures/signing/test-release-keys-one-block.asc");
    const SUMS: &[u8] = include_bytes!("../tests/fixtures/signing/SHA256SUMS");
    const SIG: &[u8] = include_bytes!("../tests/fixtures/signing/SHA256SUMS.asc");
    /// Signed by the second test key.
    const SECOND_SIG: &[u8] = include_bytes!("../tests/fixtures/signing/SHA256SUMS.second-key.asc");
    /// Signed by a third key that is in neither file.
    const OTHER_SIG: &[u8] = include_bytes!("../tests/fixtures/signing/SHA256SUMS.other-key.asc");
    const DEB: &str = "neutronsync_0.7.0-1_amd64.deb";
    /// SHA-256 of the fixture .deb's content, "deb-bytes\n", as SHA256SUMS records it.
    const DEB_SUM: &str = "274cd245459bebed3c77d2a663d41f8b96fcc76943a7242ed256f7d1c3cdd642";

    #[test]
    fn signing_is_required_from_signed_since_on() {
        assert_eq!(SIGNED_SINCE, "0.7.0");
        assert!(!signing_required("0.6.1"));
        assert!(!signing_required("0.6.99"));
        assert!(signing_required("0.7.0"));
        assert!(signing_required("0.7.1-rc1"));
        assert!(signing_required("1.0.0"));
        // Unparsable means signed-era: fail closed.
        assert!(signing_required("banana"));
    }

    #[test]
    fn fixture_sums_name_the_deb() {
        assert_eq!(sum_for(SUMS, DEB).as_deref(), Some(DEB_SUM));
        assert_eq!(sum_for(SUMS, "nothing.deb"), None);
        // Binary-mode lines and odd spacing still parse.
        assert_eq!(
            sum_for(format!("{DEB_SUM} *{DEB}\n").as_bytes(), DEB).as_deref(),
            Some(DEB_SUM)
        );
    }

    #[test]
    fn valid_signature_and_matching_sum_installs() {
        verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            Some(DEB_SUM),
            Some(SUMS),
            Some(SIG),
        )
        .expect("signed and matching must pass");
        // The API digest is optional on a signed release.
        verify_release("0.7.0", TEST_KEY, DEB, DEB_SUM, None, Some(SUMS), Some(SIG))
            .expect("signed without an API digest must pass");
    }

    #[test]
    fn tampered_package_fails() {
        let wrong = "00".repeat(32);
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            &wrong,
            Some(&wrong),
            Some(SUMS),
            Some(SIG),
        )
        .expect_err("a package not matching the signed list must fail");
        assert!(err.to_string().contains("signed checksum"), "{err}");
        // A matching signed list but a disagreeing API digest fails too.
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            Some(&wrong),
            Some(SUMS),
            Some(SIG),
        )
        .expect_err("the API digest still counts");
        assert!(err.to_string().contains("GitHub published"), "{err}");
    }

    #[test]
    fn tampered_sums_fail() {
        let mut sums = SUMS.to_vec();
        // Swap the first hex digit of the .deb line.
        let i = sums
            .windows(DEB_SUM.len())
            .position(|w| w == DEB_SUM.as_bytes())
            .unwrap();
        sums[i] = if sums[i] == b'2' { b'3' } else { b'2' };
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            None,
            Some(&sums),
            Some(SIG),
        )
        .expect_err("an edited list must fail its signature");
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn any_embedded_key_may_sign_but_no_other() {
        assert_eq!(parse_release_keys(TEST_KEY).len(), 1);
        assert_eq!(parse_release_keys(TWO_KEYS).len(), 2);
        assert_eq!(parse_release_keys(TWO_KEYS_ONE_BLOCK).len(), 2);
        for keys in [TWO_KEYS, TWO_KEYS_ONE_BLOCK] {
            verify_release("0.7.0", keys, DEB, DEB_SUM, None, Some(SUMS), Some(SIG))
                .expect("the first embedded key signs");
            verify_release(
                "0.7.0",
                keys,
                DEB,
                DEB_SUM,
                None,
                Some(SUMS),
                Some(SECOND_SIG),
            )
            .expect("the second embedded key signs");
            let err = verify_release(
                "0.7.0",
                keys,
                DEB,
                DEB_SUM,
                None,
                Some(SUMS),
                Some(OTHER_SIG),
            )
            .expect_err("a third key must still fail");
            assert!(err.to_string().contains("signature"), "{err}");
        }
        // With only the first key embedded, the second key's signature fails.
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            None,
            Some(SUMS),
            Some(SECOND_SIG),
        )
        .expect_err("a key not in the file must fail");
        assert!(err.to_string().contains("signature"), "{err}");
        // Garbage between or after blocks is skipped, not trusted.
        let messy = format!(
            "junk\n{TEST_KEY}\nmore junk\n-----BEGIN PGP PUBLIC KEY BLOCK-----\nnot base64\n"
        );
        assert_eq!(parse_release_keys(&messy).len(), 1);
    }

    #[test]
    fn signature_by_a_different_key_fails() {
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            None,
            Some(SUMS),
            Some(OTHER_SIG),
        )
        .expect_err("another key's signature must fail");
        assert!(err.to_string().contains("signature"), "{err}");
        // So does something that is not a signature at all.
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            None,
            Some(SUMS),
            Some(b"nope"),
        )
        .expect_err("garbage must fail");
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn missing_signature_on_a_signed_era_release_fails() {
        let err = verify_release(
            "0.7.0",
            TEST_KEY,
            DEB,
            DEB_SUM,
            Some(DEB_SUM),
            Some(SUMS),
            None,
        )
        .expect_err("no .asc must fail");
        assert!(err.to_string().contains("isn't signed"), "{err}");
        let err = verify_release("0.7.0", TEST_KEY, DEB, DEB_SUM, Some(DEB_SUM), None, None)
            .expect_err("no SHA256SUMS must fail");
        assert!(err.to_string().contains("no checksum list"), "{err}");
    }

    #[test]
    fn older_release_without_the_files_uses_the_digest_check() {
        verify_release("0.6.1", TEST_KEY, DEB, DEB_SUM, Some(DEB_SUM), None, None)
            .expect("an older release with a matching digest passes");
        let err = verify_release(
            "0.6.1",
            TEST_KEY,
            DEB,
            DEB_SUM,
            Some(&"00".repeat(32)),
            None,
            None,
        )
        .expect_err("an older release with a wrong digest fails");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        let err = verify_release("0.6.1", TEST_KEY, DEB, DEB_SUM, None, None, None)
            .expect_err("an older release with no digest fails closed, as before");
        assert!(err.to_string().contains("can't be verified"), "{err}");
    }

    #[test]
    fn placeholder_key_refuses_signed_era_releases() {
        // The embedded key as built right now. While it is the placeholder the
        // message must say so; once a real key lands this branch only checks
        // that the explicit placeholder text is refused.
        let placeholder = "NEUTRONSYNC RELEASE SIGNING KEY PLACEHOLDER\nnot a key\n";
        let err = verify_release(
            "0.7.0",
            placeholder,
            DEB,
            DEB_SUM,
            Some(DEB_SUM),
            Some(SUMS),
            Some(SIG),
        )
        .expect_err("a placeholder key must refuse");
        assert!(err.to_string().contains("no release key"), "{err}");
        if parse_release_keys(RELEASE_KEY).is_empty() {
            let err = verify_release(
                "0.7.0",
                RELEASE_KEY,
                DEB,
                DEB_SUM,
                Some(DEB_SUM),
                Some(SUMS),
                Some(SIG),
            )
            .expect_err("the embedded placeholder must refuse");
            assert_eq!(
                err.to_string(),
                "This build has no release key, so updates can't be verified. Download the release by hand."
            );
        }
        // An older target never needs the key.
        verify_release(
            "0.6.1",
            placeholder,
            DEB,
            DEB_SUM,
            Some(DEB_SUM),
            None,
            None,
        )
        .expect("the placeholder does not affect older releases");
    }

    #[test]
    fn verify_signed_reads_the_file_and_uses_the_release_version() {
        let p = tmpfile("signed", b"deb-bytes\n");
        let mut info = release_with_assets();
        info.latest = "0.6.1".into();
        let asset = Asset {
            name: DEB.into(),
            url: String::new(),
            size: 10,
            digest: Some(DEB_SUM.into()),
        };
        // Older target: the digest alone decides, so no proof is needed.
        verify_signed(&p, &asset, &info, &Proof::default()).expect("digest-era release");
        // Signed-era target with no proof is refused whatever the digest says.
        info.latest = "0.7.0".into();
        verify_signed(&p, &asset, &info, &Proof::default()).expect_err("signed-era needs proof");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unmanaged_install_refuses_rather_than_overwriting() {
        let err = install(Path::new("/nonexistent.deb"), InstallKind::Unmanaged)
            .expect_err("unmanaged must not try to install");
        assert!(err.to_string().contains("package manager"), "{err}");
    }
}
