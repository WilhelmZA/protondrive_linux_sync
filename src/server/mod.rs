//! `neutronsync serve`: the sync engine with no desktop, driven over HTTP.
//!
//! One [`Controller`] owns the engine exactly as it does in the desktop
//! window; this module only translates HTTP requests into its commands and
//! publishes its state. The API lives under `/api/v1` and is described in
//! `docs/HTTP_API.md`. Live updates go out as server-sent events from one
//! publisher task (see [`publish`]).
//!
//! The controller is synchronous. Every handler that may block (a sidecar
//! call, a config save, a database read) runs on `spawn_blocking`.

mod error;
mod files;
mod publish;
mod routes;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::watch;

use crate::config::{self, Config};
use crate::logger::Logger;
use crate::service::Controller;

pub use error::ApiError;

/// The API version in every path (`/api/v1`). A breaking change means v2.
pub const API_VERSION: u32 = 1;
/// Shortest API token accepted.
pub const MIN_TOKEN_LEN: usize = 32;
/// Largest request body accepted.
pub const BODY_LIMIT: usize = 64 * 1024;
/// How long a shutdown waits for a running sync to stop before exiting. Kept
/// under the ten seconds a supervisor allows, with room for the rest of the
/// shutdown (a transfer already under way is not interrupted mid-file).
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
/// The desktop tray daemon's lock in `state_dir`.
const TRAY_LOCK: &str = "tray-daemon.lock";
/// This server's lock in `state_dir`.
const SERVE_LOCK: &str = "serve.lock";

/// What `neutronsync serve` was started with.
pub struct ServeOptions {
    pub config: Option<String>,
    pub listen: String,
    pub token_file: Option<PathBuf>,
    pub local_roots: Vec<PathBuf>,
}

/// Shared by every handler.
pub struct Ctx {
    pub(crate) ctrl: Arc<Controller>,
    pub(crate) config_path: PathBuf,
    pub(crate) token: Option<String>,
    /// Listening on a loopback address: `Host` must then be a loopback name.
    pub(crate) loopback: bool,
    /// Canonical folders the API may browse and put synced folders under.
    pub(crate) browse_roots: Vec<PathBuf>,
    /// The `--local-root` folders (canonical); empty when none were given.
    pub(crate) pair_roots: Vec<PathBuf>,
    pub(crate) published: watch::Receiver<Arc<publish::Published>>,
    pub(crate) revisions: Arc<publish::Revisions>,
    pub(crate) log: Logger,
    /// One config change at a time, start to finish.
    pub(crate) config_lock: tokio::sync::Mutex<()>,
    /// One sign-in call at a time: the sidecar keeps a single pending login.
    pub(crate) auth_lock: tokio::sync::Mutex<()>,
    /// Checking "busy" and starting a run happen as one step under this.
    pub(crate) start_lock: Mutex<()>,
    /// When the account was last checked.
    pub(crate) account_checked: Mutex<Instant>,
    refused_classes: Mutex<HashSet<&'static str>>,
}

impl Ctx {
    /// Note a refused request: one stderr line per kind of refusal, never the
    /// request itself.
    pub(crate) fn refused(&self, class: &'static str) {
        if self.refused_classes.lock().unwrap().insert(class) {
            eprintln!("neutronsync serve: refusing requests: {class}");
        }
        // Info level: the logger is quiet on the console, so this reaches
        // sync.log only and stderr keeps its one line per kind.
        self.log
            .info(&format!("serve: refused a request ({class})"));
    }

    /// The pairs that take part in auto sync, as the desktop window picks
    /// them: every pair with `auto = true`.
    pub(crate) fn auto_names(cfg: &Config) -> Vec<String> {
        cfg.pairs
            .iter()
            .filter(|p| p.auto)
            .map(|p| p.name.clone())
            .collect()
    }
}

fn read_token(opts: &ServeOptions) -> Result<Option<String>> {
    let token = match &opts.token_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the token file {}", path.display()))?;
            Some(text.trim().to_string())
        }
        None => std::env::var("NEUTRONSYNC_API_TOKEN")
            .ok()
            .map(|t| t.trim().to_string()),
    };
    let token = token.filter(|t| !t.is_empty());
    if let Some(t) = &token {
        if t.chars().count() < MIN_TOKEN_LEN {
            bail!("The API token is too short. Use at least {MIN_TOKEN_LEN} characters.");
        }
    }
    Ok(token)
}

/// D3: a listen address that is not loopback needs a token.
fn check_listen(listen: SocketAddr, token: Option<&str>) -> Result<()> {
    if token.is_none() && !listen.ip().is_loopback() {
        bail!(
            "Refusing to listen on {listen} without an API token. Set NEUTRONSYNC_API_TOKEN or --token-file."
        );
    }
    Ok(())
}

fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|r| {
            std::fs::canonicalize(r)
                .with_context(|| format!("--local-root {} is not a folder", r.display()))
        })
        .collect()
}

/// Run the server until SIGTERM or SIGINT. Exit 2 for a bad start (no token
/// on a public address, a short token, a bad config), 1 when another
/// NeutronSync already owns the state directory, 0 after a clean stop.
pub fn run(opts: ServeOptions) -> Result<ExitCode> {
    let listen: SocketAddr = opts.listen.parse().with_context(|| {
        format!(
            "--listen {} is not an address and port such as 127.0.0.1:8090",
            opts.listen
        )
    })?;
    let token = read_token(&opts)?;
    check_listen(listen, token.as_deref())?;
    let pair_roots = canonical_roots(&opts.local_roots)?;
    let browse_roots = if pair_roots.is_empty() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; pass --local-root")?;
        canonical_roots(&[home])?
    } else {
        pair_roots.clone()
    };

    let cfg = config::load(opts.config.as_deref())?;
    let config_path = cfg
        .source_path
        .clone()
        .context("the config file has no path")?;

    // D4: one owner per state directory.
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("cannot create {}", cfg.state_dir.display()))?;
    let serve_lock = cfg.state_dir.join(SERVE_LOCK);
    let tray_lock = cfg.state_dir.join(TRAY_LOCK);
    if crate::process_lock::is_alive(&serve_lock) || crate::process_lock::is_alive(&tray_lock) {
        eprintln!(
            "Another NeutronSync is already syncing this state (the desktop tray or another serve). Stop it first."
        );
        return Ok(ExitCode::from(1));
    }
    crate::process_lock::write(&serve_lock)
        .with_context(|| format!("cannot write {}", serve_lock.display()))?;
    let _lock = LockGuard(serve_lock);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the server runtime")?;
    let result = runtime.block_on(serve(
        cfg,
        config_path,
        listen,
        token,
        pair_roots,
        browse_roots,
    ));
    // A sidecar call still in flight must not hold the exit up.
    runtime.shutdown_timeout(Duration::from_millis(200));
    result.map(|()| ExitCode::SUCCESS)
}

/// Removes `serve.lock` however `run` returns.
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        crate::process_lock::remove_owned(&self.0);
    }
}

async fn serve(
    cfg: Config,
    config_path: PathBuf,
    listen: SocketAddr,
    token: Option<String>,
    pair_roots: Vec<PathBuf>,
    browse_roots: Vec<PathBuf>,
) -> Result<()> {
    let log = Logger::new(&cfg.log_dir(), false, true);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let auto = cfg.auto_sync;
    let ctrl = Arc::new(tokio::task::spawn_blocking(move || Controller::new(cfg)).await?);
    if auto {
        let names = Ctx::auto_names(&ctrl.config());
        if !names.is_empty() {
            ctrl.start_watch(names);
        }
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let revisions = Arc::new(publish::Revisions::new(ctrl.clone()));
    let (published, publisher) = publish::start(revisions.clone(), shutdown_rx.clone());
    let ctx = Arc::new(Ctx {
        ctrl: ctrl.clone(),
        config_path,
        token,
        loopback: listen.ip().is_loopback(),
        browse_roots,
        pair_roots,
        published,
        revisions,
        log: log.clone(),
        config_lock: tokio::sync::Mutex::new(()),
        auth_lock: tokio::sync::Mutex::new(()),
        start_lock: Mutex::new(()),
        account_checked: Mutex::new(Instant::now()),
        refused_classes: Mutex::new(HashSet::new()),
    });
    let app = routes::router(ctx.clone());

    let local = listener.local_addr().unwrap_or(listen);
    eprintln!(
        "neutronsync serve: listening on http://{local}/api/v{API_VERSION}{}",
        if ctx.token.is_some() {
            " (API token required)"
        } else {
            ""
        }
    );
    log.info(&format!("serve: listening on {local}"));

    let mut stop_rx = shutdown_rx.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.wait_for(|stop| *stop).await;
            })
            .await
    });

    wait_for_signal().await;
    let started = Instant::now();
    eprintln!("neutronsync serve: stopping");
    log.info("serve: stopping");
    let _ = shutdown_tx.send(true);
    let _ = publisher.await;
    // Open requests get a moment to finish; event streams have already ended.
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;

    let c = ctrl.clone();
    let finished = tokio::task::spawn_blocking(move || {
        c.stop_watch();
        c.cancel();
        let watch_lock = c.config().state_dir.join("watch.lock");
        while started.elapsed() < SHUTDOWN_WAIT {
            if !c.is_busy() && !crate::process_lock::is_alive(&watch_lock) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    })
    .await
    .unwrap_or(false);
    if finished {
        eprintln!("neutronsync serve: stopped");
        log.info("serve: stopped");
    } else {
        eprintln!("neutronsync serve: stopped while a sync was still finishing its current file");
        log.info("serve: stopped while a sync was still finishing its current file");
    }
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Stop auto sync, wait until the watcher has let go of `watch.lock`, then
/// start it again for the current auto pairs (or leave it off when `start`
/// is false or no pair takes part). Runs on a blocking thread.
pub(crate) fn restart_watch(ctrl: &Controller, start: bool) {
    let watch_lock = ctrl.config().state_dir.join("watch.lock");
    ctrl.stop_watch();
    if !start {
        return;
    }
    let t0 = Instant::now();
    while crate::process_lock::is_alive(&watch_lock) && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let names = Ctx::auto_names(&ctrl.config());
    if !names.is_empty() {
        ctrl.start_watch(names);
    }
}

/// Write `text` over `path` atomically: a temp file in the same folder,
/// flushed, given the old file's mode, then renamed over it.
pub(crate) fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".neutronsync-config-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-2 without binding a public address on the test machine: the
    /// refusal is decided before anything listens.
    #[test]
    fn public_addresses_need_a_long_enough_token() {
        let public: SocketAddr = "0.0.0.0:8090".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:8090".parse().unwrap();
        let v6: SocketAddr = "[::1]:8090".parse().unwrap();
        let err = check_listen(public, None).unwrap_err().to_string();
        assert_eq!(
            err,
            "Refusing to listen on 0.0.0.0:8090 without an API token. Set NEUTRONSYNC_API_TOKEN or --token-file."
        );
        assert!(check_listen(public, Some(&"t".repeat(32))).is_ok());
        assert!(check_listen(loopback, None).is_ok());
        assert!(check_listen(v6, None).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        let opts = |path: &Path| ServeOptions {
            config: None,
            listen: "127.0.0.1:0".into(),
            token_file: Some(path.to_path_buf()),
            local_roots: vec![],
        };
        std::fs::write(&file, format!("{}\n", "a".repeat(31))).unwrap();
        assert!(read_token(&opts(&file))
            .unwrap_err()
            .to_string()
            .contains("too short"));
        std::fs::write(&file, format!("  {}\n", "a".repeat(32))).unwrap();
        assert_eq!(read_token(&opts(&file)).unwrap(), Some("a".repeat(32)));
        std::fs::write(&file, "\n").unwrap();
        assert_eq!(read_token(&opts(&file)).unwrap(), None);
    }
}
