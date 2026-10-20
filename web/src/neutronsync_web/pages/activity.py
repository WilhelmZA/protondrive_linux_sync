"""Activity page."""

from __future__ import annotations

from nicegui import ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError, HeldItem
from neutronsync_web.fmt import held_subtitle, held_title, plural, rel_time
from neutronsync_web.pages.layout import page_shell


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


async def _sync(dry_run: bool = False) -> None:
    try:
        await ctx.get_api().sync(dry_run=dry_run)
    except Exception as exc:
        _err(exc)


async def _cancel() -> None:
    try:
        await ctx.get_api().cancel()
    except Exception as exc:
        _err(exc)


async def _restore(pair: str) -> None:
    try:
        await ctx.get_api().held_restore(pair)
        ui.notify("Restore started", type="positive")
    except Exception as exc:
        _err(exc)


def _confirm_delete(pair: str) -> None:
    with ui.dialog() as dialog, ui.card().classes("w-full").style("max-width:420px"):
        ui.label("Delete them?").classes("text-h6")
        ui.label(
            "The files go to Proton's trash or the desktop trash and can be restored "
            "from there. This lets the held deletions through for one run."
        ).classes("ns-muted")
        with ui.row().classes("w-full justify-end q-mt-md"):
            ui.button("Cancel", on_click=dialog.close).props("flat")

            async def do_delete() -> None:
                dialog.close()
                try:
                    await ctx.get_api().held_delete(pair)
                    ui.notify("Delete started", type="positive")
                except Exception as exc:
                    _err(exc)

            ui.button("Delete them", on_click=do_delete).props("color=negative")
    dialog.open()


def _held_banner(hold: HeldItem) -> None:
    with ui.element("div").classes("ns-held"):
        ui.label(held_title(hold)).classes("text-subtitle1")
        ui.label(held_subtitle(hold)).classes("ns-muted")
        with ui.row().classes("q-mt-sm gap-2"):
            ui.button("Restore them", on_click=lambda h=hold: _restore(h.pair)).props(
                "color=primary"
            )
            ui.button("Delete them", on_click=lambda h=hold: _confirm_delete(h.pair)).props(
                "outline"
            )


def _op_icon(action: str, ok: bool) -> str:
    if not ok:
        return "error"
    a = (action or "").lower()
    if "upload" in a:
        return "cloud_upload"
    if "download" in a:
        return "cloud_download"
    if "delete" in a or "trash" in a:
        return "delete"
    if "conflict" in a:
        return "warning"
    return "sync"


def render() -> None:
    def body() -> None:
        live = ctx.get_live()

        # Status line and action buttons are separate refreshables: a tracked-count
        # change must not remount Sync now / Dry run / Cancel (planner HOLD).
        with ui.element("div").classes("ns-card"):

            @ui.refreshable
            def status_line() -> None:
                env = live.state.envelope
                if not env:
                    ui.label("Waiting for the engine…").classes("ns-muted")
                    return
                st = env.state
                watched = len(st.pairs)
                tracked = sum(p.tracked for p in st.pairs)
                ui.label(
                    f"{'Auto sync on' if st.watching else 'Auto sync off'} · "
                    f"{plural(watched, 'folder')} watched · "
                    f"{plural(tracked, 'file')} tracked"
                )

            @ui.refreshable
            def action_buttons() -> None:
                env = live.state.envelope
                if not env:
                    return
                with ui.row().classes("q-mt-sm gap-2 flex-wrap"):
                    if env.state.busy:
                        ui.button("Cancel", on_click=_cancel).props("color=negative")
                    else:
                        ui.button("Sync now", on_click=lambda: _sync(False)).props("color=primary")
                        ui.button("Dry run", on_click=lambda: _sync(True)).props("outline")

            status_line()
            action_buttons()

        @ui.refreshable
        def held() -> None:
            env = live.state.envelope
            if not env:
                return
            for hold in env.held:
                _held_banner(hold)

        @ui.refreshable
        def active() -> None:
            env = live.state.envelope
            if not env or not env.state.active:
                return
            st = env.state
            ui.label("In progress").classes("text-subtitle1 q-mt-md")
            for op in st.active:
                with ui.element("div").classes("ns-card"):
                    ui.label(f"{op.action} {op.path}".strip()).classes("text-body1")
                    ui.label(op.pair).classes("ns-muted")
                    pair = next((p for p in st.pairs if p.name == op.pair), None)
                    if pair and pair.progress.total:
                        ui.linear_progress(value=pair.progress.done / pair.progress.total).props(
                            "rounded"
                        )
                        ui.label(f"{pair.progress.done} of {pair.progress.total}").classes(
                            "ns-muted"
                        )

        @ui.refreshable
        def feed() -> None:
            env = live.state.envelope
            ui.label("Recent activity").classes("text-subtitle1 q-mt-md")
            if not env:
                ui.label("Nothing yet.").classes("ns-muted")
                return
            items = list(reversed(env.state.activity[-100:]))
            if not items:
                ui.label("Nothing yet.").classes("ns-muted")
            for item in items:
                with ui.element("div").classes("ns-card"):
                    with ui.row().classes("items-start no-wrap gap-3 w-full"):
                        action = item.op.action if item.op else item.kind
                        ok = item.op.ok if item.op else item.kind != "Error"
                        ui.icon(_op_icon(action, ok)).classes("text-primary" if ok else "ns-err")
                        with ui.column().classes("ns-grow gap-0"):
                            name = item.op.path if item.op else item.text
                            ui.label(name or item.text)
                            meta = []
                            if item.op and item.op.pair:
                                meta.append(item.op.pair)
                            meta.append(rel_time(item.ts))
                            ui.label(" · ".join(meta)).classes("ns-muted")
                            if item.op and item.op.error:
                                ui.label(item.op.error).classes("ns-err")
                            elif item.kind == "Error":
                                ui.label(item.text).classes("ns-err")

        held()
        active()
        feed()

        seen: dict[str, object] = {
            "status": None,
            "buttons": None,
            "held": None,
            "active": None,
            "feed": None,
        }

        def tick() -> None:
            env = live.state.envelope
            if not env:
                empty = ("empty",)
                if seen["status"] != empty:
                    seen["status"] = empty
                    status_line.refresh()
                if seen["buttons"] != empty:
                    seen["buttons"] = empty
                    action_buttons.refresh()
                return
            st = env.state
            status_m = (
                st.watching,
                len(st.pairs),
                sum(p.tracked for p in st.pairs),
            )
            # Buttons remount only when busy flips (or envelope first arrives).
            buttons_m = (True, st.busy)
            held_m = tuple((h.pair, h.local, h.remote, h.tracked) for h in env.held)
            active_bits = []
            for op in st.active:
                pair = next((x for x in st.pairs if x.name == op.pair), None)
                active_bits.append(
                    (
                        op.action,
                        op.path,
                        op.pair,
                        pair.progress.done if pair else 0,
                        pair.progress.total if pair else 0,
                    )
                )
            active_m = tuple(active_bits)
            feed_m = (
                len(st.activity),
                st.activity[-1].ts if st.activity else 0,
                st.activity[-1].text if st.activity else "",
            )
            if status_m != seen["status"]:
                seen["status"] = status_m
                status_line.refresh()
            if buttons_m != seen["buttons"]:
                seen["buttons"] = buttons_m
                action_buttons.refresh()
            if held_m != seen["held"]:
                seen["held"] = held_m
                held.refresh()
            if active_m != seen["active"]:
                seen["active"] = active_m
                active.refresh()
            if feed_m != seen["feed"]:
                seen["feed"] = feed_m
                feed.refresh()

        # Do not clear markers on every rev — that remounts Sync now on each
        # tracked-count tick and drops clicks (planner HOLD). tick() diffs.
        tick()
        ui.timer(0.25, tick)

    page_shell("Activity", body)
