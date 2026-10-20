"""Password auth store (AC-5, AC-7)."""

from __future__ import annotations

import json
import logging
from pathlib import Path

import pytest

from neutronsync_web.auth import AuthStore
from neutronsync_web.settings import Settings


def _settings(tmp_path: Path, password: str | None, mode: str = "password") -> Settings:
    return Settings(
        api_url="http://127.0.0.1:8090",
        api_token=None,
        web_host="127.0.0.1",
        web_port=8080,
        web_auth=mode,
        web_password=password,
        web_data=tmp_path,
    )


def test_first_start_without_password(tmp_path: Path) -> None:
    store = AuthStore(_settings(tmp_path, None))
    with pytest.raises(SystemExit, match="NEUTRONSYNC_WEB_PASSWORD"):
        store.prepare()


def test_eleven_char_password_refused(tmp_path: Path) -> None:
    store = AuthStore(_settings(tmp_path, "12345678901"))
    with pytest.raises(SystemExit, match="12 characters"):
        store.prepare()


def test_password_hash_mode_and_no_plaintext(tmp_path: Path) -> None:
    password = "correct-horse-battery-staple"
    store = AuthStore(_settings(tmp_path, password))
    store.prepare()
    auth_path = tmp_path / "auth.json"
    assert auth_path.exists()
    assert (auth_path.stat().st_mode & 0o777) == 0o600
    text = auth_path.read_text(encoding="utf-8")
    assert password not in text
    data = json.loads(text)
    assert "hash" in data and "salt" in data
    assert store.verify_password(password)
    assert not store.verify_password("wrong-password-xx")


def test_rate_limit_and_success(tmp_path: Path) -> None:
    password = "correct-horse-battery-staple"
    store = AuthStore(_settings(tmp_path, password))
    store.prepare()
    client = "127.0.0.1"
    for _ in range(5):
        assert not store.rate_limited(client)
        store.record_failure(client)
    assert store.rate_limited(client)
    store.clear_failures(client)
    assert not store.rate_limited(client)
    assert store.verify_password(password)


def test_password_change_replaces_hash(tmp_path: Path) -> None:
    store = AuthStore(_settings(tmp_path, "old-password-1234"))
    store.prepare()
    old = (tmp_path / "auth.json").read_text(encoding="utf-8")
    store2 = AuthStore(_settings(tmp_path, "new-password-5678"))
    store2.prepare()
    new = (tmp_path / "auth.json").read_text(encoding="utf-8")
    assert old != new
    assert store2.verify_password("new-password-5678")
    assert not store2.verify_password("old-password-1234")


def test_proxy_mode_logs_warning(tmp_path: Path, caplog: pytest.LogCaptureFixture) -> None:
    store = AuthStore(_settings(tmp_path, None, mode="proxy"))
    with caplog.at_level(logging.WARNING, logger="neutronsync_web.auth"):
        store.prepare()
    assert any("Web login is off" in r.message for r in caplog.records)
    assert store.proxy_warning
    assert not (tmp_path / "auth.json").exists()
