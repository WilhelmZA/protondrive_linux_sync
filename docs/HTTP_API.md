# HTTP API (`neutronsync serve`)

`neutronsync serve` runs the sync engine with no desktop and exposes everything the desktop window can do over HTTP. It is built with the `serve` cargo feature (the release packages include it). A web interface or any other client drives NeutronSync entirely through this API.

```sh
neutronsync serve [--listen ADDR:PORT] [--token-file PATH] [--local-root PATH]...
```

- `--listen` defaults to `127.0.0.1:8090`. Any address that is not loopback needs an API token, or the server refuses to start: "Refusing to listen on ADDR without an API token. Set NEUTRONSYNC_API_TOKEN or --token-file." (exit status 2).
- The token comes from `--token-file` (surrounding whitespace ignored), else `NEUTRONSYNC_API_TOKEN`, else there is none. A token shorter than 32 characters is refused (exit status 2).
- `--local-root` names a folder the API may browse and put synced folders under; repeat it for several. Without it, browsing is limited to the home directory and a synced folder may be any absolute path.
- The config is found the same way as for every other command: `--config`, then `NEUTRONSYNC_CONFIG`, then `~/.config/neutronsync/neutronsync.toml`, then `./neutronsync.toml`.
- One NeutronSync owns a state directory. `serve` writes `serve.lock` there and refuses to start (exit status 1) while that lock or the desktop tray's `tray-daemon.lock` names a live process: "Another NeutronSync is already syncing this state (the desktop tray or another serve). Stop it first." Auto sync also takes `watch.lock`, so a `neutronsync watch` on the same state cannot run beside it.
- On start it loads the config, starts auto sync when `auto_sync = true`, and checks the account. On SIGTERM or SIGINT it stops accepting requests, ends every event stream, stops auto sync, cancels a running sync, waits up to eight seconds for it to stop (so the whole stop stays under ten), removes `serve.lock` and exits 0.
- Logs go to `state_dir/logs/sync.log`. Standard error gets one line when the server starts, one when it stops, and one the first time each kind of request is refused. Request bodies are never logged.

## Conventions

- Every path starts with `/api/v1`. A breaking change will get `/api/v2`; new fields may appear in v1 at any time, so clients must ignore fields they do not know.
- Bodies are JSON, at most 64 KiB (413 above that, chunked or not, on every endpoint). A body that is not empty must be sent as `Content-Type: application/json` (parameters such as `charset` allowed), or the request is refused with 415 before anything runs. An empty body counts as `{}`. Unknown fields in a body are refused with 400.
- When a token is set, every request except `GET /api/v1/health` needs `Authorization: Bearer <token>`. A missing or wrong token is 401 with no detail. The token is compared in constant time.
- The API is for server-side clients (curl, a web app's backend), never for scripts in a browser. A request carrying an `Origin` header, or a `Sec-Fetch-Site` header other than `none` or `same-origin`, is refused with 403. Every browser adds these headers to the requests a page makes, so a page the user happens to open cannot drive the server even when it has no token. There are no CORS headers.
- On a loopback address, the request's `Host` must be `localhost`, a `*.localhost` name or a loopback address (`127.0.0.1`, `[::1]`); any other name is refused with 403. This stops a page from reaching the server through a name of its own that resolves to 127.0.0.1 (DNS rebinding).
- `GET /api/v1/health` is exempt from the token, origin and host checks.
- Commands that start work return `202 {"accepted":true}`; what happens next shows in the state and the event stream.

The examples below assume:

```sh
API=http://127.0.0.1:8090/api/v1
AUTH="Authorization: Bearer $NEUTRONSYNC_API_TOKEN"   # leave out when no token is set
JSON="Content-Type: application/json"
```

### Errors

Every error has the same shape:

```json
{"error": {"code": "busy", "message": "A sync is already running. Wait for it to finish or cancel it."}}
```

| Code | Status | Meaning |
|---|---|---|
| `bad_request` | 400 (405 for a wrong method, 415 for a body that is not `application/json`) | The request is malformed: bad JSON, an unknown field, a malformed query string, an unknown pair name in a sync, a missing confirmation. |
| `unauthorized` | 401 | Missing or wrong API token. |
| `sign_in_failed` | 401 | Proton refused a sign-in step. |
| `forbidden` | 403 | A local path outside the folders this server may browse, a request from a web page, or an unexpected `Host` name. |
| `not_found` | 404 | No such endpoint, pair, hold, conflict or folder. |
| `busy` | 409 | A sync is already running. |
| `conflict` | 409 | A synced folder with that name already exists. |
| `signed_out` | 409 | The call needs a Proton session and there is none. |
| `too_large` | 413 | The body is over 64 KiB. |
| `invalid_config` | 422 | The change would make the config invalid. `message` is the config parser's own message. Nothing was changed. |
| `backend` | 502 | The sidecar (`neutronsync-drive`) failed or could not be started. |
| `internal` | 500 | Anything else. |

## The state envelope

`GET /api/v1/state` and every `state` event carry the same object:

```json
{
  "rev": 42,
  "state": { ... },
  "held": [ ... ],
  "conflicts": [ ... ],
  "auto_sync": false
}
```

- `rev` (integer): grows by one each time the engine's state, the held deletions, the conflicts or `auto_sync` change. The event stream and `GET /api/v1/state` share one counter, so equal `rev` means equal content. The only difference between two envelopes with the same `rev` is how much of the activity feed they carry: the stream carries the newest 100 items, `GET /api/v1/state` the number its `activity` asks for.
- `auto_sync` (bool): the `auto_sync` setting in the config.
- `held`: pairs whose deletions are held back (see below).
- `conflicts`: settled conflicts not yet dismissed, newest first (see below).
- `state`: the engine's live state:

| Field | Type | Meaning |
|---|---|---|
| `pairs` | array | One entry per synced folder, in config order. |
| `pairs[].name` | string | The folder's name. |
| `pairs[].local` | string | Folder on this machine. |
| `pairs[].remote` | string | Folder on Proton Drive, e.g. `/my-files/Documents`. |
| `pairs[].phase` | string | `Idle`, `Scanning`, `Syncing`, `Synced` or `Error`. |
| `pairs[].progress` | object | `{"done": n, "total": n}` operations in the current run. |
| `pairs[].scanned_folders` | integer | Folders listed so far in a scan. |
| `pairs[].current_op` | string or null | What it is doing now, e.g. `upload sub/a.txt`. |
| `pairs[].last_error` | string or null | Why the last run failed. |
| `pairs[].last_synced` | integer or null | Last successful sync, Unix seconds. |
| `pairs[].tracked` | integer | Files kept in sync. |
| `account` | object | Same object as `GET /api/v1/account`. |
| `activity` | array | Recent activity, oldest first, newest last. |
| `activity[].ts` | integer | Unix seconds. |
| `activity[].kind` | string | `Info`, `Sync` or `Error`. |
| `activity[].text` | string | One line in plain words. |
| `activity[].op` | object or null | For a file operation: `{"action","path","pair","ok","error"}`. |
| `active` | array | Operations in flight, `{"action","path","pair","ok","error"}`. |
| `busy` | bool | A sync is running. |
| `watching` | bool | Auto sync is on. |
| `signed_out` | bool | The Proton session was found signed out; syncing waits for a sign-in. |

A held deletion (`held[]`):

| Field | Type | Meaning |
|---|---|---|
| `pair` | string | The folder. |
| `local` | integer | Files that would have been deleted on this machine. |
| `remote` | integer | Files that would have been deleted on Proton Drive. |
| `tracked` | integer | Files the folder tracked when the deletions were first held. |
| `since` | integer | First held, Unix seconds. |
| `updated` | integer | Last held again, Unix seconds. |

A conflict (`conflicts[]`):

| Field | Type | Meaning |
|---|---|---|
| `id` | integer | Use it to dismiss the conflict. |
| `pair` | string | The folder. |
| `path` | string | The file, relative to the folder. |
| `outcome` | string | `kept_both`, `kept_local`, `kept_remote` or `skipped`. |
| `copy` | string or null | Where your local version went when both were kept. |
| `first_ts`, `last_ts` | integer | First and last time it happened, Unix seconds. |
| `seen` | integer | How many runs settled it the same way. |
| `dismissed` | bool | Always false in these lists. |

## Live updates: `GET /api/v1/events`

A server-sent events stream (`text/event-stream`). On connect the client gets the current envelope at once, then a new one whenever it changes:

```
event: state
data: {"rev":43,"state":{...},"held":[],"conflicts":[],"auto_sync":false}

```

- The envelope here carries the newest 100 activity items.
- One task builds the envelope every 250 ms for all clients; held deletions and conflicts are re-read every 2 seconds. A slow client is not queued up: it gets the latest envelope next.
- With nothing to report, a `: keepalive` comment arrives every 15 seconds.
- The stream ends when the server shuts down; reconnect and the first event brings you up to date.

```sh
curl -N -H "$AUTH" $API/events
```

## Endpoints

### `GET /api/v1/health`

No token needed and no state touched, for container health checks. Response `200 {"ok":true}`.

```sh
curl $API/health
```

### `GET /api/v1/version`

Response `200 {"api_version":1,"version":"0.7.0","build":"<commit>"}`. `build` is the same string `neutronsync --version` shows.

```sh
curl -H "$AUTH" $API/version
```

### `GET /api/v1/state?activity=N`

The envelope above. `activity` (default 100, at most 500) keeps the newest N activity items. Errors: `unauthorized`.

```sh
curl -H "$AUTH" "$API/state?activity=20"
```

### `POST /api/v1/sync`

Body `{"pairs":["name",...],"dry_run":false}`; both optional. An empty or absent `pairs` syncs every folder. `dry_run` plans without changing anything. Response `202 {"accepted":true}`. Errors: `bad_request` (unknown folder name), `busy`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/sync -d '{"pairs":["documents"],"dry_run":true}'
```

### `POST /api/v1/cancel`

Stops the running sync before its next operation. No body. Response `202 {"accepted":true}`.

```sh
curl -H "$AUTH" -X POST $API/cancel
```

### `POST /api/v1/auto-sync`

Body `{"enabled":true}`. Turns auto sync (watching folders and Proton Drive for changes) on or off and saves `auto_sync` to the config file. It covers the folders with `auto = true`. Response `200 {"auto_sync":true}`. Errors: `bad_request`, `invalid_config`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/auto-sync -d '{"enabled":true}'
```

### `GET /api/v1/held`

The `held` list from the envelope. Response `200 [ ... ]`.

```sh
curl -H "$AUTH" $API/held
```

### `POST /api/v1/held/{pair}/restore`

"Restore them": runs the folder once with both sides merged afresh, so the files whose deletion was held back come back. No body. Response `202 {"accepted":true}`; the hold clears when the run finishes. Errors: `not_found` (unknown folder, or nothing held for it), `busy`.

```sh
curl -H "$AUTH" -X POST $API/held/documents/restore
```

### `POST /api/v1/held/{pair}/delete`

"Delete them": runs the folder once with the deletion guard off for that run, so the held deletions go through. Body `{"confirm":true}` is required. Response `202 {"accepted":true}`. Errors: `not_found`, `bad_request` (no confirmation; nothing is deleted), `busy`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/held/documents/delete -d '{"confirm":true}'
```

### `GET /api/v1/conflicts`

The `conflicts` list from the envelope. Response `200 [ ... ]`.

```sh
curl -H "$AUTH" $API/conflicts
```

### `POST /api/v1/conflicts/{id}/dismiss`

Removes one conflict from the list. No body. Response `202 {"accepted":true}`. Errors: `not_found`.

```sh
curl -H "$AUTH" -X POST $API/conflicts/12/dismiss
```

### `POST /api/v1/conflicts/dismiss-all`

Empties the list. No body. Response `202 {"accepted":true}`.

```sh
curl -H "$AUTH" -X POST $API/conflicts/dismiss-all
```

### `GET /api/v1/settings`

The settings a server can use, with the values and spellings of the config file. Desktop-only settings (tray, X11, theme, notifications, bookmarks, update channel) are not here and are left as they are.

```json
{
  "propagate_deletes": true, "max_delete_files": 200, "max_delete_percent": 30,
  "local_delete": "trash", "conflict": "keep-both", "compare": "size+mtime",
  "scan_threads": 0, "download_threads": 0, "full_walk_interval": 86400,
  "scan_interval": 120, "debounce": 2, "upload_limit_kbps": 0,
  "download_limit_kbps": 0, "auto_sync": false, "remote_root": "/my-files"
}
```

See `neutronsync.example.toml` for what each one does.

```sh
curl -H "$AUTH" $API/settings
```

### `PUT /api/v1/settings`

Body: any subset of the settings object. The change is checked by the same parser that reads the config file: if the result would not load, nothing is changed and the reply is 422 with the parser's message, for example `options.conflict must be keep-both|newer|skip, got "maybe"`. Otherwise the config file is replaced in one step (its mode is kept) and the running engine uses the new values from its next run. Changing `auto_sync` here also turns auto sync on or off. Response `200` with the full settings object. Errors: `bad_request` (unknown key or wrong type), `invalid_config`.

```sh
curl -H "$AUTH" -H "$JSON" -X PUT $API/settings -d '{"max_delete_percent":40,"conflict":"newer"}'
```

### `GET /api/v1/pairs`

The synced folders. Response `200 [{"name","local","remote","auto","exclude":[...]}]`. `remote` is the full Proton path.

```sh
curl -H "$AUTH" $API/pairs
```

### `POST /api/v1/pairs`

Body `{"name","local","remote","auto"?,"exclude"?}`. `local` must be an absolute path without `.` or `..` and, when the server has `--local-root`, inside one of those folders. `remote` is a path under `remote_root` (`Documents`) or a full path (`/my-files/Documents`). `auto` defaults to true. Response `201` with the new folder list. A running auto sync restarts to include the change. Errors: `conflict` (name taken), `invalid_config` (bad path or anything the config parser refuses), `bad_request`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/pairs -d '{"name":"documents","local":"/data/Documents","remote":"Documents","exclude":["cache"]}'
```

### `PUT /api/v1/pairs/{name}`

Replaces a folder with the body (same fields as `POST`). The name may change; the stored sync state of the old name is then forgotten, and the folder's next run is a first sync that merges both sides and deletes nothing. Response `200` with the folder list. Errors: `not_found`, `conflict` (new name taken), `invalid_config`, `bad_request`.

```sh
curl -H "$AUTH" -H "$JSON" -X PUT $API/pairs/documents -d '{"name":"documents","local":"/data/Documents","remote":"Documents","auto":false}'
```

### `DELETE /api/v1/pairs/{name}`

Stops syncing a folder: removes it from the config and forgets its stored sync state. It never deletes a file on either side. Response:

```json
{"removed":"documents","files_deleted":false,"message":"The folder is no longer synced. No file was deleted on this computer or on Proton Drive."}
```

Errors: `not_found`.

```sh
curl -H "$AUTH" -X DELETE $API/pairs/documents
```

### `GET /api/v1/account`

The account state, checked again when the last check is more than 30 seconds old:

```json
{"checked":true,"backend_ready":true,"signed_in":true,"keyring_locked":false,"account":"you@proton.me","version":"/usr/bin/neutronsync-drive","checking":false}
```

`backend_ready` is false when the sidecar could not be reached; `version` then holds the reason. `keyring_locked` means the system keyring is locked, which is not a sign-out.

```sh
curl -H "$AUTH" $API/account
```

### Signing in

Sign-in is a short conversation. Each call returns `200` with the next step:

- `{"step":"done"}`: signed in.
- `{"step":"need_2fa"}`: send the authenticator code to `/account/2fa`.
- `{"step":"need_mailbox_password"}`: send the second password to `/account/mailbox-password`.
- `{"step":"need_human_verification","url":"https://verify.proton.me/...","token":"...","methods":["captcha"]}`: open `url` in a browser, complete the check, then call `/account/login` again with the same username and password plus `hv_token` (the `token`) and `hv_type` (one of `methods`).

A step Proton refuses is `401 {"error":{"code":"sign_in_failed","message":"..."}}`. Passwords and codes never appear in a log line or an error message. After every step the account is checked again, so `GET /api/v1/account` shows the result.

#### `POST /api/v1/account/login`

Body `{"username","password","hv_token"?,"hv_type"?}`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/account/login -d '{"username":"you@proton.me","password":"..."}'
```

#### `POST /api/v1/account/2fa`

Body `{"code":"123456"}`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/account/2fa -d '{"code":"123456"}'
```

#### `POST /api/v1/account/mailbox-password`

Body `{"password":"..."}`.

```sh
curl -H "$AUTH" -H "$JSON" -X POST $API/account/mailbox-password -d '{"password":"..."}'
```

#### `POST /api/v1/account/logout`

Signs out and removes the saved session. No body. Response `200 {"step":"done"}`.

```sh
curl -H "$AUTH" -X POST $API/account/logout
```

### `GET /api/v1/remote/list?path=/my-files/...`

Lists one Proton Drive folder, folders first, then files, each sorted by name. `path` defaults to `remote_root`. Response `200 [{"name","kind":"folder"|"file","size"?,"mtime"?}]`; `size` (bytes) and `mtime` (Unix seconds) are given for files. Errors: `signed_out`, `backend`, `bad_request` (path not starting with `/`).

```sh
curl -H "$AUTH" "$API/remote/list?path=/my-files/Documents"
```

### `GET /api/v1/local/list?path=...`

Lists the folders (never files) directly inside one folder on the server. Without `path` it lists the folders the server may browse: the `--local-root` folders, or the home directory. The path is resolved to where it really leads, links included, and must stay inside one of those folders, so the API cannot be used to look around the server's file system. Response `200 [{"name","path","hidden"}]`, sorted by name; `hidden` is true for names starting with a dot. Errors: `forbidden` (outside), `not_found`, `bad_request` (relative path).

```sh
curl -H "$AUTH" "$API/local/list?path=/data"
```

### `GET /api/v1/logs?lines=N`

The last N lines (default 200, at most 2000) of `state_dir/logs/sync.log`, read from the end of the file. Response `200 {"lines":["...", ...]}`.

```sh
curl -H "$AUTH" "$API/logs?lines=50"
```

## Keeping the session without a keyring

A container has no desktop keyring. With `session_store = "file"` in `[cli]`, the sidecar keeps the Proton session in `state_dir/session.enc`, encrypted with AES-256-GCM. The key is 32 bytes, given as base64 or 64 hex characters in `NEUTRONSYNC_SESSION_KEY` or in a file named by `session_key_file` (or `NEUTRONSYNC_SESSION_KEY_FILE`). Make one with:

```sh
head -c 32 /dev/urandom | base64 > session.key && chmod 600 session.key
```

The environment variables `NEUTRONSYNC_SESSION_STORE=file`, `NEUTRONSYNC_SESSION_FILE` and `NEUTRONSYNC_SESSION_KEY_FILE` work too and take precedence over the config. Without a usable key the sidecar does not start and says "Session file store needs a 32-byte key (NEUTRONSYNC_SESSION_KEY or NEUTRONSYNC_SESSION_KEY_FILE)." A session file that does not decrypt with the key given (a wrong key, a damaged file) stops the sidecar with "The saved session cannot be decrypted: wrong key or damaged file." and the file is left alone, so a mistyped key cannot destroy a good session; only signing out removes it.
