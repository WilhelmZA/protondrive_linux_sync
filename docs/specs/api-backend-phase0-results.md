# NeutronSync API backend: Phase 0 results

Build runtime recorded by the worker: `bun --version` prints `1.4.0`.

Owner: maintainer. Measurement date: 2026-10-06. Account or test-account label: personal test account. Documents pair path: /my-files/Documents. My-files SDK tree-event scope ID: _____. Phase 1 decision (go / no-go): _____. Reason: _____.

Run `bun install --frozen-lockfile` in `sidecar/`, then run `sidecar/build.sh`. Run `scripts/spike.sh --path /my-files/Documents` from the repository root. The script prompts for credentials through the terminal, performs both walks in the same sidecar, resolves the scope from the SDK, measures a matching web-edit event, and observes the session for 24 hours. It checks `auth.status` every five minutes. Copy its table rows below. `--duration SECONDS` supports a shorter rehearsal; record that duration and do not count it as the 24-hour check.

Feed latency is an upper bound measured from pressing Enter immediately before saving the web edit to receiving its matching `node_updated` event. Use an existing file and the same file path at the prompt. The three-minute event timeout stops the script if no matching update arrives. A partial walk stops the script and does not count as a successful measurement.

The runtime needs a Linux Secret Service session and `secret-tool` from libsecret. Its keyring item label is `NeutronSync Drive Phase 0 session`, with attributes `application=neutronsync-drive` and `purpose=phase0-session-v1`. Credentials travel through anonymous pipes. The script does not write credentials or results to files.

| Check | Target | Measured | Notes |
| --- | --- | --- | --- |
| Cold walk | under 120 s | 25.7 s | 16 concurrent folders, 0 failures. CLI walk of the same tree is about 774 s. |
| Warm walk | under 30 s | 10.7 s | Same sidecar, second walk, 0 failures. |
| Feed latency | under 60 s | 5 s or less | A web upload to a test folder arrived as node_created at 10:47:21 on the first poll after it landed. Poll interval is 5 s, about 200 ms per poll. |
| Session stability | zero sign-outs over 24 h | running | Monitor started 2026-10-06 10:41. |
| Refresh-race hypothesis | optional, test account only | | |

The refresh-race check remains a manual test-account measurement. The Phase 1 decision belongs to the maintainer after the live readings. These live checks do not gate the offline Phase 0 deliverable.

The sidecar uses JSON-RPC numeric error codes with the typed error in `error.data.code`. Rate-limited errors include `error.data.retry_after` in seconds. `events.subscribe` accepts `/my-files` as a scope-discovery alias and reports the SDK scope ID in `events.batch.scope_id`. Other scope IDs pass directly to the SDK. No event acknowledgement method exists in Phase 0.

## Fixes found during live testing

- Login returned HTTP 422 code 9001 (human verification). The sidecar now returns the verify.proton.me link and retries with the solved token. Verified live.
- The HTTP layer threw on non-OK responses. The SDK needs them returned so it can honour 429 retry-after and retry 5xx. Throwing caused "Failed to decrypt node" errors and failed folders.
- `getPublicKeys` hit the API once per node during signature checks. It is now cached per address. This was the source of the rate limiting.
- The walk used fixed chunks of 8 that waited on the slowest folder. It is now a continuous pool of 16. Cold walk went from 112 s to 25.7 s.
