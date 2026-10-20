"""Shared fixtures."""

from __future__ import annotations

import os
import socket
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
WEB = Path(__file__).resolve().parents[1]
LOGO_SRC = REPO / "assets" / "logo.png"
LOGO_WEB = WEB / "src" / "neutronsync_web" / "static" / "logo.png"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def find_binary() -> Path | None:
    for rel in ("target/debug/neutronsync", "target/release/neutronsync"):
        p = REPO / rel
        if p.is_file() and os.access(p, os.X_OK):
            return p
    return None


def skip_without_binary() -> None:
    if find_binary() is None:
        pytest.skip(
            "neutronsync binary with --features serve not found "
            "(target/debug/neutronsync or target/release/neutronsync)"
        )


def skip_without_chromium() -> None:
    """Skip before launching Chromium when the browser binary is missing."""
    cache = Path(
        os.environ.get(
            "PLAYWRIGHT_BROWSERS_PATH",
            str(Path.home() / ".cache" / "ms-playwright"),
        )
    )
    if not cache.is_dir():
        pytest.skip("Chromium not installed; run: uv run playwright install chromium")
    found = any(
        p.is_dir() and p.name.startswith(("chromium", "chromium_headless_shell"))
        for p in cache.iterdir()
    )
    if not found:
        pytest.skip("Chromium not installed; run: uv run playwright install chromium")


@pytest.fixture
def repo_root() -> Path:
    return REPO


@pytest.fixture(scope="session")
def playwright():
    skip_without_chromium()
    from playwright.sync_api import sync_playwright

    with sync_playwright() as p:
        yield p


@pytest.fixture
def browser_page(playwright):
    skip_without_binary()
    skip_without_chromium()
    browser = playwright.chromium.launch(headless=True)
    context = browser.new_context(viewport={"width": 1280, "height": 800})
    page = context.new_page()
    yield page, context
    context.close()
    browser.close()
