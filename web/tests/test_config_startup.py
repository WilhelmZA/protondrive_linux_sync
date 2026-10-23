"""Startup diagnostics use mock HTTP only; no engine or Proton account."""

import asyncio

import httpx

from neutronsync_web.api import ApiClient
from neutronsync_web.live import ConnStatus, LiveHub


def test_startup_config_error_is_distinct_from_unreachable_and_recovers() -> None:
    async def body() -> None:
        invalid = True

        def handler(request: httpx.Request) -> httpx.Response:
            if invalid:
                return httpx.Response(
                    422,
                    json={
                        "error": {
                            "code": "invalid_config",
                            "message": "pair already syncs /x; one folder cannot be in two pairs.",
                        }
                    },
                )
            return httpx.Response(200, json={"api_version": 1, "version": "0.9.0", "build": "test"})

        api = ApiClient("http://test", transport=httpx.MockTransport(handler))
        hub = LiveHub(api, "http://test")
        try:
            assert not await hub.check_version()
            assert hub.state.status == ConnStatus.INVALID_CONFIG
            assert "one folder cannot be in two pairs" in hub.state.last_error
            assert hub.state.envelope is None
            invalid = False
            assert await hub.check_version()
            assert hub.state.version.api_version == 1
        finally:
            await api.aclose()

    asyncio.run(body())


def test_page_shows_startup_error_and_hides_normal_controls(monkeypatch) -> None:
    from types import SimpleNamespace

    from nicegui import ui

    from neutronsync_web.live import LiveState
    from neutronsync_web.pages import layout

    state = LiveState(
        status=ConnStatus.INVALID_CONFIG,
        last_error="mirror already syncs /my-files/test; one folder cannot be in two pairs.",
    )

    def synchronous_refreshable(function):
        # Render refreshes inline without starting NiceGUI's server loop.
        function.refresh = function
        return function

    monkeypatch.setattr(layout.ui, "refreshable", synchronous_refreshable)
    monkeypatch.setattr(layout.ctx, "get_live", lambda: SimpleNamespace(state=state))
    monkeypatch.setattr(layout.ctx, "get_auth", lambda: SimpleNamespace(mode="proxy"))
    monkeypatch.setattr(layout.ui, "timer", lambda *args, **kwargs: None)
    monkeypatch.setattr(layout, "apply_colors", lambda: None)
    monkeypatch.setattr(layout, "apply_appearance", lambda: None)
    client = ui.context.client
    before = set(client.elements)
    try:
        with client.content:
            layout.page_shell("Activity", lambda: ui.label("normal body"))
        elements = [element for key, element in client.elements.items() if key not in before]
        texts = [getattr(element, "text", "") for element in elements]
        assert "NeutronSync configuration error" in texts
        assert state.last_error in texts
        assert not any("Cannot reach" in text for text in texts)
        normal_body = next(
            element for element in elements if getattr(element, "text", "") == "normal body"
        )
        assert not normal_body.parent_slot.parent.parent_slot.parent.visible
    finally:
        for key in set(client.elements) - before:
            if key in client.elements:
                client.elements[key].delete()


def test_event_retry_preserves_startup_config_status(monkeypatch) -> None:
    async def body() -> None:
        transport = httpx.MockTransport(
            lambda _: httpx.Response(
                422,
                json={
                    "error": {
                        "code": "invalid_config",
                        "message": "Invalid startup configuration.",
                    }
                },
            )
        )
        api = ApiClient("http://test", transport=transport)
        hub = LiveHub(api, "http://test")
        statuses = []
        hub.state.register(lambda: statuses.append(hub.state.status))
        try:
            await hub.start()
            await asyncio.sleep(0.08)
            assert len(statuses) >= 2
            assert set(statuses) == {ConnStatus.INVALID_CONFIG}
        finally:
            await hub.stop()
            await api.aclose()

    monkeypatch.setattr("neutronsync_web.live.BACKOFF", (0.01,))
    asyncio.run(body())
