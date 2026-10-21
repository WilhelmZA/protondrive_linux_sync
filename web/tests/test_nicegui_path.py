"""NiceGUI persistence directory under NEUTRONSYNC_WEB_DATA."""

from __future__ import annotations

import os
import stat

from neutronsync_web.nicegui_path import ensure_nicegui_storage_path, nicegui_storage_dir


def test_nicegui_storage_under_web_data(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("NEUTRONSYNC_WEB_DATA", str(tmp_path))
    path = ensure_nicegui_storage_path()
    assert path == nicegui_storage_dir(tmp_path).resolve()
    assert path.is_dir()
    assert stat.S_IMODE(path.stat().st_mode) == 0o700
    assert os.environ["NICEGUI_STORAGE_PATH"] == str(path)
