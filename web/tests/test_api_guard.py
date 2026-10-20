"""Version guard and unreachable notices (AC-3)."""

from __future__ import annotations

import asyncio

import httpx
import pytest

from neutronsync_web import __version__
from neutronsync_web.api import ApiClient
from neutronsync_web.live import ConnStatus, LiveHub


@pytest.mark.asyncio
async def test_unsupported_api_version_message() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/version"):
            return httpx.Response(200, json={"api_version": 2, "version": "9.9.9", "build": "x"})
        return httpx.Response(404)

    api = ApiClient("http://engine.test", transport=httpx.MockTransport(handler))
    hub = LiveHub(api, "http://engine.test")
    ok = await hub.check_version()
    assert ok is False
    assert hub.state.status == ConnStatus.UNSUPPORTED
    msg = hub.state.unsupported_message
    assert f"version {__version__}" in msg
    assert "9.9.9" in msg
    assert "does not support" in msg
    await api.aclose()


@pytest.mark.asyncio
async def test_unreachable_then_recover() -> None:
    state = {"up": False}

    def handler(request: httpx.Request) -> httpx.Response:
        if not state["up"]:
            raise httpx.ConnectError("down")
        if request.url.path.endswith("/version"):
            return httpx.Response(200, json={"api_version": 1, "version": "0.7.0", "build": "t"})
        if request.url.path.endswith("/health"):
            return httpx.Response(200, json={"ok": True})
        if request.url.path.endswith("/events"):
            body = (
                'event: state\ndata: {"rev":1,"state":{"pairs":[],"account":{},'
                '"activity":[],"active":[],"busy":false,"watching":false,'
                '"signed_out":false},"held":[],"conflicts":[],"auto_sync":false}\n\n'
            )
            return httpx.Response(
                200,
                headers={"content-type": "text/event-stream"},
                content=body.encode(),
            )
        return httpx.Response(404)

    api = ApiClient("http://engine.test", transport=httpx.MockTransport(handler))
    hub = LiveHub(api, "http://engine.test")
    import neutronsync_web.live as live_mod

    old = live_mod.BACKOFF
    live_mod.BACKOFF = (0.05, 0.05, 0.05, 0.05)
    try:
        await hub.start()
        await asyncio.sleep(0.2)
        assert hub.state.status in (ConnStatus.UNREACHABLE, ConnStatus.CONNECTING, ConnStatus.LOST)
        state["up"] = True
        for _ in range(60):
            if hub.state.status == ConnStatus.OK and hub.state.envelope is not None:
                break
            await asyncio.sleep(0.05)
        assert hub.state.status == ConnStatus.OK
        assert hub.state.envelope is not None
    finally:
        live_mod.BACKOFF = old
        await hub.stop()
        await api.aclose()
