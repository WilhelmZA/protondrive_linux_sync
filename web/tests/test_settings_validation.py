"""Client-side settings validation (#27)."""

from neutronsync_web.api import EngineSettings
from neutronsync_web.pages.settings_page import _validate_settings_keys


def test_negative_full_walk_interval_is_rejected() -> None:
    draft: dict = {
        "settings": EngineSettings(full_walk_interval=-5),
        "errors": {},
    }
    assert not _validate_settings_keys(draft, ["full_walk_interval"])
    assert "full_walk_interval" in draft["errors"]
