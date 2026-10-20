#!/usr/bin/env bash
# Install a pinned Trivy into ./.tools/bin, verified against the release
# checksum. Used by the security and release workflows; bump both values
# together from the release's trivy_<ver>_checksums.txt.
set -euo pipefail

TRIVY_VERSION=0.74.0
TRIVY_SHA256=2ae6fe3ee734b7fdf11335663e18c75ea12dccc76062f09f164a3b0f8be4371a

dest="${1:-.tools/bin}"
mkdir -p "$dest"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tarball="trivy_${TRIVY_VERSION}_Linux-64bit.tar.gz"
curl -fsSL --retry 3 -o "$tmp/$tarball" \
	"https://github.com/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/${tarball}"
echo "${TRIVY_SHA256}  $tmp/$tarball" | sha256sum -c -
tar -xzf "$tmp/$tarball" -C "$tmp" trivy
install -m 755 "$tmp/trivy" "$dest/trivy"
"$dest/trivy" --version
