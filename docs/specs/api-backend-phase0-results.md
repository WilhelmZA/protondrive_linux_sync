# NeutronSync API backend: Phase 0 results

Build runtime recorded by the worker: `bun --version` prints `1.4.0`.

Owner: maintainer. Measurement date: _____. Account or test-account label: _____. Documents pair path: _____. My-files SDK tree-event scope ID: _____. Phase 1 decision (go / no-go): _____. Reason: _____.

Run `bun install --frozen-lockfile` in `sidecar/`, then run `sidecar/build.sh`. Run `scripts/spike.sh --path /my-files/Documents` from the repository root. The script prompts for credentials through the terminal, performs both walks in the same sidecar, resolves the scope from the SDK, measures a matching web-edit event, and observes the session for 24 hours. It checks `auth.status` every five minutes. Copy its table rows below. `--duration SECONDS` supports a shorter rehearsal; record that duration and do not count it as the 24-hour check.

Feed latency is an upper bound measured from pressing Enter immediately before saving the web edit to receiving its matching `node_updated` event. Use an existing file and the same file path at the prompt. The three-minute event timeout stops the script if no matching update arrives. A partial walk stops the script and does not count as a successful measurement.

The runtime needs a Linux Secret Service session and `secret-tool` from libsecret. Its keyring item label is `NeutronSync Drive Phase 0 session`, with attributes `application=neutronsync-drive` and `purpose=phase0-session-v1`. Credentials travel through anonymous pipes. The script does not write credentials or results to files.

| Check | Target | Measured | Notes |
| --- | --- | --- | --- |
| Cold walk | under 120 s | | |
| Warm walk | under 30 s | | |
| Feed latency | under 60 s | | |
| Session stability | zero sign-outs over 24 h | | |
| Refresh-race hypothesis | optional, test account only | | |

The refresh-race check remains a manual test-account measurement. The Phase 1 decision belongs to the maintainer after the live readings. These live checks do not gate the offline Phase 0 deliverable.

The sidecar uses JSON-RPC numeric error codes with the typed error in `error.data.code`. Rate-limited errors include `error.data.retry_after` in seconds. `events.subscribe` accepts `/my-files` as a scope-discovery alias and reports the SDK scope ID in `events.batch.scope_id`. Other scope IDs pass directly to the SDK. No event acknowledgement method exists in Phase 0.
