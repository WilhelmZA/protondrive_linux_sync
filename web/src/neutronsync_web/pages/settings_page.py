"""Settings page."""

from __future__ import annotations

from nicegui import app, ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError, EngineSettings
from neutronsync_web.fmt import (
    COMPARE_LABELS,
    CONFLICT_LABELS,
    LOCAL_DELETE_LABELS,
    SPEED_PRESETS,
    THREAD_MAX,
    THREAD_MIN,
    WHOLE_NUMBER_RANGE,
    speed_limit_label,
)
from neutronsync_web.pages.layout import page_shell
from neutronsync_web.theme import set_appearance

_DRAFTS: dict[str, dict] = {}


def _draft_for_client() -> dict:
    """Per-browser-client draft so sessions cannot overwrite each other."""
    try:
        cid = str(ui.context.client.id)
    except Exception:
        cid = "_anon"
    draft = _DRAFTS.get(cid)
    if draft is None:
        draft = {"settings": EngineSettings(), "errors": {}, "loaded": False}
        _DRAFTS[cid] = draft
    return draft


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


def _whole_number(value, *, lo: int = 0, hi: int | None = None) -> int | None:
    if value is None or value == "":
        return 0
    try:
        n = int(value)
    except (TypeError, ValueError):
        return None
    if n < lo or (hi is not None and n > hi):
        return None
    return n


def _validate_settings_keys(draft: dict, keys: list[str]) -> bool:
    s = draft["settings"]
    errors = draft["errors"]
    ok = True
    thread_keys = {"scan_threads", "download_threads"}
    for key in keys:
        errors.pop(key, None)
        val = getattr(s, key, None)
        if key in thread_keys:
            n = _whole_number(val, lo=THREAD_MIN, hi=THREAD_MAX)
            if n is None:
                errors[key] = WHOLE_NUMBER_RANGE
                ok = False
            continue
        if key == "max_delete_percent":
            n = _whole_number(val, lo=0, hi=100)
            if n is None:
                errors[key] = "max_delete_percent must be a whole number from 0 to 100."
                ok = False
            continue
        if key in {
            "max_delete_files",
            "full_walk_interval",
            "scan_interval",
            "debounce",
            "upload_limit_kbps",
            "download_limit_kbps",
        }:
            n = _whole_number(val, lo=0)
            if n is None:
                errors[key] = "Enter a whole number from 0 upward."
                ok = False
    return ok


def render() -> None:
    draft = _draft_for_client()

    def body() -> None:
        @ui.refreshable
        def form() -> None:
            s = draft["settings"]
            errors = draft["errors"]

            def group(title: str, keys: list[str], build) -> None:
                with ui.element("div").classes("ns-card"):
                    ui.label(title).classes("text-subtitle1")
                    build()
                    err_keys = [k for k in keys if k in errors]
                    if err_keys:
                        ui.label(errors[err_keys[0]]).classes("ns-err q-mt-sm")

                    async def save() -> None:
                        if "max_delete_files" in keys and draft.get("_files_input"):
                            draft["settings"].max_delete_files = int(
                                draft["_files_input"].value or 0
                            )
                        if "max_delete_percent" in keys and draft.get("_pct_input"):
                            draft["settings"].max_delete_percent = int(
                                draft["_pct_input"].value or 0
                            )
                        for key, widget in draft.get("_adv_inputs", {}).items():
                            if key in keys:
                                setattr(
                                    draft["settings"],
                                    key,
                                    int(widget.value or 0),
                                )
                        if not _validate_settings_keys(draft, keys):
                            form.refresh()
                            return
                        patch = {k: getattr(draft["settings"], k) for k in keys}
                        try:
                            draft["settings"] = await ctx.get_api().put_settings(patch)
                            for k in keys:
                                draft["errors"].pop(k, None)
                            ui.notify("Saved", type="positive")
                            form.refresh()
                        except ApiError as exc:
                            for k in keys:
                                draft["errors"][k] = exc.message
                            form.refresh()
                        except Exception as exc:
                            _err(exc)

                    ui.button("Save", on_click=save).props("color=primary").classes("q-mt-sm")

            def group_sync() -> None:
                ui.select(
                    dict(CONFLICT_LABELS),
                    label="When both sides changed",
                    value=s.conflict,
                    on_change=lambda e: setattr(draft["settings"], "conflict", e.value),
                ).classes("w-full")
                ui.select(
                    dict(COMPARE_LABELS),
                    label="Compare files by",
                    value=s.compare,
                    on_change=lambda e: setattr(draft["settings"], "compare", e.value),
                ).classes("w-full")
                ui.switch(
                    "Propagate deletions",
                    value=s.propagate_deletes,
                    on_change=lambda e: setattr(draft["settings"], "propagate_deletes", e.value),
                )
                ui.select(
                    dict(LOCAL_DELETE_LABELS),
                    label="Local deletes go to",
                    value=s.local_delete,
                    on_change=lambda e: setattr(draft["settings"], "local_delete", e.value),
                ).classes("w-full")

            def group_guard() -> None:
                files = ui.number(
                    "Most files one run may delete",
                    value=s.max_delete_files,
                    min=0,
                    on_change=lambda e: setattr(
                        draft["settings"], "max_delete_files", int(e.value or 0)
                    ),
                ).classes("w-full")
                pct = ui.number(
                    "Most percent one run may delete",
                    value=s.max_delete_percent,
                    min=0,
                    on_change=lambda e: setattr(
                        draft["settings"], "max_delete_percent", int(e.value or 0)
                    ),
                ).classes("w-full")
                # Keep references so Save can read typed values even if change
                # events were coalesced away by a live refresh.
                draft["_files_input"] = files
                draft["_pct_input"] = pct

            def group_network() -> None:
                choices = {str(k): speed_limit_label(k) for k in SPEED_PRESETS}
                if s.upload_limit_kbps not in SPEED_PRESETS:
                    choices[str(s.upload_limit_kbps)] = speed_limit_label(s.upload_limit_kbps)
                if s.download_limit_kbps not in SPEED_PRESETS:
                    choices[str(s.download_limit_kbps)] = speed_limit_label(s.download_limit_kbps)
                ui.select(
                    choices,
                    label="Upload limit",
                    value=str(s.upload_limit_kbps),
                    on_change=lambda e: setattr(
                        draft["settings"], "upload_limit_kbps", int(e.value)
                    ),
                ).classes("w-full")
                ui.select(
                    choices,
                    label="Download limit",
                    value=str(s.download_limit_kbps),
                    on_change=lambda e: setattr(
                        draft["settings"], "download_limit_kbps", int(e.value)
                    ),
                ).classes("w-full")
                custom = ui.number("Custom limit (KB/s)", value=None).classes("w-full")

                def apply_custom(which: str) -> None:
                    if custom.value is None:
                        return
                    setattr(draft["settings"], which, int(custom.value))
                    form.refresh()

                with ui.row():
                    ui.button(
                        "Use as upload",
                        on_click=lambda: apply_custom("upload_limit_kbps"),
                    ).props("flat")
                    ui.button(
                        "Use as download",
                        on_click=lambda: apply_custom("download_limit_kbps"),
                    ).props("flat")

            def group_advanced() -> None:
                draft["_adv_inputs"] = {}
                for label, key in [
                    ("Scan threads", "scan_threads"),
                    ("Download threads", "download_threads"),
                    ("Full check every (seconds)", "full_walk_interval"),
                    ("Scan interval (seconds)", "scan_interval"),
                    ("Debounce (seconds)", "debounce"),
                ]:
                    props = ""
                    if key in ("scan_threads", "download_threads"):
                        props = f"min={THREAD_MIN} max={THREAD_MAX}"
                    num = (
                        ui.number(
                            label,
                            value=getattr(s, key),
                            on_change=lambda e, k=key: setattr(
                                draft["settings"], k, int(e.value or 0)
                            ),
                        )
                        .props(props)
                        .classes("w-full")
                    )
                    draft["_adv_inputs"][key] = num
                ui.input(
                    "Remote root",
                    value=s.remote_root,
                    on_change=lambda e: setattr(draft["settings"], "remote_root", e.value),
                ).classes("w-full")

            with ui.element("div").classes("ns-grid"):
                group(
                    "Sync",
                    ["conflict", "compare", "propagate_deletes", "local_delete"],
                    group_sync,
                )
                group(
                    "Deletion guard",
                    ["max_delete_files", "max_delete_percent"],
                    group_guard,
                )
                group(
                    "Network",
                    ["upload_limit_kbps", "download_limit_kbps"],
                    group_network,
                )
                group(
                    "Advanced",
                    [
                        "scan_threads",
                        "download_threads",
                        "full_walk_interval",
                        "scan_interval",
                        "debounce",
                        "remote_root",
                    ],
                    group_advanced,
                )

                with ui.element("div").classes("ns-card"):
                    ui.label("Appearance").classes("text-subtitle1")
                    current = app.storage.user.get("appearance", "system")
                    ui.select(
                        {"system": "System", "light": "Light", "dark": "Dark"},
                        label="Theme",
                        value=current,
                        on_change=lambda e: set_appearance(e.value),
                    ).classes("w-full")

        async def load() -> None:
            if draft["loaded"]:
                return
            try:
                draft["settings"] = await ctx.get_api().get_settings()
                draft["loaded"] = True
                form.refresh()
            except Exception as exc:
                _err(exc)

        form()
        ui.timer(0.05, load, once=True)

    page_shell("Settings", body, live_body=False)
