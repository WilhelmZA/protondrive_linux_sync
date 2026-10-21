"""About page."""

from __future__ import annotations

from nicegui import ui

from neutronsync_web import __version__, ctx
from neutronsync_web.api import ApiError
from neutronsync_web.fmt import format_log_lines_local
from neutronsync_web.pages.layout import page_shell


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


def render() -> None:
    def body() -> None:
        live = ctx.get_live()
        settings = ctx.get_settings()
        auth = ctx.get_auth()

        with ui.element("div").classes("ns-card"):
            ui.label(f"Web interface {__version__}").classes("text-subtitle1")
            ver = live.state.version
            if ver:
                ui.label(f"Engine {ver.version} ({ver.build})")
                ui.label(f"API version {ver.api_version}")
            ui.label(f"Engine address {settings.api_url}")
            mode = "Proxy" if auth.mode == "proxy" else "Password"
            ui.label(f"Login mode: {mode}")
            if auth.proxy_warning:
                ui.label(auth.proxy_warning).classes("ns-muted")
            ui.link(
                "GitHub repository",
                "https://github.com/WilhelmZA/protondrive_linux_sync",
            )

        with ui.expansion("Recent log", icon="article").classes("w-full q-mt-md"):
            log_box = ui.markdown("").classes("ns-mono w-full")

            async def refresh_log() -> None:
                try:
                    lines = await ctx.get_api().logs(200)
                    shown = format_log_lines_local(lines)
                    log_box.set_content("```\n" + "\n".join(shown) + "\n```")
                except Exception as exc:
                    _err(exc)

            ui.button("Refresh", on_click=refresh_log).props("outline")
            ui.timer(0.1, refresh_log, once=True)

    page_shell("About", body, live_body=False)
