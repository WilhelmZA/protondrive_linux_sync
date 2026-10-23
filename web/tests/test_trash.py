"""Trash page interactions with a fake API and an in-memory UI."""

from contextlib import nullcontext
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from neutronsync_web.pages import trash


class Element:
    def __init__(self):
        self.closed = False

    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass

    def classes(self, *args):
        return self

    def props(self, *args):
        return self

    def clear(self):
        pass

    def open(self):
        pass

    def close(self):
        self.closed = True


class UI:
    def __init__(self):
        self.buttons = []
        self.labels = []
        self.context = SimpleNamespace(client=nullcontext())

    def column(self):
        return Element()

    card = row = dialog = column

    def label(self, text):
        self.labels.append(text)
        return Element()

    def button(self, text, *, on_click):
        self.buttons.append((text, on_click))
        return Element()

    def notify(self, *args, **kwargs):
        pass

    def timer(self, _, callback, **kwargs):
        self.load = callback


@pytest.mark.asyncio
async def test_delete_requires_dialog_confirmation_and_restore_uses_api(monkeypatch):
    ui = UI()
    item = {
        "id": "one",
        "pair": "Documents",
        "name": "a.txt",
        "original_path": "/data/a.txt",
        "size": 12,
        "deleted_at": "2026-10-23T12:34:56",
    }
    api = SimpleNamespace(
        trash=AsyncMock(return_value=[item]), trash_restore=AsyncMock(), trash_delete=AsyncMock()
    )
    monkeypatch.setattr(trash, "ui", ui)
    monkeypatch.setattr(trash, "page_shell", lambda _, body: body())
    monkeypatch.setattr(trash.ctx, "get_api", lambda: api)
    trash.render()
    await ui.load()
    assert "Folder: Documents" in ui.labels
    assert "/data/a.txt" in ui.labels
    assert "12 bytes · Deleted 2026-10-23T12:34:56" in ui.labels
    restore = next(cb for text, cb in ui.buttons if text == "Restore")
    await restore()
    api.trash_restore.assert_awaited_once_with("Documents", "one")
    delete = next(cb for text, cb in ui.buttons if text == "Delete permanently")
    delete()
    api.trash_delete.assert_not_awaited()
    assert "This cannot be undone." in ui.labels
    cancel = next(cb for text, cb in ui.buttons if text == "Cancel")
    cancel()
    api.trash_delete.assert_not_awaited()
    delete()
    await ui.buttons[-1][1]()
    api.trash_delete.assert_awaited_once_with("Documents", "one", confirm=True)


@pytest.mark.asyncio
async def test_empty_and_error_states(monkeypatch):
    ui = UI()
    api = SimpleNamespace(trash=AsyncMock(return_value=[]))
    monkeypatch.setattr(trash, "ui", ui)
    monkeypatch.setattr(trash, "page_shell", lambda _, body: body())
    monkeypatch.setattr(trash.ctx, "get_api", lambda: api)
    trash.render()
    await ui.load()
    assert "Trash is empty." in ui.labels
    api.trash.side_effect = trash.ApiError("conflict", "Folder is not mounted", 409)
    await ui.load()
    assert "Folder is not mounted" in ui.labels
