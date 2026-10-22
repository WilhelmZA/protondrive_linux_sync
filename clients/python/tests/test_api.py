"""ApiClient over a mock transport, including unix socket construction."""

from __future__ import annotations

import json
import os

import httpx
import pytest

from neutronsync_client import ApiClient, ApiError, EventSource

SOCKET = "/run/user/1000/neutronsync/api.sock"


def _envelope() -> dict:
    return {
        "rev": 4,
        "extra_top": True,
        "state": {
            "pairs": [],
            "account": {"checked": True, "signed_in": True, "account": "a"},
            "activity": [],
            "active": [],
            "busy": False,
            "watching": True,
            "signed_out": False,
            "future_field": 1,
        },
        "held": [],
        "conflicts": [],
        "auto_sync": False,
    }


def _client(handler, **kwargs) -> ApiClient:
    return ApiClient("http://test", transport=httpx.MockTransport(handler), **kwargs)


async def test_state_ignores_unknown_fields() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "GET"
        assert request.url.path == "/api/v1/state"
        assert request.url.params["activity"] == "100"
        assert request.headers["authorization"] == "Bearer secret"
        return httpx.Response(200, json=_envelope())

    client = ApiClient(
        "http://127.0.0.1:8090",
        token="secret",
        transport=httpx.MockTransport(handler),
    )
    try:
        envelope = await client.state()
    finally:
        await client.aclose()
    assert envelope.rev == 4
    assert envelope.state.watching is True
    assert envelope.state.account.account == "a"


@pytest.mark.parametrize(
    ("status", "code", "message"),
    [
        (401, "unauthorized", "missing token"),
        (409, "conflict", "sync already running"),
        (422, "validation", "confirm must be true"),
    ],
)
async def test_error_mapping(status: int, code: str, message: str) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            status,
            json={"error": {"code": code, "message": message, "detail": "ignored"}},
        )

    client = _client(handler)
    try:
        with pytest.raises(ApiError) as caught:
            await client.state()
    finally:
        await client.aclose()
    err = caught.value
    assert err.status == status
    assert err.code == code
    assert err.message == message


async def test_reset_posts_confirm() -> None:
    seen: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["path"] = request.url.path
        seen["body"] = json.loads(request.content)
        return httpx.Response(204)

    client = _client(handler)
    try:
        await client.reset(True)
    finally:
        await client.aclose()
    assert seen == {"path": "/api/v1/reset", "body": {"confirm": True}}


async def test_unix_transport_receives_socket_path() -> None:
    client = ApiClient("http://localhost", socket_path=SOCKET)
    events = client._open_events_client()
    try:
        assert client.socket_path == SOCKET
        assert client._client._transport._pool._uds == SOCKET
        assert events._transport._pool._uds == SOCKET
        assert events._transport is not client._client._transport
    finally:
        await events.aclose()
        await client.aclose()


async def test_explicit_transport_replaces_socket() -> None:
    transport = httpx.MockTransport(lambda request: httpx.Response(200, json={"ok": True}))
    client = ApiClient("http://localhost", socket_path=SOCKET, transport=transport)
    try:
        assert client._client._transport is transport
        assert client._events_transport() is transport
    finally:
        await client.aclose()


async def test_tcp_transport_has_no_socket() -> None:
    client = ApiClient("http://127.0.0.1:8090", token="t")
    try:
        assert client.socket_path is None
        assert client._client._transport._pool._uds is None
        assert client._client.headers["authorization"] == "Bearer t"
    finally:
        await client.aclose()


async def test_event_source_yields_state_and_keepalive() -> None:
    payload = (
        f"event: state\ndata: {json.dumps(_envelope())}\n\n"
        ": keepalive\n\n"
        "event: other\ndata: hi\n\n"
    )

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/api/v1/events"
        assert "text/event-stream" in request.headers["accept"]
        return httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=payload.encode(),
        )

    client = _client(handler)
    kinds: list[str] = []
    revs: list[int] = []
    try:
        async for item in EventSource(client):
            kinds.append(item.kind)
            if item.envelope is not None:
                revs.append(item.envelope.rev)
    finally:
        await client.aclose()
    assert kinds[0] == "open"
    assert "state" in kinds
    assert "keepalive" in kinds
    assert "event" in kinds
    assert revs == [4]


async def test_follow_stops_when_reconnect_is_off() -> None:
    payload = f"event: state\ndata: {json.dumps(_envelope())}\n\n"

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=payload.encode(),
        )

    client = _client(handler)
    try:
        envelopes = [item async for item in EventSource(client).follow()]
    finally:
        await client.aclose()
    assert [item.rev for item in envelopes] == [4]


async def test_unix_socket_skipped_without_server() -> None:
    path = os.environ.get("NEUTRONSYNC_TEST_SOCKET")
    if not path or not os.path.exists(path):
        pytest.skip("NEUTRONSYNC_TEST_SOCKET is not a live socket")
    client = ApiClient(
        "http://localhost",
        token=os.environ.get("NEUTRONSYNC_API_TOKEN"),
        socket_path=path,
    )
    try:
        assert await client.health() is True
    finally:
        await client.aclose()
