"""Folders page."""

from __future__ import annotations

from nicegui import ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError, PairConfig
from neutronsync_web.fmt import rel_time
from neutronsync_web.pages.layout import page_shell

_PAIR_CFGS: dict[str, PairConfig] = {}


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


async def _sync_pair(name: str) -> None:
    try:
        await ctx.get_api().sync(pairs=[name])
    except ApiError as exc:
        if exc.code == "signed_out":
            ui.notify(exc.message, type="warning")
        else:
            _err(exc)
    except Exception as exc:
        _err(exc)


async def _set_auto(name: str, local: str, remote: str, exclude: list[str], enabled: bool) -> None:
    try:
        await ctx.get_api().update_pair(
            name,
            {
                "name": name,
                "local": local,
                "remote": remote,
                "auto": enabled,
                "exclude": exclude,
            },
        )
    except Exception as exc:
        _err(exc)


def _open_dialog():
    """Create a dialog on the client layout so card refresh cannot detach it."""
    return ui.dialog().props("persistent")


def _confirm_remove(pair: PairConfig) -> None:
    # Parent under the client, not the pair card refreshable (R1-005).
    with ui.context.client:
        dialog = _open_dialog()
        with dialog, ui.card().classes("w-full").style("max-width:420px"):
            ui.label(f"Remove {pair.name}?").classes("text-h6")
            ui.label("Your files stay where they are on both sides.").classes("ns-muted")
            with ui.row().classes("w-full justify-end q-mt-md"):
                ui.button("Cancel", on_click=dialog.close).props("flat")

                async def do_remove() -> None:
                    dialog.close()
                    try:
                        await ctx.get_api().delete_pair(pair.name)
                        ui.notify("Folder removed", type="positive")
                    except Exception as exc:
                        _err(exc)

                ui.button("Remove", on_click=do_remove).props("color=negative")
        dialog.open()


def _folder_picker(*, title: str, kind: str, start: str | None, on_pick) -> None:
    root = None if kind == "local" else "/my-files"
    state: dict = {"path": start or root, "seq": 0, "cache": {}, "busy": False}

    with ui.context.client:
        dialog = _open_dialog()
        with dialog, ui.card().classes("w-full").style("max-width:560px;width:560px"):
            ui.label(title).classes("text-h6")
            crumbs = ui.row().classes("items-center gap-1 ns-crumbs")
            with ui.element("div").classes("ns-picker w-full"):
                listing = ui.list().props("dense separator").classes("w-full")
                with ui.element("div").classes("ns-picker-busy") as busy:
                    ui.spinner(size="lg")
                busy.set_visibility(False)
            typed = ui.input("Or type a path", value=state["path"] or "").classes("w-full")

            def parent_of(p: str | None) -> str | None:
                if not p or p == root:
                    return None
                head = p.rstrip("/").rsplit("/", 1)[0]
                if kind == "remote" and (not head or len(head) < len(root)):
                    return root
                return head or None

            def draw_crumbs(p: str | None) -> None:
                crumbs.clear()
                with crumbs:
                    base = "Proton Drive" if kind == "remote" else "Folders"
                    ui.link(base).classes("ns-crumb").on("click", lambda: go(root))
                    if not p or p == root:
                        return
                    rel = p[len(root) :] if kind == "remote" else p
                    acc = root if kind == "remote" else ""
                    for part in [x for x in rel.split("/") if x]:
                        acc = f"{acc.rstrip('/')}/{part}" if acc else f"/{part}"
                        ui.label("/").classes("ns-muted")
                        target = acc
                        ui.link(part).classes("ns-crumb").on("click", lambda t=target: go(t))

            def draw(p: str | None, items: list[dict]) -> None:
                listing.clear()
                with listing:
                    up = parent_of(p)
                    if p and p != root:
                        with ui.item(on_click=lambda: go(up)).props("clickable"):
                            with ui.item_section().props("avatar"):
                                ui.icon("arrow_upward")
                            ui.item_section("Up one level")
                    folders = [
                        i for i in items if not (kind == "remote" and i.get("kind") == "file")
                    ]
                    if not folders:
                        with ui.item():
                            ui.item_section("No folders here").classes("ns-muted")
                    for item in folders:
                        name = item.get("name") or ""
                        if kind == "local":
                            nxt = item.get("path") or ""
                        else:
                            nxt = f"{(p or root).rstrip('/')}/{name}"
                        with ui.item(on_click=lambda n=nxt: go(n)).props("clickable"):
                            with ui.item_section().props("avatar"):
                                ui.icon("folder").classes("text-primary")
                            ui.item_section(name or nxt)

            def draw_error(p: str | None, message: str) -> None:
                listing.clear()
                with listing:
                    with ui.item():
                        ui.item_section(message).classes("ns-err")
                    with ui.item(on_click=lambda: go(p, refresh=True)).props("clickable"):
                        with ui.item_section().props("avatar"):
                            ui.icon("refresh")
                        ui.item_section("Retry")

            async def go(p: str | None, *, refresh: bool = False) -> None:
                if state["busy"]:
                    return
                state["seq"] += 1
                seq = state["seq"]
                key = p or ""
                state["path"] = p
                draw_crumbs(p)
                typed.value = p or ""
                cached = None if refresh else state["cache"].get(key)
                if cached is not None:
                    draw(p, cached)
                    return
                state["busy"] = True
                busy.set_visibility(True)
                listing.classes(add="ns-dim")
                try:
                    if kind == "local":
                        items = await ctx.get_api().local_list(p)
                    else:
                        items = await ctx.get_api().remote_list(p or root)
                except Exception as exc:
                    if seq == state["seq"]:
                        draw_error(p, exc.message if isinstance(exc, ApiError) else str(exc))
                    return
                finally:
                    state["busy"] = False
                    busy.set_visibility(False)
                    listing.classes(remove="ns-dim")
                state["cache"][key] = items
                if seq == state["seq"]:
                    draw(p, items)

            async def go_typed() -> None:
                await go((typed.value or "").strip() or root)

            typed.on("keydown.enter", go_typed)

            with ui.row().classes("w-full justify-end q-mt-md"):
                ui.button("Cancel", on_click=dialog.close).props("flat")

                def use_current() -> None:
                    dialog.close()
                    on_pick((typed.value or "").strip())

                ui.button("Use this folder", on_click=use_current).props("color=primary")

            ui.timer(0.05, lambda: go(state["path"]), once=True)
        dialog.open()


def _pair_dialog(existing: PairConfig | None = None) -> None:
    form = {
        "name": existing.name if existing else "",
        "local": existing.local if existing else "",
        "remote": existing.remote if existing else "/my-files",
        "auto": existing.auto if existing else True,
        "exclude": list(existing.exclude) if existing else [],
    }

    with ui.context.client:
        dialog = _open_dialog()
        with dialog, ui.card().classes("w-full").style("max-width:560px"):
            ui.label("Edit folder" if existing else "Add folder").classes("text-h6")
            name_in = ui.input("Name", value=form["name"]).classes("w-full")
            local_in = ui.input("Local folder", value=form["local"]).classes("w-full")
            with ui.row():
                ui.button(
                    "Browse…",
                    on_click=lambda: _folder_picker(
                        title="Local folder",
                        kind="local",
                        start=local_in.value or None,
                        on_pick=lambda p: setattr(local_in, "value", p),
                    ),
                ).props("flat")
            remote_in = ui.input("Proton folder", value=form["remote"]).classes("w-full")
            with ui.row():
                ui.button(
                    "Browse…",
                    on_click=lambda: _folder_picker(
                        title="Proton folder",
                        kind="remote",
                        start=remote_in.value or "/my-files",
                        on_pick=lambda p: setattr(remote_in, "value", p),
                    ),
                ).props("flat")
            ui.label("Exclusions").classes("q-mt-sm")
            excl_box = ui.column().classes("w-full")
            new_excl = ui.input("Add exclusion").classes("w-full")

            def redraw_excl() -> None:
                excl_box.clear()
                with excl_box:
                    for i, ex in enumerate(form["exclude"]):
                        with ui.row().classes("items-center w-full"):
                            ui.label(ex).classes("ns-grow")

                            def remove(idx=i) -> None:
                                form["exclude"].pop(idx)
                                redraw_excl()

                            ui.button(icon="close", on_click=remove).props("flat dense round")

            def add_excl() -> None:
                val = (new_excl.value or "").strip()
                if val and val not in form["exclude"]:
                    form["exclude"].append(val)
                    new_excl.value = ""
                    redraw_excl()

            ui.button("Add", on_click=add_excl).props("flat")
            new_excl.on("keydown.enter", add_excl)
            redraw_excl()
            auto_sw = ui.switch("Auto sync this folder", value=form["auto"])

            with ui.row().classes("w-full justify-end q-mt-md"):
                ui.button("Cancel", on_click=dialog.close).props("flat")

                async def save() -> None:
                    val = (new_excl.value or "").strip()
                    if val and val not in form["exclude"]:
                        form["exclude"].append(val)
                        new_excl.value = ""
                        redraw_excl()
                    body = {
                        "name": name_in.value.strip(),
                        "local": local_in.value.strip(),
                        "remote": remote_in.value.strip(),
                        "auto": bool(auto_sw.value),
                        "exclude": list(form["exclude"]),
                    }
                    try:
                        if existing:
                            await ctx.get_api().update_pair(existing.name, body)
                        else:
                            await ctx.get_api().create_pair(body)
                        dialog.close()
                        ui.notify("Saved", type="positive")
                    except Exception as exc:
                        _err(exc)

                ui.button("Save", on_click=save).props("color=primary")
        dialog.open()


def render() -> None:
    def body() -> None:
        with ui.row().classes("q-mb-md"):
            ui.button("Add folder", on_click=lambda: _pair_dialog(None)).props("color=primary")

        live = ctx.get_live()
        # Per-pair refreshables so one sync's progress cannot remount another
        # pair's Edit/Remove/Auto/Sync buttons (R1-005).
        card_hosts: dict[str, object] = {}

        def _render_card(name: str) -> None:
            env = live.state.envelope
            if not env:
                return
            pair = next((p for p in env.state.pairs if p.name == name), None)
            if pair is None:
                return
            cfg = _PAIR_CFGS.get(name) or PairConfig(
                name=pair.name, local=pair.local, remote=pair.remote, auto=True
            )
            with ui.element("div").classes("ns-card"):
                ui.label(pair.name).classes("text-subtitle1")
                ui.label(pair.local).classes("ns-muted")
                ui.label(pair.remote).classes("ns-muted")
                ui.label(
                    f"{pair.phase} · {pair.tracked} files tracked · "
                    f"last synced {rel_time(pair.last_synced)}"
                ).classes("ns-muted")
                if pair.phase in ("Syncing", "Scanning") and pair.progress.total:
                    ui.linear_progress(
                        value=pair.progress.done / max(pair.progress.total, 1)
                    ).props("rounded")
                with ui.row().classes("q-mt-sm gap-2 flex-wrap items-center"):
                    ui.switch(
                        "Auto",
                        value=cfg.auto if name in _PAIR_CFGS else True,
                        on_change=lambda e, p=pair: _toggle_auto_loaded(p, e.value),
                    )
                    ui.button("Sync now", on_click=lambda p=pair: _sync_pair(p.name)).props(
                        "outline"
                    )
                    ui.button("Edit", on_click=lambda p=pair: _edit_loaded(p)).props("flat")
                    ui.button(
                        "Remove",
                        on_click=lambda p=pair: _confirm_remove(
                            PairConfig(name=p.name, local=p.local, remote=p.remote)
                        ),
                    ).props("flat color=negative")

        @ui.refreshable
        def list_shell() -> None:
            card_hosts.clear()
            env = live.state.envelope
            if not env:
                ui.label("Waiting for the engine…").classes("ns-muted")
                return
            if not env.state.pairs:
                ui.label("No folders yet.").classes("ns-muted")
                return
            for p in env.state.pairs:
                name = p.name

                @ui.refreshable
                def card(n=name) -> None:
                    _render_card(n)

                card()
                card_hosts[name] = card

        async def load_cfg() -> None:
            try:
                configs = await ctx.get_api().list_pairs()
                _PAIR_CFGS.clear()
                _PAIR_CFGS.update({c.name: c for c in configs})
                # Refresh each card for Auto values without tearing down siblings.
                if card_hosts:
                    for host in list(card_hosts.values()):
                        host.refresh()  # type: ignore[attr-defined]
                else:
                    list_shell.refresh()
            except Exception:
                pass

        list_shell()
        ui.timer(0.05, load_cfg, once=True)

        seen: dict[str, object] = {"names": None, "pairs": {}}

        def tick() -> None:
            env = live.state.envelope
            if not env:
                names: object = ("empty",)
            else:
                names = tuple(p.name for p in env.state.pairs)
            if names != seen["names"]:
                seen["names"] = names
                seen["pairs"] = {}
                list_shell.refresh()
                return
            if not env:
                return
            pair_seen: dict = seen["pairs"]  # type: ignore[assignment]
            for p in env.state.pairs:
                marker = (
                    p.phase,
                    p.tracked,
                    p.progress.done,
                    p.progress.total,
                    p.last_synced,
                    (_PAIR_CFGS[p.name].auto if p.name in _PAIR_CFGS else True),
                )
                if pair_seen.get(p.name) != marker:
                    pair_seen[p.name] = marker
                    host = card_hosts.get(p.name)
                    if host is not None:
                        host.refresh()  # type: ignore[attr-defined]

        # tick() diffs markers; do not invalidate all cards on every rev.
        ui.timer(0.25, tick)

    page_shell("Folders", body)


async def _loaded_config(name: str, local: str, remote: str) -> PairConfig:
    """Always fetch pair config from the API before Edit/Auto mutations."""
    configs = await ctx.get_api().list_pairs()
    _PAIR_CFGS.clear()
    _PAIR_CFGS.update({c.name: c for c in configs})
    cfg = _PAIR_CFGS.get(name)
    if cfg is not None:
        return cfg
    return PairConfig(name=name, local=local, remote=remote, auto=True, exclude=[])


async def _edit_loaded(pair) -> None:
    try:
        cfg = await _loaded_config(pair.name, pair.local, pair.remote)
        _pair_dialog(cfg)
    except Exception as exc:
        _err(exc)


async def _toggle_auto_loaded(pair, enabled: bool) -> None:
    try:
        cfg = await _loaded_config(pair.name, pair.local, pair.remote)
        await _set_auto(pair.name, pair.local, pair.remote, list(cfg.exclude), enabled)
    except Exception as exc:
        _err(exc)
