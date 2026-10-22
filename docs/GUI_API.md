# Desktop window and the engine

The desktop window is an HTTP client. It does not call `Controller` and it does not read `status.json` as its live view. Every sync, folder, settings and sign-in action is a request on the unix socket the engine binds at `$XDG_RUNTIME_DIR/neutronsync/api.sock` (mode 0600). There is no bearer token on that socket. The window sends `Host: x` and does not send `Origin` or `Sec-Fetch-Site`.

`Controller` remains the in-process engine API. The tray child owns one, and `neutronsync serve` owns one. When tray mode is off, the window starts a `Controller` and `spawn_attached` in a helper, keeps only the `ServerHandle` so it can shut the listeners down when the window closes, and still drives the UI through the socket.

The contract for those requests is [HTTP_API.md](HTTP_API.md). The window follows `GET /api/v1/state` with `GET /api/v1/events` on a background thread and renders each new envelope. Held deletions and conflicts come from that envelope.

Desktop preferences (`run_in_tray`, `x11_compat`, `notifications`, `theme`, `file_manager_bookmarks`, `update_channel`, `check_on_launch`) are optional fields on `GET` and `PUT /api/v1/settings`. The window writes them there. File-manager bookmarks are still reconciled on this machine after a successful settings or folder write. Moving an excluded folder to the desktop trash stays a local filesystem action.

For this release, if the socket has never answered and `status.json` is still under 10 seconds old, the window renders that file once and logs it. That covers a tray from the previous release, which only published the file. A socket that has already answered is the live view after that.

Updater checks, desktop notifications over D-Bus, the folder picker, `xdg-open` and the autostart desktop file stay in the window. They are not engine calls. In tray mode the tray process sends notifications. The window sends them only when it is the process hosting the engine.

## In-process engine

The tray and `serve` still use `Controller` directly: `snapshot` for a cheap copy of `AppState`, and `sync`, `cancel`, `start_watch`, `stop_watch`, `commit_config` and `save` for work that must not block the caller. Those methods take `&self`. The HTTP routes are the supported way for another process, including this window, to do the same work.
