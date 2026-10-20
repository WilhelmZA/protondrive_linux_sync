"""Web password login page."""

from __future__ import annotations

import asyncio
import time

from nicegui import app, ui
from starlette.requests import Request

from neutronsync_web import ctx
from neutronsync_web.auth import FAIL_DELAY, client_address
from neutronsync_web.pages.layout import LOGO


def render() -> None:
    auth = ctx.get_auth()
    if auth.mode == "proxy":
        ui.navigate.to("/")
        return

    apply = __import__("neutronsync_web.theme", fromlist=["apply_colors", "apply_appearance"])
    apply.apply_colors()
    apply.apply_appearance()

    next_path = "/"
    # NiceGUI exposes query via ui.context.client.request when available
    try:
        req = ui.context.client.request
        next_path = req.query_params.get("next") or "/"
        if not next_path.startswith("/"):
            next_path = "/"
    except Exception:
        next_path = "/"

    with (
        ui.column()
        .classes("absolute-center items-stretch gap-3")
        .style(
            "width:min(380px,92vw);padding:28px;border-radius:16px;"
            "border:1px solid rgba(128,128,128,0.22)"
        )
    ):
        with ui.row().classes("items-center no-wrap gap-3 q-mb-sm"):
            if LOGO.exists():
                ui.image(str(LOGO)).style("width:40px;height:40px")
            with ui.column().classes("gap-0"):
                ui.label("NeutronSync").classes("text-h6").style("font-weight:650;line-height:1.2")
                ui.label("Proton Drive sync").classes("text-caption").style("opacity:0.7")
        ui.label("Sign in to this server").style("font-size:1.05rem;font-weight:600")
        password = ui.input("Password", password=True, password_toggle_button=True).classes(
            "w-full"
        )
        message = ui.label("").classes("ns-err")

        async def submit() -> None:
            message.set_text("")
            request: Request | None = None
            try:
                request = ui.context.client.request
            except Exception:
                pass
            addr = client_address(request) if request else "unknown"
            if auth.rate_limited(addr):
                message.set_text("Too many attempts. Try again in a few minutes.")
                return
            ok = auth.verify_password(password.value or "")
            password.value = ""
            if not ok:
                await asyncio.sleep(FAIL_DELAY)
                auth.record_failure(addr)
                if auth.rate_limited(addr):
                    message.set_text("Too many attempts. Try again in a few minutes.")
                else:
                    message.set_text("Wrong password.")
                return
            auth.clear_failures(addr)
            app.storage.user["signed_in"] = True
            app.storage.user["signed_at"] = time.time()
            ui.navigate.to(next_path)

        password.on("keydown.enter", submit)
        ui.button("Sign in", on_click=submit).props("color=primary").classes("w-full")
