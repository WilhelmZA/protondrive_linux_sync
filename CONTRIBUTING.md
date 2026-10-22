# Contributing

## Layout

A single Cargo crate: a **library** (the reusable core) plus two binaries, plus the Bun sidecar.

- `src/lib.rs` — core: `config`, `models`, `backend` (builds the `Remote`), `remote` (the `Remote` trait), `driveapi` (API sidecar client), `changefeed`, `watcher_feed`, `engine` (three-way merge), `state`, `trash`, `datefmt`, `logger`, `events`, `service` (`Controller` for the tray and `serve`), `server` (`/api/v1`), `watcher`.
- `src/main.rs` — the `neutronsync` CLI.
- `src/bin/gui.rs` — the `neutronsync-gui` GUI (behind the `gui` feature). The window is an HTTP client of the engine; see `docs/GUI_API.md`.
- `sidecar/` — `neutronsync-drive`, the Proton Drive SDK helper (Bun).
- `clients/` — Python and TypeScript clients of `/api/v1`.
- `web/` — the browser interface, also an API client.

The core has no UI or CLI assumptions. `Controller` is the in-process engine used by the tray and `neutronsync serve`; other front ends use the HTTP API.

## Build & test

```sh
cargo build                       # lib + CLI
cargo test                        # engine + unit tests
(cd sidecar && bun install --frozen-lockfile && bun test)
sidecar/build.sh                  # -> sidecar/dist/neutronsync-drive
cargo build --features gui --bin neutronsync-gui
cargo run   --features gui --bin neutronsync-gui
```

The `gui` feature includes `serve`, so `neutronsync-gui` hosts the same `/api/v1` server as `neutronsync serve`.

CI blocks a merge on any of these, so run them before pushing:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check                  # advisories, licences, bans, sources (deny.toml)
(cd sidecar && bun audit --audit-level=high)
```

`cargo deny` also runs weekly, so a new advisory can turn a quiet branch red. Fix it by upgrading the crate; add an `ignore` to `deny.toml` only with a reason.

The Security workflow scans the source tree and the Docker image with Trivy on every push and weekly, and uploads the results to code scanning. It fails on a HIGH or CRITICAL finding that has a fix available. The release workflow runs the same image check before it pushes an image. `scripts/install-trivy.sh` installs the pinned Trivy version for a local run. Accept a finding in `.trivyignore.yaml` only with a reason.

GUI build needs system libs (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev
```

## Releasing

Write the `[Unreleased]` section of `CHANGELOG.md` first: it becomes the release body verbatim. Then:

```sh
scripts/release.sh 0.7.1          # bump, close the changelog section, commit, signed tag
git push origin rust && git push origin v0.7.1
scripts/promote.sh 0.7.1          # later: pre-release -> stable, which also publishes to the AUR
```

The tag runs `.github/workflows/release.yml` in an `ubuntu:22.04` container (glibc 2.35 floor, checked by `scripts/check-glibc.sh`). It builds the .deb, .rpm, binary tarball and source tarball, signs `SHA256SUMS` with the release key, and publishes a pre-release. Key handling and rotation are in [docs/RELEASE_SIGNING.md](docs/RELEASE_SIGNING.md).

Packaging metadata lives in `Cargo.toml` (`[package.metadata.deb]` and `[package.metadata.generate-rpm]`). Both packages ship the CLI, the GUI, the `.desktop` entry, and the systemd user units. Packaging requires the `gui` feature (the GUI binary is one of the shipped assets):

```sh
cargo deb --features gui
cargo generate-rpm            # after a `cargo build --release --features gui`
```

## Conventions

- No third-party runtime deps in the core beyond what's in `Cargo.toml`; keep the CLI light (GUI-only deps go behind the `gui` feature).
- Keep backend specifics behind the `Remote` trait (`backend::select`); do not leak sidecar details into the engine.
- `cargo test` must stay green; `bun test` in `sidecar/` must stay green; the GUI and CLI should build warning-free.
