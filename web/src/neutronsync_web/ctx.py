"""Process-wide handles set at startup."""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from neutronsync_web.api import ApiClient
    from neutronsync_web.auth import AuthStore
    from neutronsync_web.live import LiveHub
    from neutronsync_web.settings import Settings

api: ApiClient | None = None
live: LiveHub | None = None
auth: AuthStore | None = None
settings: Settings | None = None


def get_api() -> ApiClient:
    assert api is not None
    return api


def get_live() -> LiveHub:
    assert live is not None
    return live


def get_auth() -> AuthStore:
    assert auth is not None
    return auth


def get_settings() -> Settings:
    assert settings is not None
    return settings
