"""Activity feed grouping."""

from neutronsync_web.activity_feed import group_activity_runs
from neutronsync_web.api import ActivityItem, ActivityOp


def test_groups_file_ops_under_run_summary() -> None:
    items = [
        ActivityItem(ts=1, kind="Sync", text="upload a", op=ActivityOp(action="upload", path="a")),
        ActivityItem(ts=2, kind="Sync", text="photos: Uploaded 1 file"),
    ]
    runs = group_activity_runs(items)
    assert len(runs) == 1
    assert "Uploaded 1 file" in runs[0].summary.text
    assert len(runs[0].files) == 1
