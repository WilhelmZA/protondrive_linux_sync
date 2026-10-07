#!/usr/bin/env bash
# Regenerate the documentation screenshots in docs/screenshots/.
#
# Runs `neutronsync-gui --screenshots` inside a private X server with made-up
# data. HOME and the XDG dirs point at a scratch directory, so the run reads
# and writes nothing of yours and never reaches your running sidecar.
#
#   scripts/screenshots.sh            # build (release, gui feature) and capture
#   scripts/screenshots.sh --no-build # reuse target/release/neutronsync-gui
set -euo pipefail
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
out="$root/docs/screenshots"

if [[ "${1:-}" != "--no-build" ]]; then
  (cd "$root" && cargo build --release --features gui --bin neutronsync-gui)
fi

command -v xvfb-run >/dev/null || { echo "xvfb-run is required (apt install xvfb)" >&2; exit 1; }

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/home" "$scratch/run" "$out"
chmod 700 "$scratch/run"

# A 2x canvas: the window is 2360x1480 physical pixels at zoom 2.0.
xvfb-run -a -s "-screen 0 2600x1700x24" \
  env WAYLAND_DISPLAY= XDG_SESSION_TYPE=x11 WINIT_UNIX_BACKEND=x11 \
      HOME="$scratch/home" \
      XDG_CONFIG_HOME="$scratch/home/.config" \
      XDG_STATE_HOME="$scratch/home/.local/state" \
      XDG_DATA_HOME="$scratch/home/.local/share" \
      XDG_RUNTIME_DIR="$scratch/run" \
  "$root/target/release/neutronsync-gui" --screenshots "$out" \
  2> >(grep -v -E 'libEGL|vulkan|DRI3' >&2 || true)

if command -v optipng >/dev/null; then
  optipng -quiet -o2 "$out"/*.png
fi
ls -la "$out"
