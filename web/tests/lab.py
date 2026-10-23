"""Scratch world: neutronsync serve + web app, both on 127.0.0.1."""

from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import time
from pathlib import Path

from tests.conftest import REPO, WEB, find_binary, free_port

SIDECAR_SRC = REPO / "tests" / "fixtures" / "drive_sidecar.py"
WEB_PASSWORD = "web-test-password-ok"


class Lab:
    def __init__(self, tmp: Path, *, fake_auth: str | None = None, extra_options: str = "") -> None:
        self.root = tmp
        self.root.mkdir(parents=True, exist_ok=True)
        self.binary = find_binary()
        if self.binary is None:
            raise RuntimeError(
                "neutronsync binary with serve not found "
                "(expected target/debug/neutronsync or target/release/neutronsync)"
            )
        self.sidecar = tmp / "sidecar.py"
        shutil.copy(SIDECAR_SRC, self.sidecar)
        self.sidecar.chmod(0o700)
        for name in ("local", "home", "runtime", "webdata"):
            (tmp / name).mkdir(parents=True, exist_ok=True)
        (tmp / "runtime").chmod(0o700)
        self.config = tmp / "config.toml"
        self.config.write_text(
            f"""
[cli]
backend = "api"
sidecar = {str(self.sidecar)!r}
download_threads = 1
session_store = "file"
[options]
state_dir = {str(tmp / "state")!r}
propagate_deletes = true
local_delete = "remove"
auto_sync = false
max_delete_files = 5
max_delete_percent = 10
{extra_options}
[[pair]]
name = "photos"
local = {str(tmp / "local" / "photos")!r}
remote = "/my-files/photos"
""",
            encoding="utf-8",
        )
        (tmp / "local" / "photos").mkdir(parents=True, exist_ok=True)
        (tmp / "tree.json").write_text(
            json.dumps(
                {
                    "root": {"name": "my-files", "type": "folder", "parent_uid": None},
                    "photos": {
                        "name": "photos",
                        "type": "folder",
                        "parent_uid": "root",
                    },
                }
            ),
            encoding="utf-8",
        )
        # Session key for file store
        key = tmp / "session.key"
        key.write_bytes(os.urandom(32))
        key.chmod(0o600)
        self.session_key = key
        self.token = "t" * 32
        self.fake_auth = fake_auth
        self.serve_port = free_port()
        self.web_port = free_port()
        self.serve_proc: subprocess.Popen | None = None
        self.web_proc: subprocess.Popen | None = None
        self.api_url = f"http://127.0.0.1:{self.serve_port}"
        self.web_url = f"http://127.0.0.1:{self.web_port}"

    def controls(self, value: dict) -> None:
        (self.root / "controls.json").write_text(json.dumps(value), encoding="utf-8")

    def tree(self) -> dict:
        return json.loads((self.root / "tree.json").read_text(encoding="utf-8"))

    def save_tree(self, value: dict) -> None:
        (self.root / "tree.json").write_text(json.dumps(value), encoding="utf-8")

    def remote_file(self, uid: str, name: str, parent: str, body: str) -> None:
        tree = self.tree()
        tree[uid] = {
            "name": name,
            "type": "file",
            "parent_uid": parent,
            "size": len(body),
            "sha1": "a" * 40,
            "data": body.encode().hex(),
            "mtime": 1_700_000_000,
        }
        self.save_tree(tree)

    def both(self, name: str, body: str) -> None:
        self.remote_file(name, name, "photos", body)
        p = self.root / "local" / "photos" / name
        p.write_text(body, encoding="utf-8")
        os.utime(p, (1_700_000_000, 1_700_000_000))

    def _env_base(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "NEUTRONSYNC_DRIVE_STDIO": "1",
                "HOME": str(self.root / "home"),
                "XDG_CONFIG_HOME": str(self.root / "home" / ".config"),
                "XDG_STATE_HOME": str(self.root / "home" / ".local" / "state"),
                "XDG_DATA_HOME": str(self.root / "home" / ".local" / "share"),
                "XDG_RUNTIME_DIR": str(self.root / "runtime"),
                "NEUTRONSYNC_SESSION_STORE": "file",
                "NEUTRONSYNC_SESSION_KEY_FILE": str(self.session_key),
                "NEUTRONSYNC_API_TOKEN": self.token,
            }
        )
        env.pop("NEUTRONSYNC_CONFIG", None)
        if self.fake_auth:
            env["NEUTRONSYNC_FAKE_AUTH"] = self.fake_auth
        else:
            env.pop("NEUTRONSYNC_FAKE_AUTH", None)
        return env

    def start_serve(self) -> None:
        stderr = open(self.root / "serve.stderr", "w", encoding="utf-8")
        self.serve_proc = subprocess.Popen(
            [
                str(self.binary),
                "--config",
                str(self.config),
                "serve",
                "--listen",
                f"127.0.0.1:{self.serve_port}",
                "--local-root",
                str(self.root / "local"),
            ],
            env=self._env_base(),
            stdout=subprocess.DEVNULL,
            stderr=stderr,
            cwd=str(self.root),
        )
        self._wait_port(self.serve_port, "serve")

    def start_web(
        self,
        *,
        auth_mode: str = "password",
        web_host: str = "127.0.0.1",
        web_password: str | None = WEB_PASSWORD,
        password_reset: bool = False,
    ) -> None:
        env = self._env_base()
        env.pop("NEUTRONSYNC_WEB_PASSWORD_FILE", None)
        env.pop("NEUTRONSYNC_WEB_PASSWORD_RESET", None)
        env.update(
            {
                "NEUTRONSYNC_API_URL": self.api_url,
                "NEUTRONSYNC_API_TOKEN": self.token,
                "NEUTRONSYNC_WEB_HOST": web_host,
                "NEUTRONSYNC_WEB_PORT": str(self.web_port),
                "NEUTRONSYNC_WEB_AUTH": auth_mode,
                "NEUTRONSYNC_WEB_DATA": str(self.root / "webdata"),
                "PYTHONPATH": str(WEB / "src"),
            }
        )
        if auth_mode == "password" and web_password is not None:
            env["NEUTRONSYNC_WEB_PASSWORD"] = web_password
        else:
            env.pop("NEUTRONSYNC_WEB_PASSWORD", None)
        if password_reset:
            env["NEUTRONSYNC_WEB_PASSWORD_RESET"] = "1"
        # NiceGUI treats PYTEST_CURRENT_TEST as "run under screen fixture".
        env.pop("PYTEST_CURRENT_TEST", None)
        stderr = open(self.root / "web.stderr", "w", encoding="utf-8")
        self.web_proc = subprocess.Popen(
            [
                str(WEB / ".venv" / "bin" / "python"),
                "-m",
                "neutronsync_web",
            ],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=stderr,
            cwd=str(WEB),
        )
        self._wait_port(self.web_port, "web")

    def start(self, *, auth_mode: str = "password", web_host: str = "127.0.0.1") -> None:
        self.start_serve()
        self.start_web(auth_mode=auth_mode, web_host=web_host)

    def _wait_port(self, port: int, name: str, timeout: float = 30.0) -> None:
        t0 = time.time()
        while time.time() - t0 < timeout:
            proc = self.serve_proc if name == "serve" else self.web_proc
            if proc and proc.poll() is not None:
                err = (self.root / f"{name}.stderr").read_text(encoding="utf-8")
                raise RuntimeError(f"{name} exited early:\n{err}")
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.2)
                try:
                    s.connect(("127.0.0.1", port))
                    return
                except OSError:
                    time.sleep(0.05)
        raise TimeoutError(f"{name} did not listen on {port}")

    def stop(self) -> None:
        for proc in (self.web_proc, self.serve_proc):
            if proc and proc.poll() is None:
                proc.send_signal(signal.SIGTERM)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
        self.web_proc = None
        self.serve_proc = None

    def config_text(self) -> str:
        return self.config.read_text(encoding="utf-8")
