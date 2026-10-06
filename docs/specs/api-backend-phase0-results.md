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

## Phase 1

Measured on 2026-10-06 with the built Rust CLI and `sidecar/build.sh` output. Every command uses a scratch config, an empty scratch local folder, remote `/my-files/Documents`, and a separate scratch state directory. The configured CLI binary deliberately does not exist, confirming that API selection bypasses the CLI-binary check.

- Explicit dry run: `sync --dry-run --backend api` completes in **57.586 seconds**. The plan contains **5,483 operations: 750 mkdir-local and 4,733 downloads**. It applies zero changes and reports zero errors.
- Non-dry-run request: `sync --backend api` completes in **28.721 seconds** with the same plan. It logs `read-only backend: forcing a dry run, nothing will change (writes require Phase 2)`. It applies zero changes and reports zero errors.
- The scratch local folder remains empty. SHA-256 snapshots of every scratch state file match before and after both runs, with an empty snapshot diff. The `stats.db` checksum remains `44dcec3b342340cd9fe249324d72dba1c4119577874c98d9d3b449adf0debc8c`; the sentinel file also matches. The DB includes a pre-existing sync timestamp and 1,002 operation rows, so the check detects timestamp writes and history pruning. No state logs or additional files appear.
- `pgrep -af neutronsync-drive` finds the Phase 0 monitor's sidecar, PID **2778568**, before the checks. The monitor, PID **2778566**, continues running under the explicit read-only-poller exception in the Phase 1 spec. After the checks, only that original sidecar remains. Neither process reports `auth.signed_out`; the monitor still reports `signed_in: true` at 11:31:23 local time.

The equivalence check copies `~/.local/state/neutronsync` to scratch before opening any DB. The API walk reports **zero failed folders**. All baseline paths occur in the API tree. Their types, sizes and mtimes match exactly. Some of these have a SHA-1 from the API where the baseline stores NULL; all other shared metadata matches. These are newly available hashes, not observed content changes.

Every one of the 1,056 API-only paths falls into an evidenced category: **151 match the engine's built-in junk rules**; **904 belong to seven top-level subtrees absent wholesale from the confirmed-sync baseline**; and **one is the web-uploaded test file changed after the last sync**. The 904 untracked paths comprise Apps (444), Other (194), Travel (156), Coding Projects (101), Public (5), Scanned Documents (3), and Screenshots (1). The raw walk includes junk, whereas the sync plan filters it. A confirmed-sync baseline records only tracked state, so it cannot establish the upload dates or historical exclusion settings of those seven omitted subtrees. This comparison establishes equivalence for its recorded subset, not equality of the two full live backends.

A live CLI-versus-API comparison is not possible while the CLI session is signed out. The copied `status.json` confirms `account.signed_in: false`; the CLI session remains untouched. Full outputs, checksums, command timings, per-path differences and classifications are under `Plans/neutronsync-api-backend-phase-1/evidence/`. `live_checks.py` reproduces the measurements with scratch-only configuration; `explain_equivalence.py` classifies every recorded difference. The first combined runner reaches its external 120-second timeout during comparison; resuming against the same scratch state completes the comparison, with the final detailed comparison taking 56.181 seconds.
