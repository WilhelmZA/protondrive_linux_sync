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

/// The running build's version (from Cargo).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Outcome of an update check.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    /// True when `latest` is newer than the running version.
    pub newer: bool,
    /// The release's GitHub page (for an "Open release" action).
    pub html_url: String,
    /// Release notes / body (may be empty).
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
        // kind: take the first non-draft entry (the API lists newest first).
        UpdateChannel::Prerelease => {
            let body = fetch(&format!(
                "https://api.github.com/repos/{REPO}/releases?per_page=20"
            ))?;
            let arr: Vec<serde_json::Value> =
                serde_json::from_str(&body).context("parsing releases JSON")?;
            arr.into_iter()
                .find(|r| !r["draft"].as_bool().unwrap_or(false))
                .ok_or_else(|| anyhow!("no published releases yet"))?
        }
    };

    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("release has no tag_name"))?
        .to_string();
    let html_url = release["html_url"].as_str().unwrap_or("").to_string();
    let notes = release["body"].as_str().unwrap_or("").to_string();
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
    let expected = asset.digest.as_deref().ok_or_else(|| {
        anyhow!(
            "{} has no published checksum, so it can't be verified; \
             install it by hand if you trust it",
            asset.name
        )
    })?;
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "checksum mismatch on {}: expected {expected}, got {actual}. \
             The download was discarded and nothing was installed.",
            asset.name
        );
    }
    Ok(())
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

    #[test]
    fn unmanaged_install_refuses_rather_than_overwriting() {
        let err = install(Path::new("/nonexistent.deb"), InstallKind::Unmanaged)
            .expect_err("unmanaged must not try to install");
        assert!(err.to_string().contains("package manager"), "{err}");
    }
}
