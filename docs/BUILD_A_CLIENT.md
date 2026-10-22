# Build your own front end

The engine runs once: `neutronsync serve` in a container or on a headless host, or the desktop tray. That process exposes `/api/v1`. Your GUI is a client of that API. [`HTTP_API.md`](HTTP_API.md) is the endpoint contract (methods, bodies, status codes, error shape). This page is how to connect and what a client may rely on.

## How to connect

### Desktop: Unix socket

The tray listens on `$XDG_RUNTIME_DIR/neutronsync/api.sock`. The socket is mode `0600` and belongs to the user who started the tray. Only that user can connect. That permission is the authentication: a request on the socket does not need a bearer token.

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/neutronsync/api.sock" http://x/api/v1/state
```

`x` is the `Host` curl sends. The socket accepts it. (On TCP loopback the host check is stricter; see [`HTTP_API.md`](HTTP_API.md).)

### TCP

```sh
neutronsync serve --listen 127.0.0.1:8090
```

The desktop can open the same listener with `api_listen` under `[cli]` in the config file. When a token is configured, send `Authorization: Bearer <token>`. The token comes from `NEUTRONSYNC_API_TOKEN`, or from `--token-file` on `serve` / `token_file` in the config (surrounding whitespace ignored). A token shorter than 32 characters is refused at startup. Listening on an address that is not loopback requires a token, and that token must be at least 32 characters; otherwise the server refuses to start.

`GET /api/v1/health` does not need the token. Every other path does, when a token is set. A missing or wrong token is `401` with no detail.

### Browsers

A request that carries an `Origin` header, or a `Sec-Fetch-Site` header other than `none` or `same-origin`, is refused with `403`. There are no CORS headers. Clients are server-side or native. A script in a page the user happens to open cannot drive the engine.

## The state envelope

`GET /api/v1/state` and every SSE `state` event carry the same object:

```json
{"rev": 42, "state": {}, "held": [], "conflicts": [], "auto_sync": false}
```

`rev` is an integer shared by the poll and the stream. It grows by one when the engine's state, the held deletions, the conflicts, or `auto_sync` change. Equal `rev` means equal content. The only difference between two envelopes with the same `rev` is how many activity lines they include: the stream keeps the newest 100, and `GET /api/v1/state` keeps the number its `activity` query asks for (default 100, at most 500).

`state` holds the live pairs, account, activity, and flags (`busy`, `watching`, `signed_out`). `held` and `conflicts` are the lists a UI shows for deletions held back and settled conflicts. Field-by-field meanings are in [`HTTP_API.md`](HTTP_API.md).

v1 may grow optional fields. Clients must ignore fields they do not know. A request body is the opposite: an unknown field is `400`.

## The event stream

`GET /api/v1/events` is `text/event-stream`. The first event is the current envelope. Later events are updates. With nothing to report, the server sends a `: keepalive` comment every 15 seconds. The stream ends when the server shuts down or the connection drops. Reconnect; the first event on the new stream is the current envelope again.

```
event: state
data: {"rev":43,"state":{},"held":[],"conflicts":[],"auto_sync":false}

: keepalive
```

Work that starts a sync returns `202 {"accepted":true}`. The envelope that follows on the stream (or a later `GET /api/v1/state`) is where you see the run.

## Sign-in

Sign-in is a short conversation. Each call returns `200` and a `step`:

| `step` | What the client does next |
|---|---|
| `done` | Signed in. |
| `need_2fa` | `POST /api/v1/account/2fa` with `{"code":"..."}`. |
| `need_mailbox_password` | `POST /api/v1/account/mailbox-password` with `{"password":"..."}`. |
| `need_human_verification` | The body also has `url`, `token`, and `methods`. Open `url` in a browser, complete the check, then `POST /api/v1/account/login` again with the same username and password plus `hv_token` (the `token`) and `hv_type` (one of `methods`). |

Start with `POST /api/v1/account/login` and `{"username","password"}`. A step Proton refuses is `401` with `sign_in_failed`. Passwords and codes are never written to a log line or an error message. After every step the account is checked again, so `GET /api/v1/account` shows whether the session is signed in.

## Minimal examples

### Python

The package is `clients/python` (`neutronsync-client`). Install it from this repo; it is not on PyPI. Connect, read state, follow events, start a sync:

```python
import asyncio
from neutronsync_client import ApiClient, EventSource

async def main() -> None:
    # Desktop socket. TCP: ApiClient("http://127.0.0.1:8090", token="...")
    api = ApiClient("http://localhost", socket_path="/run/user/1000/neutronsync/api.sock")
    env = await api.state()
    print(env.rev, env.state.busy)
    async for env in EventSource(api).follow():
        print(env.rev, len(env.conflicts))
        if not env.state.busy:
            await api.sync()  # 202; the next event shows the run
            break
    await api.aclose()

asyncio.run(main())
```

### TypeScript

The package is `clients/typescript` (`@neutronsync/client`). Install it from this repo (`"@neutronsync/client": "file:clients/typescript"`); it is not on npm. Connect, read state, follow events, start a sync:

```typescript
import { createClient, followEvents } from "@neutronsync/client";

const api = createClient({
  baseUrl: "http://localhost",
  socketPath: "/run/user/1000/neutronsync/api.sock",
});
const env = await api.state();
console.log(env.rev, env.state.busy);
for await (const env of followEvents(api)) {
  console.log(env.rev, env.conflicts.length);
  if (!env.state.busy) {
    await api.sync();
    break;
  }
}
```

TCP is `createClient({ baseUrl: "http://127.0.0.1:8090", token })`. Omit `token` when the server has none. A unix socket sets `socketPath` and uses `baseUrl` only as the Host header (`http://localhost` or `http://x`).

## Versioning

These are the stability rules for the HTTP API.

- `/api/v1` guarantees: no field or endpoint is removed, and none changes its meaning. New optional response fields and new endpoints may be added.
- Deprecations are announced in [`HTTP_API.md`](HTTP_API.md) and `CHANGELOG.md` and keep working until the next major API version.
- A breaking change means `/api/v2` is served alongside `/api/v1` for at least one release.
- Clients must ignore unknown fields in responses. Request bodies still refuse unknown fields with `400`.
- OpenAPI 3.1 lives at `docs/openapi.json` and at `GET /api/v1/openapi.json`.

## Affiliation

The engine talks to Proton Drive through an unofficial, independent helper built on Proton's Drive SDK. NeutronSync is not affiliated with Proton AG. A client of this API inherits that: it is not a Proton product, and Proton does not support it.

## What a third-party client must not assume

- Remote control of a desktop that is not this machine. The socket is owner-only on this host. TCP is loopback unless you deliberately listen wider, and wider listen still needs the token rules above.
- More than one user. There is no multi-user session on one engine.
- More than one engine on one state directory. One NeutronSync owns a state directory. `neutronsync serve` and the desktop tray refuse to run together on that directory; the second process exits and tells you to stop the first.
