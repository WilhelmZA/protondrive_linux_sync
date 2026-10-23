"""Local trash is read and changed only through the serve API."""

from __future__ import annotations

from nicegui import ui

from neutronsync_web import ctx
from neutronsync_web.api import ApiError
from neutronsync_web.pages.layout import page_shell


def render() -> None:
    def body() -> None:
        ui.label(
            "Restore deleted files to their original path. Existing files are never overwritten."
        ).classes("ns-muted")
        listing = ui.column().classes("w-full")
        busy = False

        async def action(item: dict, restore: bool, *, confirm: bool = False) -> None:
            nonlocal busy
            if busy or (not restore and not confirm):
                return
            busy = True
            try:
                api = ctx.get_api()
                if restore:
                    await api.trash_restore(item["pair"], item["id"])
                else:
                    await api.trash_delete(item["pair"], item["id"], confirm=True)
                ui.notify(
                    "File restored" if restore else "File permanently deleted", type="positive"
                )
            except Exception as exc:
                ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")
            finally:
                busy = False
            await refresh()

        def confirm_delete(item: dict) -> None:
            with ui.context.client:
                with ui.dialog() as dialog, ui.card():
                    ui.label(f"Permanently delete {item['name']}?").classes("text-h6")
                    ui.label(item["original_path"])
                    ui.label("This cannot be undone.")
                    with ui.row():
                        ui.button("Cancel", on_click=dialog.close).props("flat")

                        async def delete() -> None:
                            dialog.close()
                            await action(item, False, confirm=True)

                        ui.button("Delete permanently", on_click=delete).props("color=negative")
                dialog.open()

        async def refresh() -> None:
            try:
                items = await ctx.get_api().trash()
            except Exception as exc:
                listing.clear()
                with listing:
                    ui.label(exc.message if isinstance(exc, ApiError) else str(exc)).classes(
                        "ns-err"
                    )
                return
            listing.clear()
            with listing:
                if not items:
                    ui.label("Trash is empty.")
                for item in items:
                    with ui.card().classes("w-full"):
                        ui.label(item["name"]).classes("text-subtitle1")
                        ui.label(f"Folder: {item['pair']}")
                        ui.label(item["original_path"])
                        ui.label(f"{item['size']} bytes · Deleted {item['deleted_at']}").classes(
                            "ns-muted"
                        )
                        with ui.row():
                            ui.button("Restore", on_click=lambda i=item: action(i, True))
                            ui.button(
                                "Delete permanently", on_click=lambda i=item: confirm_delete(i)
                            ).props("flat color=negative")

        ui.button("Refresh", on_click=refresh).props("flat")
        ui.timer(0.1, refresh, once=True)

    page_shell("Trash", body)
