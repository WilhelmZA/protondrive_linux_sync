//! `neutronsync serve` and the desktop tray: one sync engine, driven over HTTP.
//!
//! One [`Controller`] owns the engine; this module translates HTTP requests
//! into its commands and publishes its state. The API lives under `/api/v1`
//! and is described in `docs/HTTP_API.md`. Live updates go out as server-sent
//! events from one publisher task (see [`publish`]).
//!
//! `neutronsync serve` binds TCP and, when `XDG_RUNTIME_DIR` is set, the unix
//! socket. The tray calls [`spawn_attached`] with the controller it already
//! owns, so the GTK thread is not the one that `block_on`s the server.
//!
//! The controller is synchronous. Every handler that may block (a sidecar
//! call, a config save, a database read) runs on `spawn_blocking`.

mod error;
mod files;
mod openapi;
mod publish;
mod recovery;
mod routes;
mod trash;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
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

/// Pretty-printed OpenAPI 3.1 JSON for this server, the same document as
/// `docs/openapi.json` and `GET /api/v1/openapi.json`.
pub fn openapi_json() -> String {
    openapi::json_pretty()
}

/// The API version in every path (`/api/v1`). A breaking change means v2.
pub const API_VERSION: u32 = 1;
/// Shortest API token accepted.
pub const MIN_TOKEN_LEN: usize = 32;
/// Largest request body accepted.
pub const BODY_LIMIT: usize = 64 * 1024;
/// Printed when `serve` or the tray finds the other already syncing this state.
pub const ENGINE_BUSY: &str = "Another NeutronSync is already syncing this state (the desktop tray or another serve). Stop it first.";
/// How long a shutdown waits for a running sync to stop before exiting. Kept
/// under the ten seconds a supervisor allows, with room for the rest of the
/// shutdown (a transfer already under way is not interrupted mid-file).
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
/// How long the tray waits for the API thread to finish after Quit.
const ATTACHED_SHUTDOWN: Duration = Duration::from_secs(2);
/// The desktop tray daemon's lock in `state_dir`.
const TRAY_LOCK: &str = "tray-daemon.lock";
/// This server's lock in `state_dir`.
const SERVE_LOCK: &str = "serve.lock";
const API_SOCK_DIR: &str = "neutronsync";
const API_SOCK_NAME: &str = "api.sock";

/// What `neutronsync serve` was started with.
pub struct ServeOptions {
    pub config: Option<String>,
    pub listen: String,
    pub token_file: Option<PathBuf>,
    pub local_roots: Vec<PathBuf>,
}

/// Where to bind. `None` means that listener is not started.
pub struct Listeners {
    pub unix: Option<PathBuf>,
    pub tcp: Option<SocketAddr>,
}

/// What the tray passes to [`spawn_attached`]. The controller already exists.
pub struct AttachOptions {
    pub listeners: Listeners,
    pub token: Option<String>,
    pub config_path: PathBuf,
    pub pair_roots: Vec<PathBuf>,
    pub browse_roots: Vec<PathBuf>,
}

/// Stops an API server started with [`spawn_attached`].
pub struct ServerHandle {
    shutdown_tx: watch::Sender<bool>,
    done: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ServerHandle {
    fn idle() -> Self {
        let (shutdown_tx, _rx) = watch::channel(false);
        ServerHandle {
            shutdown_tx,
            done: Mutex::new(None),
            thread: Mutex::new(None),
        }
    }

    /// Ask the listeners to stop and wait a couple of seconds for the thread.
    pub fn shutdown(self) {
        self.signal_and_wait();
    }

    fn signal_and_wait(&self) {
        let _ = self.shutdown_tx.send(true);
        let finished = self
            .done
            .lock()
            .unwrap()
            .take()
            .is_some_and(|done| done.recv_timeout(ATTACHED_SHUTDOWN).is_ok());
        if let Some(thread) = self.thread.lock().unwrap().take() {
            if finished {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.signal_and_wait();
    }
}

/// Engine state shared by the unix and TCP routers. The two [`Ctx`] values
/// differ only in token and Host rules, so a sync cannot be started twice
/// by one request on each listener.
pub(crate) struct Core {
    ctrl: Arc<Controller>,
    config_path: PathBuf,
    /// Canonical folders the API may browse and put synced folders under.
    browse_roots: Vec<PathBuf>,
    /// The `--local-root` folders (canonical); empty when none were given.
    pair_roots: Vec<PathBuf>,
    published: watch::Receiver<Arc<publish::Published>>,
    revisions: Arc<publish::Revisions>,
    log: Logger,
    /// One config change at a time, start to finish.
    config_lock: tokio::sync::Mutex<()>,
    /// One sign-in call at a time: the sidecar keeps a single pending login.
    auth_lock: tokio::sync::Mutex<()>,
    /// Checking "busy" and starting a run happen as one step under this.
    start_lock: Mutex<()>,
    /// When the account was last checked.
    account_checked: Mutex<Instant>,
    refused_classes: Mutex<HashSet<&'static str>>,
}

/// Per-listener view of [`Core`]. Unix sets `token` to `None` and `loopback`
/// to false: socket mode 0600 is the auth, and `Host: x` must be allowed.
pub(crate) struct Ctx {
    core: Arc<Core>,
    pub(crate) token: Option<String>,
    /// Listening on a loopback TCP address: `Host` must then be a loopback name.
    pub(crate) loopback: bool,
}

impl Deref for Ctx {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.core
    }
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

/// Read the API token. `token_file` wins when `Some` (the CLI `--token-file`).
/// Otherwise `NEUTRONSYNC_API_TOKEN` is used, then `fallback_file`
/// (`[cli] api_token_file`, for the tray). Empty means no token. A non-empty
/// token shorter than [`MIN_TOKEN_LEN`] is an error.
pub fn read_api_token(
    token_file: Option<&Path>,
    fallback_file: Option<&Path>,
) -> Result<Option<String>> {
    let token = if let Some(path) = token_file {
        Some(read_token_file(path)?)
    } else if let Some(token) = env_token() {
        Some(token)
    } else if let Some(path) = fallback_file {
        Some(read_token_file(path)?)
    } else {
        None
    };
    let token = token.filter(|t| !t.is_empty());
    if let Some(t) = &token {
        if t.chars().count() < MIN_TOKEN_LEN {
            bail!("The API token is too short. Use at least {MIN_TOKEN_LEN} characters.");
        }
    }
    Ok(token)
}

fn env_token() -> Option<String> {
    std::env::var("NEUTRONSYNC_API_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn read_token_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the token file {}", path.display()))
        .map(|text| text.trim().to_string())
}

/// Unix socket `neutronsync serve` binds when `XDG_RUNTIME_DIR` is set.
pub fn serve_socket_path() -> Option<PathBuf> {
    xdg_runtime_socket()
}

/// Unix socket the tray binds. Falls back to `/run/user/<uid>/neutronsync/api.sock`
/// when that user's runtime directory exists and `XDG_RUNTIME_DIR` is unset.
pub fn tray_socket_path() -> Option<PathBuf> {
    if let Some(path) = xdg_runtime_socket() {
        return Some(path);
    }
    let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    dir.is_dir()
        .then(|| dir.join(API_SOCK_DIR).join(API_SOCK_NAME))
}

fn xdg_runtime_socket() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(API_SOCK_DIR).join(API_SOCK_NAME))
}

/// True when `serve.lock` names a live process. The tray checks this before
/// taking `tray-daemon.lock`, and prints [`ENGINE_BUSY`] when it is.
pub fn serve_lock_alive(state_dir: &Path) -> bool {
    crate::process_lock::is_alive(&state_dir.join(SERVE_LOCK))
}

/// True when `serve.lock` or `tray-daemon.lock` names a live process.
pub fn engine_owner_alive(state_dir: &Path) -> bool {
    serve_lock_alive(state_dir) || crate::process_lock::is_alive(&state_dir.join(TRAY_LOCK))
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
    let token = read_api_token(opts.token_file.as_deref(), None)?;
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

    let cfg = match config::load(opts.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(err) => match recovery::wait_for_config(&opts, listen, token.clone(), err)? {
            Some(cfg) => cfg,
            None => return Ok(ExitCode::SUCCESS),
        },
    };
    let config_path = cfg
        .source_path
        .clone()
        .context("the config file has no path")?;

    // One owner per state directory: serve.lock or a live tray.
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("cannot create {}", cfg.state_dir.display()))?;
    if engine_owner_alive(&cfg.state_dir) {
        eprintln!("{ENGINE_BUSY}");
        return Ok(ExitCode::from(1));
    }
    let serve_lock = cfg.state_dir.join(SERVE_LOCK);
    crate::process_lock::write(&serve_lock)
        .with_context(|| format!("cannot write {}", serve_lock.display()))?;
    let _lock = LockGuard(serve_lock);

    let listeners = Listeners {
        unix: serve_socket_path(),
        tcp: Some(listen),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the server runtime")?;
    let result = runtime.block_on(async {
        // Bind before the controller so a busy port fails without a sidecar.
        let bound = bind_listeners(&listeners).await?;
        let log = Logger::new(&cfg.log_dir(), false, true);
        let auto = cfg.auto_sync;
        let ctrl = Arc::new(tokio::task::spawn_blocking(move || Controller::new(cfg)).await?);
        if auto {
            let names = Ctx::auto_names(&ctrl.config());
            if !names.is_empty() {
                ctrl.start_watch(names);
            }
        }
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        serve_bound(
            Running {
                ctrl,
                config_path,
                token,
                pair_roots,
                browse_roots,
                log,
                announce: "neutronsync serve",
                mode: ServeMode::Cli,
            },
            bound,
            shutdown_tx,
            shutdown_rx,
        )
        .await
    });
    // A sidecar call still in flight must not hold the exit up.
    runtime.shutdown_timeout(Duration::from_millis(200));
    result.map(|()| ExitCode::SUCCESS)
}

/// Tray entry: no lock (the supervisor holds `tray-daemon.lock`), the caller's
/// controller, and a background runtime. A non-loopback TCP address without a
/// token is not bound; the unix socket still starts. [`ServerHandle::shutdown`]
/// stops the listeners. Drop does the same.
pub fn spawn_attached(ctrl: Arc<Controller>, mut opts: AttachOptions) -> Result<ServerHandle> {
    if let Some(addr) = opts.listeners.tcp {
        if let Err(err) = check_listen(addr, opts.token.as_deref()) {
            eprintln!("{err:#}");
            opts.listeners.tcp = None;
        }
    }
    if opts.listeners.unix.is_none() && opts.listeners.tcp.is_none() {
        return Ok(ServerHandle::idle());
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_for_thread = shutdown_tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("neutronsync-api".into())
        .spawn(move || {
            let _done = Done(done_tx);
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("neutronsync: cannot start the API runtime: {err}");
                    return;
                }
            };
            let result = runtime.block_on(async move {
                let log = Logger::new(&ctrl.config().log_dir(), false, true);
                let bound = bind_listeners(&opts.listeners).await?;
                serve_bound(
                    Running {
                        ctrl,
                        config_path: opts.config_path,
                        token: opts.token,
                        pair_roots: opts.pair_roots,
                        browse_roots: opts.browse_roots,
                        log,
                        announce: "neutronsync",
                        mode: ServeMode::Attached,
                    },
                    bound,
                    shutdown_for_thread,
                    shutdown_rx,
                )
                .await
            });
            if let Err(err) = result {
                eprintln!("neutronsync: API not started: {err:#}");
            }
            runtime.shutdown_timeout(Duration::from_millis(200));
        })
        .context("cannot start the API thread")?;
    Ok(ServerHandle {
        shutdown_tx,
        done: Mutex::new(Some(done_rx)),
        thread: Mutex::new(Some(thread)),
    })
}

/// Sends on drop, including a panic, so [`ServerHandle::shutdown`] can return.
struct Done(std::sync::mpsc::Sender<()>);

impl Drop for Done {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

enum ServeMode {
    /// Wait for SIGTERM or SIGINT, then stop the engine.
    Cli,
    /// Wait until [`ServerHandle::shutdown`]. The caller stops the engine.
    Attached,
}

struct Running {
    ctrl: Arc<Controller>,
    config_path: PathBuf,
    token: Option<String>,
    pair_roots: Vec<PathBuf>,
    browse_roots: Vec<PathBuf>,
    log: Logger,
    announce: &'static str,
    mode: ServeMode,
}

struct Bound {
    unix: Option<(PathBuf, tokio::net::UnixListener)>,
    tcp: Option<(SocketAddr, tokio::net::TcpListener)>,
}

/// Removes `serve.lock` however `run` returns.
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        crate::process_lock::remove_owned(&self.0);
    }
}

fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        // The socket mode is the auth. The directory must not let anyone else
        // replace the socket out from under it.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot set permissions on {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("cannot replace {}", path.display()));
        }
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    Ok(listener)
}

async fn bind_listeners(listeners: &Listeners) -> Result<Bound> {
    let unix = match &listeners.unix {
        Some(path) => Some((path.clone(), bind_unix(path)?)),
        None => None,
    };
    let tcp = match listeners.tcp {
        Some(addr) => match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => Some((addr, listener)),
            Err(err) => {
                if let Some((path, listener)) = unix {
                    drop(listener);
                    let _ = std::fs::remove_file(path);
                }
                return Err(err).with_context(|| format!("cannot listen on {addr}"));
            }
        },
        None => None,
    };
    Ok(Bound { unix, tcp })
}

/// Axum's serve future only implements `IntoFuture` for a concrete listener,
/// so the two binds cannot share one generic function.
macro_rules! spawn_listener {
    ($listener:expr, $app:expr, $stop:expr) => {{
        let app = $app;
        let mut stop_rx = $stop;
        tokio::spawn(async move {
            let result = axum::serve($listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop_rx.wait_for(|stop| *stop).await;
                })
                .await;
            if let Err(err) = result {
                eprintln!("neutronsync: API listener ended: {err}");
            }
        })
    }};
}

async fn serve_bound(
    running: Running,
    bound: Bound,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let Running {
        ctrl,
        config_path,
        token,
        pair_roots,
        browse_roots,
        log,
        announce,
        mode,
    } = running;

    if let Some((addr, listener)) = &bound.tcp {
        let local = listener.local_addr().unwrap_or(*addr);
        eprintln!(
            "{announce}: listening on http://{local}/api/v{API_VERSION}{}",
            if token.is_some() {
                " (API token required)"
            } else {
                ""
            }
        );
        log.info(&format!("serve: listening on {local}"));
    }
    if let Some((path, _)) = &bound.unix {
        eprintln!("{announce}: listening on unix {}", path.display());
        log.info(&format!("serve: listening on unix {}", path.display()));
    }

    let revisions = Arc::new(publish::Revisions::new(ctrl.clone()));
    let (published, publisher) = publish::start(revisions.clone(), shutdown_rx.clone());
    let core = Arc::new(Core {
        ctrl: ctrl.clone(),
        config_path,
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

    let unix_path = bound.unix.as_ref().map(|(path, _)| path.clone());
    let mut tasks = Vec::new();
    if let Some((_path, listener)) = bound.unix {
        // Socket mode is the auth. loopback is false so Host `x` is allowed;
        // Origin and Sec-Fetch-Site are still refused.
        let ctx = Arc::new(Ctx {
            core: core.clone(),
            token: None,
            loopback: false,
        });
        tasks.push(spawn_listener!(
            listener,
            routes::router(ctx),
            shutdown_rx.clone()
        ));
    }
    if let Some((addr, listener)) = bound.tcp {
        let ctx = Arc::new(Ctx {
            core,
            token,
            loopback: addr.ip().is_loopback(),
        });
        tasks.push(spawn_listener!(
            listener,
            routes::router(ctx),
            shutdown_rx.clone()
        ));
    }

    // The eight-second engine wait includes the moment spent draining listeners,
    // so the whole stop stays under the ten seconds a supervisor allows.
    let stop_started = match mode {
        ServeMode::Cli => {
            wait_for_signal().await;
            let started = Instant::now();
            let _ = shutdown_tx.send(true);
            eprintln!("{announce}: stopping");
            log.info("serve: stopping");
            Some(started)
        }
        ServeMode::Attached => {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
            None
        }
    };
    let _ = publisher.await;
    // Open requests get a moment to finish; event streams have already ended.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        for task in tasks {
            let _ = task.await;
        }
    })
    .await;
    if let Some(path) = unix_path {
        let _ = std::fs::remove_file(path);
    }

    let Some(started) = stop_started else {
        return Ok(());
    };
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
        eprintln!("{announce}: stopped");
        log.info("serve: stopped");
    } else {
        eprintln!("{announce}: stopped while a sync was still finishing its current file");
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
        std::fs::write(&file, format!("{}\n", "a".repeat(31))).unwrap();
        assert!(read_api_token(Some(&file), None)
            .unwrap_err()
            .to_string()
            .contains("too short"));
        std::fs::write(&file, format!("  {}\n", "a".repeat(32))).unwrap();
        assert_eq!(
            read_api_token(Some(&file), None).unwrap(),
            Some("a".repeat(32))
        );
        std::fs::write(&file, "\n").unwrap();
        assert_eq!(read_api_token(Some(&file), None).unwrap(), None);
    }
}
