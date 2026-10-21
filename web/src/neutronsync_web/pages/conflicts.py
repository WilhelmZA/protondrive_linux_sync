"""Conflicts page."""

from __future__ import annotations

import json

from nicegui import ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError
from neutronsync_web.fmt import conflict_sentence, rel_time
from neutronsync_web.pages.layout import page_shell


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


async def _dismiss(cid: int) -> None:
    try:
        await ctx.get_api().conflict_dismiss(cid)
    except Exception as exc:
        _err(exc)


async def _dismiss_all() -> None:
    try:
        await ctx.get_api().conflicts_dismiss_all()
    except Exception as exc:
        _err(exc)


def render() -> None:
    def body() -> None:
        live = ctx.get_live()

        @ui.refreshable
        def panel() -> None:
            env = live.state.envelope
            conflicts = env.conflicts if env else []
            with ui.row().classes("w-full items-center"):
                ui.space()
                if conflicts:
                    ui.button("Dismiss all", on_click=_dismiss_all).props("outline")

            if not conflicts:
                ui.label("No conflicts.").classes("text-subtitle1")
                ui.label(
                    "A file changed on both sides since its last sync shows up here once "
                    "a sync has settled it."
                ).classes("ns-muted")
                return

            ui.label(
                "Files that changed on both sides since their last sync, and how each "
                "one was settled. Dismiss a row once you have looked at it."
            ).classes("ns-muted q-mb-md")

            for c in conflicts:
                with ui.element("div").classes("ns-card"):
                    ui.label(c.path).classes("text-subtitle1")
                    ui.label(conflict_sentence(c)).classes("ns-muted")
                    when = (
                        f"{rel_time(c.last_ts)} · {c.seen} times"
                        if c.seen > 1
                        else rel_time(c.last_ts)
                    )
                    ui.label(f"{c.pair} · {when}").classes("ns-muted")
                    with ui.row().classes("q-mt-sm gap-2"):
                        path_to_copy = c.path

                        async def copy_path(p=path_to_copy) -> None:
                            copied = await ui.run_javascript(
                                f"""
                                (async () => {{
                                    const text = {json.dumps(p)};
                                    if (window.isSecureContext) {{
                                        try {{
                                            await navigator.clipboard.writeText(text);
                                            return true;
                                        }} catch (e) {{}}
                                    }}
                                    return false;
                                }})()
                                """,
                                timeout=5.0,
                            )
                            if copied:
                                ui.notify("Path copied", type="info")
                                return
                            with (
                                ui.dialog() as dialog,
                                ui.card().classes("w-full").style("max-width:480px"),
                            ):
                                ui.label("Copy it from here").classes("text-h6")
                                ui.input(value=p).props("readonly").classes("w-full")
                                ui.button("Close", on_click=dialog.close).props("flat")
                            dialog.open()

                        ui.button("Copy path", on_click=copy_path).props("flat")
                        ui.button("Dismiss", on_click=lambda cid=c.id: _dismiss(cid)).props("flat")

        panel()
        seen: dict[str, object] = {"m": None}

        def tick() -> None:
            env = live.state.envelope
            conflicts = env.conflicts if env else []
            marker = tuple((c.id, c.path, c.outcome, c.last_ts) for c in conflicts)
            if marker != seen["m"]:
                seen["m"] = marker
                panel.refresh()

        # tick() diffs the conflict marker; do not remount on every rev.
        tick()
        ui.timer(0.25, tick)

    page_shell("Conflicts", body)
