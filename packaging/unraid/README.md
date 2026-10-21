# NeutronSync on Unraid

## Community Applications

The template lives in [WilhelmZA/unraid-templates](https://github.com/WilhelmZA/unraid-templates), the repository submitted to Community Applications. Until NeutronSync is listed there, add it under **Apps → Settings → Template Repositories**:

```
https://github.com/WilhelmZA/unraid-templates
```

Then search for NeutronSync and install from the template. The template pulls `ghcr.io/wilhelmza/neutronsync:latest`.

## First start

Set **Web Password** (`NEUTRONSYNC_WEB_PASSWORD`) to at least 12 characters before starting. The web interface will not become healthy without it.

Open `http://<unraid-ip>:8080`, sign in with that password, then sign in to Proton Drive from the Account page.

## Paths

| Container | Host default | Purpose |
| --- | --- | --- |
| `/config` | `/mnt/user/appdata/neutronsync` | Config, state, encrypted session, web login data, logs |
| `/data` | `/mnt/user` | Default local root for folder pairs |

Narrow `/data` to the shares you actually sync. Leaving it on `/mnt/user` works but gives the container a wider view than it needs.

## Session key

On first start the container writes `/config/secrets/session.key` next to the encrypted session file. That protects a copied `session.enc` alone, but not a copy of the whole `/config` folder. To keep them apart, set **Session Key** (`NEUTRONSYNC_SESSION_KEY`) to a 32-byte key (base64) from a Docker/Unraid secret or password manager, and do not store that key under appdata.

## Unraid inotify gap

`/mnt/user` is the `shfs` FUSE union. Changes made through `/mnt/user` (SMB, other containers mapped there) raise inotify events. Writes straight to `/mnt/diskN` or `/mnt/cache` do not. The container's starter config sets `full_walk_interval = 21600` (six hours) so the safety-net full walk catches those.

## Deleted files

With `local_delete = "trash"` (the default), a local file removed because it was deleted on Proton Drive goes into that mount's `.Trash-<uid>/files/` folder (freedesktop per-volume trash via `gio`). It stays on the share and can be restored from the Unraid file browser.

## Updates

Unraid follows the `latest` tag. That tag moves when a release is promoted to stable (`scripts/promote.sh`), not when a pre-release is published.

To follow every pre-release, change **Repository** to `ghcr.io/wilhelmza/neutronsync:edge`; to stay on one version, use a version tag such as `ghcr.io/wilhelmza/neutronsync:0.8.2`.
