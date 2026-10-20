#!/usr/bin/env bash
#
# Promote a pre-release to stable, or put one back.
#
#   scripts/promote.sh 0.3.4            # stable, takes the "Latest" badge
#   scripts/promote.sh 0.3.4 --demote   # back to pre-release
#
# GitHub carries exactly two badges: "Latest" on one release, "Pre-release" on
# each flagged one. A stable release that is not the newest stable shows
# nothing at all, so "(stable)" goes in the title to keep the distinction
# visible down the list. This does both halves in one step.

set -euo pipefail

die() {
	printf 'promote: %s\n' "$1" >&2
	exit 1
}

ver=''
demote=0

while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help)
		sed -n '2,/^$/{/^#!/d; s/^# \?//p}' "$0"
		exit 0
		;;
	-d | --demote) demote=1 ;;
	-*) die "unknown option $1" ;;
	*)
		[ -z "$ver" ] || die "give exactly one version"
		ver="${1#v}"
		;;
	esac
	shift
done

[ -n "$ver" ] || die "which version? try --help"

tag="v$ver"

gh release view "$tag" >/dev/null 2>&1 ||
	die "no release $tag on GitHub"

if [ "$demote" -eq 1 ]; then
	gh release edit "$tag" \
		--prerelease=true \
		--title "NeutronSync $tag" \
		--verify-tag >/dev/null
	echo "$tag is a pre-release again."
	echo
	echo "The \"Latest\" badge has moved to the newest remaining stable release:"
	echo "    $(gh api "repos/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/latest" -q .tag_name 2>/dev/null || echo 'none, nothing is promoted')"
else
	gh release edit "$tag" \
		--prerelease=false \
		--latest \
		--title "NeutronSync $tag (stable)" \
		--verify-tag >/dev/null
	echo "$tag is stable and now carries the \"Latest\" badge."
	echo
	echo "The updater's stable channel starts offering it immediately."
	echo "The Docker image tag latest moves to this version in a few minutes."
fi
