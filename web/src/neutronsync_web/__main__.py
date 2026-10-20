"""Entry point: `python -m neutronsync_web` / `neutronsync-web`."""

from __future__ import annotations

import logging
from pathlib import Path
from urllib.parse import quote

from fastapi.responses import JSONResponse, RedirectResponse
from nicegui import app, ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiClient
from neutronsync_web.auth import AuthStore
from neutronsync_web.live import LiveHub
from neutronsync_web.pages import (
    about,
    account,
    activity,
    conflicts,
    folders,
    login,
    settings_page,
)
from neutronsync_web.pages.layout import require_session
from neutronsync_web.settings import ConfigError, load_settings

log = logging.getLogger("neutronsync_web")

STATIC = Path(__file__).resolve().parent / "static"
_PAGES_REGISTERED = False


def bootstrap() -> tuple:
    """Load settings, prepare auth, wire API and live hub into ctx."""
    settings = load_settings()
    auth = AuthStore(settings)
    storage_secret = auth.prepare()
    api = ApiClient(settings.api_url, settings.api_token)
    live = LiveHub(api, settings.api_url)
    ctx.settings = settings
    ctx.auth = auth
    ctx.api = api
    ctx.live = live
    return settings, auth, api, live, storage_secret


def register_routes() -> None:
    global _PAGES_REGISTERED
    if _PAGES_REGISTERED:
        return
    _PAGES_REGISTERED = True

    app.add_static_files("/static", str(STATIC))

    @app.get("/healthz")
    async def healthz() -> JSONResponse:
        live = ctx.get_live()
        ok = bool(live.state.api_healthy)
        return JSONResponse({"ok": ok}, status_code=200 if ok else 503)

    def guarded(page_render):
        def wrapper():
            if not require_session():
                path = "/"
                try:
                    path = ui.context.client.page.path or "/"
                except Exception:
                    pass
                return RedirectResponse(f"/login?next={quote(path, safe='/')}")
            page_render()
            return None

        return wrapper

    @ui.page("/login")
    def page_login() -> None:
        if ctx.get_auth().mode == "proxy":
            ui.navigate.to("/")
            return
        login.render()

    @ui.page("/")
    def page_activity():
        return guarded(activity.render)()

    @ui.page("/folders")
    def page_folders():
        return guarded(folders.render)()

    @ui.page("/conflicts")
    def page_conflicts():
        return guarded(conflicts.render)()

    @ui.page("/settings")
    def page_settings():
        return guarded(settings_page.render)()

    @ui.page("/account")
    def page_account():
        return guarded(account.render)()

    @ui.page("/about")
    def page_about():
        return guarded(about.render)()

    @app.on_startup
    async def _start() -> None:
        await ctx.get_live().start()

    @app.on_shutdown
    async def _stop() -> None:
        await ctx.get_live().stop()
        await ctx.get_api().aclose()


def create_app(*, reload: bool = False) -> None:
    settings, _auth, _api, _live, storage_secret = bootstrap()
    register_routes()
    secret = storage_secret.hex() if isinstance(storage_secret, bytes) else str(storage_secret)
    ui.run(
        host=settings.web_host,
        port=settings.web_port,
        reload=reload,
        storage_secret=secret,
        title="NeutronSync",
        favicon=str(STATIC / "logo.png") if (STATIC / "logo.png").exists() else None,
        show=False,
        dark=None,
    )


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    try:
        create_app(reload=False)
    except ConfigError as exc:
        raise SystemExit(str(exc)) from exc
    except SystemExit:
        raise


if __name__ == "__main__":
    main()
