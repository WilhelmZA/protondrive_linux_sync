"""Plain-word formatting helpers matching the desktop window."""

from __future__ import annotations

import re
import time
from datetime import UTC, datetime

from neutronsync_web.api import ConflictItem, HeldItem

_LOG_STAMP = re.compile(r"^(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2})Z?\s")


def thousands(n: int) -> str:
    s = str(n)
    parts: list[str] = []
    while s:
        parts.append(s[-3:])
        s = s[:-3]
    return ",".join(reversed(parts))


def plural(n: int, one: str) -> str:
    if n == 1:
        return f"1 {one}"
    return f"{thousands(n)} {one}s"


def held_where(hold: HeldItem) -> str:
    match (hold.remote > 0, hold.local > 0):
        case (True, True):
            return "on this computer and in Proton Drive"
        case (True, False):
            return "on this computer"
        case _:
            return "in Proton Drive"


def held_title(hold: HeldItem) -> str:
    total = hold.local + hold.remote
    return f"{plural(total, 'deletion')} held back in {hold.pair}"


def held_subtitle(hold: HeldItem) -> str:
    total = hold.local + hold.remote
    if hold.tracked:
        pct = max(1, (total * 100) // hold.tracked)
        share = f"That is {pct}% of the {plural(hold.tracked, 'file')} this folder keeps in sync. "
    else:
        share = ""
    return f"{share}They were removed {held_where(hold)}. Nothing was deleted."


def conflict_sentence(c: ConflictItem) -> str:
    if c.outcome == "kept_both":
        name = (c.copy or "").rsplit("/", 1)[-1] if c.copy else "a conflict copy"
        return f"Changed on both sides. Kept both; your version is saved as {name}."
    if c.outcome == "kept_local":
        return "Changed on both sides. Kept the newer version from this computer."
    if c.outcome == "kept_remote":
        return "Changed on both sides. Kept the newer version from Proton Drive."
    if c.outcome == "skipped":
        return "Changed on both sides. Left as it is until you choose."
    return "Changed on both sides."


def rel_time(ts: int | None) -> str:
    if not ts:
        return "never"
    delta = int(time.time()) - ts
    if delta < 0:
        delta = 0
    if delta < 60:
        return "just now"
    if delta < 3600:
        m = delta // 60
        return f"{m} minute{'s' if m != 1 else ''} ago"
    if delta < 86400:
        h = delta // 3600
        return f"{h} hour{'s' if h != 1 else ''} ago"
    d = delta // 86400
    return f"{d} day{'s' if d != 1 else ''} ago"


def speed_limit_label(kbps: int) -> str:
    if kbps == 0:
        return "No limit"
    if kbps % 1024 == 0:
        return f"{kbps // 1024} MB/s"
    return f"{kbps} KB/s"


SPEED_PRESETS = [0, 256, 512, 1024, 2048, 5120, 10240]

CONFLICT_LABELS = {
    "keep-both": "Keep both",
    "newer": "Keep newer",
    "skip": "Skip",
}

COMPARE_LABELS = {
    "size": "Size",
    "size+mtime": "Size + modified time",
    "sha1": "SHA-1 (exact)",
}

LOCAL_DELETE_LABELS = {
    "trash": "Trash folder on the same drive",
    "remove": "Delete forever",
}

THREAD_MIN = 0
THREAD_MAX = 64
WHOLE_NUMBER_RANGE = f"Enter a whole number from {THREAD_MIN} to {THREAD_MAX}."


def format_log_lines_local(lines: list[str]) -> list[str]:
    """Rewrite leading UTC timestamps in log lines to local wall time."""
    out: list[str] = []
    for line in lines:
        m = _LOG_STAMP.match(line)
        if not m:
            out.append(line)
            continue
        try:
            dt = datetime.strptime(f"{m.group(1)} {m.group(2)}", "%Y-%m-%d %H:%M:%S").replace(
                tzinfo=UTC
            )
            local = dt.astimezone().strftime("%Y-%m-%d %H:%M:%S")
            out.append(f"{local} {line[m.end() :]}")
        except ValueError:
            out.append(line)
    return out
