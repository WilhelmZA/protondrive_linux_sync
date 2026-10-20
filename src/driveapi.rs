//! JSON-RPC backend. All instances share one lazy sidecar.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::changefeed::{FeedMessage, RemoteChangeFeed, Subscription};
use crate::config::{remote_join, Config, SessionStore};
use crate::logger::Logger;
use crate::models::{DownloadJob, Entry, TreeScan, Uploaded};
use crate::remote::{is_safe_component, ListOutcome, Remote};

pub const SIDECAR_NAME: &str = "neutronsync-drive";
/// A call fails after this long with no reply. For `file.upload` and
/// `file.download` it is an idle timeout: every `transfer.progress`
/// notification for the call starts it again, so a slow but moving transfer
/// is never cut off.
const CALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Longest single wait before progress is checked.
const WAIT_SLICE: Duration = Duration::from_secs(5);

#[cfg(test)]
pub(crate) static TEST_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

fn call_timeout() -> Duration {
    #[cfg(test)]
    {
        let ms = TEST_TIMEOUT_MS.load(Ordering::Relaxed);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    CALL_TIMEOUT
}

#[derive(Debug)]
pub struct RpcError {
    pub code: String,
    pub retry_after: Option<u64>,
}

impl RpcError {
    fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            retry_after: None,
        }
    }
    fn from_value(error: &Value) -> Self {
        Self {
            code: error
                .pointer("/data/code")
                .and_then(Value::as_str)
                .unwrap_or("fatal")
                .into(),
            retry_after: error.pointer("/data/retry_after").and_then(Value::as_u64),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.code == "auth" {
            write!(f, "api backend: not logged in (sign in with the sidecar)")
        } else {
            write!(f, "api backend: {}", self.code)
        }
    }
}
impl std::error::Error for RpcError {}

fn is_code(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|e| e.code == code)
}

type Reply = std::result::Result<Value, RpcError>;

struct Client {
    binary: PathBuf,
    stdin: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Option<Child>>,
    socket: Option<UnixStream>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    /// When the sidecar last reported progress for a pending transfer.
    progress: Mutex<HashMap<u64, Instant>>,
    walk_sink: Mutex<Option<mpsc::Sender<Value>>>,
    feed: Mutex<VecDeque<FeedMessage>>,
    // Notifications have no request id, so only one walk can be in flight.
    walk_lock: Mutex<()>,
    retry_at: Mutex<Option<Instant>>,
    dead: AtomicBool,
}

/// Environment the spawned sidecar needs beyond what it inherits. With
/// `session_store = "file"` it selects the encrypted session file under
/// `state_dir` and, when configured, the key file. A variable already set on
/// this process wins, so a container can choose the store with env alone.
/// With `keyring` nothing is added and desktops behave as before.
pub fn sidecar_env(cfg: &Config) -> Vec<(String, String)> {
    if cfg.session_store != SessionStore::File {
        return Vec::new();
    }
    let mut wanted = vec![
        ("NEUTRONSYNC_SESSION_STORE".to_string(), "file".to_string()),
        (
            "NEUTRONSYNC_SESSION_FILE".to_string(),
            cfg.state_dir.join("session.enc").display().to_string(),
        ),
    ];
    if let Some(key) = &cfg.session_key_file {
        wanted.push((
            "NEUTRONSYNC_SESSION_KEY_FILE".to_string(),
            key.display().to_string(),
        ));
    }
    wanted
        .into_iter()
        .filter(|(name, _)| std::env::var_os(name).is_none())
        .collect()
}

impl Client {
    fn spawn(binary: &Path, env: &[(String, String)]) -> Result<Arc<Self>> {
        if std::env::var_os("NEUTRONSYNC_DRIVE_STDIO").is_none() {
            let socket = connect_session(binary, env)?;
            let read = socket.try_clone()?;
            let write = socket.try_clone()?;
            return Ok(Self::start(
                binary,
                Box::new(write),
                None,
                Some(socket),
                read,
            ));
        }
        // Explicit fixture/spike compatibility. Production clients use the socket.
        let mut child = Command::new(binary)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("cannot start cli.sidecar {}: {e}", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("sidecar stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("sidecar stdout unavailable"))?;
        Ok(Self::start(
            binary,
            Box::new(stdin),
            Some(child),
            None,
            stdout,
        ))
    }

    fn start(
        binary: &Path,
        stdin: Box<dyn Write + Send>,
        child: Option<Child>,
        socket: Option<UnixStream>,
        stdout: impl std::io::Read + Send + 'static,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            binary: binary.into(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            socket,
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            walk_sink: Mutex::new(None),
            feed: Mutex::new(VecDeque::new()),
            walk_lock: Mutex::new(()),
            retry_at: Mutex::new(None),
            dead: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&client);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Some(reader) = weak.upgrade() else { return };
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                reader.dispatch(msg);
            }
            if let Some(reader) = weak.upgrade() {
                reader.fail();
            }
        });
        client
    }

    fn fail(&self) {
        // Registration checks dead under this same lock: EOF cannot race a
        // caller into adding an orphaned pending request after the drain.
        {
            let mut pending = self.pending.lock().unwrap();
            self.dead.store(true, Ordering::SeqCst);
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(RpcError::new("transient")));
            }
        }
        self.progress.lock().unwrap().clear();
        *self.walk_sink.lock().unwrap() = None;
        self.stop();
    }

    fn stop(&self) {
        let mut child = self.child.lock().unwrap();
        if let Some(child) = child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    fn dispatch(&self, msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let reply = if let Some(error) = msg.get("error") {
                let error = RpcError::from_value(error);
                if error.code == "auth" {
                    self.feed.lock().unwrap().push_back(FeedMessage::SignedOut);
                }
                if error.code == "rate_limited" {
                    *self.retry_at.lock().unwrap() = Instant::now()
                        .checked_add(Duration::from_secs(error.retry_after.unwrap_or(1)));
                }
                Err(error)
            } else if let Some(result) = msg.get("result") {
                Ok(result.clone())
            } else {
                Err(RpcError::new("fatal"))
            };
            if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                let _ = tx.send(reply);
            }
        } else if msg.get("method").and_then(Value::as_str) == Some("walk.entry") {
            if let Some(tx) = &*self.walk_sink.lock().unwrap() {
                let _ = tx.send(msg.get("params").cloned().unwrap_or(Value::Null));
            }
        } else if msg.get("method").and_then(Value::as_str) == Some("transfer.progress") {
            // Only the timeout reads this; progress is not forwarded anywhere.
            if let Some(id) = msg["params"]["id"].as_u64() {
                if self.pending.lock().unwrap().contains_key(&id) {
                    self.progress.lock().unwrap().insert(id, Instant::now());
                }
            }
        } else if msg.get("method").and_then(Value::as_str) == Some("auth.signed_out") {
            self.feed.lock().unwrap().push_back(FeedMessage::SignedOut);
        } else if msg.get("method").and_then(Value::as_str) == Some("events.batch") {
            if let Ok(batch) = serde_json::from_value(msg["params"].clone()) {
                self.feed
                    .lock()
                    .unwrap()
                    .push_back(FeedMessage::Batch(batch));
            }
        } else if msg.get("method").and_then(Value::as_str) == Some("events.refresh_required") {
            if let (Some(scope), Some(reason)) = (
                msg["params"]["scope_id"].as_str(),
                msg["params"]["reason"].as_str(),
            ) {
                self.feed.lock().unwrap().push_back(FeedMessage::Refresh {
                    scope_id: scope.into(),
                    reason: reason.into(),
                });
            }
        }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let retry_at = *self.retry_at.lock().unwrap();
        if let Some(at) = retry_at {
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.dead.load(Ordering::SeqCst) {
                bail!(RpcError::new("transient"));
            }
            pending.insert(id, tx);
        }
        let line = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let sent = {
            let mut stdin = self.stdin.lock().unwrap();
            writeln!(stdin, "{line}").and_then(|_| stdin.flush())
        };
        if sent.is_err() {
            self.fail();
        }
        let timeout = call_timeout();
        let idle = matches!(method, "file.upload" | "file.download");
        let mut deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout((deadline - now).min(WAIT_SLICE)) {
                Ok(reply) => {
                    self.progress.lock().unwrap().remove(&id);
                    return Ok(reply?);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if idle {
                        if let Some(at) = self.progress.lock().unwrap().get(&id) {
                            deadline = deadline.max(*at + timeout);
                        }
                    }
                }
            }
        }
        // A timed-out walk must not leak notifications into the next walk.
        self.fail();
        Err(RpcError::new("transient").into())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

fn shared(binary: &Path, env: &[(String, String)]) -> Result<Arc<Client>> {
    static SHARED: OnceLock<Mutex<Option<Arc<Client>>>> = OnceLock::new();
    let mut slot = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(client) = &*slot {
        let exited = client
            .child
            .lock()
            .unwrap()
            .as_mut()
            .map(|c| c.try_wait())
            .transpose()?
            .flatten()
            .is_some();
        if !client.dead.load(Ordering::SeqCst) && !exited {
            if client.binary != binary {
                bail!("cli.sidecar differs from the sidecar already running in this process");
            }
            return Ok(Arc::clone(client));
        }
        client.fail(); // Reap the old process before any replacement can start.
        std::thread::sleep(Duration::from_millis(100));
    }
    let client = Client::spawn(binary, env)?;
    *slot = Some(Arc::clone(&client));
    Ok(client)
}

fn session_socket() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR and HOME are both unset"))?;
    let dir = base.join("neutronsync-drive");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let metadata = std::fs::symlink_metadata(&dir)?;
    // The server repeats this check and verifies SO_PEERCRED before reading RPC.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        bail!("unsafe sidecar socket directory: {}", dir.display());
    }
    Ok(dir.join("session.sock"))
}

fn connect_session(binary: &Path, env: &[(String, String)]) -> Result<UnixStream> {
    let path = session_socket()?;
    let canonical = std::fs::canonicalize(binary)?;
    let started = Instant::now();
    let mut launched: Option<Child> = None;
    loop {
        match UnixStream::connect(&path) {
            Ok(mut socket) => {
                socket.set_read_timeout(Some(Duration::from_secs(10)))?;
                // Read only the greeting, without buffering the next RPC frame.
                let mut line = Vec::new();
                use std::io::Read;
                for _ in 0..16384 {
                    let mut buffer = [0u8; 1];
                    socket.read_exact(&mut buffer)?;
                    let byte = buffer[0];
                    if byte == b'\n' {
                        break;
                    }
                    line.push(byte);
                }
                let hello: Value = serde_json::from_slice(&line)?;
                if hello["method"] != "sidecar.hello"
                    || hello["params"]["binary"].as_str() != canonical.to_str()
                {
                    bail!(
                        "cli.sidecar differs from the sidecar already running in this user session"
                    );
                }
                socket.set_read_timeout(None)?;
                if let Some(mut child) = launched.take() {
                    // The session owner outlives its initiating GUI/CLI.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                return Ok(socket);
            }
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(error.into())
            }
            Err(_) => {}
        }
        if started.elapsed() > Duration::from_secs(20) {
            bail!("cannot connect to {SIDECAR_NAME}; check secret-tool and the Secret Service session");
        }
        let exited = launched
            .as_mut()
            .map(|c| c.try_wait())
            .transpose()?
            .flatten()
            .is_some();
        if launched.is_none() || exited {
            launched = Some(
                Command::new(binary)
                    .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .arg("--socket")
                    .arg(&path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(
                        if std::env::var("NEUTRONSYNC_DRIVE_TRACE").as_deref() == Ok("1") {
                            Stdio::inherit()
                        } else {
                            Stdio::null()
                        },
                    )
                    .spawn()?,
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Configured path, then executable sibling, then PATH.
pub fn resolve_sidecar(cfg: &Config) -> Option<PathBuf> {
    if let Some(path) = &cfg.sidecar {
        return executable(path).then(|| path.clone());
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        let path = dir.join(SIDECAR_NAME);
        if executable(&path) {
            return Some(path);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(SIDECAR_NAME))
            .find(|p| executable(p))
    })
}

fn to_entry(raw: &Value) -> Option<Entry> {
    let name = raw.get("name")?.as_str()?;
    if !is_safe_component(name) {
        return None;
    }
    let is_dir = match raw.get("type")?.as_str()? {
        "folder" => true,
        "file" => false,
        _ => return None,
    };
    Some(Entry {
        path: name.into(),
        is_dir,
        size: if is_dir {
            0
        } else {
            raw.get("size").and_then(Value::as_u64).unwrap_or(0)
        },
        mtime: if is_dir {
            None
        } else {
            raw.get("mtime").and_then(Value::as_i64)
        },
        sha1: if is_dir {
            None
        } else {
            raw.get("sha1").and_then(Value::as_str).map(String::from)
        },
        remote_id: raw.get("uid").and_then(Value::as_str).map(String::from),
    })
}

pub struct DriveApi {
    binary: PathBuf,
    /// Extra environment for the sidecar process (see [`sidecar_env`]).
    env: Vec<(String, String)>,
    download_threads: usize,
    /// Upload and download limits in bytes per second (0 = no limit).
    limits: (u64, u64),
    /// Set once `transfer.set_limits` has been sent from this instance. The
    /// lock also holds a second transfer back until the first has sent it.
    limits_sent: Mutex<bool>,
    feed_client: Mutex<Option<Arc<Client>>>,
}

impl DriveApi {
    pub fn new(cfg: &Config) -> Result<Self> {
        let binary = resolve_sidecar(cfg).ok_or_else(|| {
            anyhow!(
                "{SIDECAR_NAME} not found or not executable. Install the NeutronSync package, build sidecar/, or set cli.sidecar. For the legacy fallback set [cli] backend = \"cli\"."
            )
        })?;
        Ok(Self {
            binary,
            env: sidecar_env(cfg),
            download_threads: cfg.download_threads,
            limits: (
                cfg.upload_limit_kbps.saturating_mul(1024),
                cfg.download_limit_kbps.saturating_mul(1024),
            ),
            limits_sent: Mutex::new(false),
            feed_client: Mutex::new(None),
        }) // Lazy session and process.
    }

    fn client(&self) -> Result<Arc<Client>> {
        shared(&self.binary, &self.env)
    }

    /// Send the configured speed limits once, before this instance's first
    /// transfer. The engine builds a backend per run from a reloaded config, so
    /// a change in Settings reaches the sidecar on the next run. A limit must
    /// never block syncing: on any failure but `auth` the transfer goes ahead
    /// unthrottled after one warning.
    fn ensure_limits(&self, client: &Client) -> Result<()> {
        let mut sent = self.limits_sent.lock().unwrap();
        if *sent {
            return Ok(());
        }
        *sent = true;
        let (upload, download) = self.limits;
        match client.call(
            "transfer.set_limits",
            json!({"upload_bytes_per_sec": upload, "download_bytes_per_sec": download}),
        ) {
            Ok(_) => Ok(()),
            Err(e) if is_code(&e, "auth") => Err(e),
            Err(e) => {
                Logger::console(false, false).warn(&format!(
                    "speed limits not applied, syncing without them: {e}"
                ));
                Ok(())
            }
        }
    }

    pub fn status(&self) -> Result<Value> {
        self.client()?.call("auth.status", json!({}))
    }

    pub fn auth(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "auth.login" | "auth.submit_2fa" | "auth.submit_mailbox_password" | "auth.logout"
        ) {
            bail!("unsupported authentication method");
        }
        self.client()?.call(method, params)
    }

    fn resolve(client: &Client, path: &str) -> Result<String> {
        // Resolve afresh so a move, deletion or process restart cannot stale a UID cache.
        let result = client.call("node.resolve", json!({"path":path}))?;
        if result.get("type").and_then(Value::as_str) != Some("folder") {
            bail!(RpcError::new("conflict"));
        }
        result
            .get("uid")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("api backend: resolve returned no uid"))
    }

    fn node(client: &Client, path: &str) -> Result<String> {
        let result = client.call("node.resolve", json!({"path":path}))?;
        result
            .get("uid")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("api backend: resolve returned no uid"))
    }

    fn probe(&self, path: &str) -> Result<ListOutcome> {
        let client = self.client()?;
        let result = Self::resolve(&client, path)
            .and_then(|uid| client.call("node.list", json!({"uid":uid})));
        match result {
            Ok(result) => {
                let entries = result
                    .get("entries")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("api backend: list returned no entries"))?;
                Ok(ListOutcome::Listed(
                    entries.iter().filter_map(to_entry).collect(),
                ))
            }
            Err(e) if is_code(&e, "not_found") => Ok(ListOutcome::NotFound),
            Err(e) => Err(e),
        }
    }
}

fn split_remote(path: &str) -> Result<(&str, &str)> {
    let (parent, name) = path
        .trim_end_matches('/')
        .rsplit_once('/')
        .unwrap_or(("", path));
    if !is_safe_component(name) {
        bail!("api backend: unsafe name");
    }
    Ok((parent, name))
}

impl Remote for DriveApi {
    fn change_feed(&self) -> Option<&dyn RemoteChangeFeed> {
        Some(self)
    }
    fn read_only(&self) -> bool {
        false
    }
    fn list_dir(&self, path: &str) -> Result<Vec<Entry>> {
        match self.probe(path)? {
            ListOutcome::Listed(entries) => Ok(entries),
            ListOutcome::NotFound => Ok(Vec::new()),
        }
    }
    fn list_dir_probe(&self, path: &str) -> Result<ListOutcome> {
        self.probe(path)
    }
    fn create_folder(&self, parent: &str, name: &str) -> Result<()> {
        if !is_safe_component(name) {
            bail!("api backend: unsafe name");
        }
        let client = self.client()?;
        let uid = Self::resolve(&client, parent)?;
        client.call("node.create_folder", json!({"parent_uid":uid,"name":name}))?;
        Ok(())
    }
    fn upload(&self, local: &str, parent: &str) -> Result<Option<Uploaded>> {
        let name = Path::new(local)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("api backend: unsafe name"))?;
        if !is_safe_component(name) {
            bail!("api backend: unsafe name");
        }
        let client = self.client()?;
        self.ensure_limits(&client)?;
        let uid = Self::resolve(&client, parent)?;
        let result = client.call(
            "file.upload",
            json!({"parent_uid":uid,"name":name,"local_path":local}),
        )?;
        // The sidecar uploads an immutable snapshot and reports its size,
        // mtime and sha1: exactly what is now on the remote.
        Ok(result
            .get("size")
            .and_then(Value::as_u64)
            .map(|size| Uploaded {
                size,
                mtime: result.get("mtime").and_then(Value::as_i64),
                sha1: result
                    .get("sha1")
                    .and_then(Value::as_str)
                    .map(str::to_lowercase),
            }))
    }
    fn download(&self, remote: &str, dest: &str) -> Result<()> {
        let (_, name) = split_remote(remote)?;
        let client = self.client()?;
        self.ensure_limits(&client)?;
        let uid = Self::node(&client, remote)?;
        client.call(
            "file.download",
            json!({"uid":uid,"local_path":Path::new(dest).join(name)}),
        )?;
        Ok(())
    }
    fn trash(&self, remote: &str) -> Result<()> {
        let client = self.client()?;
        let uid = Self::node(&client, remote)?;
        client.call("node.trash", json!({"uids":[uid]}))?;
        Ok(())
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (fp, fname) = split_remote(from)?;
        let (tp, tname) = split_remote(to)?;
        let client = self.client()?;
        let uid = Self::node(&client, from)?;
        if fp != tp {
            let parent = Self::resolve(&client, tp)?;
            client.call("node.move", json!({"uid":uid,"new_parent_uid":parent}))?;
        }
        if fp == tp || fname != tname {
            client.call("node.rename", json!({"uid":uid,"new_name":tname}))?;
        }
        Ok(())
    }

    fn download_many(
        &self,
        jobs: &[DownloadJob],
        threads: usize,
        cancel: &AtomicBool,
        on_start: &(dyn Fn(&DownloadJob) + Sync),
        on_done: &(dyn Fn(&DownloadJob, Result<()>) + Sync),
    ) {
        let workers = if threads > 0 {
            threads
        } else if self.download_threads > 0 {
            self.download_threads
        } else {
            4
        }
        .clamp(1, 8);
        let queue: Mutex<VecDeque<_>> = Mutex::new(jobs.iter().collect());
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let job = {
                        let mut queue = queue.lock().unwrap();
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        queue.pop_front()
                    };
                    let Some(job) = job else { break };
                    on_start(job);
                    on_done(job, self.download(&job.remote_path, &job.dest_dir));
                });
            }
        });
    }

    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        let client = self.client()?;
        let root = match Self::resolve(&client, base) {
            Ok(root) => root,
            Err(e) if is_code(&e, "not_found") => {
                return Ok(TreeScan {
                    root_missing: true,
                    ..TreeScan::default()
                })
            }
            Err(e) => return Err(e),
        };
        let _walk = client.walk_lock.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        *client.walk_sink.lock().unwrap() = Some(tx);
        // The opaque engine closure cannot become globs. Phase 1 still lists
        // excluded subtrees remotely, paying their network and decryption cost.
        let result = client.call("node.walk", json!({"uid":root, "exclude_globs":[]}));
        *client.walk_sink.lock().unwrap() = None;
        match result {
            Err(e) if is_code(&e, "not_found") => Ok(TreeScan {
                root_missing: true,
                ..TreeScan::default()
            }),
            Err(e) => Err(e),
            Ok(summary) => {
                let failed = summary
                    .get("failed")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("api backend: walk returned no failed list"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(String::from)
                            .ok_or_else(|| anyhow!("api backend: invalid failed path"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut paths = HashMap::from([(root, (String::new(), false))]);
                let mut entries = Vec::new();
                progress(base); // Includes a genuinely empty walk root.
                for note in rx.try_iter() {
                    let parent = note
                        .get("parent_uid")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("api backend: walk entry has no parent"))?;
                    let raw = note
                        .get("entry")
                        .ok_or_else(|| anyhow!("api backend: walk entry missing"))?;
                    let Some(entry) = to_entry(raw) else { continue };
                    // Contract: every parent folder notification precedes its children.
                    let Some((parent_path, hidden)) = paths.get(parent) else {
                        continue;
                    };
                    let rel = if parent_path.is_empty() {
                        entry.path.clone()
                    } else {
                        format!("{parent_path}/{}", entry.path)
                    };
                    let hidden = *hidden || exclude(&rel);
                    if entry.is_dir {
                        let uid = entry
                            .remote_id
                            .as_ref()
                            .ok_or_else(|| anyhow!("api backend: folder has no uid"))?;
                        progress(&remote_join(base, &rel));
                        paths.insert(uid.clone(), (rel.clone(), hidden));
                    }
                    if !hidden {
                        entries.push((rel.clone(), Entry { path: rel, ..entry }));
                    }
                }
                Ok(TreeScan {
                    entries,
                    failed,
                    root_missing: false,
                })
            }
        }
    }
}

impl RemoteChangeFeed for DriveApi {
    fn subscribe(&self, scope: &str, since: Option<&str>) -> Result<Subscription> {
        let client = self.client()?;
        client.feed.lock().unwrap().clear();
        *self.feed_client.lock().unwrap() = Some(Arc::clone(&client));
        let mut params = json!({"scope_id":scope,"ack_required":true});
        if let Some(id) = since {
            params["since_event_id"] = json!(id);
        }
        let result = client.call("events.subscribe", params)?;
        Ok(Subscription {
            scope_id: result["scope_id"]
                .as_str()
                .ok_or_else(|| anyhow!("missing event scope"))?
                .into(),
            last_event_id: result["last_event_id"].as_str().map(String::from),
        })
    }
    fn next_batch(&self) -> Result<Option<FeedMessage>> {
        let slot = self.feed_client.lock().unwrap();
        let client = slot
            .as_ref()
            .ok_or_else(|| anyhow!("feed is not subscribed"))?;
        if client.dead.load(Ordering::SeqCst) {
            bail!(RpcError::new("transient"));
        }
        let message = client.feed.lock().unwrap().pop_front();
        Ok(message)
    }
    fn ack(&self, scope: &str, event_id: &str) -> Result<()> {
        self.client()?
            .call("events.ack", json!({"scope_id":scope,"event_id":event_id}))?;
        Ok(())
    }
    fn resolve_root(&self, path: &str) -> Result<String> {
        Self::resolve(self.client()?.as_ref(), path)
    }
    fn node_path(&self, uid: &str) -> Result<String> {
        let result = self.client()?.call("node.path", json!({"uid":uid}))?;
        let path = result["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing node path"))?
            .to_string();
        if !path.starts_with("/my-files") || !path.split('/').skip(1).all(is_safe_component) {
            bail!(RpcError::new("fatal"));
        }
        Ok(path)
    }
    fn signed_in(&self) -> Result<bool> {
        let status = self.status()?;
        if status["keyring_locked"] == true {
            bail!(RpcError::new("keyring_locked"));
        }
        Ok(status["signed_in"] == true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_entries_and_drops_unsafe_or_unknown_names() {
        let file = to_entry(&json!({"name":"a.txt","type":"file","size":42,"mtime":1700000000,"sha1":"ABC","uid":"u1"})).unwrap();
        assert_eq!(
            (file.path.as_str(), file.is_dir, file.size, file.mtime),
            ("a.txt", false, 42, Some(1_700_000_000))
        );
        assert_eq!(file.sha1.as_deref(), Some("ABC"));
        assert_eq!(file.remote_id.as_deref(), Some("u1"));
        let folder = to_entry(
            &json!({"name":"dir","type":"folder","size":9,"mtime":5,"sha1":"bad","uid":"u2"}),
        )
        .unwrap();
        assert_eq!(folder.path, "dir");
        assert!(
            folder.is_dir && folder.size == 0 && folder.mtime.is_none() && folder.sha1.is_none()
        );
        assert_eq!(folder.remote_id.as_deref(), Some("u2"));
        for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(to_entry(&json!({"name":name,"type":"file","uid":"x"})).is_none());
        }
        assert!(to_entry(&json!({"name":"x","type":"album","uid":"x"})).is_none());
    }

    /// AC-20: the file store reaches the sidecar as three variables; the
    /// keyring store adds nothing; a variable already set on this process
    /// is left alone so a container can choose with env alone.
    #[test]
    fn sidecar_env_follows_the_session_store_setting() {
        let temp = tempfile::tempdir().unwrap();
        let cfg_path = temp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "[cli]\nsidecar = {:?}\n[options]\nstate_dir = {:?}\n",
                temp.path().join("sidecar"),
                temp.path().join("state")
            ),
        )
        .unwrap();
        let mut cfg = crate::config::load(cfg_path.to_str()).unwrap();
        assert!(sidecar_env(&cfg).is_empty());

        cfg.session_store = SessionStore::File;
        let env = sidecar_env(&cfg);
        assert_eq!(
            env,
            vec![
                ("NEUTRONSYNC_SESSION_STORE".to_string(), "file".to_string()),
                (
                    "NEUTRONSYNC_SESSION_FILE".to_string(),
                    temp.path().join("state/session.enc").display().to_string()
                ),
            ]
        );
        cfg.session_key_file = Some(temp.path().join("session.key"));
        let env = sidecar_env(&cfg);
        assert_eq!(env.len(), 3);
        assert_eq!(
            env[2],
            (
                "NEUTRONSYNC_SESSION_KEY_FILE".to_string(),
                temp.path().join("session.key").display().to_string()
            )
        );

        std::env::set_var("NEUTRONSYNC_SESSION_FILE", "/elsewhere/session.enc");
        let env = sidecar_env(&cfg);
        std::env::remove_var("NEUTRONSYNC_SESSION_FILE");
        assert!(env.iter().all(|(k, _)| k != "NEUTRONSYNC_SESSION_FILE"));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn auth_error_uses_existing_signout_classifier() {
        let error = RpcError::from_value(&json!({"data":{"code":"auth"}}));
        assert!(crate::remote::is_not_logged_in(&error.to_string()));
    }

    /// AC-6: a transfer that keeps reporting progress outlives the call
    /// timeout; one that goes quiet fails as `transient` once the timeout has
    /// passed with no progress. The fixture sidecar runs over stdio with a
    /// 1 s timeout injected under test.
    #[test]
    fn transfer_progress_restarts_the_call_timeout() {
        use std::os::unix::fs::PermissionsExt;
        std::env::set_var("NEUTRONSYNC_DRIVE_STDIO", "1");
        TEST_TIMEOUT_MS.store(1000, Ordering::Relaxed);
        let temp = tempfile::tempdir().unwrap();
        let sidecar = temp.path().join("sidecar");
        std::fs::write(&sidecar, include_str!("../tests/fixtures/drive_sidecar.py")).unwrap();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            temp.path().join("tree.json"),
            json!({
                "root": {"name":"my-files","type":"folder","parent_uid":null},
                "test": {"name":"test","type":"folder","parent_uid":"root"},
                "file": {"name":"file","type":"file","parent_uid":"test","data":"6869","size":2,"sha1":"x","mtime":1700000000}
            })
            .to_string(),
        )
        .unwrap();
        let local = temp.path().join("local");
        std::fs::create_dir(&local).unwrap();
        let cfg_path = temp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "[cli]\nsidecar = {:?}\n[options]\nstate_dir = {:?}\n",
                sidecar,
                temp.path().join("state")
            ),
        )
        .unwrap();
        let cfg = crate::config::load(cfg_path.to_str()).unwrap();
        let api = DriveApi::new(&cfg).unwrap();
        let controls = |value: Value| {
            std::fs::write(temp.path().join("controls.json"), value.to_string()).unwrap();
        };

        // Progress every 300 ms through a 3 s transfer: three times the timeout.
        controls(json!({"download_delay": 3, "download_progress_every": 0.3}));
        let started = Instant::now();
        api.download("/my-files/test/file", local.to_str().unwrap())
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(3));
        assert_eq!(std::fs::read(local.join("file")).unwrap(), b"hi");

        // Silence for longer than the timeout fails as transient, promptly.
        controls(json!({"download_delay": 4}));
        let started = Instant::now();
        let error = api
            .download("/my-files/test/file", local.to_str().unwrap())
            .unwrap_err();
        assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, "transient");
        assert!(
            started.elapsed() >= Duration::from_secs(1)
                && started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
        // Every request the fixture saw, in order: the limits call precedes
        // the first transfer and is sent once for this instance.
        let calls: Vec<String> = std::fs::read_to_string(temp.path().join("calls"))
            .unwrap()
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            calls.iter().filter(|m| *m == "transfer.set_limits").count(),
            1
        );
        assert!(
            calls
                .iter()
                .position(|m| m == "transfer.set_limits")
                .unwrap()
                < calls.iter().position(|m| m == "file.download").unwrap()
        );
        TEST_TIMEOUT_MS.store(0, Ordering::Relaxed);
        // The timed-out client was torn down; the sidecar is gone with it.
        let _ = api.status();
    }
}
