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

## Phase 2

Measured on 2026-10-06 with a personal test account. The isolated config is a scratch config. Its only pair uses a newly created local scratch folder and `/my-files/NeutronSync-Phase2-Test`. State and logs stay under the scratch directory. Every sync uses `--config` and runs with `dry_run: false`.

- Local creation uploads two files and creates one nested folder: three operations, zero errors, 8.236 seconds.
- Local editing creates a new revision on the same node UID: one operation, zero errors, 5.757 seconds.
- Local rename sends `node.rename` on the original UID: one operation, zero errors, 4.156 seconds. Local move sends `node.move` on that UID: one operation, zero errors, 5.247 seconds. Captured RPCs prove neither operation trashes or re-uploads the file.
- Local deletion sends `node.trash`: one operation, zero errors, 4.427 seconds.
- Direct sidecar upload, rename and trash propagate locally: one operation each, zero errors, respectively 4.771, 3.373 and 3.417 seconds. The desktop trash contains the deleted file and its `.trashinfo` receipt. Deliberately incorrect upload `size` and `mtime` values are overridden by file metadata.
- A simultaneous local and remote edit keeps both contents: one conflict operation, zero errors, 7.518 seconds.
- All nine second syncs plan zero operations and apply zero changes. Each finishes in 3.133–3.703 seconds. All 18 syncs report zero errors.
- Cleanup sends `node.trash` for the test folder. A subsequent resolve returns `not_found`. An independent read-only query finds its exact UID in Proton trash on the fourth page, recorded in `live-run-02/trash-confirmation.json`.

The complete successful wire capture contains 20 write requests. Its UID-to-path audit asserts that every write target stays inside the test folder. These are the remote paths written, including rename and move destinations:

- `/my-files/NeutronSync-Phase2-Test`
- `/my-files/NeutronSync-Phase2-Test/nested`
- `/my-files/NeutronSync-Phase2-Test/a.txt`
- `/my-files/NeutronSync-Phase2-Test/nested/b.txt`
- `/my-files/NeutronSync-Phase2-Test/renamed.txt`
- `/my-files/NeutronSync-Phase2-Test/nested/renamed.txt`
- `/my-files/NeutronSync-Phase2-Test/remote.txt`
- `/my-files/NeutronSync-Phase2-Test/remote-renamed.txt`
- `/my-files/NeutronSync-Phase2-Test/nested/renamed (conflict 20261006-105043).txt`

`pgrep -af neutronsync-drive` finds the existing read-only stability monitor before and after verification: monitor PID 2778566, sidecar PID 2778568. The monitor continues under the explicit exception in the Phase 2 spec. Neither the live wire capture nor the monitor log contains `auth.signed_out` during these checks. The latest observed monitor authentication check reports `signed_in: true` at 12:46:23 local time.

The recoverable-delete check returns no matches. No DELETE endpoint exception is added; the HTTP allowlist also refuses destructive POST endpoints. Exact command output:

```text
$ grep -rnE 'deleteNodes|emptyTrash|deleteRevision' sidecar/src

exit: 1
$ grep -rnE 'deleteNodes|emptyTrash|deleteRevision' src || echo "no destructive SDK calls"
no destructive SDK calls

exit: 0
```

Evidence lives in `Plans/neutronsync-api-backend-phase-2/evidence/`. `live-run-02/` holds all successful sync outputs, RPCs, write-target assertions, revision identity assertions and trash confirmation. The first attempt successfully uploads and trashes its isolated folder, then stops because the evidence runner expects JSON-only stdout. Its original outputs remain in `evidence/live-*`. The corrected runner parses the JSON after console messages and completes the full sequence.

Automated checks pass: `cargo fmt --check`, `cargo clippy --all-targets --features gui`, `cargo build --release --features gui`, `cargo test`, and `bun test`. Rust reports 83 passing tests and one pre-existing ignored live helper. Bun reports 29 passing tests. The sidecar typecheck passes. Clippy reports existing warnings, with none in new code. The worker records a separate full-tree local-scanner scope question in `STATE.md`; these results do not assert that existing engine behavior has changed.

## Phase 3

Measured on 2026-10-06 with a personal test account. The final isolated config is a scratch config. Its single pair uses a new local scratch folder and `/my-files/NeutronSync-Phase3-Test`. State and logs stay under that scratch directory. The configured CLI binary deliberately does not exist.

- Startup subscribes before one `first_start` full walk. The initial cursor is stored after 3.011 seconds.
- Remote create reaches the local folder in 12.064 seconds; edit in 9.888 seconds; rename in 7.453 seconds; move in 6.388 seconds; trash in 2.117 seconds. Each operation uses a separate sidecar call. Every measurement includes the write call and local observation. None triggers a full walk.
- A local file uploads through inotify in 5.557 seconds, with no full walk.
- After stopping the daemon, a remote upload arrives 4.804 seconds after restart. The second subscribe carries the stored `since_event_id`, `<event-id>`. Restart performs no full walk.
- The entire run records exactly one full walk, with cause `first_start`.
- Cleanup moves the test folder to Proton trash. An independent read-only query confirms its exact UID in trash on page four. The UID-to-path write audit proves every write target stays within the test folder.
- The Phase 0 read-only monitor remains running: monitor PID 2778566 and sidecar PID 2778568. The Phase 3 capture and monitor show no `auth.signed_out`. The monitor reports `signed_in: true` at 14:11:23 local time.

Evidence is under `Plans/neutronsync-api-backend-phase-3/evidence/`. The final live evidence is in `live-run-02/`: config, watcher logs, RPC capture, results, latency CSV, full-walk cause tally, 16-write UID-to-path audit and independent trash confirmation. The earlier successful run remains at the evidence root. Automated outputs and monitor observations are at the evidence root. The 16 fake-sidecar watcher tests cover the thirteen required cases, delayed apply, bounded failed-delivery recovery and refused resume cursors. Sidecar tests cover acknowledged replay, history-free acknowledged subscribe, default cursor advancement, resumed delivery and bounded path resolution.

Automated checks pass: formatting, clippy, release build with GUI, 99 Rust tests, 32 Bun tests and the sidecar typecheck. One existing Rust live helper remains ignored. Clippy reports the same existing warning sites as the pre-change baseline. Both required source scans return no matches.
