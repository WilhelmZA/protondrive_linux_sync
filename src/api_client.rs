//! Blocking HTTP/1.1 client for the desktop window, over the engine unix socket.
//!
//! No bearer token, no `Origin`, no `Sec-Fetch-Site`. `Host` is `x`, which the
//! unix listener allows. The window calls this from a background worker so an
//! egui frame never waits on the socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::models::{ConflictOutcome, Entry};
use crate::service::AppState;
use crate::stats::{ConflictRecord, HeldDeletes};

const HOST: &str = "x";
/// How long a successful health check, or a stream byte, keeps the engine "up".
const FRESH_MS: u64 = 10_000;

/// One HTTP response. `body` is the payload after chunked decoding.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// The live envelope the window renders. Held deletions and conflicts come
/// from the engine, not from a local database read.
#[derive(Clone, Debug)]
pub struct ClientState {
    pub rev: u64,
    pub state: AppState,
    pub held: Vec<HeldDeletes>,
    pub conflicts: Vec<ConflictRecord>,
    pub auto_sync: bool,
}

#[derive(Deserialize)]
struct WireEnvelope {
    rev: u64,
    state: AppState,
    #[serde(default)]
    held: Vec<WireHeld>,
    #[serde(default)]
    conflicts: Vec<WireConflict>,
    #[serde(default)]
    auto_sync: bool,
}

#[derive(Deserialize)]
struct WireHeld {
    pair: String,
    local: usize,
    remote: usize,
    tracked: usize,
    since: i64,
    updated: i64,
}

#[derive(Deserialize)]
struct WireConflict {
    id: i64,
    pair: String,
    path: String,
    outcome: String,
    copy: Option<String>,
    first_ts: i64,
    last_ts: i64,
    seen: i64,
    #[serde(default)]
    dismissed: bool,
}

#[derive(Deserialize)]
struct WirePair {
    name: String,
    local: String,
    remote: String,
    auto: bool,
    #[serde(default)]
    exclude: Vec<String>,
}

/// Percent-encode one path or query component.
pub fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `GET`/`POST`/… against `sock`. One connection, `Connection: close`.
pub fn request(
    sock: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Reply, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| {
        format!(
            "cannot connect to the engine socket {}: {e}",
            sock.display()
        )
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    write_request(&mut stream, method, path, body, true)?;
    read_reply(&mut stream)
}

fn write_request(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    close: bool,
) -> Result<(), String> {
    let body = body.unwrap_or(b"");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {HOST}\r\n");
    if close {
        head.push_str("Connection: close\r\n");
    } else {
        head.push_str("Accept: text/event-stream\r\n");
    }
    if !body.is_empty() || method != "GET" {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("cannot write the request: {e}"))?;
    stream
        .write_all(body)
        .map_err(|e| format!("cannot write the request: {e}"))?;
    Ok(())
}

fn read_reply(stream: &mut UnixStream) -> Result<Reply, String> {
    let (status, headers, mut raw) = read_headers(stream)?;
    let chunked = headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let length = content_length(&headers);
    let payload = if chunked {
        let mut dec = ChunkDecoder::default();
        dec.push(&raw)?;
        let mut buf = [0u8; 8192];
        while !dec.done {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => dec.push(&buf[..n])?,
                Err(e) => return Err(format!("cannot read the response: {e}")),
            }
        }
        dec.decoded
    } else if let Some(len) = length {
        while raw.len() < len {
            let mut buf = [0u8; 8192];
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) => return Err(format!("cannot read the response: {e}")),
            }
        }
        raw.truncate(len);
        raw
    } else {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) => return Err(format!("cannot read the response: {e}")),
            }
        }
        raw
    };
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(&payload).into_owned(),
    })
}

fn read_headers(stream: &mut UnixStream) -> Result<(u16, String, Vec<u8>), String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let split = loop {
        if let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if raw.len() > 1024 * 1024 {
            return Err("the response headers are too large".into());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Err("the engine closed the connection".into()),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => return Err(format!("cannot read the response: {e}")),
        }
    };
    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "the engine's status line was not HTTP".to_string())?;
    let rest = raw[split + 4..].to_vec();
    Ok((status, headers, rest))
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

#[derive(Default)]
struct ChunkDecoder {
    raw: Vec<u8>,
    decoded: Vec<u8>,
    done: bool,
}

impl ChunkDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        self.raw.extend_from_slice(bytes);
        while let Some(end) = self.raw.windows(2).position(|w| w == b"\r\n") {
            let line = std::str::from_utf8(&self.raw[..end])
                .map_err(|_| "a chunk size was not text".to_string())?;
            let size_txt = line.trim().split(';').next().unwrap_or("");
            let size = usize::from_str_radix(size_txt, 16)
                .map_err(|_| format!("a chunk size was not hex: {size_txt}"))?;
            let header = end + 2;
            if self.raw.len() < header + size + 2 {
                break;
            }
            if size == 0 {
                self.done = true;
                self.raw.clear();
                break;
            }
            self.decoded
                .extend_from_slice(&self.raw[header..header + size]);
            self.raw.drain(..header + size + 2);
        }
        Ok(())
    }
}

/// Split complete SSE blocks out of `buf` and return each `data:` payload.
pub fn drain_sse(buf: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    let normalized = buf.replace("\r\n", "\n");
    let mut rest = normalized.as_str();
    while let Some(idx) = rest.find("\n\n") {
        let block = &rest[..idx];
        rest = &rest[idx + 2..];
        let mut data = String::new();
        for line in block.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(payload.trim_start());
            }
        }
        if !data.is_empty() {
            out.push(data);
        }
    }
    *buf = rest.to_string();
    out
}

pub fn parse_state(body: &str) -> Result<ClientState, String> {
    let wire: WireEnvelope = serde_json::from_str(body)
        .map_err(|e| format!("the engine's state was not readable: {e}"))?;
    Ok(ClientState {
        rev: wire.rev,
        state: wire.state,
        held: wire
            .held
            .into_iter()
            .map(|h| HeldDeletes {
                pair: h.pair,
                local: h.local,
                remote: h.remote,
                tracked: h.tracked,
                since: h.since,
                updated: h.updated,
            })
            .collect(),
        conflicts: wire
            .conflicts
            .into_iter()
            .map(|c| ConflictRecord {
                id: c.id,
                pair: c.pair,
                path: c.path,
                outcome: outcome_from_api(&c.outcome),
                copy: c.copy,
                first_ts: c.first_ts,
                last_ts: c.last_ts,
                seen: c.seen,
                dismissed: c.dismissed,
            })
            .collect(),
        auto_sync: wire.auto_sync,
    })
}

fn outcome_from_api(s: &str) -> ConflictOutcome {
    match s {
        "kept_both" => ConflictOutcome::KeptBoth,
        "kept_local" => ConflictOutcome::KeptLocal,
        "kept_remote" => ConflictOutcome::KeptRemote,
        "skipped" => ConflictOutcome::Skipped,
        other => ConflictOutcome::parse(other).unwrap_or(ConflictOutcome::Skipped),
    }
}

/// Plain `error.message` from a failed reply, or a short status line.
pub fn error_message(reply: &Reply) -> String {
    serde_json::from_str::<Value>(&reply.body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("The engine replied {}.", reply.status))
}

pub fn ok_json(reply: Reply) -> Result<Value, String> {
    if !(200..300).contains(&reply.status) {
        return Err(error_message(&reply));
    }
    if reply.body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&reply.body).map_err(|e| format!("the engine's reply was not JSON: {e}"))
}

/// Folders from `GET /remote/list`.
pub fn remote_entries(value: &Value) -> Result<Vec<Entry>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "the folder list was not a list".to_string())?;
    Ok(arr
        .iter()
        .map(|e| Entry {
            path: e["name"].as_str().unwrap_or("").to_string(),
            is_dir: e["kind"].as_str() == Some("folder"),
            size: e["size"].as_u64().unwrap_or(0),
            mtime: e["mtime"].as_i64(),
            sha1: None,
            remote_id: None,
        })
        .collect())
}

/// Every settings key the desktop window may change, in the API spelling.
pub fn settings_value(cfg: &Config) -> Value {
    json!({
        "propagate_deletes": cfg.propagate_deletes,
        "max_delete_files": cfg.max_delete_files,
        "max_delete_percent": cfg.max_delete_percent,
        "local_delete": config::local_delete_str(cfg.local_delete),
        "conflict": config::conflict_str(cfg.conflict),
        "compare": config::compare_str(cfg.compare),
        "scan_threads": cfg.scan_threads,
        "download_threads": cfg.download_threads,
        "full_walk_interval": cfg.full_walk_interval,
        "scan_interval": cfg.scan_interval_secs,
        "debounce": cfg.debounce_secs,
        "upload_limit_kbps": cfg.upload_limit_kbps,
        "download_limit_kbps": cfg.download_limit_kbps,
        "auto_sync": cfg.auto_sync,
        "remote_root": cfg.remote_root,
        "run_in_tray": cfg.run_in_tray,
        "x11_compat": cfg.x11_compat,
        "notifications": cfg.notifications,
        "theme": config::theme_str(cfg.theme),
        "file_manager_bookmarks": cfg.file_manager_bookmarks,
        "update_channel": config::update_channel_str(cfg.update_channel),
        "check_on_launch": cfg.check_on_launch,
    })
}

fn pair_body(pair: &config::Pair) -> Value {
    json!({
        "name": pair.name,
        "local": pair.local.display().to_string(),
        "remote": pair.remote,
        "auto": pair.auto,
        "exclude": pair.exclude,
    })
}

fn expect_ok(reply: Reply) -> Result<(), String> {
    ok_json(reply).map(|_| ())
}

/// Write settings and the folder list. The engine parses them the same way
/// the config file does.
pub fn push_config(sock: &Path, cfg: &Config) -> Result<(), String> {
    let body = serde_json::to_vec(&settings_value(cfg)).map_err(|e| e.to_string())?;
    expect_ok(request(
        sock,
        "PUT",
        "/api/v1/settings",
        Some(&body),
        Duration::from_secs(30),
    )?)?;
    let listed = request(sock, "GET", "/api/v1/pairs", None, Duration::from_secs(30))?;
    let listed = ok_json(listed)?;
    let server: Vec<WirePair> = serde_json::from_value(listed)
        .map_err(|e| format!("the folder list was not readable: {e}"))?;
    for published in &server {
        if !cfg.pairs.iter().any(|p| p.name == published.name) {
            let path = format!("/api/v1/pairs/{}", encode_component(&published.name));
            let reply = request(sock, "DELETE", &path, None, Duration::from_secs(30))?;
            if reply.status != 404 {
                expect_ok(reply)?;
            }
        }
    }
    for wanted in &cfg.pairs {
        let same = server.iter().find(|p| p.name == wanted.name);
        match same {
            None => {
                let body = serde_json::to_vec(&pair_body(wanted)).map_err(|e| e.to_string())?;
                expect_ok(request(
                    sock,
                    "POST",
                    "/api/v1/pairs",
                    Some(&body),
                    Duration::from_secs(30),
                )?)?;
            }
            Some(published) if pair_differs(cfg, wanted, published) => {
                let body = serde_json::to_vec(&pair_body(wanted)).map_err(|e| e.to_string())?;
                let path = format!("/api/v1/pairs/{}", encode_component(&wanted.name));
                expect_ok(request(
                    sock,
                    "PUT",
                    &path,
                    Some(&body),
                    Duration::from_secs(30),
                )?)?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn pair_differs(cfg: &Config, wanted: &config::Pair, published: &WirePair) -> bool {
    let local = Path::new(&published.local) != wanted.local;
    let remote = published.remote != wanted.remote
        && config::remote_join(&cfg.remote_root, &wanted.remote) != published.remote;
    local || remote || published.auto != wanted.auto || published.exclude != wanted.exclude
}

pub fn push_and_sync(
    sock: &Path,
    cfg: &Config,
    sync: Option<(Vec<String>, bool)>,
) -> Result<(), String> {
    push_config(sock, cfg)?;
    if let Some((pairs, dry_run)) = sync {
        let body = serde_json::to_vec(&json!({"pairs": pairs, "dry_run": dry_run}))
            .map_err(|e| e.to_string())?;
        expect_ok(request(
            sock,
            "POST",
            "/api/v1/sync",
            Some(&body),
            Duration::from_secs(30),
        )?)?;
    }
    Ok(())
}

/// `POST /reset`, then remove each folder and turn auto sync off.
/// Reset itself leaves the folder list in the config.
pub fn reset_engine(sock: &Path, names: &[String]) -> Result<(), String> {
    expect_ok(request(
        sock,
        "POST",
        "/api/v1/reset",
        Some(br#"{"confirm":true}"#),
        Duration::from_secs(60),
    )?)?;
    for name in names {
        let path = format!("/api/v1/pairs/{}", encode_component(name));
        let reply = request(sock, "DELETE", &path, None, Duration::from_secs(30))?;
        if reply.status != 404 {
            expect_ok(reply)?;
        }
    }
    expect_ok(request(
        sock,
        "POST",
        "/api/v1/auto-sync",
        Some(br#"{"enabled":false}"#),
        Duration::from_secs(30),
    )?)?;
    Ok(())
}

enum Job {
    Call {
        method: String,
        path: String,
        body: Option<Vec<u8>>,
        reply: Sender<Result<Reply, String>>,
    },
    Push {
        cfg: Box<Config>,
        sync: Option<(Vec<String>, bool)>,
        reply: Sender<Result<(), String>>,
    },
    Reset {
        names: Vec<String>,
        reply: Sender<Result<(), String>>,
    },
}

/// Background socket worker. Drop stops both threads.
pub struct EngineIo {
    cmd_tx: Sender<Job>,
    state_rx: Receiver<ClientState>,
    last_health_ms: Arc<AtomicU64>,
    last_event_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    threads: Option<(JoinHandle<()>, JoinHandle<()>)>,
}

impl EngineIo {
    pub fn spawn(socket: PathBuf, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let last_health_ms = Arc::new(AtomicU64::new(0));
        let last_event_ms = Arc::new(AtomicU64::new(0));
        let repaint = Arc::new(repaint);

        let stop_job = Arc::clone(&stop);
        let health_job = Arc::clone(&last_health_ms);
        let sock_job = socket.clone();
        let jobs = thread::spawn(move || job_loop(sock_job, cmd_rx, stop_job, health_job));

        let stop_stream = Arc::clone(&stop);
        let events = Arc::clone(&last_event_ms);
        let stream = thread::spawn(move || {
            stream_loop(socket, state_tx, stop_stream, events, repaint);
        });

        EngineIo {
            cmd_tx,
            state_rx,
            last_health_ms,
            last_event_ms,
            stop,
            threads: Some((jobs, stream)),
        }
    }

    pub fn call(
        &self,
        method: impl Into<String>,
        path: impl Into<String>,
        body: Option<Vec<u8>>,
    ) -> Receiver<Result<Reply, String>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let job = Job::Call {
            method: method.into(),
            path: path.into(),
            body,
            reply: reply_tx,
        };
        self.send_or_fail(job, reply_rx, |tx| {
            let _ = tx.send(Err("The engine connection stopped.".into()));
        })
    }

    pub fn push(
        &self,
        cfg: Config,
        sync: Option<(Vec<String>, bool)>,
    ) -> Receiver<Result<(), String>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let job = Job::Push {
            cfg: Box::new(cfg),
            sync,
            reply: reply_tx,
        };
        self.send_or_fail(job, reply_rx, |tx| {
            let _ = tx.send(Err("The engine connection stopped.".into()));
        })
    }

    pub fn reset(&self, names: Vec<String>) -> Receiver<Result<(), String>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let job = Job::Reset {
            names,
            reply: reply_tx,
        };
        self.send_or_fail(job, reply_rx, |tx| {
            let _ = tx.send(Err("The engine connection stopped.".into()));
        })
    }

    fn send_or_fail<T>(
        &self,
        job: Job,
        rx: Receiver<Result<T, String>>,
        fail: impl FnOnce(Sender<Result<T, String>>),
    ) -> Receiver<Result<T, String>> {
        if self.cmd_tx.send(job).is_err() {
            let (tx, rx) = mpsc::channel();
            fail(tx);
            return rx;
        }
        rx
    }

    pub fn try_state(&self) -> Option<ClientState> {
        self.state_rx.try_recv().ok()
    }

    /// True when `/health` succeeded within the last 10 seconds.
    pub fn health_ok(&self) -> bool {
        let ms = self.last_health_ms.load(Ordering::Relaxed);
        ms != 0 && now_ms().saturating_sub(ms) <= FRESH_MS
    }

    /// Seconds since the last event-stream byte. `None` if the stream has
    /// never delivered one.
    pub fn events_quiet_secs(&self) -> Option<f64> {
        let ms = self.last_event_ms.load(Ordering::Relaxed);
        if ms == 0 {
            return None;
        }
        Some(now_ms().saturating_sub(ms) as f64 / 1000.0)
    }
}

impl Drop for EngineIo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some((jobs, stream)) = self.threads.take() {
            let _ = jobs.join();
            let _ = stream.join();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn job_loop(
    sock: PathBuf,
    rx: Receiver<Job>,
    stop: Arc<AtomicBool>,
    last_health_ms: Arc<AtomicU64>,
) {
    let mut last_probe = UNIX_EPOCH;
    while !stop.load(Ordering::SeqCst) {
        let due = last_probe.elapsed().unwrap_or(Duration::from_secs(3)) >= Duration::from_secs(2);
        let wait = if due {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(200)
        };
        match rx.recv_timeout(wait) {
            Ok(job) => run_job(&sock, job),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if due {
                    probe_health(&sock, &last_health_ms);
                    last_probe = SystemTime::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn probe_health(sock: &Path, last_health_ms: &AtomicU64) {
    if let Ok(reply) = request(sock, "GET", "/api/v1/health", None, Duration::from_secs(2)) {
        if reply.status == 200 {
            last_health_ms.store(now_ms(), Ordering::Relaxed);
        }
    }
}

fn run_job(sock: &Path, job: Job) {
    match job {
        Job::Call {
            method,
            path,
            body,
            reply,
        } => {
            let result = request(
                sock,
                &method,
                &path,
                body.as_deref(),
                Duration::from_secs(60),
            );
            let _ = reply.send(result);
        }
        Job::Push { cfg, sync, reply } => {
            let _ = reply.send(push_and_sync(sock, &cfg, sync));
        }
        Job::Reset { names, reply } => {
            let _ = reply.send(reset_engine(sock, &names));
        }
    }
}

fn stream_loop(
    sock: PathBuf,
    tx: Sender<ClientState>,
    stop: Arc<AtomicBool>,
    last_event_ms: Arc<AtomicU64>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    while !stop.load(Ordering::SeqCst) {
        if let Ok(reply) = request(&sock, "GET", "/api/v1/state", None, Duration::from_secs(5)) {
            if reply.status == 200 {
                if let Ok(state) = parse_state(&reply.body) {
                    last_event_ms.store(now_ms(), Ordering::Relaxed);
                    if tx.send(state).is_err() {
                        return;
                    }
                    repaint();
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let _ = follow_events(&sock, &stop, &last_event_ms, |body| {
            if let Ok(state) = parse_state(body) {
                if tx.send(state).is_ok() {
                    repaint();
                }
            }
        });
        if stop.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(300));
    }
}

/// Hold `GET /api/v1/events` until the socket closes, `stop` is set, or a
/// read sits idle. Each `data:` payload is handed to `on_data`.
fn follow_events(
    sock: &Path,
    stop: &AtomicBool,
    last_event_ms: &AtomicU64,
    mut on_data: impl FnMut(&str),
) -> Result<(), String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    write_request(&mut stream, "GET", "/api/v1/events", None, false)?;
    let (status, headers, leftover) = read_headers(&mut stream)?;
    if status != 200 {
        return Err(format!("the event stream replied {status}"));
    }
    let chunked = headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let mut dec = ChunkDecoder::default();
    let mut text = String::new();
    if chunked {
        dec.push(&leftover)?;
        text.push_str(&String::from_utf8_lossy(&dec.decoded));
        dec.decoded.clear();
    } else {
        text.push_str(&String::from_utf8_lossy(&leftover));
    }
    for data in drain_sse(&mut text) {
        last_event_ms.store(now_ms(), Ordering::Relaxed);
        on_data(&data);
    }
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::SeqCst) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                last_event_ms.store(now_ms(), Ordering::Relaxed);
                let bytes = if chunked {
                    dec.push(&buf[..n])?;
                    let got = dec.decoded.clone();
                    dec.decoded.clear();
                    got
                } else {
                    buf[..n].to_vec()
                };
                text.push_str(&String::from_utf8_lossy(&bytes));
                for data in drain_sse(&mut text) {
                    on_data(&data);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn serve(response: Vec<u8>) -> (tempfile::TempDir, PathBuf, thread::JoinHandle<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        req.extend_from_slice(&buf[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            sock.write_all(&response).unwrap();
            String::from_utf8_lossy(&req).into_owned()
        });
        (dir, path, handle)
    }

    #[test]
    fn unix_request_uses_host_x_and_no_browser_headers() {
        let body = br#"{"ok":true}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut response = raw.into_bytes();
        response.extend_from_slice(body);
        let (_dir, path, handle) = serve(response);
        let reply = request(&path, "GET", "/api/v1/health", None, Duration::from_secs(5)).unwrap();
        let seen = handle.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, r#"{"ok":true}"#);
        assert!(seen.contains("Host: x\r\n"), "{seen}");
        assert!(
            seen.starts_with("GET /api/v1/health HTTP/1.1\r\n"),
            "{seen}"
        );
        assert!(!seen.contains("Origin:"), "{seen}");
        assert!(!seen.contains("Sec-Fetch-Site"), "{seen}");
        assert!(!seen.contains("Authorization:"), "{seen}");
    }

    #[test]
    fn error_message_comes_from_the_json_body() {
        let payload = br#"{"error":{"code":"signed_out","message":"Signed out of Proton. Sign in on the Account page."}}"#;
        let raw = format!(
            "HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let mut response = raw.into_bytes();
        response.extend_from_slice(payload);
        let (_dir, path, handle) = serve(response);
        let reply = request(
            &path,
            "POST",
            "/api/v1/sync",
            Some(br#"{"pairs":[],"dry_run":false}"#),
            Duration::from_secs(5),
        )
        .unwrap();
        let _ = handle.join();
        let err = ok_json(reply).unwrap_err();
        assert_eq!(err, "Signed out of Proton. Sign in on the Account page.");
    }

    #[test]
    fn chunked_event_stream_yields_one_state() {
        let env = serde_json::json!({
            "rev": 7,
            "state": AppState::default(),
            "held": [],
            "conflicts": [],
            "auto_sync": true,
        });
        let frame = format!("event: state\ndata: {env}\n\n");
        let mut chunk = format!("{:x}\r\n", frame.len()).into_bytes();
        chunk.extend_from_slice(frame.as_bytes());
        chunk.extend_from_slice(b"\r\n0\r\n\r\n");
        let header = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut response = header.to_vec();
        response.extend_from_slice(&chunk);
        let (_dir, path, handle) = serve(response);
        let stop = AtomicBool::new(false);
        let last = AtomicU64::new(0);
        let mut got = Vec::new();
        follow_events(&path, &stop, &last, |data| got.push(data.to_string())).unwrap();
        let seen = handle.join().unwrap();
        assert!(seen.contains("GET /api/v1/events HTTP/1.1\r\n"), "{seen}");
        assert!(!seen.contains("Origin:"), "{seen}");
        assert_eq!(got.len(), 1, "{got:?}");
        let state = parse_state(&got[0]).unwrap();
        assert_eq!(state.rev, 7);
        assert!(state.auto_sync);
        assert!(state.held.is_empty());
    }

    #[test]
    fn sse_blocks_split_across_reads() {
        let mut buf = "event: state\ndata: {\"rev\":1}\n".to_string();
        assert!(drain_sse(&mut buf).is_empty());
        buf.push_str("\n: keepalive\n\n");
        let frames = drain_sse(&mut buf);
        assert_eq!(frames, vec![r#"{"rev":1}"#.to_string()]);
        assert!(buf.is_empty());
    }
}
