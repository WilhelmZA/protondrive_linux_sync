"""Web login: password hash, sessions, rate limits."""

from __future__ import annotations

import hashlib
import json
import logging
import os
import secrets
import time

from neutronsync_web.settings import Settings

log = logging.getLogger("neutronsync_web.auth")

AUTH_FILE = "auth.json"
SECRET_FILE = "secret"
SESSION_DAYS = 30
MAX_ATTEMPTS = 5
WINDOW_SECONDS = 300
FAIL_DELAY = 1.0

_SCRYPT_N = 2**15
_SCRYPT_R = 8
_SCRYPT_P = 1
_SALT_LEN = 16


class AuthStore:
    def __init__(self, settings: Settings) -> None:
        self.settings = settings
        self.data_dir = settings.web_data
        self.mode = settings.web_auth
        self._attempts: dict[str, list[float]] = {}
        self.proxy_warning = (
            "Web login is off; access is controlled by your proxy."
            if self.mode == "proxy"
            else None
        )

    def prepare(self) -> bytes:
        """Create data dir, secret, and password hash. Returns storage_secret."""
        self.data_dir.mkdir(parents=True, exist_ok=True)
        os.chmod(self.data_dir, 0o700)

        secret_path = self.data_dir / SECRET_FILE
        if not secret_path.exists():
            secret_path.write_bytes(secrets.token_bytes(32))
            os.chmod(secret_path, 0o600)
        storage_secret = secret_path.read_bytes()

        if self.mode == "proxy":
            log.warning(self.proxy_warning)
            return storage_secret

        password = self.settings.web_password
        auth_path = self.data_dir / AUTH_FILE
        if not auth_path.exists():
            if not password or len(password) < 12:
                raise SystemExit(
                    "Set NEUTRONSYNC_WEB_PASSWORD (12 characters or more) "
                    "to protect the web interface."
                )
            self._write_hash(password)
        elif password is not None:
            if len(password) < 12:
                raise SystemExit(
                    "Set NEUTRONSYNC_WEB_PASSWORD (12 characters or more) "
                    "to protect the web interface."
                )
            if not self.verify_password(password):
                self._write_hash(password)

        return storage_secret

    def _write_hash(self, password: str) -> None:
        salt = secrets.token_bytes(_SALT_LEN)
        digest = hashlib.scrypt(
            password.encode("utf-8"),
            salt=salt,
            n=_SCRYPT_N,
            r=_SCRYPT_R,
            p=_SCRYPT_P,
            dklen=32,
            maxmem=128 * 1024 * 1024,
        )
        payload = {
            "algo": "scrypt",
            "n": _SCRYPT_N,
            "r": _SCRYPT_R,
            "p": _SCRYPT_P,
            "salt": salt.hex(),
            "hash": digest.hex(),
        }
        path = self.data_dir / AUTH_FILE
        path.write_text(json.dumps(payload) + "\n", encoding="utf-8")
        os.chmod(path, 0o600)

    def verify_password(self, password: str) -> bool:
        path = self.data_dir / AUTH_FILE
        data = json.loads(path.read_text(encoding="utf-8"))
        salt = bytes.fromhex(data["salt"])
        expected = bytes.fromhex(data["hash"])
        digest = hashlib.scrypt(
            password.encode("utf-8"),
            salt=salt,
            n=int(data.get("n", _SCRYPT_N)),
            r=int(data.get("r", _SCRYPT_R)),
            p=int(data.get("p", _SCRYPT_P)),
            dklen=len(expected),
            maxmem=128 * 1024 * 1024,
        )
        return secrets.compare_digest(digest, expected)

    def rate_limited(self, client: str) -> bool:
        now = time.monotonic()
        stamps = [t for t in self._attempts.get(client, []) if now - t < WINDOW_SECONDS]
        self._attempts[client] = stamps
        return len(stamps) >= MAX_ATTEMPTS

    def record_failure(self, client: str) -> None:
        now = time.monotonic()
        stamps = [t for t in self._attempts.get(client, []) if now - t < WINDOW_SECONDS]
        stamps.append(now)
        self._attempts[client] = stamps

    def clear_failures(self, client: str) -> None:
        self._attempts.pop(client, None)

    @staticmethod
    def session_expired(signed_at: float | None) -> bool:
        if signed_at is None:
            return True
        return (time.time() - signed_at) > SESSION_DAYS * 86400


def client_address(request) -> str:
    if request.client:
        return request.client.host or "unknown"
    return "unknown"
