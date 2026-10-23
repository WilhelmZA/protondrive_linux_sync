//! `neutronsync serve` end to end: the real binary against the fake sidecar
//! over stdio, on a free loopback port, in a scratch world under the temp
//! directory. Nothing here touches a real config, keyring or Proton account,
//! and nothing listens on anything but 127.0.0.1.
#![cfg(feature = "serve")]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_neutronsync");
const PAIR: &str = "test";
const PASSWORD: &str = "serve-test-password-7Qx";

// ---- the scratch world -------------------------------------------------------

struct Lab {
    dir: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        Self::with_options("")
    }

    /// `extra` is appended to `[options]`.
    fn with_options(extra: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sidecar = root.join("sidecar");
        fs::write(&sidecar, include_str!("fixtures/drive_sidecar.py")).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o700)).unwrap();
        for d in ["local", "home", "runtime"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            root.join("config.toml"),
            format!(
                r#"
[cli]
backend = "api"
sidecar = {:?}
download_threads = 1
[options]
state_dir = {:?}
propagate_deletes = true
local_delete = "remove"
auto_sync = false
{extra}
[[pair]]
name = "test"
local = {:?}
remote = "/my-files/test"
"#,
                sidecar,
                root.join("state"),
                root.join("local")
            ),
        )
        .unwrap();
        fs::write(
            root.join("tree.json"),
            json!({
                "root": {"name":"my-files","type":"folder","parent_uid":null},
                "test": {"name":"test","type":"folder","parent_uid":"root"}
            })
            .to_string(),
        )
        .unwrap();
        Lab { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn state_dir(&self) -> PathBuf {
        self.path("state")
    }
    fn local_root(&self) -> PathBuf {
        self.path("local")
    }
    fn config_text(&self) -> String {
        fs::read_to_string(self.path("config.toml")).unwrap()
    }
    fn controls(&self, value: Value) {
        fs::write(self.path("controls.json"), value.to_string()).unwrap();
    }
    fn tree(&self) -> Value {
        serde_json::from_slice(&fs::read(self.path("tree.json")).unwrap()).unwrap()
    }
    fn save_tree(&self, value: Value) {
        fs::write(self.path("tree.json"), value.to_string()).unwrap();
    }
    fn remote(&self, uid: &str, name: &str, kind: &str, parent: &str, body: &str) {
        let mut tree = self.tree();
        let hex: String = body.bytes().map(|b| format!("{b:02x}")).collect();
        tree[uid] = json!({
            "name": name, "type": kind, "parent_uid": parent,
            "size": body.len(), "sha1": sha1_smol::Sha1::from(body).digest().to_string(),
            "data": hex, "mtime": 1700000000
        });
        self.save_tree(tree);
    }
    /// The same file on both sides with the fixture's mtime.
    fn both(&self, name: &str, body: &str) {
        self.remote(name, name, "file", "test", body);
        let p = self.local_root().join(name);
        fs::write(&p, body).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
    }
    fn lines(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.path(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("NEUTRONSYNC_DRIVE_STDIO", "1")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_STATE_HOME", self.path("home/.local/state"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env_remove("NEUTRONSYNC_CONFIG")
            .env_remove("NEUTRONSYNC_API_TOKEN")
            .env_remove("NEUTRONSYNC_FAKE_AUTH")
            .arg("--config")
            .arg(self.path("config.toml"))
            .stdin(Stdio::null());
        c
    }

    /// `neutronsync <args>` to completion: (exit code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = self
            .command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn serve(&self, args: &[&str]) -> Server {
        self.serve_env(args, &[])
    }

    fn serve_env(&self, args: &[&str], env: &[(&str, &str)]) -> Server {
        let port = free_port();
        let listen = format!("127.0.0.1:{port}");
        let stderr = self.path(&format!("serve-{port}.stderr"));
        let mut c = self.command();
        c.arg("serve").arg("--listen").arg(&listen).args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        let child = c
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let mut server = Server {
            child,
            port,
            stderr,
            token: None,
        };
        server.wait_ready();
        server
    }

    /// Wait for every fake sidecar this lab started to exit, so a late write
    /// cannot land in a removed folder.
    fn wait_for_sidecars(&self) {
        let pids: Vec<i32> = self
            .lines("starts")
            .iter()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        wait_for("the sidecars to exit", Duration::from_secs(30), || {
            pids.iter()
                .all(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        });
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ready() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- the server and a tiny HTTP client --------------------------------------

struct Server {
    child: Child,
    port: u16,
    stderr: PathBuf,
    token: Option<String>,
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

/// Undo chunked transfer encoding.
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(end) = raw.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(std::str::from_utf8(&raw[..end]).unwrap().trim(), 16).unwrap();
        raw = &raw[end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
    out
}

impl Server {
    fn wait_ready(&mut self) {
        let t0 = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "serve exited early ({status}): {}",
                    fs::read_to_string(&self.stderr).unwrap_or_default()
                );
            }
            if let Ok(reply) = self.try_request("GET", "/api/v1/health", None, None) {
                if reply.status == 200 {
                    return;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "serve never came up"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn try_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        token: Option<&str>,
    ) -> std::io::Result<Reply> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut head =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        if let Some(t) = token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        let body = body.unwrap_or(b"");
        if !body.is_empty() || method != "GET" {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes())?;
        s.write_all(body)?;
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| std::io::Error::other("no header end"))?;
        let headers = String::from_utf8_lossy(&raw[..split]).to_string();
        let status: u16 = headers
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("no status"))?;
        let mut payload = raw[split + 4..].to_vec();
        if headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            payload = dechunk(&payload);
        }
        Ok(Reply {
            status,
            body: String::from_utf8_lossy(&payload).into_owned(),
        })
    }

    /// Send `head` (request line and headers, without the blank line) and
    /// `body` exactly as given.
    fn raw(&self, head: &str, body: &[u8]) -> Reply {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.write_all(format!("{head}\r\nConnection: close\r\n\r\n").as_bytes())
            .unwrap();
        // Written from another thread while this one reads: the server may
        // answer (413) and close before it has read the whole body.
        let mut writer = s.try_clone().unwrap();
        let body = body.to_vec();
        let sending = std::thread::spawn(move || {
            let _ = writer.write_all(&body);
        });
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        let _ = sending.join();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut payload = raw[split + 4..].to_vec();
        if headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            payload = dechunk(&payload);
        }
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json"),
            "every reply is JSON: {headers}"
        );
        Reply {
            status,
            body: String::from_utf8_lossy(&payload).into_owned(),
        }
    }

    fn req(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        let bytes = body.map(|b| b.to_string().into_bytes());
        self.try_request(method, path, bytes.as_deref(), self.token.as_deref())
            .unwrap_or_else(|e| panic!("{method} {path}: {e}"))
    }
    fn get(&self, path: &str) -> Reply {
        self.req("GET", path, None)
    }
    fn post(&self, path: &str, body: Value) -> Reply {
        self.req("POST", path, Some(body))
    }
    fn put(&self, path: &str, body: Value) -> Reply {
        self.req("PUT", path, Some(body))
    }
    fn state(&self) -> Value {
        let r = self.get("/api/v1/state");
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    }
    fn wait_idle(&self) {
        wait_for("the sync to finish", Duration::from_secs(60), || {
            self.state()["state"]["busy"] == false
        });
    }

    /// SIGTERM, then wait: (exit code, how long it took).
    fn stop(mut self) -> (i32, Duration) {
        let t0 = Instant::now();
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) },
            0
        );
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "serve did not stop after SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        (status.code().unwrap_or(-1), t0.elapsed())
    }

    fn stderr_text(&self) -> String {
        fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// Open the event stream; each item is (arrival, line).
    fn events(&self) -> mpsc::Receiver<(Instant, String)> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut head = String::from("GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        if let Some(t) = &self.token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(s);
            let mut line = String::new();
            // Headers.
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
            }
            // Chunked body, split into lines.
            let mut pending = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let Ok(size) = usize::from_str_radix(line.trim(), 16) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                let mut chunk = vec![0u8; size + 2];
                if r.read_exact(&mut chunk).is_err() {
                    return;
                }
                pending.push_str(&String::from_utf8_lossy(&chunk[..size]));
                while let Some(i) = pending.find('\n') {
                    let l: String = pending.drain(..=i).collect();
                    if tx.send((Instant::now(), l.trim_end().to_string())).is_err() {
                        return;
                    }
                }
            }
        });
        rx
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let _ = self.child.wait();
        }
    }
}

/// The next `event: state` envelope within `limit`, skipping keepalives.
fn next_state(rx: &mpsc::Receiver<(Instant, String)>, limit: Duration) -> Option<Value> {
    let deadline = Instant::now() + limit;
    let mut is_state = false;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let (_, line) = rx.recv_timeout(left).ok()?;
        if line == "event: state" {
            is_state = true;
        } else if let Some(data) = line.strip_prefix("data: ") {
            if is_state {
                return Some(serde_json::from_str(data).unwrap());
            }
        } else if line.is_empty() {
            is_state = false;
        }
    }
}

/// The record `process_lock::write` makes for `pid`.
fn lock_record(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    format!("{pid} {start} {}", boot.trim())
}

// ---- AC-2, AC-3: start-up refusals, tokens, body limit -----------------------

#[test]
fn startup_refuses_public_addresses_and_short_tokens() {
    let lab = Lab::new();
    // Refused before anything binds, so 0.0.0.0 is never actually opened.
    let (code, _, err) = lab.run(&["serve", "--listen", "0.0.0.0:8090"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(
            "Refusing to listen on 0.0.0.0:8090 without an API token. Set NEUTRONSYNC_API_TOKEN or --token-file."
        ),
        "{err}"
    );
    let short = lab.path("short.token");
    fs::write(&short, "a".repeat(31)).unwrap();
    let (code, _, err) = lab.run(&[
        "serve",
        "--listen",
        "0.0.0.0:8090",
        "--token-file",
        short.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("too short"), "{err}");
    // A 32-character token starts; loopback here, the public case is the
    // unit test beside check_listen.
    let good = lab.path("good.token");
    fs::write(&good, format!("{}\n", "b".repeat(32))).unwrap();
    let mut server = lab.serve(&["--token-file", good.to_str().unwrap()]);
    server.token = Some("b".repeat(32));
    assert_eq!(server.get("/api/v1/version").status, 200);
    let (code, _) = server.stop();
    assert_eq!(code, 0);

    // Loopback with no token accepts requests without a header.
    let server = lab.serve(&[]);
    let r = server.get("/api/v1/state");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(server.stop().0, 0);
}

#[test]
fn openapi_document_is_served() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let r = server.get("/api/v1/openapi.json");
    assert_eq!(r.status, 200, "{}", r.body);
    let doc = r.json();
    let version = doc["openapi"].as_str().expect("openapi version");
    assert!(version.starts_with("3.1"), "{version}");
    assert!(doc["paths"].get("/api/v1/state").is_some());
    assert!(doc["paths"].get("/api/v1/events").is_some());
}

#[test]
fn every_endpoint_but_health_needs_the_token_and_bodies_are_capped() {
    let lab = Lab::new();
    let token = "t0ken-for-the-serve-api-tests-0123456789";
    let server = lab.serve_env(&[], &[("NEUTRONSYNC_API_TOKEN", token)]);
    let endpoints = [
        ("GET", "/api/v1/version"),
        ("GET", "/api/v1/openapi.json"),
        ("GET", "/api/v1/state"),
        ("GET", "/api/v1/events"),
        ("POST", "/api/v1/sync"),
        ("POST", "/api/v1/cancel"),
        ("POST", "/api/v1/reset"),
        ("POST", "/api/v1/auto-sync"),
        ("GET", "/api/v1/held"),
        ("POST", "/api/v1/held/test/restore"),
        ("POST", "/api/v1/held/test/delete"),
        ("GET", "/api/v1/conflicts"),
        ("POST", "/api/v1/conflicts/1/dismiss"),
        ("POST", "/api/v1/conflicts/dismiss-all"),
        ("GET", "/api/v1/settings"),
        ("PUT", "/api/v1/settings"),
        ("GET", "/api/v1/pairs"),
        ("POST", "/api/v1/pairs"),
        ("PUT", "/api/v1/pairs/test"),
        ("DELETE", "/api/v1/pairs/test"),
        ("GET", "/api/v1/account"),
        ("POST", "/api/v1/account/login"),
        ("POST", "/api/v1/account/2fa"),
        ("POST", "/api/v1/account/mailbox-password"),
        ("POST", "/api/v1/account/logout"),
        ("GET", "/api/v1/remote/list"),
        ("GET", "/api/v1/local/list"),
        ("GET", "/api/v1/logs"),
        ("GET", "/api/v1/no-such-endpoint"),
    ];
    for (method, path) in endpoints {
        for given in [None, Some("wrong-token-wrong-token-wrong-token-00")] {
            let r = server.try_request(method, path, None, given).unwrap();
            assert_eq!(r.status, 401, "{method} {path} with {given:?}: {}", r.body);
            assert_eq!(r.code(), "unauthorized");
        }
    }
    // The pair is untouched by all of that.
    assert!(lab.config_text().contains("name = \"test\""));
    let open = server
        .try_request("GET", "/api/v1/health", None, None)
        .unwrap();
    assert_eq!((open.status, open.json()), (200, json!({"ok": true})));
    let right = server
        .try_request("GET", "/api/v1/version", None, Some(token))
        .unwrap();
    assert_eq!(right.status, 200);

    let big = format!("{{\"pairs\":[\"{}\"]}}", "x".repeat(70 * 1024));
    let r = server
        .try_request("POST", "/api/v1/sync", Some(big.as_bytes()), Some(token))
        .unwrap();
    assert_eq!(r.status, 413, "{}", r.body);
    // A refusal is one stderr line per kind, never the request.
    let err = server.stderr_text();
    assert_eq!(
        err.matches("missing or wrong API token").count(),
        1,
        "one stderr line per kind of refusal: {err}"
    );
    assert!(!err.contains("wrong-token-wrong-token"), "{err}");
    assert_eq!(server.stop().0, 0);
}

/// HTTP/1.1 on an already-connected stream. `host` is sent as `Host`.
fn http_exchange(
    mut stream: impl Read + Write,
    method: &str,
    path: &str,
    host: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
    extra: &str,
) -> Reply {
    let body = body.unwrap_or(b"");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if !extra.is_empty() {
        head.push_str(extra);
        if !extra.ends_with("\r\n") {
            head.push_str("\r\n");
        }
    }
    if !body.is_empty() || method != "GET" {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header end");
    let headers = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut payload = raw[split + 4..].to_vec();
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        payload = dechunk(&payload);
    }
    Reply {
        status,
        body: String::from_utf8_lossy(&payload).into_owned(),
    }
}

fn unix_exchange(
    sock: &Path,
    method: &str,
    path: &str,
    host: &str,
    body: Option<&[u8]>,
    extra: &str,
) -> Reply {
    let stream = UnixStream::connect(sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    http_exchange(stream, method, path, host, body, None, extra)
}

// ---- unix socket, reset, shared owner ---------------------------------------

#[test]
fn unix_socket_accepts_host_x_without_a_bearer_token() {
    let lab = Lab::new();
    let token = "t0ken-for-the-serve-api-tests-0123456789";
    let server = lab.serve_env(&[], &[("NEUTRONSYNC_API_TOKEN", token)]);
    let sock = lab.path("runtime").join("neutronsync").join("api.sock");

    let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode {mode:o}");

    let tcp = server
        .try_request("GET", "/api/v1/state", None, None)
        .unwrap();
    assert_eq!(tcp.status, 401, "{}", tcp.body);

    let unix = unix_exchange(&sock, "GET", "/api/v1/state", "x", None, "");
    assert_eq!(unix.status, 200, "{}", unix.body);
    let env = unix.json();
    assert!(env.get("rev").is_some(), "{env}");
    assert!(env.get("state").is_some(), "{env}");

    let page = unix_exchange(
        &sock,
        "GET",
        "/api/v1/state",
        "x",
        None,
        "Origin: http://evil.example\r\n",
    );
    assert_eq!(page.status, 403, "{}", page.body);
    assert_eq!(server.stop().0, 0);
}

#[test]
fn reset_requires_confirm_and_does_not_delete_local_files() {
    let lab = Lab::new();
    let marker = lab.local_root().join("keep-me.txt");
    fs::write(&marker, "safe").unwrap();
    let server = lab.serve(&[]);

    let missing = server.post("/api/v1/reset", json!({}));
    assert_eq!(missing.status, 400, "{}", missing.body);
    assert_eq!(missing.code(), "bad_request");

    let no = server.post("/api/v1/reset", json!({"confirm": false}));
    assert_eq!(no.status, 400, "{}", no.body);
    assert_eq!(no.code(), "bad_request");

    let yes = server.post("/api/v1/reset", json!({"confirm": true}));
    assert_eq!(yes.status, 202, "{}", yes.body);
    assert_eq!(yes.json()["accepted"], true);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "safe");
    assert!(
        lab.config_text().contains("name = \"test\""),
        "reset does not rewrite the folder list"
    );
    assert_eq!(server.stop().0, 0);
}

#[test]
fn engine_owner_alive_is_the_shared_lock_rule() {
    let lab = Lab::new();
    fs::create_dir_all(lab.state_dir()).unwrap();
    assert!(!neutronsync::server::serve_lock_alive(&lab.state_dir()));
    assert!(!neutronsync::server::engine_owner_alive(&lab.state_dir()));

    let mut holder = Command::new("sleep").arg("60").spawn().unwrap();
    fs::write(lab.state_dir().join("serve.lock"), lock_record(holder.id())).unwrap();
    // The tray refuses on this, before it takes tray-daemon.lock.
    assert!(neutronsync::server::serve_lock_alive(&lab.state_dir()));
    assert!(neutronsync::server::engine_owner_alive(&lab.state_dir()));

    fs::remove_file(lab.state_dir().join("serve.lock")).unwrap();
    fs::write(
        lab.state_dir().join("tray-daemon.lock"),
        lock_record(holder.id()),
    )
    .unwrap();
    assert!(!neutronsync::server::serve_lock_alive(&lab.state_dir()));
    assert!(neutronsync::server::engine_owner_alive(&lab.state_dir()));

    unsafe { libc::kill(holder.id() as i32, libc::SIGKILL) };
    let _ = holder.wait();
    assert!(
        !neutronsync::server::engine_owner_alive(&lab.state_dir()),
        "a dead pid does not own the state"
    );
}

// ---- AC-4: one owner per state directory, clean stop --------------------------

#[test]
fn one_owner_per_state_and_a_clean_stop_during_a_transfer() {
    let lab = Lab::new();
    let first = lab.serve(&[]);
    let (code, _, err) = lab.run(&["serve", "--listen", &format!("127.0.0.1:{}", free_port())]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(
            "Another NeutronSync is already syncing this state (the desktop tray or another serve). Stop it first."
        ),
        "{err}"
    );
    assert_eq!(first.stop().0, 0);
    assert!(!lab.state_dir().join("serve.lock").exists());

    // A scratch "tray" that is alive.
    let mut tray = Command::new("sleep").arg("60").spawn().unwrap();
    fs::write(
        lab.state_dir().join("tray-daemon.lock"),
        lock_record(tray.id()),
    )
    .unwrap();
    let (code, _, err) = lab.run(&["serve", "--listen", &format!("127.0.0.1:{}", free_port())]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("Another NeutronSync is already syncing this state"),
        "{err}"
    );
    unsafe { libc::kill(tray.id() as i32, libc::SIGKILL) };
    let _ = tray.wait();
    // A dead tray's lock does not block.
    let server = lab.serve(&[]);

    // SIGTERM while a slow download runs: exit 0 inside 10 s, lock gone.
    for i in 0..3 {
        lab.remote(
            &format!("f{i}"),
            &format!("f{i}.txt"),
            "file",
            "test",
            "data",
        );
    }
    lab.controls(json!({"download_delay": 12}));
    assert_eq!(
        server
            .post("/api/v1/sync", json!({"pairs":[PAIR],"dry_run":false}))
            .status,
        202
    );
    wait_for("a download to start", Duration::from_secs(20), || {
        lab.lines("transfers").iter().any(|l| l.contains("start"))
    });
    let (code, took) = server.stop();
    assert_eq!(code, 0);
    assert!(took < Duration::from_secs(10), "stop took {took:?}");
    assert!(!lab.state_dir().join("serve.lock").exists());
    eprintln!("AC-4: stopped in {took:?} with a download in flight");
    lab.wait_for_sidecars();
}

// ---- AC-5, AC-7, AC-16: state, version, logs ------------------------------------

#[test]
fn state_envelope_version_and_logs() {
    let lab = Lab::new();
    {
        let stats = neutronsync::stats::Stats::open(&lab.state_dir()).unwrap();
        for i in 0..150 {
            stats
                .record_op(
                    PAIR,
                    "upload",
                    &format!("file-{i:03}"),
                    true,
                    None,
                    1_700_000_000 + i,
                )
                .unwrap();
        }
    }
    fs::create_dir_all(lab.state_dir().join("logs")).unwrap();
    fs::write(
        lab.state_dir().join("logs/sync.log"),
        (0..50)
            .map(|i| format!("old line {i}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let server = lab.serve(&[]);

    let env = server.state();
    for key in ["rev", "state", "held", "conflicts", "auto_sync"] {
        assert!(env.get(key).is_some(), "missing {key}: {env}");
    }
    let state: neutronsync::service::AppState =
        serde_json::from_value(env["state"].clone()).expect("state is an AppState");
    assert_eq!(state.pairs.len(), 1);
    assert_eq!(env["auto_sync"], false);

    let five = server.get("/api/v1/state?activity=5").json();
    let items = five["state"]["activity"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[4]["op"]["path"], "file-149", "newest kept: {items:?}");
    assert_eq!(items[0]["op"]["path"], "file-145");
    let all = server.get("/api/v1/state?activity=9999").json();
    let n = all["state"]["activity"].as_array().unwrap().len();
    assert!((100..=500).contains(&n), "{n}");

    let v = server.get("/api/v1/version").json();
    let (_, out, _) = lab.run(&["--version"]);
    assert_eq!(v["api_version"], 1);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        out.trim(),
        format!(
            "neutronsync {} ({})",
            v["version"].as_str().unwrap(),
            v["build"].as_str().unwrap()
        )
    );

    let logs = server.get("/api/v1/logs?lines=3").json();
    let on_disk = lab.lines("state/logs/sync.log");
    assert_eq!(logs["lines"], json!(on_disk[on_disk.len() - 3..]));
    assert_eq!(server.stop().0, 0);
}

// ---- AC-6: the event stream ----------------------------------------------------

#[test]
fn event_stream_sends_state_changes_and_keepalives_to_every_client() {
    let lab = Lab::new();
    lab.both("a.txt", "alpha");
    let server = lab.serve(&[]);
    lab.controls(json!({"walk_delay": 1.0}));

    let t0 = Instant::now();
    let a = server.events();
    let b = server.events();
    let first_a = next_state(&a, Duration::from_secs(1)).expect("an event within 1 s");
    let first_b = next_state(&b, Duration::from_secs(1)).expect("an event within 1 s");
    eprintln!("AC-6: first event after {:?}", t0.elapsed());
    let rev = first_a["rev"].as_u64().unwrap();
    assert!(first_b["rev"].as_u64().unwrap() >= rev);

    // Let any start-up changes settle.
    while next_state(&a, Duration::from_millis(800)).is_some() {}
    while next_state(&b, Duration::from_millis(10)).is_some() {}

    let t1 = Instant::now();
    assert_eq!(
        server
            .post("/api/v1/sync", json!({"pairs":[],"dry_run":true}))
            .status,
        202
    );
    let next_a = next_state(&a, Duration::from_secs(1)).expect("an update within 1 s");
    let next_b = next_state(&b, Duration::from_secs(1)).expect("an update within 1 s");
    eprintln!("AC-6: update after a sync started: {:?}", t1.elapsed());
    assert!(next_a["rev"].as_u64().unwrap() > rev);
    assert!(next_b["rev"].as_u64().unwrap() > rev);
    assert_eq!(next_a["state"]["busy"], true);

    server.wait_idle();
    while next_state(&a, Duration::from_millis(1500)).is_some() {}
    // Idle: only a keepalive comment arrives.
    let t2 = Instant::now();
    let mut keepalive = None;
    while t2.elapsed() < Duration::from_secs(16) {
        match a.recv_timeout(Duration::from_secs(16) - t2.elapsed()) {
            Ok((at, line)) => {
                assert_ne!(line, "event: state", "no state event while idle");
                if line == ": keepalive" {
                    keepalive = Some(at - t2);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let keepalive = keepalive.expect("a keepalive within 16 s");
    eprintln!("AC-6: keepalive after {keepalive:?} of quiet");
    assert_eq!(server.stop().0, 0);
}

// ---- AC-8, AC-9: sync, cancel, auto sync ---------------------------------------

#[test]
fn sync_runs_refuses_while_busy_and_cancels() {
    let lab = Lab::new();
    lab.both("a.txt", "alpha");
    lab.remote("b", "b.txt", "file", "test", "beta");
    let server = lab.serve(&[]);
    lab.controls(json!({"walk_delay": 1.5}));
    let events = server.events();

    let r = server.post("/api/v1/sync", json!({"pairs":["nope"],"dry_run":true}));
    assert_eq!((r.status, r.code().as_str()), (400, "bad_request"));

    assert_eq!(
        server
            .post("/api/v1/sync", json!({"pairs":[PAIR],"dry_run":true}))
            .status,
        202
    );
    let r = server.post("/api/v1/sync", json!({"dry_run":true}));
    assert_eq!((r.status, r.code().as_str()), (409, "busy"), "{}", r.body);

    let mut phases = Vec::new();
    // Wait out the whole deadline, not a fixed gap between events: under a
    // loaded test run the walk can go quiet for longer than any short gap.
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Some(env) = next_state(&events, left) else {
            break;
        };
        let phase = env["state"]["pairs"][0]["phase"]
            .as_str()
            .unwrap()
            .to_string();
        if phases.last() != Some(&phase) {
            phases.push(phase.clone());
        }
        if phase == "Synced" && env["state"]["busy"] == false {
            break;
        }
    }
    assert!(
        phases.iter().any(|p| p == "Scanning" || p == "Syncing"),
        "{phases:?}"
    );
    assert_eq!(
        phases.last().map(String::as_str),
        Some("Synced"),
        "{phases:?}"
    );
    assert!(
        !lab.local_root().join("b.txt").exists(),
        "a dry run downloads nothing"
    );

    // Cancel: six files to fetch (b.txt and c0..c4), one at a time, 3 s each;
    // cancelled while the first is in flight. A working cancel stops the
    // run after that one: one download started, one file arrived, done in
    // about 3 s. A cancel that did nothing would start all six and take 18 s.
    lab.controls(json!({"download_delay": 3}));
    for i in 0..5 {
        lab.remote(
            &format!("c{i}"),
            &format!("c{i}.txt"),
            "file",
            "test",
            "gamma",
        );
    }
    assert_eq!(server.post("/api/v1/sync", json!({})).status, 202);
    wait_for("a download to start", Duration::from_secs(20), || {
        lab.lines("transfers").iter().any(|l| l.contains("start"))
    });
    assert_eq!(server.post("/api/v1/cancel", json!({})).status, 202);
    let t1 = Instant::now();
    server.wait_idle();
    assert!(t1.elapsed() < Duration::from_secs(6), "{:?}", t1.elapsed());
    let started = lab
        .lines("transfers")
        .iter()
        .filter(|l| l.contains("start"))
        .count();
    assert_eq!(started, 1, "only the download already in flight may run");
    let fetched: Vec<String> = ["b.txt", "c0.txt", "c1.txt", "c2.txt", "c3.txt", "c4.txt"]
        .iter()
        .filter(|n| lab.local_root().join(n).exists())
        .map(|n| n.to_string())
        .collect();
    assert!(fetched.len() <= 1, "cancelled run fetched {fetched:?}");
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn auto_sync_toggles_the_watcher_and_the_config() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let r = server.post("/api/v1/auto-sync", json!({"enabled": true}));
    assert_eq!((r.status, r.json()), (200, json!({"auto_sync": true})));
    assert!(lab.config_text().contains("auto_sync = true"));
    assert_eq!(server.state()["state"]["watching"], true);
    let r = server.post("/api/v1/auto-sync", json!({"enabled": false}));
    assert_eq!((r.status, r.json()), (200, json!({"auto_sync": false})));
    assert!(lab.config_text().contains("auto_sync = false"));
    assert_eq!(server.state()["state"]["watching"], false);
    // On again straight away: the old watcher is waited for, not raced.
    assert_eq!(
        server
            .post("/api/v1/auto-sync", json!({"enabled": true}))
            .status,
        200
    );
    std::thread::sleep(Duration::from_millis(1500));
    let s = server.state();
    assert_eq!(s["state"]["watching"], true, "{s}");
    assert!(
        !s["state"]["activity"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["text"].as_str().unwrap_or("").contains("already running")),
        "{s}"
    );
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

// ---- AC-10, AC-11: held deletions and conflicts -----------------------------------

#[test]
fn held_deletions_can_be_listed_refused_and_restored() {
    let lab = Lab::new();
    for i in 0..300 {
        lab.both(&format!("f{i}.txt"), &format!("tracked {i}"));
    }
    let (code, _, err) = lab.run(&["--quiet", "sync"]);
    assert_eq!(code, 0, "{err}");
    for i in 0..250 {
        fs::remove_file(lab.local_root().join(format!("f{i}.txt"))).unwrap();
    }
    let (code, _, err) = lab.run(&["sync"]);
    assert_eq!(code, 1, "{err}");
    lab.wait_for_sidecars();

    let server = lab.serve(&[]);
    let held = server.get("/api/v1/held").json();
    assert_eq!(held[0]["pair"], PAIR, "{held}");
    assert_eq!(held[0]["remote"], 250);
    assert_eq!(server.state()["held"], held);

    let r = server.post("/api/v1/held/test/delete", json!({}));
    assert_eq!(
        (r.status, r.code().as_str()),
        (400, "bad_request"),
        "{}",
        r.body
    );
    for action in ["restore", "delete"] {
        let r = server.post(
            &format!("/api/v1/held/nope/{action}"),
            json!({"confirm": true}),
        );
        assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    }
    assert_eq!(
        server.post("/api/v1/held/test/restore", json!({})).status,
        202
    );
    server.wait_idle();
    assert_eq!(server.get("/api/v1/held").json(), json!([]));
    assert_eq!(fs::read_dir(lab.local_root()).unwrap().count(), 300);
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn conflicts_are_listed_and_dismissed() {
    let lab = Lab::new();
    {
        let stats = neutronsync::stats::Stats::open(&lab.state_dir()).unwrap();
        use neutronsync::models::ConflictOutcome;
        stats
            .record_conflict(
                PAIR,
                "a.txt",
                ConflictOutcome::KeptBoth,
                Some("a (conflict).txt"),
                1,
            )
            .unwrap();
        stats
            .record_conflict(PAIR, "b.txt", ConflictOutcome::Skipped, None, 2)
            .unwrap();
        stats
            .record_conflict(PAIR, "c.txt", ConflictOutcome::KeptRemote, None, 3)
            .unwrap();
    }
    let server = lab.serve(&[]);
    let list = server.get("/api/v1/conflicts").json();
    assert_eq!(list.as_array().unwrap().len(), 3);
    let outcomes: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, vec!["kept_remote", "skipped", "kept_both"]);
    let id = list[0]["id"].as_i64().unwrap();
    assert_eq!(
        server
            .post(&format!("/api/v1/conflicts/{id}/dismiss"), json!({}))
            .status,
        202
    );
    assert_eq!(
        server
            .get("/api/v1/conflicts")
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let r = server.post("/api/v1/conflicts/99999/dismiss", json!({}));
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    assert_eq!(
        server
            .post("/api/v1/conflicts/dismiss-all", json!({}))
            .status,
        202
    );
    assert_eq!(server.get("/api/v1/conflicts").json(), json!([]));
    assert_eq!(server.stop().0, 0);
}

// ---- AC-12, AC-13: settings and pairs -----------------------------------------------

#[test]
fn settings_go_through_the_real_parser_and_keep_desktop_keys() {
    let lab = Lab::with_options(
        "theme = \"dark\"\nnotifications = false\nrun_in_tray = true\nfile_manager_bookmarks = true\nupdate_channel = \"prerelease\"\n",
    );
    let server = lab.serve(&[]);
    let desktop = [
        "theme = \"dark\"",
        "notifications = false",
        "run_in_tray = true",
        "file_manager_bookmarks = true",
        "update_channel = \"prerelease\"",
    ];
    let before = server.get("/api/v1/settings").json();
    assert_eq!(before["max_delete_percent"], 30);
    assert_eq!(before["conflict"], "keep-both");
    assert_eq!(before["theme"], "dark");
    assert_eq!(before["notifications"], false);
    assert_eq!(before["run_in_tray"], true);
    assert_eq!(before["file_manager_bookmarks"], true);
    assert_eq!(before["update_channel"], "prerelease");
    assert_eq!(before["x11_compat"], false);
    assert_eq!(before["check_on_launch"], false);

    let r = server.put("/api/v1/settings", json!({"max_delete_percent": 40}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(lab.config_text().contains("max_delete_percent = 40"));
    assert_eq!(
        server.get("/api/v1/settings").json()["max_delete_percent"],
        40
    );
    for key in desktop {
        assert!(
            lab.config_text().contains(key),
            "{key} lost:\n{}",
            lab.config_text()
        );
    }

    let bytes = fs::read(lab.path("config.toml")).unwrap();
    let r = server.put("/api/v1/settings", json!({"conflict": "maybe"}));
    assert_eq!(
        (r.status, r.code().as_str()),
        (422, "invalid_config"),
        "{}",
        r.body
    );
    let message = r.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(
        message.contains("options.conflict must be keep-both|newer|skip"),
        "{message}"
    );
    assert_eq!(fs::read(lab.path("config.toml")).unwrap(), bytes);
    assert_eq!(
        server.get("/api/v1/settings").json()["conflict"],
        "keep-both"
    );

    let r = server.put("/api/v1/settings", json!({"not_a_setting": true}));
    assert_eq!((r.status, r.code().as_str()), (400, "bad_request"));
    assert_eq!(fs::read(lab.path("config.toml")).unwrap(), bytes);

    let r = server.put("/api/v1/settings", json!({"theme": "light"}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["theme"], "light");
    assert!(lab.config_text().contains("theme = \"light\""));
    for key in [
        "notifications = false",
        "run_in_tray = true",
        "file_manager_bookmarks = true",
        "update_channel = \"prerelease\"",
    ] {
        assert!(
            lab.config_text().contains(key),
            "{key} lost:\n{}",
            lab.config_text()
        );
    }
    // No temp file is left beside the config.
    let leftovers: Vec<_> = fs::read_dir(lab.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".neutronsync-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(server.stop().0, 0);
}

#[test]
fn pairs_are_added_renamed_and_removed_without_touching_files() {
    let lab = Lab::new();
    let docs = lab.path("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("keep.txt"), "keep me").unwrap();
    let server = lab.serve(&[]);

    let add = json!({"name":"docs","local":docs,"remote":"Documents","exclude":["cache"]});
    let r = server.post("/api/v1/pairs", add.clone());
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(lab.config_text().contains("name = \"docs\""));
    let pairs = server.get("/api/v1/pairs").json();
    let docs_pair = pairs
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "docs")
        .unwrap()
        .clone();
    assert_eq!(docs_pair["remote"], "/my-files/Documents");
    assert_eq!(docs_pair["exclude"], json!(["cache"]));
    assert_eq!(docs_pair["auto"], true);

    let r = server.post("/api/v1/pairs", add);
    assert_eq!((r.status, r.code().as_str()), (409, "conflict"));
    let r = server.put(
        "/api/v1/pairs/docs",
        json!({"name":"test","local":docs,"remote":"Documents"}),
    );
    assert_eq!((r.status, r.code().as_str()), (409, "conflict"));

    let r = server.put(
        "/api/v1/pairs/docs",
        json!({"name":"papers","local":docs,"remote":"Papers","auto":false}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let text = lab.config_text();
    assert!(
        text.contains("name = \"papers\"") && !text.contains("name = \"docs\""),
        "{text}"
    );
    let other = lab.path("other");
    fs::create_dir_all(&other).unwrap();
    let r = server.put(
        "/api/v1/pairs/nope",
        json!({"name":"x","local":other,"remote":"Other"}),
    );
    assert_eq!(r.status, 404);

    let r = server.req("DELETE", "/api/v1/pairs/papers", None);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["files_deleted"], false);
    assert!(!lab.config_text().contains("papers"));
    assert_eq!(
        fs::read_to_string(docs.join("keep.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        server.req("DELETE", "/api/v1/pairs/papers", None).status,
        404
    );

    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"rel","local":"relative/path","remote":"x"}),
    );
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert_eq!(server.stop().0, 0);

    // With --local-root, a folder outside it is refused.
    let allowed = lab.path("x");
    fs::create_dir_all(allowed.join("inside")).unwrap();
    let server = lab.serve(&["--local-root", allowed.to_str().unwrap()]);
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"out","local":docs,"remote":"x"}),
    );
    assert_eq!(
        (r.status, r.code().as_str()),
        (422, "invalid_config"),
        "{}",
        r.body
    );
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"in","local":allowed.join("inside"),"remote":"x"}),
    );
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(server.stop().0, 0);
}

#[test]
fn settings_and_pairs_reject_invalid_limits_and_overlap() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let r = server.put("/api/v1/settings", json!({"scan_threads": 9999}));
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("scan_threads"));
    let r = server.put("/api/v1/settings", json!({"scan_threads": 64}));
    assert_eq!(r.status, 200, "{}", r.body);
    let r = server.put("/api/v1/settings", json!({"remote_root": "relative"}));
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("remote_root"));

    let a = lab.path("a");
    let nested = a.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"outer","local":a,"remote":"/my-files/Outer"}),
    );
    assert_eq!(r.status, 201);
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"inner","local":nested,"remote":"/my-files/Other"}),
    );
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("outer"));
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"bad","local":a,"remote":"/my-files/../x"}),
    );
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    let r = server.put(
        "/api/v1/settings",
        json!({"remote_root": "/my-files//Shared"}),
    );
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("remote_root"));
    let other = lab.path("other");
    fs::create_dir_all(&other).unwrap();
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"root","local":other,"remote":"/"}),
    );
    assert_eq!((r.status, r.code().as_str()), (422, "invalid_config"));
    assert_eq!(server.stop().0, 0);
}

// ---- AC-14: sign-in ------------------------------------------------------------------

fn login(server: &Server, user: &str, extra: Value) -> Reply {
    let mut body = json!({"username": user, "password": PASSWORD});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            body[k] = v;
        }
    }
    server.post("/api/v1/account/login", body)
}

fn signed_in(server: &Server) -> bool {
    let r = server.get("/api/v1/account");
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["signed_in"] == true
}

#[test]
fn sign_in_steps_follow_the_sidecar_and_never_log_secrets() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let logout = |server: &Server| {
        let r = server.post("/api/v1/account/logout", json!({}));
        assert_eq!(r.json(), json!({"step":"done"}), "{}", r.body);
        assert!(!signed_in(server));
    };

    assert_eq!(
        login(&server, "ok", json!({})).json(),
        json!({"step":"done"})
    );
    assert!(signed_in(&server));
    logout(&server);

    assert_eq!(
        login(&server, "totp", json!({})).json(),
        json!({"step":"need_2fa"})
    );
    let r = server.post("/api/v1/account/2fa", json!({"code":"123456"}));
    assert_eq!(r.json(), json!({"step":"done"}));
    assert!(signed_in(&server));
    logout(&server);

    assert_eq!(
        login(&server, "mailbox", json!({})).json(),
        json!({"step":"need_mailbox_password"})
    );
    let r = server.post(
        "/api/v1/account/mailbox-password",
        json!({"password": PASSWORD}),
    );
    assert_eq!(r.json(), json!({"step":"done"}));
    assert!(signed_in(&server));
    logout(&server);

    let hv = login(&server, "hv", json!({})).json();
    assert_eq!(hv["step"], "need_human_verification");
    assert_eq!(hv["token"], "hv-token");
    assert_eq!(hv["methods"], json!(["captcha"]));
    assert!(hv["url"]
        .as_str()
        .unwrap()
        .starts_with("https://verify.proton.me/"));
    let r = login(
        &server,
        "hv",
        json!({"hv_token":"hv-token","hv_type":"captcha"}),
    );
    assert_eq!(r.json(), json!({"step":"done"}));
    assert!(signed_in(&server));
    logout(&server);

    let r = login(&server, "error", json!({}));
    assert_eq!(
        (r.status, r.code().as_str()),
        (401, "sign_in_failed"),
        "{}",
        r.body
    );
    assert!(!r.body.contains(PASSWORD));
    assert!(!signed_in(&server));
    // A body that does not parse is refused without echoing it.
    let r = server.post("/api/v1/account/login", json!({"password": PASSWORD}));
    assert_eq!(r.status, 400);
    assert!(!r.body.contains(PASSWORD), "{}", r.body);
    let stderr = server.stderr_text();
    assert_eq!(server.stop().0, 0);

    // NEUTRONSYNC_FAKE_AUTH selects the script regardless of the username.
    let server = lab.serve_env(&[], &[("NEUTRONSYNC_FAKE_AUTH", "totp")]);
    assert_eq!(
        login(&server, "anyone", json!({})).json(),
        json!({"step":"need_2fa"})
    );
    assert_eq!(
        server
            .post("/api/v1/account/2fa", json!({"code":"000000"}))
            .json(),
        json!({"step":"done"})
    );
    assert!(signed_in(&server));
    let stderr = stderr + &server.stderr_text();
    assert_eq!(server.stop().0, 0);

    let log = fs::read_to_string(lab.state_dir().join("logs/sync.log")).unwrap_or_default();
    for (name, text) in [("sync.log", &log), ("stderr", &stderr)] {
        assert!(!text.contains(PASSWORD), "{name} holds the password");
        assert!(!text.contains("123456"), "{name} holds the code");
    }
    lab.wait_for_sidecars();
}

// ---- AC-15: browsing -------------------------------------------------------------------

#[test]
fn browsing_lists_remote_folders_first_and_keeps_local_inside_the_roots() {
    let lab = Lab::new();
    lab.remote("zeta", "zeta.txt", "file", "root", "z");
    lab.remote("alpha-file", "alpha.txt", "file", "root", "a");
    lab.remote("beta", "beta", "folder", "root", "");
    let x = lab.path("x");
    fs::create_dir_all(x.join("sub/.hidden")).unwrap();
    fs::create_dir_all(x.join("sub/visible")).unwrap();
    fs::write(x.join("sub/file.txt"), "f").unwrap();
    std::os::unix::fs::symlink("/etc", x.join("sub/etc-link")).unwrap();
    let server = lab.serve(&["--local-root", x.to_str().unwrap()]);

    let list = server.get("/api/v1/remote/list?path=/my-files").json();
    let names: Vec<(&str, &str)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("beta", "folder"),
            ("test", "folder"),
            ("alpha.txt", "file"),
            ("zeta.txt", "file")
        ]
    );
    assert_eq!(list[2]["size"], 1);

    lab.controls(json!({"list_errors": {"root": "auth"}}));
    let r = server.get("/api/v1/remote/list?path=/my-files");
    assert_eq!(
        (r.status, r.code().as_str()),
        (409, "signed_out"),
        "{}",
        r.body
    );
    lab.controls(json!({}));

    let real = fs::canonicalize(&x).unwrap();
    let top = server.get("/api/v1/local/list").json();
    assert_eq!(top[0]["path"], real.display().to_string());
    let sub = server
        .get(&format!("/api/v1/local/list?path={}/sub", real.display()))
        .json();
    let names: Vec<(&str, bool)> = sub
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["hidden"].as_bool().unwrap()))
        .collect();
    assert_eq!(names, vec![(".hidden", true), ("visible", false)]);

    for path in [
        format!("{}/../../etc", real.display()),
        format!("{}/sub/etc-link", real.display()),
        "/etc".to_string(),
    ] {
        let r = server.get(&format!("/api/v1/local/list?path={path}"));
        assert_eq!(
            (r.status, r.code().as_str()),
            (403, "forbidden"),
            "{path}: {}",
            r.body
        );
    }
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn unknown_routes_and_methods_are_json_errors() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let r = server.get("/api/v1/nothing");
    assert_eq!((r.status, r.code().as_str()), (404, "not_found"));
    let r = server.req("DELETE", "/api/v1/state", None);
    assert_eq!(r.status, 405);
    assert_eq!(r.code(), "bad_request");
    let r = server.req("POST", "/api/v1/sync", None);
    assert_eq!(r.status, 202, "an empty body means defaults: {}", r.body);
    server.wait_idle();
    let r = server
        .try_request("POST", "/api/v1/sync", Some(b"{not json"), None)
        .unwrap();
    assert_eq!((r.status, r.code().as_str()), (400, "bad_request"));
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

// ---- review fixes: revision, body cap, query errors, browsers, links -------------

#[test]
fn state_and_stream_share_one_revision() {
    let lab = Lab::new();
    lab.both("a.txt", "alpha");
    let server = lab.serve(&[]);
    lab.controls(json!({"walk_delay": 1.0}));
    let events = server.events();
    let mut seen: BTreeMap<u64, Value> = BTreeMap::new();
    let mut record = |env: &Value| {
        let rev = env["rev"].as_u64().unwrap();
        // The content behind a revision; the activity share differs by
        // request, so compare the rest and the newest activity item.
        let mut key = env.clone();
        let last = key["state"]["activity"].as_array().unwrap().last().cloned();
        key["state"]["activity"] = json!(last);
        if let Some(prev) = seen.get(&rev) {
            assert_eq!(prev, &key, "rev {rev} named two different states");
        }
        seen.insert(rev, key);
    };
    assert_eq!(
        server.post("/api/v1/sync", json!({"dry_run": true})).status,
        202
    );
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(4) {
        record(&server.state());
        while let Some(env) = next_state(&events, Duration::from_millis(5)) {
            record(&env);
        }
    }
    server.wait_idle();
    record(&server.state());
    while let Some(env) = next_state(&events, Duration::from_millis(600)) {
        record(&env);
    }
    assert!(
        seen.len() >= 2,
        "the run changed the state: {:?}",
        seen.keys()
    );
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn bodies_are_capped_even_when_chunked_and_must_be_json() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    // 70 000 bytes, chunked, to a command that takes no body.
    let mut chunked = Vec::new();
    for _ in 0..7 {
        chunked.extend_from_slice(format!("{:x}\r\n", 10_000).as_bytes());
        chunked.extend_from_slice(&[b'x'; 10_000]);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let r = server.raw(
        "POST /api/v1/cancel HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked",
        &chunked,
    );
    assert_eq!(
        (r.status, r.code().as_str()),
        (413, "too_large"),
        "{}",
        r.body
    );
    // A small chunked body is still fine.
    let r = server.raw(
        "POST /api/v1/cancel HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked",
        b"2\r\n{}\r\n0\r\n\r\n",
    );
    assert_eq!(r.status, 202, "{}", r.body);
    // A body in any other type is refused before the handler runs.
    for kind in ["text/plain", "application/x-www-form-urlencoded"] {
        let r = server.raw(
            &format!("POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {kind}\r\nContent-Length: 2"),
            b"{}",
        );
        assert_eq!(r.status, 415, "{kind}: {}", r.body);
        assert_eq!(r.code(), "bad_request");
    }
    let r = server.raw(
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2",
        b"{}",
    );
    assert_eq!(r.status, 415, "no type: {}", r.body);
    assert_eq!(server.state()["state"]["busy"], false, "no sync started");
    let r = server.raw(
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 16",
        br#"{"dry_run":true}"#,
    );
    assert_eq!(r.status, 202, "{}", r.body);
    server.wait_idle();
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn malformed_query_strings_are_json_errors() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    for path in [
        "/api/v1/state?activity=abc",
        "/api/v1/logs?lines=-1",
        "/api/v1/logs?lines=1e9",
    ] {
        let r = server.raw(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1"), b"");
        assert_eq!(
            (r.status, r.code().as_str()),
            (400, "bad_request"),
            "{path}: {}",
            r.body
        );
    }
    assert_eq!(server.stop().0, 0);
}

#[test]
fn web_pages_and_rebound_host_names_are_refused() {
    let lab = Lab::new();
    lab.both("a.txt", "alpha");
    let server = lab.serve(&[]);
    // What a page's no-cors fetch sends: an Origin and a simple body type.
    for head in [
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1:8090\r\nOrigin: http://evil.example\r\nContent-Type: text/plain\r\nContent-Length: 2",
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1:8090\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: 2",
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1:8090\r\nOrigin: null\r\nContent-Length: 2",
        "POST /api/v1/sync HTTP/1.1\r\nHost: 127.0.0.1:8090\r\nSec-Fetch-Site: cross-site\r\nContent-Type: application/json\r\nContent-Length: 2",
    ] {
        let r = server.raw(head, b"{}");
        assert_eq!((r.status, r.code().as_str()), (403, "forbidden"), "{head}: {}", r.body);
    }
    for path in ["/api/v1/account/logout", "/api/v1/account/login"] {
        let r = server.raw(
            &format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\nContent-Length: 0"),
            b"",
        );
        assert_eq!(r.status, 403, "{path}: {}", r.body);
    }
    // DNS rebinding: a page's own name, pointed at 127.0.0.1.
    let r = server.raw("GET /api/v1/state HTTP/1.1\r\nHost: evil.example:8090", b"");
    assert_eq!((r.status, r.code().as_str()), (403, "forbidden"));
    for host in [
        "127.0.0.1:8090",
        "localhost:8090",
        "[::1]:8090",
        "localhost",
    ] {
        let r = server.raw(
            &format!("GET /api/v1/version HTTP/1.1\r\nHost: {host}"),
            b"",
        );
        assert_eq!(r.status, 200, "{host}: {}", r.body);
    }
    // Same-origin fetch metadata from a non-browser tool is not a page.
    let r = server.raw(
        "GET /api/v1/version HTTP/1.1\r\nHost: 127.0.0.1\r\nSec-Fetch-Site: none",
        b"",
    );
    assert_eq!(r.status, 200);
    // The health check stays open.
    let r = server.raw(
        "GET /api/v1/health HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example",
        b"",
    );
    assert_eq!(r.status, 200);
    std::thread::sleep(Duration::from_millis(300));
    let s = server.state();
    assert_eq!(s["state"]["busy"], false, "no sync was started: {s}");
    assert_eq!(s["state"]["pairs"][0]["phase"], "Idle", "{s}");
    assert_eq!(server.stop().0, 0);
}

#[test]
fn a_new_pair_folder_below_a_link_out_of_the_root_is_refused() {
    let lab = Lab::new();
    let root = lab.path("data");
    fs::create_dir_all(root.join("real")).unwrap();
    fs::create_dir_all(lab.path("config-outside")).unwrap();
    std::os::unix::fs::symlink(lab.path("config-outside"), root.join("media")).unwrap();
    let server = lab.serve(&["--local-root", root.to_str().unwrap()]);
    for local in [
        root.join("media"),
        root.join("media/new"),
        root.join("media/new/deeper"),
    ] {
        let r = server.post(
            "/api/v1/pairs",
            json!({"name":"p","local":local,"remote":"/my-files/payload"}),
        );
        assert_eq!(
            (r.status, r.code().as_str()),
            (422, "invalid_config"),
            "{local:?}: {}",
            r.body
        );
    }
    let r = server.post(
        "/api/v1/pairs",
        json!({"name":"p","local":root.join("real/new"),"remote":"/my-files/payload"}),
    );
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(!lab.path("config-outside/new").exists());
    assert_eq!(server.stop().0, 0);
}

// ---- R1-003: simultaneous starts ------------------------------------------------

#[test]
fn simultaneous_starts_give_one_run_and_busy_for_the_rest() {
    let lab = Lab::new();
    for i in 0..300 {
        lab.both(&format!("f{i}.txt"), &format!("tracked {i}"));
    }
    let (code, _, err) = lab.run(&["--quiet", "sync"]);
    assert_eq!(code, 0, "{err}");
    for i in 0..250 {
        fs::remove_file(lab.local_root().join(format!("f{i}.txt"))).unwrap();
    }
    let (code, _, err) = lab.run(&["sync"]);
    assert_eq!(code, 1, "the hold is stored: {err}");
    lab.wait_for_sidecars();

    let server = std::sync::Arc::new(lab.serve(&[]));
    let walks = || {
        lab.lines("calls")
            .iter()
            .filter(|l| l.contains("\"node.walk\""))
            .count()
    };
    let walks_before = walks();
    // Hold every run open long enough that none can finish inside the burst.
    lab.controls(json!({"walk_delay": 2.0}));
    let gate = std::sync::Arc::new(std::sync::Barrier::new(18));
    let mut threads = Vec::new();
    for i in 0..18 {
        let server = server.clone();
        let gate = gate.clone();
        threads.push(std::thread::spawn(move || {
            let (path, body) = match i {
                0 => ("/api/v1/held/test/restore", json!({})),
                1 => ("/api/v1/held/test/delete", json!({"confirm": true})),
                _ => ("/api/v1/sync", json!({"dry_run": true})),
            };
            let bytes = body.to_string().into_bytes();
            gate.wait();
            let r = server
                .try_request("POST", path, Some(&bytes), None)
                .unwrap();
            (r.status, r.code())
        }));
    }
    let results: Vec<(u16, String)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let accepted = results.iter().filter(|(s, _)| *s == 202).count();
    let busy = results
        .iter()
        .filter(|(s, c)| *s == 409 && c == "busy")
        .count();
    assert_eq!(accepted, 1, "{results:?}");
    assert_eq!(busy, 17, "{results:?}");
    server.wait_idle();
    // One pair, one run: exactly one tree walk happened in the burst.
    assert_eq!(walks() - walks_before, 1, "exactly one run started");
    let server = std::sync::Arc::try_unwrap(server).ok().expect("one owner");
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn sync_with_no_configured_pairs_is_refused() {
    let lab = Lab::new();
    let cfg = lab.config_text();
    let trimmed = cfg.split("[[pair]]").next().unwrap_or(&cfg);
    fs::write(lab.path("config.toml"), trimmed).unwrap();
    let server = lab.serve(&[]);
    let r = server.post("/api/v1/sync", json!({"dry_run": true}));
    assert_eq!((r.status, r.code().as_str()), (409, "conflict"));
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Add a folder"));
    assert_eq!(server.stop().0, 0);
}

#[test]
fn sync_while_signed_out_does_not_change_pair_phase() {
    let lab = Lab::new();
    lab.both("a.txt", "alpha");
    // Set the session before the startup account probe. Polling /state only
    // reads published state; it does not request another auth.status probe.
    lab.controls(json!({"signed_in": false}));
    let server = lab.serve(&[]);
    wait_for("signed out", Duration::from_secs(15), || {
        let state = server.state();
        state["state"]["account"]["checked"] == true
            && state["state"]["account"]["checking"] == false
            && state["state"]["signed_out"] == true
    });
    let before = server.state()["state"]["pairs"][0]["phase"]
        .as_str()
        .unwrap()
        .to_string();
    let r = server.post("/api/v1/sync", json!({"pairs": [PAIR]}));
    assert_eq!((r.status, r.code().as_str()), (409, "signed_out"));
    let snap = server.state();
    let after = snap["state"]["pairs"][0]["phase"].as_str().unwrap();
    assert_eq!(after, before);
    assert_eq!(snap["state"]["busy"], false);
    assert!(
        !lab.lines("calls")
            .iter()
            .any(|line| line.contains("\"node.walk\"")),
        "a refused sync must not start a scan"
    );
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

fn headless_account_recovery(auto_sync: bool, keyring_locked: bool) {
    let lab = Lab::new();
    if auto_sync {
        fs::write(
            lab.path("config.toml"),
            lab.config_text()
                .replace("auto_sync = false", "auto_sync = true"),
        )
        .unwrap();
    }
    lab.remote("unlock.txt", "unlock.txt", "file", "test", "resumed");
    lab.controls(json!({"keyring_locked": keyring_locked, "signed_in": false}));
    let server = lab.serve(&[]);
    wait_for("unavailable account", Duration::from_secs(10), || {
        let s = server.state();
        s["state"]["account"]["checked"] == true
            && s["state"]["account"]["checking"] == false
            && s["state"]["account"]["keyring_locked"] == keyring_locked
    });
    assert_eq!(server.state()["state"]["signed_out"], !keyring_locked);
    assert!(!lab.local_root().join("unlock.txt").exists());
    lab.controls(json!({"keyring_locked": false}));
    // No window, account endpoint, refresh request, or auth signal. The
    // publisher must discover the session even with auto-sync disabled.
    wait_for("headless account recovery", Duration::from_secs(27), || {
        let s = server.state();
        s["state"]["account"]["signed_in"] == true
            && s["state"]["account"]["keyring_locked"] == false
            && s["state"]["signed_out"] == false
    });
    if auto_sync {
        wait_for("sync after unlock", Duration::from_secs(10), || {
            fs::read_to_string(lab.local_root().join("unlock.txt"))
                .ok()
                .as_deref()
                == Some("resumed")
        });
    } else {
        assert!(!lab.local_root().join("unlock.txt").exists());
        assert_eq!(server.state()["state"]["watching"], false);
    }
    assert_eq!(server.stop().0, 0);
    lab.wait_for_sidecars();
}

#[test]
fn headless_keyring_unlock_refreshes_account_without_a_watcher() {
    headless_account_recovery(false, true);
}

#[test]
fn headless_keyring_unlock_refreshes_account_and_resumes_sync() {
    headless_account_recovery(true, true);
}

#[test]
fn headless_session_recovery_refreshes_account_without_a_watcher() {
    headless_account_recovery(false, false);
}

#[test]
fn settings_put_rejects_negative_numbers_with_field_name() {
    let lab = Lab::new();
    let server = lab.serve(&[]);
    let r = server.put("/api/v1/settings", json!({"max_delete_files": -5}));
    assert_eq!((r.status, r.code().as_str()), (400, "bad_request"));
    let body = r.json();
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("max_delete_files"));
    assert!(!msg.contains("serde"));
    assert!(!msg.contains("u64"));
    assert_eq!(server.stop().0, 0);
}

#[test]
fn trash_api_lists_restores_refuses_overwrite_and_requires_delete_confirmation() {
    let lab = Lab::new();
    let trash = lab.path(&format!(".Trash-{}", unsafe { libc::geteuid() }));
    fs::create_dir_all(trash.join("files")).unwrap();
    fs::create_dir_all(trash.join("info")).unwrap();
    let original = lab.local_root().join("restored.txt");
    let seed = || {
        fs::write(trash.join("files/item"), "saved").unwrap();
        fs::write(
            trash.join("info/item.trashinfo"),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-10-23T12:34:56\n",
                original.display()
            ),
        )
        .unwrap();
    };
    seed();
    let server = lab.serve(&[]);
    let reply = server.get("/api/v1/trash");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let items = reply.json();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["pair"], PAIR);
    assert_eq!(items[0]["size"], 5);
    let id = items[0]["id"].as_str().unwrap();
    let base = format!("/api/v1/trash/{PAIR}/{id}");
    for body in [json!({}), json!({"confirm": false})] {
        assert_eq!(server.post(&format!("{base}/delete"), body).status, 400);
        assert!(trash.join("files/item").exists());
    }
    assert_eq!(
        server
            .post(&format!("/api/v1/trash/other/{id}/restore"), json!({}))
            .status,
        404
    );
    fs::write(&original, "existing").unwrap();
    assert_eq!(
        server.post(&format!("{base}/restore"), json!({})).status,
        409
    );
    assert_eq!(fs::read_to_string(&original).unwrap(), "existing");
    fs::remove_file(&original).unwrap();
    assert_eq!(
        server.post(&format!("{base}/restore"), json!({})).status,
        200
    );
    assert_eq!(fs::read_to_string(&original).unwrap(), "saved");
    assert_eq!(
        server.post(&format!("{base}/restore"), json!({})).status,
        404
    );
    seed();
    assert_eq!(
        server
            .post(&format!("{base}/delete"), json!({"confirm": true}))
            .status,
        200
    );
    assert!(!trash.join("files/item").exists());
    assert!(!trash.join("info/item.trashinfo").exists());
    assert_eq!(server.get("/api/v1/trash").json(), json!([]));
    assert_eq!(server.stop().0, 0);
}
