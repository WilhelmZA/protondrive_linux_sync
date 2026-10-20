#!/usr/bin/env python3
"""Playwright AC-8 (and helpers for AC-9) against a running container web UI.

Env:
  WEB_URL          e.g. http://127.0.0.1:32768
  WEB_PASSWORD     container web password
  DATA_PHOTOS      host path for /data/photos (created if missing)
  PHASE            ac8 | ac9-prepare | ac9-assert (default ac8)
  EXPECT_TRASH     host path to .Trash-<uid>/files (ac9-assert)
"""
from __future__ import annotations

import os
import re
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

WEB_URL = os.environ["WEB_URL"].rstrip("/")
WEB_PASSWORD = os.environ["WEB_PASSWORD"]
# Host path for assertions; container path for the folder picker (serve --local-root /data).
DATA_PHOTOS = Path(os.environ.get("DATA_PHOTOS", "/tmp/ns-photos"))
CONTAINER_PHOTOS = os.environ.get("CONTAINER_PHOTOS", "/data/photos")
PHASE = os.environ.get("PHASE", "ac8")
EXPECT_TRASH = Path(os.environ["EXPECT_TRASH"]) if os.environ.get("EXPECT_TRASH") else None


def _login(page) -> None:
    page.goto(f"{WEB_URL}/login", wait_until="domcontentloaded")
    page.get_by_label("Password").fill(WEB_PASSWORD)
    page.get_by_role("button", name=re.compile("sign in", re.I)).click()
    page.wait_for_function("() => !window.location.pathname.includes('/login')", timeout=20000)


def _wait_live(page, timeout: float = 20000) -> None:
    page.locator(".ns-header.ns-live-ready").first.wait_for(state="attached", timeout=timeout)


def _fill_synced(page, locator, text: str) -> None:
    locator.click()
    locator.fill("")
    locator.type(text, delay=10)
    locator.press("Tab")
    page.wait_for_timeout(200)


def _pick_folder(page, *, local_path: str | None = None, remote_name: str | None = None) -> None:
    picker = page.locator(".q-dialog").last
    picker.get_by_role("button", name=re.compile("use this folder", re.I)).wait_for(timeout=15000)
    if local_path is not None:
        root = str(Path(local_path).parent)
        picker.get_by_role("button", name=root).click()
        picker.get_by_role("button", name=Path(local_path).name).click()
    elif remote_name is not None:
        picker.get_by_role("button", name=remote_name).click()
    picker.get_by_role("button", name=re.compile("use this folder", re.I)).click()


def _click_sync_and_wait(page, *, timeout: float = 90.0) -> None:
    """Click Sync now and wait until the engine is idle again.

    The Sync now button can still be visible for a tick after the click, so a
    plain ``wait_for(Sync now)`` returns immediately and races the sync. Wait
    for Cancel (busy) then Sync now, or a few seconds of idle if the run was
    too fast to observe Cancel.
    """
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    deadline = time.time() + timeout
    saw_cancel = False
    idle_ticks = 0
    while time.time() < deadline:
        if page.get_by_role("button", name=re.compile("^cancel$", re.I)).count():
            saw_cancel = True
            idle_ticks = 0
        elif page.get_by_role("button", name=re.compile("sync now", re.I)).count():
            if saw_cancel:
                page.wait_for_timeout(400)
                return
            idle_ticks += 1
            # ~5s idle without ever seeing Cancel: empty/no-op or missed busy.
            if idle_ticks >= 10:
                return
        page.wait_for_timeout(500)
    raise SystemExit("timed out waiting for Sync now to finish")


def phase_ac8(page) -> None:
    DATA_PHOTOS.mkdir(parents=True, exist_ok=True)
    (DATA_PHOTOS / "keep.txt").write_text("stay", encoding="utf-8")

    _login(page)

    # Proton sign-in with FAKE_AUTH=totp
    page.goto(f"{WEB_URL}/account")
    page.get_by_label("Email or username").wait_for(timeout=20000)
    _wait_live(page)
    _fill_synced(page, page.get_by_label("Email or username"), "user")
    _fill_synced(page, page.get_by_label("Password", exact=True), "secret")
    page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
    page.get_by_label("6-digit code").wait_for(timeout=20000)
    _fill_synced(page, page.get_by_label("6-digit code"), "123456")
    page.get_by_role("button", name=re.compile("^continue$", re.I)).click()
    page.get_by_text(re.compile(r"Signed in|\d{2,}", re.I)).first.wait_for(timeout=20000)

    # Add pair under /data/photos
    page.goto(f"{WEB_URL}/folders")
    page.get_by_text("Folders", exact=False).first.wait_for(timeout=15000)
    _wait_live(page)
    page.get_by_role("button", name=re.compile("add folder", re.I)).click()
    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_label("Name").fill("photos")
    dialog.get_by_role("button", name="Browse…").first.click()
    _pick_folder(page, local_path=CONTAINER_PHOTOS)
    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_role("button", name="Browse…").nth(1).click()
    _pick_folder(page, remote_name="photos")
    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_role("button", name=re.compile("^save$", re.I)).click()
    page.get_by_text("photos", exact=False).first.wait_for(timeout=20000)

    # Dry run on Activity
    page.goto(f"{WEB_URL}/")
    page.get_by_text("Activity", exact=True).first.wait_for(timeout=15000)
    _wait_live(page)
    page.get_by_role("button", name=re.compile("dry run", re.I)).click()
    page.get_by_text(
        re.compile(r"photos|Scanning|Syncing|dry|keep\.txt|download|mkdir", re.I)
    ).first.wait_for(timeout=30000)
    print("ac8_ok")


def phase_ac9_prepare(page) -> None:
    """Sync once so both sides share a baseline for remote.txt."""
    _login(page)
    page.goto(f"{WEB_URL}/")
    page.get_by_text("Activity", exact=True).first.wait_for(timeout=15000)
    _wait_live(page)
    _click_sync_and_wait(page)
    # keep.txt + remote.txt should both be tracked after this sync.
    page.get_by_text(re.compile(r"[2-9]\s+files?\s+tracked", re.I)).first.wait_for(
        timeout=30000
    )
    print("ac9_prepare_ok")


def phase_ac9_assert(page) -> None:
    """After remote deletion in the fixture, sync from Activity and check trash.

    The Activity feed labels each item with ``op.path`` (and an icon for the
    action). Catch ``delete-local remote.txt`` while In progress when we can;
    require the host-visible trash file as hard proof.
    """
    if EXPECT_TRASH is None:
        raise SystemExit("EXPECT_TRASH not set")

    _login(page)
    page.goto(f"{WEB_URL}/")
    page.get_by_text("Activity", exact=True).first.wait_for(timeout=15000)
    _wait_live(page)

    saw_delete_local = False

    def _watch_body() -> None:
        nonlocal saw_delete_local
        try:
            body = page.inner_text("body")
            if re.search(r"delete-local\s+remote\.txt", body, re.I):
                saw_delete_local = True
        except Exception:
            pass

    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    deadline = time.time() + 90
    found_trash = False
    saw_cancel = False
    idle_ticks = 0
    while time.time() < deadline:
        _watch_body()
        if page.get_by_role("button", name=re.compile("^cancel$", re.I)).count():
            saw_cancel = True
            idle_ticks = 0
        elif page.get_by_role("button", name=re.compile("sync now", re.I)).count():
            if saw_cancel:
                idle_ticks += 1
                if idle_ticks >= 2:
                    break
            else:
                idle_ticks += 1
                if idle_ticks >= 10:
                    break
        if EXPECT_TRASH.is_dir() and any(
            "remote" in p.name for p in EXPECT_TRASH.iterdir()
        ):
            found_trash = True
            break
        page.wait_for_timeout(500)

    # Drain a little longer for gio trash to land on the host mount.
    drain = time.time() + 20
    while time.time() < drain and not found_trash:
        _watch_body()
        if EXPECT_TRASH.is_dir() and any(
            "remote" in p.name for p in EXPECT_TRASH.iterdir()
        ):
            found_trash = True
            break
        page.wait_for_timeout(400)

    body = page.inner_text("body")
    if not saw_delete_local and not re.search(r"remote\.txt", body, re.I):
        raise SystemExit(
            "Activity never reported remote.txt delete; "
            f"body={body[:1500]!r}"
        )
    if not found_trash:
        listing = list(EXPECT_TRASH.iterdir()) if EXPECT_TRASH.is_dir() else []
        raise SystemExit(f"trash missing remote.txt under {EXPECT_TRASH}: {listing}")
    if (DATA_PHOTOS / "remote.txt").exists():
        raise SystemExit("remote.txt still present under local photos after delete sync")
    print("ac9_ok")


def main() -> int:
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 1280, "height": 900})
        try:
            if PHASE == "ac8":
                phase_ac8(page)
            elif PHASE == "ac9-prepare":
                phase_ac9_prepare(page)
            elif PHASE == "ac9-assert":
                phase_ac9_assert(page)
            else:
                raise SystemExit(f"unknown PHASE={PHASE}")
        finally:
            browser.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
