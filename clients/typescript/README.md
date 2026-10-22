# @neutronsync/client

TypeScript client for the NeutronSync HTTP API (`neutronsync serve`). It speaks TCP and Unix domain sockets. Install it from this repo (`"@neutronsync/client": "file:clients/typescript"`). It is not published to npm. The package is ESM TypeScript. Node.js 22.18 or newer runs it directly. Node.js 18 has the socket API this client uses; run it there with a TypeScript loader.

NeutronSync reaches Proton Drive through an unofficial, independent helper built on Proton's Drive SDK. It is not affiliated with Proton AG. Clients of this package talk to that engine, so they inherit the same status.

Pass `socketPath` and the call uses `http.request` with that Unix socket. `baseUrl` is then only the Host header (`http://localhost` or `http://x`). Omit `socketPath` and the call is TCP to `baseUrl`, for example `http://127.0.0.1:8090`. A `token` sends `Authorization: Bearer`. The client does not send `Origin` or `Sec-Fetch-Site`.

```typescript
import { createClient, followEvents } from "@neutronsync/client";

const socket = createClient({
  baseUrl: "http://localhost",
  socketPath: "/run/user/1000/neutronsync/api.sock",
});
const state = await socket.state();
console.log(state.rev, state.state.pairs.length);
for await (const envelope of followEvents(socket)) {
  console.log("rev", envelope.rev, "busy", envelope.state.busy);
  break;
}
await socket.sync();

const tcp = createClient({
  baseUrl: "http://127.0.0.1:8090",
  token: "replace-me",
});
console.log((await tcp.health()).ok);
```

`followEvents` yields an envelope for each `event: state` and skips SSE comments (`: keepalive`). `followEvents(client, { reconnect: true })` connects again after the stream ends. Unknown JSON fields are left on the object and are not rejected.

Named methods cover the v1 paths (`state`, `sync`, `cancel`, `reset`, `autoSync`, held, conflicts, settings, pairs, account and sign-in, `remoteList`, `localList`, `logs`, `health`, `version`, `openapi`). `request(method, path, body?)` is the same call without a helper.

`src/openapi.ts` is generated and committed, so installing the client does not run the generator. When `docs/openapi.json` changes, regenerate from this directory:

```sh
./generate.sh
```

That runs the `openapi-typescript` version pinned in `package.json`. The script writes no timestamps.

`examples/sync-once.ts` reads state, takes one event, and starts one sync. Point it at a server you already started:

```sh
node examples/sync-once.ts --socket "$XDG_RUNTIME_DIR/neutronsync/api.sock"
node examples/sync-once.ts --base-url http://127.0.0.1:8090 --token "$NEUTRONSYNC_API_TOKEN"
```
