//! Phase 1 read-only JSON-RPC backend. All instances share one lazy sidecar.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::config::{remote_join, Config};
use crate::models::{Entry, TreeScan};
use crate::protoncli::{is_safe_component, ListOutcome, Remote};

pub const SIDECAR_NAME: &str = "neutronsync-drive";
const CALL_TIMEOUT: Duration = Duration::from_secs(900);

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
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    walk_sink: Mutex<Option<mpsc::Sender<Value>>>,
    // Notifications have no request id, so only one walk can be in flight.
    walk_lock: Mutex<()>,
    retry_at: Mutex<Option<Instant>>,
    dead: AtomicBool,
}

impl Client {
    fn spawn(binary: &Path) -> Result<Arc<Self>> {
        let mut child = Command::new(binary)
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
        let client = Arc::new(Self {
            binary: binary.into(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            walk_sink: Mutex::new(None),
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
        Ok(client)
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
        *self.walk_sink.lock().unwrap() = None;
        self.stop();
    }

    fn stop(&self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    fn dispatch(&self, msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let reply = if let Some(error) = msg.get("error") {
                let error = RpcError::from_value(error);
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
        } else if msg.get("method").and_then(Value::as_str) == Some("auth.signed_out") {
            eprintln!("api backend: auth.signed_out (not logged in)");
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
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(reply) => Ok(reply?),
            Err(_) => {
                // A timed-out walk must not leak notifications into the next walk.
                self.fail();
                Err(RpcError::new("transient").into())
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

fn shared(binary: &Path) -> Result<Arc<Client>> {
    static SHARED: OnceLock<Mutex<Option<Arc<Client>>>> = OnceLock::new();
    let mut slot = SHARED.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(client) = &*slot {
        let exited = client.child.lock().unwrap().try_wait()?.is_some();
        if !client.dead.load(Ordering::SeqCst) && !exited {
            if client.binary != binary {
                bail!("cli.sidecar differs from the sidecar already running in this process");
            }
            return Ok(Arc::clone(client));
        }
        client.fail(); // Reap the old process before any replacement can start.
        std::thread::sleep(Duration::from_millis(100));
    }
    let client = Client::spawn(binary)?;
    *slot = Some(Arc::clone(&client));
    Ok(client)
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
}

impl DriveApi {
    pub fn new(cfg: &Config) -> Result<Self> {
        let binary = resolve_sidecar(cfg).ok_or_else(|| {
            anyhow!(
                "{SIDECAR_NAME} not found or not executable. Build sidecar/ or set cli.sidecar."
            )
        })?;
        Ok(Self { binary }) // No session or process until the first read.
    }

    pub fn status(&self) -> Result<Value> {
        shared(&self.binary)?.call("auth.status", json!({}))
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

    fn probe(&self, path: &str) -> Result<ListOutcome> {
        let client = shared(&self.binary)?;
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

fn phase2(op: &str) -> anyhow::Error {
    anyhow!("api backend is read-only until Phase 2: {op} is not supported")
}

impl Remote for DriveApi {
    fn read_only(&self) -> bool {
        true
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
    fn create_folder(&self, _: &str, _: &str) -> Result<()> {
        Err(phase2("create_folder"))
    }
    fn upload(&self, _: &str, _: &str) -> Result<()> {
        Err(phase2("upload"))
    }
    fn download(&self, _: &str, _: &str) -> Result<()> {
        Err(phase2("download"))
    }
    fn trash(&self, _: &str) -> Result<()> {
        Err(phase2("trash"))
    }
    fn rename(&self, _: &str, _: &str) -> Result<()> {
        Err(phase2("rename"))
    }

    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<TreeScan> {
        let client = shared(&self.binary)?;
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

    #[test]
    fn auth_error_uses_existing_signout_classifier() {
        let error = RpcError::from_value(&json!({"data":{"code":"auth"}}));
        assert!(crate::protoncli::is_not_logged_in(&error.to_string()));
    }
}
