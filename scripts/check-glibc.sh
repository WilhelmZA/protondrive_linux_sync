#!/usr/bin/env bash
#
# Print the highest GLIBC_x.y version each binary needs and fail if any is
# above MAX. A binary that needs a newer glibc than a distro ships does not
# start there ("GLIBC_2.39 not found"), so the release job runs this on every
# shipped binary before packaging.
#
#   scripts/check-glibc.sh MAX FILE...
#   scripts/check-glibc.sh 2.35 target/release/neutronsync target/release/neutronsync-gui sidecar/dist/neutronsync-drive
#
# Exit status: 0 when every file is at or under MAX, 1 when one is above it or
# is not an ELF binary, 2 on bad usage.

set -euo pipefail

if [ $# -lt 2 ]; then
	sed -n '2,/^$/{/^#!/d; s/^# \?//p}' "$0" >&2
	exit 2
fi

max="$1"
shift
[[ $max =~ ^[0-9]+\.[0-9]+$ ]] || {
	echo "check-glibc: MAX must look like 2.35, not '$max'" >&2
	exit 2
}

# True when $1 sorts after $2 as a version (2.39 > 2.35, 2.35 > 2.9).
newer_than() {
	[ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]
}

status=0
for file in "$@"; do
	if [ ! -f "$file" ]; then
		echo "check-glibc: $file: no such file" >&2
		status=1
		continue
	fi
	if [ "$(head -c 4 "$file" | tr -d '\0')" != $'\x7fELF' ]; then
		echo "check-glibc: $file: not an ELF binary" >&2
		status=1
		continue
	fi
	highest="$(objdump -T "$file" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -V | tail -1 || true)"
	if [ -z "$highest" ]; then
		# Static or non-glibc binary: nothing to require, so nothing to exceed.
		printf '%-40s needs no glibc symbol version\n' "$file"
		continue
	fi
	# Compare on major.minor only: GLIBC_2.2.5 is 2.2.
	short="$(printf '%s' "$highest" | cut -d. -f1,2)"
	if newer_than "$short" "$max"; then
		printf '%-40s needs GLIBC_%s, above the %s floor\n' "$file" "$highest" "$max"
		status=1
	else
		printf '%-40s needs GLIBC_%s (floor %s)\n' "$file" "$highest" "$max"
	fi
done
exit "$status"
