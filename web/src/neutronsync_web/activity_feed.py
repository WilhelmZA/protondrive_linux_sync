"""Group engine activity into one row per sync run."""

from __future__ import annotations

from dataclasses import dataclass, field

from neutronsync_web.api import ActivityItem


@dataclass
class ActivityRun:
    summary: ActivityItem
    files: list[ActivityItem] = field(default_factory=list)


def _is_run_summary(item: ActivityItem) -> bool:
    if item.op is not None:
        return False
    if item.kind not in ("Sync", "Error"):
        return False
    text = item.text or ""
    return ": " in text and (
        "Uploaded " in text
        or "Downloaded " in text
        or "Deleted " in text
        or "Changed " in text
        or "Nothing to do" in text
        or "finished with errors" in text
    )


def group_activity_runs(items: list[ActivityItem]) -> list[ActivityRun]:
    """Newest run first; file rows belong to the summary that ends each run."""
    runs: list[ActivityRun] = []
    current_files: list[ActivityItem] = []
    for item in items:
        if item.op is not None:
            current_files.append(item)
        elif _is_run_summary(item):
            runs.append(ActivityRun(summary=item, files=list(current_files)))
            current_files = []
        elif item.kind == "Error" and item.text:
            runs.append(ActivityRun(summary=item, files=[]))
        elif item.kind == "Info" and item.text and not _is_run_summary(item):
            runs.append(ActivityRun(summary=item, files=[]))
    if current_files:
        orphan = ActivityItem(text="Earlier activity", kind="Info")
        runs.append(ActivityRun(summary=orphan, files=current_files))
    runs.reverse()
    return runs
