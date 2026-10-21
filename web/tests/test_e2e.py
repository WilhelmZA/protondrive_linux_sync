"""End-to-end Playwright tests against scratch serve + web (AC-2, AC-5..AC-16)."""

from __future__ import annotations

import re
import time
from pathlib import Path

import httpx
import pytest

from tests.conftest import skip_without_binary, skip_without_chromium
from tests.lab import WEB_PASSWORD, Lab

pytestmark = pytest.mark.e2e

ROUTES = ["/", "/folders", "/conflicts", "/settings", "/account", "/about"]


@pytest.fixture
def lab(tmp_path: Path):
    skip_without_binary()
    lab = Lab(tmp_path)
    lab.start()
    yield lab
    lab.stop()


def _login(page, url: str, password: str = WEB_PASSWORD) -> None:
    page.goto(f"{url}/login")
    page.get_by_label("Password").fill(password)
    page.get_by_role("button", name=re.compile("sign in", re.I)).click()
    page.wait_for_timeout(800)
    page.wait_for_function("() => !window.location.pathname.includes('/login')", timeout=15000)


def _wait_live(page, timeout: float = 20000) -> None:
    """Wait until chrome has painted a live envelope (NiceGUI client is up)."""
    page.locator(".ns-header.ns-live-ready").first.wait_for(state="attached", timeout=timeout)


def _goto(page, url: str, route: str, expect: str) -> None:
    page.goto(f"{url}{route}")
    page.get_by_text(expect, exact=False).first.wait_for(timeout=15000)
    _wait_live(page)


def _fill_synced(page, locator, text: str) -> None:
    """Fill a NiceGUI input so the server-side value updates (fill alone often does not)."""
    locator.click()
    locator.fill("")
    locator.type(text, delay=10)
    locator.press("Tab")
    page.wait_for_timeout(200)


def _api(lab: Lab, method: str, path: str, **kwargs):
    return httpx.request(
        method,
        f"{lab.api_url}{path}",
        headers={"Authorization": f"Bearer {lab.token}"},
        timeout=60,
        **kwargs,
    )


def _wait_idle(lab: Lab, timeout: float = 60.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline and _api(lab, "GET", "/api/v1/state").json()["state"]["busy"]:
        time.sleep(0.4)


def _wait_signed_out(lab: Lab, timeout: float = 30.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _api(lab, "GET", "/api/v1/state").json()["state"].get("signed_out"):
            return
        time.sleep(0.3)
    raise AssertionError("timed out waiting for engine signed_out")


def test_routes_redirect_when_signed_out(lab: Lab) -> None:
    for route in ROUTES:
        r = httpx.get(f"{lab.web_url}{route}", follow_redirects=False, timeout=10)
        if r.status_code in (301, 302, 303, 307, 308):
            assert "/login" in r.headers.get("location", "")
        else:
            assert r.status_code == 200
            assert "/login" in r.text or "Sign in" in r.text or "window.location" in r.text


def test_healthz(lab: Lab) -> None:
    r = httpx.get(f"{lab.web_url}/healthz", timeout=10)
    deadline = time.time() + 20
    while time.time() < deadline and r.status_code != 200:
        time.sleep(0.5)
        r = httpx.get(f"{lab.web_url}/healthz", timeout=10)
    assert r.status_code == 200
    assert r.json() == {"ok": True}

    lab.serve_proc.send_signal(__import__("signal").SIGTERM)
    lab.serve_proc.wait(timeout=10)
    lab.serve_proc = None
    deadline = time.time() + 20
    status = 200
    while time.time() < deadline:
        r = httpx.get(f"{lab.web_url}/healthz", timeout=10)
        status = r.status_code
        if status == 503:
            break
        time.sleep(0.5)
    assert status == 503


def test_token_never_in_browser(lab: Lab, browser_page) -> None:
    page, context = browser_page
    blobs: list[str] = []

    def on_response(response) -> None:
        try:
            blobs.append(response.text())
        except Exception:
            pass

    def on_ws(ws) -> None:
        ws.on("framereceived", lambda payload: blobs.append(str(payload)))
        ws.on("framesent", lambda payload: blobs.append(str(payload)))

    page.on("response", on_response)
    page.on("websocket", on_ws)
    _login(page, lab.web_url)
    for route in ROUTES:
        page.goto(f"{lab.web_url}{route}")
        page.wait_for_timeout(500)
    assert lab.token not in "\n".join(blobs)


def test_password_login_and_sign_out(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    page.goto(f"{lab.web_url}/login")
    page.get_by_label("Password").fill("wrong-password!!")
    page.get_by_role("button", name=re.compile("sign in", re.I)).click()
    page.wait_for_timeout(1500)
    assert "Wrong password" in page.inner_text("body") or "wrong" in page.inner_text("body").lower()
    _login(page, lab.web_url)
    page.get_by_role("button", name=re.compile("^sign out$", re.I)).click()
    page.wait_for_url(re.compile(r"/login"), timeout=15000)
    r = httpx.get(f"{lab.web_url}/", follow_redirects=False, timeout=10)
    assert r.status_code in (200, 302, 303, 307, 308)
    if r.status_code == 200:
        assert "/login" in r.text or "Sign in" in r.text


def test_proxy_mode(tmp_path: Path, browser_page) -> None:
    lab = Lab(tmp_path)
    lab.start(auth_mode="proxy")
    try:
        page, _ = browser_page
        page.goto(f"{lab.web_url}/")
        page.get_by_text("Activity", exact=True).first.wait_for(timeout=15000)
        assert "/login" not in page.url
        page.goto(f"{lab.web_url}/about")
        page.get_by_text("Web login is off", exact=False).wait_for(timeout=15000)
        err = (tmp_path / "web.stderr").read_text(encoding="utf-8")
        assert "Web login is off" in err
    finally:
        lab.stop()


def test_sign_in_totp(tmp_path: Path, browser_page) -> None:
    lab = Lab(tmp_path, fake_auth="totp")
    lab.controls({"signed_in": False})
    lab.start()
    try:
        page, _ = browser_page
        _login(page, lab.web_url)
        _goto(page, lab.web_url, "/account", "Email or username")
        _fill_synced(page, page.get_by_label("Email or username"), "user")
        _fill_synced(page, page.get_by_label("Password", exact=True), "secret")
        page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
        page.get_by_label("6-digit code").wait_for(timeout=20000)
        _fill_synced(page, page.get_by_label("6-digit code"), "123456")
        page.get_by_role("button", name=re.compile("^continue$", re.I)).click()
        page.get_by_text(re.compile(r"Signed in|\d{2,}"), exact=False).first.wait_for(timeout=20000)
    finally:
        lab.stop()


def test_sign_in_mailbox_and_hv_and_error(tmp_path: Path, browser_page) -> None:
    page, _ = browser_page

    lab = Lab(tmp_path / "mb", fake_auth="mailbox")
    lab.controls({"signed_in": False})
    lab.start()
    try:
        _login(page, lab.web_url)
        _goto(page, lab.web_url, "/account", "Email or username")
        _fill_synced(page, page.get_by_label("Email or username"), "u")
        _fill_synced(page, page.get_by_label("Password", exact=True), "p")
        page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
        page.get_by_label("Mailbox password").wait_for(timeout=20000)
    finally:
        lab.stop()

    lab2 = Lab(tmp_path / "hv", fake_auth="hv")
    lab2.controls({"signed_in": False})
    lab2.start()
    try:
        _login(page, lab2.web_url)
        _goto(page, lab2.web_url, "/account", "Email or username")
        _fill_synced(page, page.get_by_label("Email or username"), "u")
        _fill_synced(page, page.get_by_label("Password", exact=True), "p")
        page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
        page.get_by_text("human check", exact=False).wait_for(timeout=20000)
        page.get_by_role("button", name=re.compile("i've completed it", re.I)).wait_for(
            timeout=10000
        )
    finally:
        lab2.stop()

    lab3 = Lab(tmp_path / "err", fake_auth="error")
    lab3.controls({"signed_in": False})
    lab3.start()
    try:
        _login(page, lab3.web_url)
        _goto(page, lab3.web_url, "/account", "Email or username")
        _fill_synced(page, page.get_by_label("Email or username"), "keep-me")
        _fill_synced(page, page.get_by_label("Password", exact=True), "p")
        page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
        page.get_by_text("did not accept", exact=False).wait_for(timeout=15000)
        assert page.get_by_label("Email or username").input_value() == "keep-me"
    finally:
        lab3.stop()


def _picker_row(picker, text: str):
    return picker.locator(".ns-picker .q-item").filter(
        has=picker.page.get_by_text(text, exact=True)
    )


def _pick_folder(page, *, local_path: str | None = None, remote_name: str | None = None) -> None:
    """Use the open folder-picker dialog (last .q-dialog)."""
    picker = page.locator(".q-dialog").last
    picker.get_by_role("button", name=re.compile("use this folder", re.I)).wait_for(timeout=10000)
    if local_path is not None:
        # Top listing is the allowed root path; enter it, then the folder name.
        root = str(Path(local_path).parent)
        _picker_row(picker, root).click()
        _picker_row(picker, Path(local_path).name).click()
    elif remote_name is not None:
        _picker_row(picker, remote_name).click()
    picker.get_by_role("button", name=re.compile("use this folder", re.I)).click()


def test_folders_add_edit_remove(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/folders", "photos")
    page.wait_for_timeout(1500)

    page.locator(".ns-card").filter(has_text="photos").get_by_role(
        "button", name=re.compile("^edit$", re.I)
    ).click()
    dialog = page.locator(".q-dialog")
    dialog.get_by_label("Name").wait_for(timeout=10000)
    dialog.get_by_label("Add exclusion").fill("*.tmp")
    dialog.get_by_role("button", name=re.compile("^add$", re.I)).click()
    dialog.get_by_text("*.tmp").wait_for(timeout=5000)
    dialog.get_by_role("button", name=re.compile("^save$", re.I)).click()
    deadline = time.time() + 15
    while time.time() < deadline and "*.tmp" not in lab.config_text():
        time.sleep(0.3)
    assert "*.tmp" in lab.config_text()

    page.get_by_role("button", name=re.compile("add folder", re.I)).click()
    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_label("Name").fill("docs")
    local = str(lab.root / "local" / "docs")
    Path(local).mkdir(parents=True, exist_ok=True)
    (Path(local) / "keep.txt").write_text("stay", encoding="utf-8")
    tree = lab.tree()
    tree["docs"] = {"name": "docs", "type": "folder", "parent_uid": "root"}
    lab.save_tree(tree)

    dialog.get_by_role("button", name="Browse…").first.click()
    _pick_folder(page, local_path=local)

    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_role("button", name="Browse…").nth(1).click()
    _pick_folder(page, remote_name="docs")

    dialog = page.locator(".q-dialog").filter(has_text="Add folder")
    dialog.get_by_role("button", name=re.compile("^save$", re.I)).click()
    deadline = time.time() + 15
    while time.time() < deadline and 'name = "docs"' not in lab.config_text():
        time.sleep(0.3)
    assert "docs" in lab.config_text()

    page.reload()
    _goto(page, lab.web_url, "/folders", "docs")
    cards = page.locator(".ns-card").filter(has_text="docs")
    remove_btn = cards.get_by_role("button", name=re.compile("remove", re.I))
    remove_btn.wait_for(timeout=15000)
    remove_btn.click()
    confirm = page.locator(".q-dialog").get_by_role("button", name=re.compile("^remove$", re.I))
    confirm.wait_for(timeout=15000)
    confirm.click()
    page.wait_for_timeout(1500)
    assert (Path(local) / "keep.txt").read_text(encoding="utf-8") == "stay"
    assert "*.tmp" in lab.config_text()


def test_activity_dry_run_and_cancel(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    lab.both("a.txt", "hello")
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Dry run")
    page.get_by_role("button", name=re.compile("dry run", re.I)).click()
    page.get_by_text(re.compile(r"a\.txt|photos|Scanning|Syncing|dry", re.I)).first.wait_for(
        timeout=25000
    )
    page.get_by_role("button", name=re.compile("sync now", re.I)).wait_for(timeout=30000)
    # Remote-only files + download delay keep Sync now busy long enough for Cancel.
    lab.controls({"download_delay": 12})
    for i in range(4):
        lab.remote_file(f"slow{i}", f"slow{i}.txt", "photos", f"payload-{i}")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    page.get_by_role("button", name=re.compile("^cancel$", re.I)).wait_for(timeout=25000)
    page.get_by_role("button", name=re.compile("^cancel$", re.I)).click()
    page.get_by_role("button", name=re.compile("sync now", re.I)).wait_for(timeout=30000)
    deadline = time.time() + 20
    while time.time() < deadline and _api(lab, "GET", "/api/v1/state").json()["state"]["busy"]:
        time.sleep(0.5)
    assert not _api(lab, "GET", "/api/v1/state").json()["state"]["busy"]


def test_settings_and_auto_sync(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/settings", "Most files one run may delete")
    field = page.get_by_label("Most files one run may delete")
    deadline = time.time() + 10
    while time.time() < deadline and field.input_value() in ("", None):
        time.sleep(0.2)
    _fill_synced(page, field, "42")
    guard = page.locator(".ns-card").filter(has_text="Deletion guard")
    save = guard.get_by_role("button", name=re.compile("^save$", re.I))
    save.click()
    page.locator(".q-notification__message").filter(has_text="Saved").wait_for(timeout=10000)
    deadline = time.time() + 15
    while time.time() < deadline and "max_delete_files = 42" not in lab.config_text():
        time.sleep(0.3)
    if "max_delete_files = 42" not in lab.config_text():
        # Live remount can leave the DOM value ahead of the server; sync and retry once.
        _fill_synced(page, page.get_by_label("Most files one run may delete"), "42")
        save.click()
        page.locator(".q-notification__message").filter(has_text="Saved").wait_for(timeout=10000)
        deadline = time.time() + 15
        while time.time() < deadline and "max_delete_files = 42" not in lab.config_text():
            time.sleep(0.3)
    cfg = lab.config_text()
    assert "max_delete_files = 42" in cfg or "max_delete_files=42" in cfg.replace(" ", "")

    pct = page.get_by_label("Most percent one run may delete")
    _fill_synced(page, pct, "101")
    guard.get_by_role("button", name=re.compile("^save$", re.I)).click()
    page.get_by_text("max_delete_percent", exact=False).wait_for(timeout=10000)
    assert "101" in page.get_by_label("Most percent one run may delete").input_value()

    _goto(page, lab.web_url, "/", "Auto sync")
    before = _api(lab, "GET", "/api/v1/state").json()
    watching_before = bool(before["state"]["watching"])
    toggle = page.locator(".ns-header.ns-live-ready .q-toggle").first
    toggle.wait_for(timeout=15000)
    toggle.click()
    deadline = time.time() + 10
    while time.time() < deadline:
        after = _api(lab, "GET", "/api/v1/state").json()
        if bool(after["state"]["watching"]) != watching_before:
            break
        time.sleep(0.3)
    assert bool(_api(lab, "GET", "/api/v1/state").json()["state"]["watching"]) != watching_before
    page.reload()
    page.get_by_text("Activity", exact=True).first.wait_for(timeout=15000)
    _wait_live(page)
    assert bool(_api(lab, "GET", "/api/v1/state").json()["state"]["watching"]) != watching_before


def test_phone_width(lab: Lab, playwright) -> None:
    skip_without_binary()
    skip_without_chromium()
    browser = playwright.chromium.launch(headless=True)
    context = browser.new_context(viewport={"width": 390, "height": 844})
    page = context.new_page()
    try:
        _login(page, lab.web_url)
        for route in ROUTES:
            page.goto(f"{lab.web_url}{route}")
            _wait_live(page)
            width = page.evaluate("() => document.documentElement.scrollWidth")
            assert width <= 390, f"{route} scrollWidth={width}"
    finally:
        context.close()
        browser.close()


def test_conflicts_badge_and_dismiss(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    lab.both("c.txt", "same")
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Sync now")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    _wait_idle(lab, 30)
    (lab.root / "local" / "photos" / "c.txt").write_text("local-new", encoding="utf-8")
    lab.remote_file("c.txt", "c.txt", "photos", "remote-new")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    deadline = time.time() + 40
    conflicts: list = []
    while time.time() < deadline:
        conflicts = _api(lab, "GET", "/api/v1/conflicts").json()
        if len(conflicts) >= 1:
            break
        time.sleep(0.5)
    assert len(conflicts) >= 1
    _wait_idle(lab, 30)
    _goto(page, lab.web_url, "/", "Activity")
    badge = page.locator(".ns-badge")
    deadline = time.time() + 20
    while time.time() < deadline and badge.count() == 0:
        page.wait_for_timeout(500)
    badge.wait_for(timeout=5000)
    assert int(badge.inner_text().strip()) >= 1
    page.goto(f"{lab.web_url}/conflicts")
    page.get_by_text("c.txt", exact=False).first.wait_for(timeout=20000)
    dismiss = page.get_by_role("button", name=re.compile("^dismiss$", re.I)).first
    dismiss.wait_for(timeout=10000)
    dismiss.click()
    deadline = time.time() + 15
    while time.time() < deadline:
        if _api(lab, "GET", "/api/v1/conflicts").json() == []:
            break
        time.sleep(0.3)
    assert _api(lab, "GET", "/api/v1/conflicts").json() == []
    _goto(page, lab.web_url, "/", "Activity")
    deadline = time.time() + 15
    while time.time() < deadline and page.locator(".ns-badge").count() > 0:
        page.wait_for_timeout(400)
    assert page.locator(".ns-badge").count() == 0


def test_held_deletions(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    for i in range(20):
        lab.both(f"f{i}.txt", f"body-{i}")
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Sync now")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    tracked = 0
    deadline = time.time() + 60
    while time.time() < deadline:
        st = _api(lab, "GET", "/api/v1/state").json()["state"]
        tracked = sum(p["tracked"] for p in st["pairs"])
        if tracked >= 10 and not st["busy"]:
            break
        time.sleep(0.4)
    assert tracked >= 10
    for i in range(20):
        p = lab.root / "local" / "photos" / f"f{i}.txt"
        if p.exists():
            p.unlink()
    _wait_live(page)
    sync_now = page.get_by_role("button", name=re.compile("sync now", re.I))
    sync_now.wait_for(timeout=15000)
    sync_now.click()
    deadline = time.time() + 60
    held: list = []
    while time.time() < deadline:
        held = _api(lab, "GET", "/api/v1/held").json()
        if len(held) >= 1:
            break
        time.sleep(0.5)
    assert len(held) >= 1
    _wait_idle(lab, 30)
    # Banner is .ns-held; activity feed rows also say "held back".
    page.locator(".ns-held").wait_for(timeout=30000)
    page.locator(".ns-held").get_by_role("button", name=re.compile("delete them", re.I)).click()
    page.locator(".q-dialog").get_by_text("Delete them?", exact=False).wait_for(timeout=5000)
    page.locator(".q-dialog").get_by_role("button", name=re.compile("^cancel$", re.I)).click()
    page.locator(".ns-held").get_by_role("button", name=re.compile("restore them", re.I)).click()
    deadline = time.time() + 60
    while time.time() < deadline:
        held = _api(lab, "GET", "/api/v1/held").json()
        if len(held) == 0:
            break
        time.sleep(0.5)
    assert _api(lab, "GET", "/api/v1/held").json() == []
    restored = list((lab.root / "local" / "photos").glob("f*.txt"))
    assert len(restored) >= 1


def test_sync_disabled_with_no_pairs(tmp_path: Path, browser_page) -> None:
    lab = Lab(tmp_path)
    text = lab.config.read_text(encoding="utf-8")
    lab.config.write_text(text.split("[[pair]]")[0], encoding="utf-8")
    lab.start()
    try:
        page, _ = browser_page
        _login(page, lab.web_url)
        _goto(page, lab.web_url, "/", "Activity")
        page.get_by_text("Add a folder first", exact=False).wait_for(timeout=10000)
        assert page.get_by_role("button", name=re.compile("sync now", re.I)).is_disabled()
        assert page.get_by_role("button", name=re.compile("dry run", re.I)).is_disabled()
        r = _api(lab, "POST", "/api/v1/sync", json={"dry_run": True})
        assert r.status_code == 409
        assert "Add a folder" in r.json()["error"]["message"]
    finally:
        lab.stop()


def test_folder_sync_signed_out_shows_message_once(tmp_path: Path, browser_page) -> None:
    lab = Lab(tmp_path)
    lab.controls({"signed_in": False})
    lab.start()
    try:
        _wait_signed_out(lab)
        page, _ = browser_page
        _login(page, lab.web_url)
        _goto(page, lab.web_url, "/folders", "photos")
        _wait_live(page)
        sync_btn = (
            page.locator(".ns-card")
            .filter(has_text="photos")
            .get_by_role("button", name=re.compile("sync now", re.I))
        )
        sync_btn.click()
        msg = "Signed out of Proton. Sign in on the Account page."
        notif = page.locator(".q-notification__message").filter(has_text=msg)
        notif.wait_for(timeout=20000)
        assert notif.count() == 1
    finally:
        lab.stop()


def test_exclusions_enter_and_save_on_save(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/folders", "photos")
    page.locator(".ns-card").filter(has_text="photos").get_by_role(
        "button", name=re.compile("^edit$", re.I)
    ).click()
    dialog = page.locator(".q-dialog")
    excl = dialog.get_by_label("Add exclusion")
    excl.fill("*.enter")
    excl.press("Enter")
    dialog.get_by_text("*.enter").wait_for(timeout=5000)
    excl.fill("*.save")
    dialog.get_by_role("button", name=re.compile("^save$", re.I)).click()
    deadline = time.time() + 15
    while time.time() < deadline and "*.save" not in lab.config_text():
        time.sleep(0.3)
    cfg = lab.config_text()
    assert "*.enter" in cfg
    assert "*.save" in cfg


def test_activity_wording_auto_sync_off(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Activity")
    page.get_by_text(re.compile(r"Auto sync off · 1 folder(?!s watched)", re.I)).wait_for(
        timeout=15000
    )


def test_sign_in_totp_shows_spinner(tmp_path: Path, browser_page) -> None:
    lab = Lab(tmp_path, fake_auth="totp")
    lab.controls({"signed_in": False, "auth_delay": 2})
    lab.start()
    try:
        page, _ = browser_page
        _login(page, lab.web_url)
        _goto(page, lab.web_url, "/account", "Email or username")
        _fill_synced(page, page.get_by_label("Email or username"), "user")
        _fill_synced(page, page.get_by_label("Password", exact=True), "secret")
        page.get_by_role("button", name=re.compile("^sign in$", re.I)).click()
        page.get_by_label("6-digit code").wait_for(timeout=20000)
        _fill_synced(page, page.get_by_label("6-digit code"), "123456")
        page.get_by_role("button", name=re.compile("^continue$", re.I)).click()
        page.get_by_text("Signing in", exact=False).wait_for(timeout=5000)
        assert page.get_by_role("button", name=re.compile("^continue$", re.I)).is_disabled()
    finally:
        lab.stop()


def test_copy_path_fallback_when_not_secure_context(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    page.add_init_script("Object.defineProperty(window, 'isSecureContext', { get: () => false });")
    lab.both("c.txt", "same")
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Sync now")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    _wait_idle(lab, 30)
    (lab.root / "local" / "photos" / "c.txt").write_text("local-new", encoding="utf-8")
    lab.remote_file("c.txt", "c.txt", "photos", "remote-new")
    page.get_by_role("button", name=re.compile("sync now", re.I)).click()
    _wait_idle(lab, 30)
    page.goto(f"{lab.web_url}/conflicts")
    page.get_by_text("c.txt", exact=False).first.wait_for(timeout=30000)
    page.get_by_role("button", name=re.compile("copy path", re.I)).first.click()
    page.get_by_text("Copy it from here", exact=False).wait_for(timeout=10000)
    assert "Path copied" not in page.inner_text("body")


def test_activity_groups_runs_without_upload_equals(lab: Lab, browser_page) -> None:
    page, _ = browser_page
    lab.both("one.txt", "a")
    _login(page, lab.web_url)
    _goto(page, lab.web_url, "/", "Sync now")
    for _ in range(3):
        page.get_by_role("button", name=re.compile("sync now", re.I)).click()
        _wait_idle(lab, 30)
    activity = _api(lab, "GET", "/api/v1/state").json()["state"]["activity"]
    blob = str(activity)
    assert "upload=" not in blob
    assert any(
        "Nothing to do" in (a.get("text") or "") or "Uploaded" in (a.get("text") or "")
        for a in activity
    )
