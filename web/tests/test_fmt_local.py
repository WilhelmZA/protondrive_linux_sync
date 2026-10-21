"""Local time formatting."""

from datetime import UTC, datetime

from neutronsync_web.fmt import format_log_lines_local


def test_log_lines_convert_utc_stamp_to_local() -> None:
    lines = ["2026-01-01 00:00:00Z started"]
    out = format_log_lines_local(lines)
    dt = datetime(2026, 1, 1, 0, 0, 0, tzinfo=UTC).astimezone()
    assert out[0].startswith(dt.strftime("%Y-%m-%d %H:%M:%S"))
