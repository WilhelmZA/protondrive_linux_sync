"""Password auth store (AC-5, AC-7)."""

from __future__ import annotations

import json
import logging
import time
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

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


def test_first_start_without_password(tmp_path: Path, caplog) -> None:
    store = AuthStore(_settings(tmp_path, None))
    store.prepare()
    assert store.needs_setup
    assert store.setup_code
    assert f"NeutronSync setup code: {store.setup_code}" in caplog.text
    assert not store.verify_password("anything")
    assert not (tmp_path / "auth.json").exists()


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


def test_environment_only_seeds_password(tmp_path: Path) -> None:
    store = AuthStore(_settings(tmp_path, "old-password-1234"))
    store.prepare()
    old = (tmp_path / "auth.json").read_text(encoding="utf-8")
    store2 = AuthStore(_settings(tmp_path, "new-password-5678"))
    store2.prepare()
    new = (tmp_path / "auth.json").read_text(encoding="utf-8")
    assert old == new
    assert not store2.verify_password("new-password-5678")
    assert store2.verify_password("old-password-1234")


def test_setup_code_rotation_validation_and_consumption(tmp_path: Path, caplog) -> None:
    first = AuthStore(_settings(tmp_path, None))
    first.prepare()
    store = AuthStore(_settings(tmp_path, None))
    store.prepare()
    assert first.setup_code != store.setup_code
    for code, password, confirmation, error in (
        (first.setup_code, "a" * 12, "a" * 12, "setup code"),
        ("WRONG", "a" * 12, "a" * 12, "setup code"),
        (store.setup_code, "short", "short", "12 characters"),
        (store.setup_code, "a" * 12, "b" * 12, "do not match"),
    ):
        with pytest.raises(ValueError, match=error):
            store.complete_setup(code, password, confirmation)
        assert store.needs_setup
    code = store.setup_code
    store.complete_setup(code, "a" * 12, "a" * 12)
    assert not store.needs_setup
    assert store.setup_code is None
    with pytest.raises(ValueError, match="expired"):
        store.complete_setup(code, "b" * 12, "b" * 12)
    caplog.clear()
    restarted = AuthStore(_settings(tmp_path, None))
    restarted.prepare()
    assert restarted.verify_password("a" * 12)
    assert not restarted.needs_setup
    assert not caplog.records


def test_change_password_and_explicit_reset(tmp_path: Path) -> None:
    settings = _settings(tmp_path, "old-password-1234")
    store = AuthStore(settings)
    store.prepare()
    version = store.session_version
    with pytest.raises(ValueError, match="current password"):
        store.change_password("wrong", "new-password-5678", "new-password-5678")
    with pytest.raises(ValueError, match="do not match"):
        store.change_password("old-password-1234", "new-password-5678", "different")
    assert store.session_version == version
    store.change_password("old-password-1234", "new-password-5678", "new-password-5678")
    assert store.session_version != version
    changed_version = store.session_version
    restarted = AuthStore(settings)
    restarted.prepare()
    assert restarted.verify_password("new-password-5678")
    assert restarted.session_version == changed_version
    reset = AuthStore(replace(settings, web_password_reset=True))
    reset.prepare()
    assert reset.verify_password("old-password-1234")
    assert reset.session_version != changed_version


def test_reset_requires_valid_seed(tmp_path: Path) -> None:
    store = AuthStore(replace(_settings(tmp_path, None), web_password_reset=True))
    with pytest.raises(SystemExit, match="12 characters"):
        store.prepare()


def test_deleting_auth_file_restores_setup(tmp_path: Path) -> None:
    store = AuthStore(_settings(tmp_path, "old-password-1234"))
    store.prepare()
    (tmp_path / "auth.json").unlink()
    restarted = AuthStore(_settings(tmp_path, None))
    restarted.prepare()
    assert restarted.needs_setup


def test_invalid_seed_is_ignored_when_password_exists(tmp_path: Path) -> None:
    AuthStore(_settings(tmp_path, "old-password-1234")).prepare()
    store = AuthStore(_settings(tmp_path, "short"))
    store.prepare()
    assert store.verify_password("old-password-1234")


def test_proxy_mode_logs_warning(tmp_path: Path, caplog: pytest.LogCaptureFixture) -> None:
    store = AuthStore(_settings(tmp_path, None, mode="proxy"))
    with caplog.at_level(logging.WARNING, logger="neutronsync_web.auth"):
        store.prepare()
    assert any("Web login is off" in r.message for r in caplog.records)
    assert store.proxy_warning
    assert not (tmp_path / "auth.json").exists()
    assert not store.needs_setup
    assert store.setup_code is None


def test_password_rotation_invalidates_other_session(tmp_path: Path, monkeypatch) -> None:
    from neutronsync_web.pages import layout

    store = AuthStore(_settings(tmp_path, "old-password-1234"))
    store.prepare()
    session = {
        "signed_in": True,
        "signed_at": time.time(),
        "session_version": store.session_version,
    }
    monkeypatch.setattr(layout.ctx, "get_auth", lambda: store)
    monkeypatch.setattr(layout, "app", SimpleNamespace(storage=SimpleNamespace(user=session)))
    assert layout.require_session()
    store.change_password("old-password-1234", "new-password-5678", "new-password-5678")
    assert not layout.require_session()
    assert session == {}
