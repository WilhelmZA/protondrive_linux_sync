"""Typed wrapper around the NeutronSync HTTP API.

TCP and Unix domain sockets both go through httpx. On a socket the base URL
is only the Host header (``http://localhost`` is fine). Pass ``transport`` in
tests; it replaces the socket transport.
"""

from __future__ import annotations

from dataclasses import dataclass, field, fields
from typing import Any

import httpx


class ApiError(Exception):
    def __init__(self, code: str, message: str, status: int = 0) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status


def _api_error(response: httpx.Response) -> ApiError:
    try:
        body = response.json()
        err = body.get("error") or {}
        code = str(err.get("code") or "error")
        message = str(err.get("message") or response.text or response.reason_phrase)
    except Exception:
        code = "error"
        message = response.text or response.reason_phrase
    return ApiError(code, message, status=response.status_code)


def _from_dict(cls: type, data: dict[str, Any] | None) -> Any:
    if data is None:
        return None
    names = {f.name for f in fields(cls)}
    return cls(**{k: v for k, v in data.items() if k in names})


@dataclass
class Progress:
    done: int = 0
    total: int = 0


@dataclass
class ActivityOp:
    action: str = ""
    path: str = ""
    pair: str = ""
    ok: bool = True
    error: str | None = None


@dataclass
class ActivityItem:
    ts: int = 0
    kind: str = "Info"
    text: str = ""
    op: ActivityOp | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ActivityItem:
        op_data = data.get("op")
        op = _from_dict(ActivityOp, op_data) if isinstance(op_data, dict) else None
        return cls(
            ts=int(data.get("ts") or 0),
            kind=str(data.get("kind") or "Info"),
            text=str(data.get("text") or ""),
            op=op,
        )


@dataclass
class PairState:
    name: str = ""
    local: str = ""
    remote: str = ""
    phase: str = "Idle"
    progress: Progress = field(default_factory=Progress)
    scanned_folders: int = 0
    current_op: str | None = None
    last_error: str | None = None
    last_synced: int | None = None
    tracked: int = 0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PairState:
        prog = data.get("progress") or {}
        return cls(
            name=str(data.get("name") or ""),
            local=str(data.get("local") or ""),
            remote=str(data.get("remote") or ""),
            phase=str(data.get("phase") or "Idle"),
            progress=Progress(
                done=int(prog.get("done") or 0),
                total=int(prog.get("total") or 0),
            ),
            scanned_folders=int(data.get("scanned_folders") or 0),
            current_op=data.get("current_op"),
            last_error=data.get("last_error"),
            last_synced=data.get("last_synced"),
            tracked=int(data.get("tracked") or 0),
        )


@dataclass
class AccountInfo:
    checked: bool = False
    backend_ready: bool = False
    signed_in: bool = False
    keyring_locked: bool = False
    account: str = ""
    version: str = ""
    checking: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any] | None) -> AccountInfo:
        if not data:
            return cls()
        return cls(
            checked=bool(data.get("checked")),
            backend_ready=bool(data.get("backend_ready")),
            signed_in=bool(data.get("signed_in")),
            keyring_locked=bool(data.get("keyring_locked")),
            account=str(data.get("account") or ""),
            version=str(data.get("version") or ""),
            checking=bool(data.get("checking")),
        )


@dataclass
class AppState:
    pairs: list[PairState] = field(default_factory=list)
    account: AccountInfo = field(default_factory=AccountInfo)
    activity: list[ActivityItem] = field(default_factory=list)
    active: list[ActivityOp] = field(default_factory=list)
    busy: bool = False
    watching: bool = False
    signed_out: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any] | None) -> AppState:
        if not data:
            return cls()
        return cls(
            pairs=[PairState.from_dict(p) for p in data.get("pairs") or []],
            account=AccountInfo.from_dict(data.get("account")),
            activity=[ActivityItem.from_dict(a) for a in data.get("activity") or []],
            active=[
                _from_dict(ActivityOp, a) or ActivityOp()
                for a in data.get("active") or []
                if isinstance(a, dict)
            ],
            busy=bool(data.get("busy")),
            watching=bool(data.get("watching")),
            signed_out=bool(data.get("signed_out")),
        )


@dataclass
class HeldItem:
    pair: str = ""
    local: int = 0
    remote: int = 0
    tracked: int = 0
    since: int = 0
    updated: int = 0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> HeldItem:
        return _from_dict(cls, data)


@dataclass
class ConflictItem:
    id: int = 0
    pair: str = ""
    path: str = ""
    outcome: str = ""
    copy: str | None = None
    first_ts: int = 0
    last_ts: int = 0
    seen: int = 0
    dismissed: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ConflictItem:
        return _from_dict(cls, data)


@dataclass
class Envelope:
    rev: int = 0
    state: AppState = field(default_factory=AppState)
    held: list[HeldItem] = field(default_factory=list)
    conflicts: list[ConflictItem] = field(default_factory=list)
    auto_sync: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Envelope:
        return cls(
            rev=int(data.get("rev") or 0),
            state=AppState.from_dict(data.get("state")),
            held=[HeldItem.from_dict(h) for h in data.get("held") or []],
            conflicts=[ConflictItem.from_dict(c) for c in data.get("conflicts") or []],
            auto_sync=bool(data.get("auto_sync")),
        )


@dataclass
class PairConfig:
    name: str = ""
    local: str = ""
    remote: str = ""
    auto: bool = True
    exclude: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PairConfig:
        return cls(
            name=str(data.get("name") or ""),
            local=str(data.get("local") or ""),
            remote=str(data.get("remote") or ""),
            auto=bool(data.get("auto", True)),
            exclude=list(data.get("exclude") or []),
        )


@dataclass
class EngineSettings:
    propagate_deletes: bool = True
    max_delete_files: int = 200
    max_delete_percent: int = 30
    local_delete: str = "trash"
    conflict: str = "keep-both"
    compare: str = "size+mtime"
    scan_threads: int = 0
    download_threads: int = 0
    full_walk_interval: int = 86400
    scan_interval: int = 120
    debounce: int = 2
    upload_limit_kbps: int = 0
    download_limit_kbps: int = 0
    auto_sync: bool = False
    remote_root: str = "/my-files"

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> EngineSettings:
        known = {f.name for f in fields(cls)}
        kwargs = {k: v for k, v in data.items() if k in known}
        return cls(**kwargs)

    def to_dict(self) -> dict[str, Any]:
        return {f.name: getattr(self, f.name) for f in fields(self)}


@dataclass
class VersionInfo:
    api_version: int = 0
    version: str = ""
    build: str = ""


@dataclass
class SignInStep:
    step: str = ""
    url: str | None = None
    token: str | None = None
    methods: list[str] = field(default_factory=list)


class ApiClient:
    """HTTP client for ``neutronsync serve``.

    TCP::

        ApiClient("http://127.0.0.1:8090", token="...")

    Unix socket (base URL is the Host header; the bytes go to ``socket_path``)::

        ApiClient("http://localhost", socket_path="/run/user/1000/neutronsync/api.sock")
    """

    def __init__(
        self,
        base_url: str,
        token: str | None = None,
        *,
        socket_path: str | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        headers: dict[str, str] = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        self.socket_path = socket_path or None
        self._base_url = base_url.rstrip("/")
        self._headers = headers
        # An explicit transport is for tests and stands in for either TCP or a socket.
        self._explicit_transport = transport
        if transport is None and self.socket_path:
            transport = httpx.AsyncHTTPTransport(uds=self.socket_path)
        self._client = httpx.AsyncClient(
            base_url=self._base_url,
            headers=headers,
            timeout=httpx.Timeout(10.0, connect=5.0),
            transport=transport,
        )
        self._sign_in_timeout = httpx.Timeout(60.0, connect=10.0)

    def _open_events_client(self) -> httpx.AsyncClient:
        """A second HTTP client for the long-lived event stream.

        Sharing one client with ordinary requests can stall the SSE read.
        Mock transports are shared so unit tests see both calls. A unix
        socket gets its own transport so closing the stream does not close
        the request connection pool.
        """
        kwargs: dict[str, Any] = {
            "base_url": self._base_url,
            "headers": dict(self._headers),
            "timeout": httpx.Timeout(None, connect=10.0),
        }
        transport = self._events_transport()
        if transport is not None:
            kwargs["transport"] = transport
        return httpx.AsyncClient(**kwargs)

    def _events_transport(self) -> httpx.AsyncBaseTransport | None:
        if isinstance(self._explicit_transport, httpx.MockTransport):
            return self._explicit_transport
        if self._explicit_transport is not None:
            return None
        if self.socket_path:
            return httpx.AsyncHTTPTransport(uds=self.socket_path)
        return None

    async def aclose(self) -> None:
        await self._client.aclose()

    async def _request(
        self,
        method: str,
        path: str,
        *,
        json: dict[str, Any] | None = None,
        params: dict[str, Any] | None = None,
        timeout: httpx.Timeout | None = None,
    ) -> Any:
        try:
            response = await self._client.request(
                method,
                path,
                json=json,
                params=params,
                timeout=timeout,
            )
        except httpx.HTTPError as exc:
            raise ApiError("unreachable", f"Cannot reach the NeutronSync engine: {exc}") from exc

        if response.status_code >= 400:
            raise _api_error(response)

        if response.status_code == 204 or not response.content:
            return None
        return response.json()

    async def health(self) -> bool:
        data = await self._request("GET", "/api/v1/health")
        return bool(data and data.get("ok"))

    async def version(self) -> VersionInfo:
        data = await self._request("GET", "/api/v1/version")
        return VersionInfo(
            api_version=int(data.get("api_version") or 0),
            version=str(data.get("version") or ""),
            build=str(data.get("build") or ""),
        )

    async def state(self, activity: int = 100) -> Envelope:
        data = await self._request("GET", "/api/v1/state", params={"activity": activity})
        return Envelope.from_dict(data)

    async def sync(self, pairs: list[str] | None = None, *, dry_run: bool = False) -> None:
        body: dict[str, Any] = {"dry_run": dry_run}
        if pairs is not None:
            body["pairs"] = pairs
        await self._request("POST", "/api/v1/sync", json=body)

    async def cancel(self) -> None:
        await self._request("POST", "/api/v1/cancel")

    async def set_auto_sync(self, enabled: bool) -> bool:
        data = await self._request("POST", "/api/v1/auto-sync", json={"enabled": enabled})
        return bool(data.get("auto_sync"))

    async def held(self) -> list[HeldItem]:
        data = await self._request("GET", "/api/v1/held")
        return [HeldItem.from_dict(h) for h in data or []]

    async def held_restore(self, pair: str) -> None:
        await self._request("POST", f"/api/v1/held/{pair}/restore")

    async def held_delete(self, pair: str) -> None:
        await self._request("POST", f"/api/v1/held/{pair}/delete", json={"confirm": True})

    async def conflicts(self) -> list[ConflictItem]:
        data = await self._request("GET", "/api/v1/conflicts")
        return [ConflictItem.from_dict(c) for c in data or []]

    async def conflict_dismiss(self, conflict_id: int) -> None:
        await self._request("POST", f"/api/v1/conflicts/{conflict_id}/dismiss")

    async def conflicts_dismiss_all(self) -> None:
        await self._request("POST", "/api/v1/conflicts/dismiss-all")

    async def get_settings(self) -> EngineSettings:
        data = await self._request("GET", "/api/v1/settings")
        return EngineSettings.from_dict(data)

    async def put_settings(self, patch: dict[str, Any]) -> EngineSettings:
        data = await self._request("PUT", "/api/v1/settings", json=patch)
        return EngineSettings.from_dict(data)

    async def list_pairs(self) -> list[PairConfig]:
        data = await self._request("GET", "/api/v1/pairs")
        return [PairConfig.from_dict(p) for p in data or []]

    async def create_pair(self, body: dict[str, Any]) -> list[PairConfig]:
        data = await self._request("POST", "/api/v1/pairs", json=body)
        return [PairConfig.from_dict(p) for p in data or []]

    async def update_pair(self, name: str, body: dict[str, Any]) -> list[PairConfig]:
        data = await self._request("PUT", f"/api/v1/pairs/{name}", json=body)
        return [PairConfig.from_dict(p) for p in data or []]

    async def delete_pair(self, name: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/api/v1/pairs/{name}")

    async def account(self) -> AccountInfo:
        data = await self._request("GET", "/api/v1/account")
        return AccountInfo.from_dict(data)

    async def login(
        self,
        username: str,
        password: str,
        *,
        hv_token: str | None = None,
        hv_type: str | None = None,
    ) -> SignInStep:
        body: dict[str, Any] = {"username": username, "password": password}
        if hv_token is not None:
            body["hv_token"] = hv_token
        if hv_type is not None:
            body["hv_type"] = hv_type
        data = await self._request(
            "POST",
            "/api/v1/account/login",
            json=body,
            timeout=self._sign_in_timeout,
        )
        return SignInStep(
            step=str(data.get("step") or ""),
            url=data.get("url"),
            token=data.get("token"),
            methods=list(data.get("methods") or []),
        )

    async def submit_2fa(self, code: str) -> SignInStep:
        data = await self._request(
            "POST",
            "/api/v1/account/2fa",
            json={"code": code},
            timeout=self._sign_in_timeout,
        )
        return SignInStep(step=str(data.get("step") or ""))

    async def submit_mailbox_password(self, password: str) -> SignInStep:
        data = await self._request(
            "POST",
            "/api/v1/account/mailbox-password",
            json={"password": password},
            timeout=self._sign_in_timeout,
        )
        return SignInStep(step=str(data.get("step") or ""))

    async def logout(self) -> None:
        await self._request("POST", "/api/v1/account/logout")

    async def remote_list(self, path: str | None = None) -> list[dict[str, Any]]:
        params = {"path": path} if path else None
        return await self._request("GET", "/api/v1/remote/list", params=params) or []

    async def local_list(self, path: str | None = None) -> list[dict[str, Any]]:
        params = {"path": path} if path else None
        return await self._request("GET", "/api/v1/local/list", params=params) or []

    async def logs(self, lines: int = 200) -> list[str]:
        data = await self._request("GET", "/api/v1/logs", params={"lines": lines})
        return list(data.get("lines") or [])

    async def reset(self, confirm: bool) -> None:
        """POST ``/api/v1/reset``. The server accepts the call only when ``confirm`` is true."""
        await self._request("POST", "/api/v1/reset", json={"confirm": confirm})
