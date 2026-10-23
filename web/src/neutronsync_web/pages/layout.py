"""Shared page frame: header, drawer, banners, connection notices."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path

from nicegui import app, ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError
from neutronsync_web.live import ConnStatus
from neutronsync_web.theme import apply_appearance, apply_colors

LOGO = Path(__file__).resolve().parent.parent / "static" / "logo.png"

NAV = [
    ("/", "Activity", "sync"),
    ("/folders", "Folders", "folder"),
    ("/trash", "Trash", "delete"),
    ("/conflicts", "Conflicts", "warning"),
    ("/settings", "Settings", "settings"),
    ("/account", "Account", "person"),
    ("/about", "About", "info"),
]


def _notify_error(exc: Exception) -> None:
    if isinstance(exc, ApiError):
        ui.notify(exc.message, type="negative")
    else:
        ui.notify(str(exc), type="negative")


async def _toggle_auto(enabled: bool) -> None:
    try:
        await ctx.get_api().set_auto_sync(enabled)
    except Exception as exc:
        _notify_error(exc)


def page_shell(title: str, body: Callable[[], None], *, live_body: bool = True) -> None:
    """Wrap a page with the shared chrome, or a full-page notice.

    The page ``body`` is mounted once and is never torn down by live ticks
    (R1-004). Chrome (header, banners, title) and the drawer badge still
    update. Pages that need live data refresh their own sub-regions.

    ``live_body`` is kept for call-site compatibility; body mounting no longer
    depends on it.
    """
    del live_body  # body is always stable; pages own their live sub-regions
    apply_colors()
    apply_appearance()
    live = ctx.get_live()

    ui.add_head_html(
        """
        <style>
        .ns-frame { width: 100%; max-width: 1120px; box-sizing: border-box;
                    margin: 0; padding: 0 32px 48px; }
        @media (max-width: 599px) { .ns-frame { padding: 0 14px 40px; } }
        .ns-header { display: flex; align-items: center; gap: 12px; flex-wrap: wrap;
                     padding: 10px 0; border-bottom: 1px solid rgba(128,128,128,0.25); }
        .ns-brand { display: flex; align-items: center; gap: 10px; font-size: 1.25rem;
                    font-weight: 650; letter-spacing: -0.02em; }
        .ns-brand img { width: 36px; height: 36px; }
        .ns-drawer { background: #f6f6f8; border-right: 1px solid rgba(0,0,0,0.06); }
        .body--dark .ns-drawer { background: #1a1a1f;
                                 border-right: 1px solid rgba(255,255,255,0.07); }
        .ns-side { display: flex; flex-direction: column; gap: 2px; padding: 18px 12px;
                   width: 100%; box-sizing: border-box; }
        .ns-side-brand { display: flex; align-items: center; gap: 10px;
                         padding: 2px 10px 20px; font-size: 1.1rem; font-weight: 650;
                         letter-spacing: -0.02em; }
        .ns-auth { max-width: 440px; margin-top: 8px; padding: 24px;
                   border: 1px solid rgba(128,128,128,0.22); border-radius: 14px;
                   display: flex; flex-direction: column; gap: 12px; }
        .ns-auth-title { font-size: 1.2rem; font-weight: 600; }
        .ns-picker { position: relative; height: 340px; overflow-y: auto;
                     border: 1px solid rgba(128,128,128,0.22); border-radius: 10px; }
        .ns-picker-busy { position: absolute; inset: 0; display: flex;
                          align-items: center; justify-content: center; }
        .ns-dim { opacity: 0.35; pointer-events: none; }
        .ns-crumbs { min-height: 28px; flex-wrap: wrap; }
        .ns-crumb { cursor: pointer; text-decoration: none; color: #6D4AFF; }
        .body--dark .ns-crumb { color: #a38bff; }
        .ns-drawer-link { display: flex; align-items: center; gap: 12px; width: 100%;
                          box-sizing: border-box; padding: 9px 12px; border-radius: 8px;
                          text-decoration: none; color: inherit; font-size: 0.95rem; }
        .ns-drawer-link .q-icon { font-size: 20px; opacity: 0.7; }
        .ns-drawer-link:hover { background: rgba(109,74,255,0.08); }
        .ns-drawer-link.active { background: rgba(109,74,255,0.14); color: #6D4AFF;
                                 font-weight: 600; }
        .body--dark .ns-drawer-link.active { color: #a38bff; }
        .ns-drawer-link.active .q-icon { opacity: 1; }
        .ns-badge { background: #E05064; color: white; border-radius: 999px;
                    padding: 0 7px; font-size: 0.75rem; margin-left: auto; }
        .ns-banner { padding: 12px 14px; border-radius: 10px; margin: 12px 0; }
        .ns-banner.warn {
          background: rgba(216,160,80,0.14); border: 1px solid rgba(216,160,80,0.4); }
        .ns-banner.danger {
          background: rgba(224,80,100,0.12); border: 1px solid rgba(224,80,100,0.35); }
        .ns-held { background: rgba(216,160,80,0.12); border: 1px solid rgba(216,160,80,0.35);
                   border-radius: 12px; padding: 14px 16px; margin: 10px 0; }
        .ns-card { border: 1px solid rgba(128,128,128,0.22); border-radius: 12px;
                   padding: 14px 16px; margin: 10px 0; box-sizing: border-box; }
        .ns-grid { display: grid; gap: 0 16px; align-items: start;
                   grid-template-columns: repeat(auto-fit, minmax(min(100%, 420px), 1fr)); }
        .ns-mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
                   white-space: pre-wrap; font-size: 0.8rem; max-height: 320px; overflow: auto; }
        .ns-stack { display: flex; flex-direction: column; gap: 8px; }
        .ns-row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
        .ns-grow { flex: 1 1 auto; min-width: 0; }
        .ns-muted { opacity: 0.72; font-size: 0.9rem; }
        .ns-err { color: #E05064; }
        @media (max-width: 1023px) {
          .ns-hide-sm { display: none !important; }
        }
        </style>
        """
    )

    with ui.left_drawer(value=None).classes("ns-drawer").props("width=232") as drawer:

        @ui.refreshable
        def nav() -> None:
            conflicts = len(live.state.envelope.conflicts) if live.state.envelope else 0
            path = "/"
            try:
                path = ui.context.client.page.path or "/"
            except Exception:
                pass
            with ui.element("div").classes("ns-side"):
                with ui.element("div").classes("ns-side-brand"):
                    if LOGO.exists():
                        ui.image(str(LOGO)).style("width:30px;height:30px")
                    ui.label("NeutronSync")
                for href, label, icon in NAV:
                    active = "active" if path == href else ""
                    with ui.link(target=href).classes(f"ns-drawer-link {active}"):
                        ui.icon(icon)
                        ui.label(label)
                        if label == "Conflicts" and conflicts > 0:
                            ui.label(str(conflicts)).classes("ns-badge")

        nav()

    content_holder: dict[str, ui.element | None] = {"el": None}

    @ui.refreshable
    def chrome() -> None:
        st = live.state
        content = content_holder["el"]
        if st.status == ConnStatus.INVALID_CONFIG:
            if content is not None:
                content.set_visibility(False)
            with ui.column().classes("ns-frame q-pa-lg"):
                ui.label("NeutronSync configuration error").classes("text-h6")
                ui.label(st.last_error).classes("ns-err")
            return
        if st.status == ConnStatus.UNSUPPORTED:
            if content is not None:
                content.set_visibility(False)
            with ui.column().classes("ns-frame q-pa-lg"):
                ui.label(st.unsupported_message).classes("text-h6")
            return
        if st.status == ConnStatus.UNREACHABLE and st.envelope is None:
            if content is not None:
                content.set_visibility(False)
            with ui.column().classes("ns-frame q-pa-lg"):
                ui.label(
                    f"Cannot reach the NeutronSync engine at {st.unreachable_url}. Retrying…"
                ).classes("text-h6")
                if st.last_error:
                    ui.label(st.last_error).classes("ns-muted")
            return

        if content is not None:
            content.set_visibility(True)
        env = st.envelope
        watching = bool(env and env.state.watching)
        signed_out = bool(
            env
            and (
                env.state.signed_out
                or (env.state.account.checked and not env.state.account.signed_in)
            )
        )

        # ns-live-ready marks that an envelope has been painted so e2e clicks
        # do not land before the NiceGUI client has live state (planner HOLD).
        live_ready = " ns-live-ready" if env is not None else ""
        with ui.element("div").classes("ns-frame").style("padding-bottom:0"):
            with ui.element("div").classes(f"ns-header{live_ready}"):
                ui.button(icon="menu", on_click=drawer.toggle).props("flat round dense").classes(
                    "lt-md"
                )
                with ui.element("div").classes("ns-brand lt-md"):
                    if LOGO.exists():
                        ui.image(str(LOGO)).style("width:36px;height:36px")
                    ui.label("NeutronSync")
                ui.space()
                with ui.row().classes("items-center gap-2"):
                    ui.label("Auto sync").classes("ns-muted")
                    ui.switch(
                        value=watching,
                        on_change=lambda e: _toggle_auto(bool(e.value)),
                    )
                    if ctx.get_auth().mode == "password":

                        def _web_sign_out() -> None:
                            app.storage.user.clear()
                            ui.navigate.to("/login")

                        ui.button("Sign out", on_click=_web_sign_out).props("flat")

            if st.status == ConnStatus.LOST:
                with ui.element("div").classes("ns-banner warn"):
                    ui.label("Connection to the NeutronSync engine was lost. Reconnecting…")

            if signed_out:
                with ui.element("div").classes("ns-banner danger"):
                    with ui.row().classes("items-center gap-2"):
                        ui.label("Signed out of Proton.")
                        ui.link("Sign in", "/account")

            ui.label(title).classes("text-h5 q-mt-md q-mb-sm")

    # Chrome first in the DOM, then a stable body host chrome.refresh() never clears.
    chrome()
    content = ui.column().classes("w-full")
    content_holder["el"] = content
    with content:
        with ui.element("div").classes("ns-frame").style("padding-top:0"):
            body()

    seen: dict[str, object] = {"nav": None, "chrome": None}

    def tick() -> None:
        st = live.state
        env = st.envelope
        conflicts = len(env.conflicts) if env else 0
        nav_marker = (conflicts, st.status)
        if nav_marker != seen["nav"]:
            seen["nav"] = nav_marker
            nav.refresh()
        signed_out = bool(
            env
            and (
                env.state.signed_out
                or (env.state.account.checked and not env.state.account.signed_in)
            )
        )
        chrome_marker = (
            st.status,
            bool(env and env.state.watching),
            signed_out,
            st.last_error,
            st.unsupported_message,
            bool(env),
        )
        if chrome_marker != seen["chrome"]:
            seen["chrome"] = chrome_marker
            chrome.refresh()

    # Markers are diffed in tick(); clearing them on every rev remounted the
    # Auto sync switch and dropped clicks (planner HOLD / AC-15).
    tick()

    def session_tick() -> None:
        if not require_session():
            ui.context.client.remove_all_elements()
            ui.navigate.to("/setup" if ctx.get_auth().needs_setup else "/login")
            return
        tick()

    ui.timer(0.25, session_tick)


def require_session() -> bool:
    """Return True if the user may see the page."""
    auth = ctx.get_auth()
    if auth.mode == "proxy":
        return True
    if auth.needs_setup:
        return False
    if not app.storage.user.get("signed_in"):
        return False
    if (
        auth.session_expired(app.storage.user.get("signed_at"))
        or app.storage.user.get("session_version") != auth.session_version
    ):
        app.storage.user.clear()
        return False
    return True
