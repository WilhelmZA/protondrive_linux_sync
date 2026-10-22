#!/usr/bin/env bash
# Regenerate the OpenAPI document from the server and fail if docs/openapi.json
# has drifted. When the newest v* tag already contains docs/openapi.json, also
# fail if this spec removes or changes a v1 field. A tag with no spec is a
# skip, not a failure: the first tagged document becomes the baseline.
set -euo pipefail

cd "$(dirname "$0")/.."

python3 scripts/openapi_breaking.py --self-test

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

cargo run --quiet --example openapi --features serve > "$tmp"

if ! diff -u docs/openapi.json "$tmp"; then
  echo "docs/openapi.json does not match the spec generated from the server." >&2
  echo "Update it with: cargo run --quiet --example openapi --features serve > docs/openapi.json" >&2
  exit 1
fi

tag="$(git for-each-ref --sort=-v:refname --format='%(refname:short)' --count=1 'refs/tags/v*')"
if [[ -z "${tag}" ]]; then
  echo "openapi breaking-change check skipped: no v* release tag"
  exit 0
fi

if ! git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "openapi breaking-change check skipped: tag ${tag} is not in this clone"
  exit 0
fi

if ! git cat-file -e "${tag}:docs/openapi.json" 2>/dev/null; then
  echo "openapi breaking-change check skipped: ${tag} has no docs/openapi.json"
  exit 0
fi

base="$(mktemp)"
trap 'rm -f "$tmp" "$base"' EXIT
git show "${tag}:docs/openapi.json" > "$base"
python3 scripts/openapi_breaking.py "$base" "$tmp"
