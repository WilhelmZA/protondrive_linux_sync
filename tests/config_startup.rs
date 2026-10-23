//! Invalid-config recovery against a loopback listener and scratch files only.
#![cfg(feature = "serve")]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const TOKEN: &str = "startup-test-token-with-at-least-32-characters";

struct Lab {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Lab {
    fn start(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("home")).unwrap();
        fs::write(root.join("config.toml"), config).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_neutronsync"))
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("home/.config"))
            .env("XDG_STATE_HOME", root.join("home/.local/state"))
            .env("XDG_DATA_HOME", root.join("home/.local/share"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("NEUTRONSYNC_API_TOKEN", TOKEN)
            .env_remove("NEUTRONSYNC_CONFIG")
            .args([
                "--config",
                root.join("config.toml").to_str().unwrap(),
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lab = Self { child, port, dir };
        let start = Instant::now();
        loop {
            assert!(
                lab.child.try_wait().unwrap().is_none(),
                "engine exited with invalid config"
            );
            if let Ok((200, _)) = lab.request("GET", "/api/v1/health", "") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "startup timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        lab
    }

    fn request(&self, method: &str, path: &str, headers: &str) -> std::io::Result<(u16, Value)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n{headers}\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        Ok((status, serde_json::from_str(body).unwrap()))
    }

    fn authorized(&self, method: &str, path: &str) -> (u16, Value) {
        self.request(method, path, &format!("Authorization: Bearer {TOKEN}\r\n"))
            .unwrap()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn overlapping_pairs_report_startup_error_without_starting_engine() {
    let config = r#"
[cli]
sidecar = "/must-not-start"
[options]
auto_sync = true
[[pair]]
name = "original"
local = "/one"
remote = "/my-files/test"
[[pair]]
name = "mirror"
local = "/two"
remote = "/my-files/test"
"#;
    let lab = Lab::start(config);
    for (method, path) in [
        ("GET", "/api/v1/version"),
        ("POST", "/api/v1/sync"),
        ("DELETE", "/api/v1/pairs/mirror"),
        ("POST", "/api/v1/account/login"),
    ] {
        let (status, body) = lab.authorized(method, path);
        assert_eq!(status, 422);
        assert_eq!(body["error"]["code"], "invalid_config");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one folder cannot be in two pairs"));
    }
    assert_eq!(lab.request("GET", "/api/v1/version", "").unwrap().0, 401);
    assert_eq!(
        lab.request(
            "GET",
            "/api/v1/version",
            &format!("Authorization: Bearer {TOKEN}\r\nOrigin: https://evil.example\r\n")
        )
        .unwrap()
        .0,
        403
    );
    assert!(!lab
        .dir
        .path()
        .join("home/.local/state/neutronsync")
        .exists());
    assert!(!lab.dir.path().join("runtime/neutronsync/api.sock").exists());
    assert_eq!(
        fs::read_to_string(lab.dir.path().join("config.toml")).unwrap(),
        config
    );
}

#[test]
fn malformed_config_is_redacted_and_external_correction_recovers() {
    let lab = Lab::start("[options]\nsecret = 'do-not-echo-me' invalid\n");
    let (status, body) = lab.authorized("GET", "/api/v1/version");
    assert_eq!(status, 422);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("invalid TOML"));
    assert!(!body.to_string().contains("do-not-echo-me"));
    // No pairs, auto-sync off, and no sidecar executable: recovery itself must
    // not require a Proton account, an auth call or a running sync.
    fs::write(
        lab.dir.path().join("config.toml"),
        "[options]\nauto_sync = false\n",
    )
    .unwrap();
    let start = Instant::now();
    loop {
        if let Ok((200, body)) = lab.request(
            "GET",
            "/api/v1/version",
            &format!("Authorization: Bearer {TOKEN}\r\n"),
        ) {
            assert_eq!(body["api_version"], 1);
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "engine did not recover"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
