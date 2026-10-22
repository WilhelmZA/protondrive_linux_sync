"""Background event stream and shared live state."""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any

from neutronsync_client import EventSource

from neutronsync_web import SUPPORTED_API_VERSIONS, __version__
from neutronsync_web.api import ApiClient, ApiError, Envelope, VersionInfo

log = logging.getLogger("neutronsync_web.live")

BACKOFF = (1.0, 2.0, 5.0, 10.0)
FRAME_TIMEOUT = 20.0
REFRESH_MIN_INTERVAL = 0.25
HEALTH_INTERVAL = 15.0


class ConnStatus(StrEnum):
    CONNECTING = "connecting"
    OK = "ok"
    LOST = "lost"
    UNSUPPORTED = "unsupported"
    UNREACHABLE = "unreachable"


@dataclass
class LiveState:
    status: ConnStatus = ConnStatus.CONNECTING
    envelope: Envelope | None = None
    version: VersionInfo | None = None
    api_healthy: bool = False
    last_frame_at: float = 0.0
    last_error: str = ""
    unsupported_message: str = ""
    unreachable_url: str = ""
    _listeners: list[Callable[[], Any]] = field(default_factory=list, repr=False)
    _last_notify: float = 0.0
    _last_rev: int = -1

    def register(self, callback: Callable[[], Any]) -> None:
        if callback not in self._listeners:
            self._listeners.append(callback)

    def unregister(self, callback: Callable[[], Any]) -> None:
        if callback in self._listeners:
            self._listeners.remove(callback)

    def notify(self, *, force: bool = False) -> None:
        now = time.monotonic()
        if not force and (now - self._last_notify) < REFRESH_MIN_INTERVAL:
            return
        self._last_notify = now
        for cb in list(self._listeners):
            try:
                cb()
            except Exception:
                log.exception("live listener failed")


class LiveHub:
    def __init__(self, api: ApiClient, api_url: str) -> None:
        self.api = api
        self.api_url = api_url
        self.state = LiveState(unreachable_url=api_url)
        self._task: asyncio.Task | None = None
        self._health_task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._clock = time.monotonic

    def set_clock(self, clock: Callable[[], float]) -> None:
        """Test hook: replace monotonic clock."""
        self._clock = clock

    async def start(self) -> None:
        self._stop.clear()
        self._task = asyncio.create_task(self._run_events(), name="live-events")
        self._health_task = asyncio.create_task(self._run_health(), name="live-health")

    async def stop(self) -> None:
        self._stop.set()
        for task in (self._task, self._health_task):
            if task:
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        self._task = None
        self._health_task = None

    async def check_version(self) -> bool:
        try:
            version = await self.api.version()
        except ApiError as exc:
            self.state.status = ConnStatus.UNREACHABLE
            self.state.last_error = exc.message
            self.state.api_healthy = False
            self.state.notify(force=True)
            return False
        self.state.version = version
        if version.api_version not in SUPPORTED_API_VERSIONS:
            self.state.status = ConnStatus.UNSUPPORTED
            self.state.unsupported_message = (
                f"This web interface (version {__version__}) does not support "
                f"the NeutronSync engine it is connected to "
                f"(version {version.version or version.api_version}). "
                "Update both to the same release."
            )
            self.state.notify(force=True)
            return False
        return True

    async def _run_health(self) -> None:
        while not self._stop.is_set():
            try:
                ok = await self.api.health()
                self.state.api_healthy = ok
            except Exception:
                self.state.api_healthy = False
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=HEALTH_INTERVAL)
            except TimeoutError:
                pass

    async def _run_events(self) -> None:
        attempt = 0
        while not self._stop.is_set():
            if not await self.check_version():
                if self.state.status == ConnStatus.UNSUPPORTED:
                    await asyncio.sleep(10)
                    continue
                delay = BACKOFF[min(attempt, len(BACKOFF) - 1)]
                attempt += 1
                self.state.status = ConnStatus.UNREACHABLE
                self.state.notify(force=True)
                try:
                    await asyncio.wait_for(self._stop.wait(), timeout=delay)
                except TimeoutError:
                    pass
                continue

            attempt = 0
            try:
                await self._consume_stream()
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                log.info("event stream ended: %s", exc)
                self.state.status = ConnStatus.LOST
                self.state.last_error = str(exc)
                self.state.notify(force=True)

            if self._stop.is_set():
                break
            delay = BACKOFF[min(attempt, len(BACKOFF) - 1)]
            attempt += 1
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=delay)
            except TimeoutError:
                pass

    async def _consume_stream(self) -> None:
        self.state.status = ConnStatus.CONNECTING
        self.state.notify(force=True)
        # EventSource owns the SSE connection, including ": keepalive" comments.
        watchdog: asyncio.Task | None = None
        try:
            async for notice in EventSource(self.api):
                if self._stop.is_set():
                    break
                if notice.kind == "open":
                    self.state.status = ConnStatus.OK
                    self.state.last_frame_at = self._clock()
                    self.state.notify(force=True)
                    watchdog = asyncio.create_task(self._watch_frames())
                    continue
                # Any line (including ": keepalive") proves the stream is alive.
                self.state.last_frame_at = self._clock()
                if self.state.status == ConnStatus.LOST:
                    self.state.status = ConnStatus.OK
                    self.state.notify(force=True)
                if notice.kind == "state" and notice.envelope is not None:
                    envelope = notice.envelope
                    self.state.envelope = envelope
                    self.state.status = ConnStatus.OK
                    self.state.api_healthy = True
                    if envelope.rev != self.state._last_rev:
                        self.state._last_rev = envelope.rev
                        self.state.notify(force=True)
        finally:
            if watchdog is not None:
                watchdog.cancel()
                try:
                    await watchdog
                except asyncio.CancelledError:
                    pass

    async def _watch_frames(self) -> None:
        while not self._stop.is_set():
            await asyncio.sleep(1.0)
            if self.state.status in (ConnStatus.OK, ConnStatus.CONNECTING):
                age = self._clock() - self.state.last_frame_at
                if self.state.last_frame_at and age > FRAME_TIMEOUT:
                    self.state.status = ConnStatus.LOST
                    self.state.notify(force=True)

    def apply_envelope(self, envelope: Envelope) -> None:
        """Test helper: apply an envelope as if it arrived on the stream."""
        self.state.envelope = envelope
        self.state.last_frame_at = self._clock()
        self.state.status = ConnStatus.OK
        if envelope.rev != self.state._last_rev:
            self.state._last_rev = envelope.rev
            self.state.notify(force=True)
