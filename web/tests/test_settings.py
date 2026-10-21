"""Configuration loading."""

from __future__ import annotations

import pytest

from neutronsync_web.settings import ConfigError, load_settings


def test_bad_port_names_variable(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("NEUTRONSYNC_WEB_PORT", "not-a-port")
    with pytest.raises(ConfigError, match="NEUTRONSYNC_WEB_PORT"):
        load_settings()


def test_defaults(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.delenv("NEUTRONSYNC_API_URL", raising=False)
    monkeypatch.delenv("NEUTRONSYNC_API_TOKEN", raising=False)
    monkeypatch.delenv("NEUTRONSYNC_WEB_HOST", raising=False)
    monkeypatch.delenv("NEUTRONSYNC_WEB_PORT", raising=False)
    monkeypatch.delenv("NEUTRONSYNC_WEB_AUTH", raising=False)
    monkeypatch.setenv("NEUTRONSYNC_WEB_DATA", str(tmp_path))
    s = load_settings()
    assert s.api_url == "http://127.0.0.1:8090"
    assert s.web_host == "127.0.0.1"
    assert s.web_port == 8080
    assert s.web_auth == "password"


def test_empty_web_auth_means_password(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.setenv("NEUTRONSYNC_WEB_AUTH", "")
    monkeypatch.setenv("NEUTRONSYNC_WEB_DATA", str(tmp_path))
    s = load_settings()
    assert s.web_auth == "password"
