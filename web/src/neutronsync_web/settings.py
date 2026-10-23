"""Configuration from the environment."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


class ConfigError(SystemExit):
    """Startup stopped because one environment variable is wrong."""


def _read_secret(var: str, file_var: str) -> str | None:
    file_path = os.environ.get(file_var)
    if file_path:
        try:
            return Path(file_path).read_text(encoding="utf-8").strip()
        except OSError as exc:
            raise ConfigError(f"Cannot read {file_var}: {exc}") from exc
    value = os.environ.get(var)
    if value is None:
        return None
    return value.strip()


@dataclass(frozen=True)
class Settings:
    api_url: str
    api_token: str | None
    web_host: str
    web_port: int
    web_auth: str
    web_password: str | None
    web_data: Path
    web_password_reset: bool = False


def load_settings() -> Settings:
    api_url = os.environ.get("NEUTRONSYNC_API_URL", "http://127.0.0.1:8090").rstrip("/")
    api_token = _read_secret("NEUTRONSYNC_API_TOKEN", "NEUTRONSYNC_API_TOKEN_FILE")
    if api_token == "":
        api_token = None

    web_host = os.environ.get("NEUTRONSYNC_WEB_HOST", "127.0.0.1")
    port_raw = os.environ.get("NEUTRONSYNC_WEB_PORT", "8080")
    try:
        web_port = int(port_raw)
    except ValueError as exc:
        raise ConfigError(f"NEUTRONSYNC_WEB_PORT must be a number, got {port_raw!r}.") from exc
    if not (1 <= web_port <= 65535):
        raise ConfigError(f"NEUTRONSYNC_WEB_PORT must be between 1 and 65535, got {web_port}.")

    web_auth_raw = os.environ.get("NEUTRONSYNC_WEB_AUTH", "password")
    web_auth = (web_auth_raw or "password").strip().lower()
    if web_auth not in ("password", "proxy"):
        raise ConfigError(f"NEUTRONSYNC_WEB_AUTH must be 'password' or 'proxy', got {web_auth!r}.")

    web_password = _read_secret("NEUTRONSYNC_WEB_PASSWORD", "NEUTRONSYNC_WEB_PASSWORD_FILE")
    if web_password == "":
        web_password = None

    data_raw = os.environ.get(
        "NEUTRONSYNC_WEB_DATA",
        str(Path.home() / ".local" / "state" / "neutronsync-web"),
    )
    web_data = Path(data_raw).expanduser()

    return Settings(
        api_url=api_url,
        api_token=api_token,
        web_host=web_host,
        web_port=web_port,
        web_auth=web_auth,
        web_password=web_password,
        web_data=web_data,
        web_password_reset=os.environ.get("NEUTRONSYNC_WEB_PASSWORD_RESET") == "1",
    )
