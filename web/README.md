# NeutronSync web interface

A browser interface for NeutronSync. It talks only to `neutronsync serve` over the HTTP API. There is no sync logic in this package.

## Requirements

- Python 3.12
- `uv`
- A running `neutronsync serve` (built with `--features serve`)

## Setup

```sh
cd web
uv sync --locked
```

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `NEUTRONSYNC_API_URL` | `http://127.0.0.1:8090` | Base URL of `neutronsync serve` |
| `NEUTRONSYNC_API_TOKEN` | (none) | Bearer token for the API |
| `NEUTRONSYNC_API_TOKEN_FILE` | (none) | File containing the token (wins over the variable) |
| `NEUTRONSYNC_WEB_HOST` | `127.0.0.1` | Address the web app listens on |
| `NEUTRONSYNC_WEB_PORT` | `8080` | Port the web app listens on |
| `NEUTRONSYNC_WEB_AUTH` | `password` | `password` or `proxy` |
| `NEUTRONSYNC_WEB_PASSWORD` | (none) | Optional first-password seed, 12 characters or more |
| `NEUTRONSYNC_WEB_PASSWORD_FILE` | (none) | File containing the password |
| `NEUTRONSYNC_WEB_PASSWORD_RESET` | (none) | Set to `1` to replace the saved password with the seed on startup; remove after recovery |
| `NEUTRONSYNC_WEB_DATA` | `~/.local/state/neutronsync-web` | Session secret and password hash |

A bad value stops startup with one plain line naming the variable.

## Login modes

### Password (default)

For a machine on a home LAN with no reverse proxy. One admin password, no usernames. On first start the password is hashed with scrypt into `$NEUTRONSYNC_WEB_DATA/auth.json` (mode 0600). The plaintext is never stored.

Without a password seed, the web interface starts and redirects to **Create your password**. Enter the one-time setup code from the startup log and enter a password of at least 12 characters twice. The code changes on each start and expires after setup. Failed setup attempts are rate-limited like logins. `/healthz` remains available.

The password variables only seed an empty password store. Change the password under **Account → Change web password** using the current password. Other browser sessions are signed out. For recovery, set a valid seed and `NEUTRONSYNC_WEB_PASSWORD_RESET=1`, then restart. Remove the reset flag afterward. Alternatively, remove the seed variables, delete `$NEUTRONSYNC_WEB_DATA/auth.json`, and restart to return to setup.

### Proxy

Set `NEUTRONSYNC_WEB_AUTH=proxy` when Authelia, Authentik or similar already protects the site. There is no login page. The app logs a warning on every start and shows it on the About page. There is no mode with no protection and no proxy.

## Run next to `neutronsync serve`

Terminal 1:

```sh
export NEUTRONSYNC_API_TOKEN="$(openssl rand -hex 16)"
neutronsync serve --listen 127.0.0.1:8090
```

Terminal 2:

```sh
cd web
export NEUTRONSYNC_API_URL=http://127.0.0.1:8090
export NEUTRONSYNC_API_TOKEN
export NEUTRONSYNC_WEB_PASSWORD='your-long-password'
uv run neutronsync-web
```

Open `http://127.0.0.1:8080`.

## Checks

```sh
uv sync --locked
uv run ruff check
uv run ruff format --check
uv run pytest
```

End-to-end tests need `target/debug/neutronsync` or `target/release/neutronsync` built with `--features serve`, and Chromium via `uv run playwright install chromium`. Without the binary they skip with a clear message.
