#!/usr/bin/env bash
set -euo pipefail
root="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
bun="${BUN:-$HOME/.local/bin/bun}"
if [[ ! -x "$bun" ]]; then bun="$(command -v bun)"; fi
mkdir -p "$root/dist"
"$bun" build --compile --target=bun-linux-x64 --no-compile-autoload-dotenv --no-compile-autoload-bunfig \
  --asset="$root/node_modules/@protontech/drive-sdk/dist/search/vendor/proton_drive_sdk_search_bg.wasm" \
  "$root/src/main.ts" --outfile "$root/dist/neutronsync-drive"
