"""Trash calls use the API and require explicit deletion confirmation."""

import json

import httpx
import pytest

from neutronsync_client import ApiClient


@pytest.mark.asyncio
async def test_trash_calls_quote_pair_and_send_confirmation():
    seen = []

    def handler(request):
        seen.append(request)
        return httpx.Response(200, json=[] if request.method == "GET" else {"ok": True})

    api = ApiClient("http://test", transport=httpx.MockTransport(handler))
    try:
        assert await api.trash() == []
        await api.trash_restore("My folder/#", "entry")
        await api.trash_delete("My folder/#", "entry")
        await api.trash_delete("My folder/#", "entry", confirm=True)
    finally:
        await api.aclose()
    assert seen[0].url.path == "/api/v1/trash"
    assert seen[1].url.raw_path == b"/api/v1/trash/My%20folder%2F%23/entry/restore"
    assert json.loads(seen[2].content) == {"confirm": False}
    assert json.loads(seen[3].content) == {"confirm": True}
