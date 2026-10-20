# Screenshots

Every page of the NeutronSync desktop app, captured from the app itself with example folders, files and an example account. Nothing here is a real Proton account or a real file.

Images are rendered at 2x (2048 × 1284 pixels for a 1024 × 642 window) so they stay sharp on high-density displays.

## Activity

The home page. A banner states what the app is doing right now; below it, files in flight and the recent feed of uploads, downloads, moves and deletes, each with the folder pair it belongs to and when it happened. A failed operation shows its reason on the row.

![Activity page during a sync](screenshots/activity-syncing.png)

With Auto sync on and nothing in flight, the banner reports what is being watched and how many files are tracked.

![Activity page while idle with Auto sync on](screenshots/activity.png)

## Folders

One card per folder pair: the local folder, the Proton Drive folder, its current state and file count, an Auto switch for live sync, a Sync button for a one-off run and the selective-sync picker. A pair that is syncing shows a progress bar.

![Folders page](screenshots/folders.png)

## Settings

Grouped cards for tray, X11 compatibility, launch at login, desktop notifications, appearance, the file manager sidebar, deletion policy, conflict handling, updates, change detection, network limits and the advanced backend options.

![Settings page](screenshots/settings.png)

## Appearance

The window follows the desktop's light or dark preference, or a fixed choice made under Settings → Appearance. These are the Activity and Settings pages in the light theme; every page keeps the same layout, only the colours change.

![Activity page in the light theme](screenshots/activity-light.png)

![Settings page in the light theme](screenshots/settings-light.png)

## Tray and notifications

The Conflicts page lists every conflict the sync settled, until it is dismissed:

![Conflicts page: two settled conflicts with the folder, when, and what was kept](screenshots/conflicts.png)

When a run would delete more than the guard allows, it holds the deletions back and asks:

![Activity page with a banner: 1,640 deletions held back in Photos, with Restore them and Delete them](screenshots/held.png)

Not pictured here, since they live in the desktop's own panel rather than in the window: the tray icon shows the sync state (the plain logo when up to date, a purple dot while syncing, amber when sync is waiting on you, red for a problem) and its menu opens with a one-line status such as "Syncing 37 of 120". Conflicts, a problem that does not clear and a sign-out arrive as desktop notifications.

## Account

The signed-in Proton account, where the session is kept and which helper is serving the app, with Refresh and Sign out.

![Account page, signed in](screenshots/account.png)

## Sign in

Shown instead of the app when there is no Proton session. Username and password go to Proton only; the session is then saved in the system keyring. Proton may follow up with a two-factor code, a mailbox password or a human-verification page, and the card changes to that step.

![Sign-in page](screenshots/sign-in.png)

## Keyring locked

The session is saved but the system keyring is locked, for example right after logging in to the desktop. The app waits rather than asking you to sign in again, and continues on its own when the keyring is unlocked.

![Keyring locked page](screenshots/keyring-locked.png)

## Web interface

The browser interface for the Docker image and other headless installs. It has the same pages as the desktop window, with a sidebar on a wide screen and a menu button on a phone. These were captured from a scratch `neutronsync serve` with example folders.

The Activity page with a sync button, a dry run and the recent feed:

![Web Activity page](screenshots/web-activity.png)

Deletions held back by the safety limits, with Restore and Delete:

![Web Activity page with held-back deletions](screenshots/web-held.png)

Folder pairs. Add folder opens a picker for local folders and Proton Drive folders, with breadcrumbs and a spinner while a folder loads:

![Web Folders page](screenshots/web-folders.png)

![Web Conflicts page](screenshots/web-conflicts.png)

![Web Settings page](screenshots/web-settings.png)

Proton sign-in, including the two-factor step:

![Web Account page](screenshots/web-account.png)

![Web Account page asking for a two-factor code](screenshots/web-account-2fa.png)

The light theme, and the layout on a phone:

![Web Activity page in the light theme](screenshots/web-activity-light.png)

![Web Activity page on a phone](screenshots/web-activity-phone.png)

## Regenerating

```sh
scripts/screenshots.sh            # build the GUI and capture into docs/screenshots/
scripts/screenshots.sh --no-build # reuse target/release/neutronsync-gui
```

The script runs `neutronsync-gui --screenshots <dir>` under `xvfb-run` with `HOME` and the XDG directories pointed at a scratch folder, so it never reads your config, your state or your running sidecar. The scenes and example data live in `src/bin/gui.rs` next to `demo_scenes()`.

The web pictures come from `web/scripts/screenshots.py`. It needs `target/debug/neutronsync` built with `--features serve` and Chromium for Playwright:

```sh
cd web && uv run python scripts/screenshots.py
```
