# Contributing

## Layout

A single Cargo crate: a **library** (the reusable core) plus two binaries, plus the Bun sidecar.

- `src/lib.rs` — core: `config`, `models`, `backend` (selects the `Remote`), `driveapi` (API sidecar client), `protoncli` (legacy CLI adapter), `changefeed`, `watcher_feed`, `engine` (three-way merge), `state`, `trash`, `datefmt`, `logger`, `events`, `service` (`Controller` for frontends), `watcher`.
- `src/main.rs` — the `neutronsync` CLI.
- `src/bin/gui.rs` — the `neutronsync-gui` GUI (behind the `gui` feature).
- `sidecar/` — `neutronsync-drive`, the Proton Drive SDK helper (Bun).

The GUI is intentionally thin over `service::Controller` (see `docs/GUI_API.md`); the core has no UI or CLI assumptions.

## Build & test

```sh
cargo build                       # lib + CLI
cargo test                        # engine + unit tests
(cd sidecar && bun install --frozen-lockfile && bun test)
sidecar/build.sh                  # -> sidecar/dist/neutronsync-drive
cargo build --features gui --bin neutronsync-gui
cargo run   --features gui --bin neutronsync-gui
```

GUI build needs system libs (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev
```

## Releasing

Releases are built by `.github/workflows/release.yml` on a `v*` tag:

```sh
git tag v0.1.0
git push origin v0.1.0     # builds .deb + .rpm + source tarball -> draft Release
```

Packaging metadata lives in `Cargo.toml` (`[package.metadata.deb]` and `[package.metadata.generate-rpm]`). Both packages ship the CLI, the GUI, the `.desktop` entry, and the systemd user units. Packaging requires the `gui` feature (the GUI binary is one of the shipped assets):

```sh
cargo deb --features gui
cargo generate-rpm            # after a `cargo build --release --features gui`
```

## Conventions

- No third-party runtime deps in the core beyond what's in `Cargo.toml`; keep the CLI light (GUI-only deps go behind the `gui` feature).
- Keep backend specifics behind the `Remote` trait (`backend::select`); do not leak API or CLI details into the engine.
- `cargo test` must stay green; `bun test` in `sidecar/` must stay green; the GUI and CLI should build warning-free.
