#!/usr/bin/env bash
# Rewrite src/openapi.ts from the committed spec.
# From this directory the spec is ../../docs/openapi.json.
# From src/ that same file is ../../../docs/openapi.json.
# Uses the openapi-typescript version pinned in package.json. No timestamps.
set -euo pipefail
cd "$(dirname "$0")"
spec="../../docs/openapi.json"
if [[ ! -f "$spec" ]]; then
  echo "missing $spec (repo docs/openapi.json)" >&2
  exit 1
fi
if [[ ! -x ./node_modules/.bin/openapi-typescript ]]; then
  echo "run npm install in clients/typescript first" >&2
  exit 1
fi
./node_modules/.bin/openapi-typescript "$spec" \
  --output src/openapi.ts \
  --alphabetize \
  --empty-objects-unknown \
  --root-types \
  --root-types-no-schema-prefix \
  --root-types-keep-casing
if grep -E '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}' src/openapi.ts >/dev/null; then
  echo "src/openapi.ts contains a timestamp" >&2
  exit 1
fi
