"""NiceGUI file storage location (must run before importing nicegui)."""

from __future__ import annotations

import os
from pathlib import Path


def web_data_dir() -> Path:
    raw = os.environ.get(
        "NEUTRONSYNC_WEB_DATA",
        str(Path.home() / ".local" / "state" / "neutronsync-web"),
    )
    return Path(raw).expanduser()


def nicegui_storage_dir(web_data: Path | None = None) -> Path:
    return (web_data if web_data is not None else web_data_dir()) / ".nicegui"


def ensure_nicegui_storage_path(web_data: Path | None = None) -> Path:
    """Point NiceGUI persistence at NEUTRONSYNC_WEB_DATA/.nicegui (mode 0700)."""
    path = nicegui_storage_dir(web_data).resolve()
    path.mkdir(parents=True, exist_ok=True)
    os.chmod(path, 0o700)
    os.environ["NICEGUI_STORAGE_PATH"] = str(path)
    return path
