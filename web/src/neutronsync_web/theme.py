"""Colours and dark mode."""

from __future__ import annotations

from nicegui import app, ui

PRIMARY = "#6D4AFF"
WARNING = "#D8A050"
NEGATIVE = "#E05064"


def apply_colors() -> None:
    ui.colors(primary=PRIMARY, warning=WARNING, negative=NEGATIVE)


def apply_appearance() -> None:
    """Apply the user's appearance preference (System / Light / Dark)."""
    mode = app.storage.user.get("appearance", "system")
    dark = ui.dark_mode()
    if mode == "light":
        dark.disable()
    elif mode == "dark":
        dark.enable()
    else:
        # Follow the browser.
        dark.auto()


def set_appearance(mode: str) -> None:
    if mode not in ("system", "light", "dark"):
        mode = "system"
    app.storage.user["appearance"] = mode
    apply_appearance()
