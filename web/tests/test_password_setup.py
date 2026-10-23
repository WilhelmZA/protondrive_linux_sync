"""Password setup and rotation against isolated scratch processes and browsers."""

from __future__ import annotations

import re
import signal
from pathlib import Path

import httpx
import pytest

from tests.conftest import skip_without_binary
from tests.lab import WEB_PASSWORD, Lab
from tests.test_e2e import _login, _wait_live

pytestmark = pytest.mark.e2e


@pytest.fixture
def setup_lab(tmp_path: Path):
    skip_without_binary()
    lab = Lab(tmp_path)
    try:
        lab.start_serve()
        lab.start_web(web_password=None)
        yield lab
    finally:
        lab.stop()


def restart_web(lab: Lab, **kwargs) -> None:
    lab.web_proc.send_signal(signal.SIGTERM)
    lab.web_proc.wait(timeout=10)
    lab.start_web(**kwargs)


def test_first_password_and_restart(setup_lab: Lab, browser_page) -> None:
    lab = setup_lab
    page, _ = browser_page
    for route in ("/", "/login", "/account", "/unknown"):
        response = httpx.get(f"{lab.web_url}{route}", follow_redirects=False)
        assert response.status_code == 307
        assert response.headers["location"] == "/setup"
    page.goto(lab.web_url)
    page.get_by_text("Create your password", exact=True).wait_for()
    assert httpx.get(f"{lab.web_url}/healthz").status_code == 200
    log = (lab.root / "web.stderr").read_text()
    code = re.search(r"NeutronSync setup code: ([A-Z2-9]{4}-[A-Z2-9]{4})", log)[1]
    page.get_by_label("Setup code", exact=True).fill("WRONG")
    page.get_by_label("Password", exact=True).fill(WEB_PASSWORD)
    page.get_by_label("Confirm password", exact=True).fill(WEB_PASSWORD)
    page.get_by_role("button", name="Create password", exact=True).click()
    page.get_by_text("Wrong or expired setup code.", exact=True).wait_for()
    page.get_by_label("Setup code", exact=True).fill(code)
    page.get_by_label("Password", exact=True).fill(WEB_PASSWORD)
    page.get_by_label("Confirm password", exact=True).fill(WEB_PASSWORD)
    page.get_by_role("button", name="Create password", exact=True).click()
    page.wait_for_url(lab.web_url + "/")
    _wait_live(page)
    restart_web(lab, web_password=None)
    assert "NeutronSync setup code:" not in (lab.root / "web.stderr").read_text()
    _login(page, lab.web_url)
    _wait_live(page)
    assert httpx.get(f"{lab.web_url}/setup", follow_redirects=False).headers["location"] == "/login"


def test_change_password_revokes_other_browser(tmp_path: Path, browser_page) -> None:
    skip_without_binary()
    lab = Lab(tmp_path)
    page, context = browser_page
    other_context = context.browser.new_context()
    other = other_context.new_page()
    new_password = "new-web-password-12345"
    try:
        lab.start()
        _login(page, lab.web_url)
        _login(other, lab.web_url)
        _wait_live(other)
        page.goto(f"{lab.web_url}/account")
        _wait_live(page)
        page.get_by_label("Current password", exact=True).fill(WEB_PASSWORD)
        page.get_by_label("New password", exact=True).fill(new_password)
        page.get_by_label("Confirm new password", exact=True).fill(new_password)
        page.get_by_role("button", name="Change web password", exact=True).click()
        page.get_by_text("Web password changed. Other sessions are signed out.").wait_for()
        other.wait_for_url(re.compile(r"/login"))
        other.goto(lab.web_url)
        other.wait_for_url(re.compile(r"/login"))
        _login(other, lab.web_url, new_password)
        restart_web(lab)  # The old environment password must not undo the change.
        _login(page, lab.web_url, new_password)
        restart_web(lab, password_reset=True)
        _login(page, lab.web_url, WEB_PASSWORD)
    finally:
        other_context.close()
        lab.stop()


def test_setup_attempts_are_rate_limited(setup_lab: Lab, browser_page) -> None:
    page, _ = browser_page
    page.goto(setup_lab.web_url)
    page.get_by_label("Setup code", exact=True).fill("WRONG")
    for attempt in range(5):
        page.get_by_label("Password", exact=True).fill(WEB_PASSWORD)
        page.get_by_label("Confirm password", exact=True).fill(WEB_PASSWORD)
        page.get_by_role("button", name="Create password", exact=True).click()
        expected = (
            "Too many attempts. Try again in a few minutes."
            if attempt == 4
            else "Wrong or expired setup code."
        )
        page.get_by_text(expected, exact=True).wait_for()
    code = re.search(
        r"NeutronSync setup code: ([A-Z2-9]{4}-[A-Z2-9]{4})",
        (setup_lab.root / "web.stderr").read_text(),
    )[1]
    page.get_by_label("Setup code", exact=True).fill(code)
    page.get_by_label("Password", exact=True).fill(WEB_PASSWORD)
    page.get_by_label("Confirm password", exact=True).fill(WEB_PASSWORD)
    page.get_by_role("button", name="Create password", exact=True).click()
    page.get_by_text("Too many attempts. Try again in a few minutes.", exact=True).wait_for()
    assert not (setup_lab.root / "webdata" / "auth.json").exists()
