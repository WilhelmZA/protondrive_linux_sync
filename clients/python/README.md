# neutronsync-client

Python client for the NeutronSync HTTP API (`neutronsync serve`). It speaks TCP and Unix domain sockets. Install it from this repo with uv (`neutronsync-client = { path = "clients/python", editable = true }`). It is not published to PyPI.

NeutronSync reaches Proton Drive through an unofficial, independent helper built on Proton's Drive SDK. It is not affiliated with Proton AG. Clients of this package talk to that engine, so they inherit the same status.

```python
import asyncio
from neutronsync_client import ApiClient, EventSource


async def main() -> None:
    # Unix socket. The base URL is only the Host header.
    client = ApiClient(
        "http://localhost",
        socket_path="/run/user/1000/neutronsync/api.sock",
    )
    state = await client.state()
    print(state.rev, len(state.state.pairs))
    async for envelope in EventSource(client).follow():
        print("rev", envelope.rev, "busy", envelope.state.busy)
        break
    await client.sync()
    await client.aclose()

    # TCP with a bearer token.
    tcp = ApiClient("http://127.0.0.1:8090", token="replace-me")
    print(await tcp.health())
    await tcp.aclose()


asyncio.run(main())
```

`EventSource` yields an envelope for each `event: state` line and notices SSE comments (`: keepalive`). `follow(reconnect=True)` is a plain reconnect loop. Pass `transport=` to `ApiClient` in tests; that replaces the socket transport.

`examples/sync_once.py` reads state and starts one sync. Point it at a server you already started:

```sh
uv run python examples/sync_once.py --socket /run/user/1000/neutronsync/api.sock
uv run python examples/sync_once.py --base-url http://127.0.0.1:8090 --token "$NEUTRONSYNC_API_TOKEN"
```
