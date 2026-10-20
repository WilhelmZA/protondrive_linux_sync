#!/usr/bin/env python3
"""Capture web UI screenshots into docs/screenshots/ from a scratch setup."""

from __future__ import annotations

import sys
import tempfile
import time
from pathlib import Path

import httpx
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "web"))
sys.path.insert(0, str(ROOT / "web" / "tests"))

from lab import WEB_PASSWORD, Lab  # noqa: E402

OUT = ROOT / "docs" / "screenshots"


def _login(page, url: str) -> None:
    page.goto(f"{url}/login")
    page.get_by_label("Password").fill(WEB_PASSWORD)
    page.get_by_role("button", name="Sign in").click()
    page.wait_for_timeout(1000)


def _api(lab: Lab, method: str, path: str, json=None) -> None:
    httpx.request(
        method,
        f"{lab.api_url}{path}",
        headers={"Authorization": f"Bearer {lab.token}"},
        json=json,
        timeout=60,
    )


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ns-web-shots-") as tmp:
        lab = Lab(Path(tmp), fake_auth="totp")
        lab.controls({"signed_in": False})
        for i in range(12):
            lab.both(f"f{i}.txt", f"body-{i}")
        lab.start()
        try:
            with sync_playwright() as p:
                browser = p.chromium.launch(headless=True)

                def shot(name: str, route: str, viewport: dict, theme: str) -> None:
                    context = browser.new_context(
                        viewport=viewport,
                        color_scheme="light" if theme == "light" else "dark",
                    )
                    page = context.new_page()
                    _login(page, lab.web_url)
                    if name == "web-account-2fa.png":
                        page.goto(f"{lab.web_url}/account")
                        page.wait_for_timeout(500)
                        page.get_by_label("Email or username").fill("u")
                        page.get_by_label("Password", exact=True).fill("p")
                        page.get_by_role("button", name="Sign in").click()
                        page.wait_for_timeout(1200)
                    else:
                        page.goto(f"{lab.web_url}{route}")
                        page.wait_for_timeout(1200)
                    page.screenshot(path=str(OUT / name), full_page=True)
                    context.close()

                # Baseline activity / folders / conflicts / light / phone
                shot("web-activity.png", "/", {"width": 1280, "height": 800}, "dark")
                shot("web-folders.png", "/folders", {"width": 1280, "height": 800}, "dark")
                shot("web-conflicts.png", "/conflicts", {"width": 1280, "height": 800}, "dark")
                shot("web-settings.png", "/settings", {"width": 1600, "height": 1000}, "dark")
                shot("web-account.png", "/account", {"width": 1280, "height": 800}, "light")
                shot("web-activity-light.png", "/", {"width": 1280, "height": 800}, "light")
                shot("web-activity-phone.png", "/", {"width": 390, "height": 844}, "dark")
                shot(
                    "web-account-2fa.png",
                    "/account",
                    {"width": 1280, "height": 800},
                    "dark",
                )

                # Held deletions: establish baseline, empty local, sync again
                _api(lab, "POST", "/api/v1/sync", {"dry_run": False})
                time.sleep(6)
                for i in range(12):
                    p = lab.root / "local" / "photos" / f"f{i}.txt"
                    if p.exists():
                        p.unlink()
                _api(lab, "POST", "/api/v1/sync", {"dry_run": False})
                time.sleep(6)
                shot("web-held.png", "/", {"width": 1280, "height": 800}, "dark")

                browser.close()
        finally:
            lab.stop()
    print(f"Wrote screenshots under {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
