"""Server-sent events from ``GET /api/v1/events``.

Yields a parsed :class:`~neutronsync_client.api.Envelope` on ``event: state``
and a keepalive notice on SSE comment lines (``: keepalive``). This is the
stream reader only. Reconnect status for the web interface stays in that package.
"""

from __future__ import annotations

import asyncio
import json
import logging
from collections.abc import AsyncIterator
from dataclasses import dataclass

import httpx
from httpx_sse._decoders import SSEDecoder

from neutronsync_client.api import ApiClient, ApiError, Envelope, _api_error

log = logging.getLogger("neutronsync_client.events")

_DEFAULT_BACKOFF = (1.0, 2.0, 5.0, 10.0)


@dataclass(frozen=True)
class StreamEvent:
    """One signal from the event stream.

    ``kind`` is ``open`` after the response is accepted as ``text/event-stream``,
    ``keepalive`` for an SSE comment, ``line`` for a field or blank line that
    did not finish an event, ``state`` when ``event: state`` carried an
    :class:`~neutronsync_client.api.Envelope`, and ``event`` for any other
    finished SSE event.
    """

    kind: str
    envelope: Envelope | None = None
    event: str = ""
    data: str = ""


class EventSource:
    """Read ``GET /api/v1/events`` on the same TCP port or Unix socket as ``client``."""

    def __init__(self, client: ApiClient) -> None:
        self._api = client

    def __aiter__(self) -> AsyncIterator[StreamEvent]:
        return self._iterate()

    async def follow(
        self,
        *,
        reconnect: bool = False,
        backoff: tuple[float, ...] = _DEFAULT_BACKOFF,
    ) -> AsyncIterator[Envelope]:
        """Yield state envelopes.

        With ``reconnect=False`` the iterator stops when the stream ends, and
        raises if the connection fails. With ``reconnect=True`` it waits and
        connects again. Delays come from ``backoff`` (seconds), reusing the
        last value. This loop does not track UI connection status.
        """
        if not backoff:
            backoff = _DEFAULT_BACKOFF
        attempt = 0
        while True:
            try:
                async for item in self:
                    if item.kind == "state" and item.envelope is not None:
                        attempt = 0
                        yield item.envelope
            except asyncio.CancelledError:
                raise
            except Exception:
                if not reconnect:
                    raise
                await asyncio.sleep(backoff[min(attempt, len(backoff) - 1)])
                attempt += 1
                continue
            if not reconnect:
                return
            await asyncio.sleep(backoff[min(attempt, len(backoff) - 1)])
            attempt += 1

    async def _iterate(self) -> AsyncIterator[StreamEvent]:
        http = self._api._open_events_client()
        try:
            headers = dict(http.headers)
            headers["Accept"] = "text/event-stream"
            headers["Cache-Control"] = "no-store"
            async with http.stream(
                "GET",
                "/api/v1/events",
                headers=headers,
                timeout=httpx.Timeout(None, connect=10.0),
            ) as response:
                if response.status_code >= 400:
                    await response.aread()
                    raise _api_error(response)
                ctype = response.headers.get("content-type", "")
                if "text/event-stream" not in ctype.partition(";")[0]:
                    raise ApiError(
                        "bad_response",
                        f"Expected text/event-stream, got {ctype!r}",
                        status=response.status_code,
                    )
                yield StreamEvent(kind="open")
                decoder = SSEDecoder()
                async for line in response.aiter_lines():
                    raw = line.rstrip("\n")
                    if raw.startswith(":"):
                        decoder.decode(raw)
                        yield StreamEvent(kind="keepalive", data=raw)
                        continue
                    event = decoder.decode(raw)
                    if event is None:
                        yield StreamEvent(kind="line")
                        continue
                    if event.event == "state" and event.data:
                        try:
                            envelope = Envelope.from_dict(json.loads(event.data))
                        except Exception:
                            log.exception("bad state event")
                            yield StreamEvent(kind="line")
                            continue
                        yield StreamEvent(
                            kind="state",
                            envelope=envelope,
                            event="state",
                            data=event.data,
                        )
                        continue
                    yield StreamEvent(kind="event", event=event.event, data=event.data)
        finally:
            await http.aclose()
