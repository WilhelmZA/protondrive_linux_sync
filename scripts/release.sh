#!/usr/bin/env bash
#
# Cut a release: bump the version, close the changelog's [Unreleased] section,
# commit, and create a signed tag. It stops before pushing, so nothing reaches
# GitHub until you say so.
#
#   scripts/release.sh 0.3.4
#   scripts/release.sh 0.3.4 -m "self-installing updates, log rotation"
#   scripts/release.sh 0.4.0-rc1 --dry-run
#
# The changelog section is written by hand, before running this. That is the
# point: the release body on GitHub is that section verbatim, and a generated
# list of commit subjects is not worth reading.
#
# It also refuses to tag a release the workflow could not sign, or one whose
# version is below SIGNED_SINCE in src/updater.rs (such a build would accept
# unsigned updates). See docs/RELEASE_SIGNING.md.

set -euo pipefail

usage() {
	sed -n '2,/^$/{/^#!/d; s/^# \?//p}' "$0"
	exit "${1:-0}"
}

die() {
	printf 'release: %s\n' "$1" >&2
	exit 1
}

ver=''
subject=''
dry_run=0

while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help) usage 0 ;;
	-n | --dry-run) dry_run=1 ;;
	-m | --message)
		[ $# -ge 2 ] || die "-m needs a value"
		subject="$2"
		shift
		;;
	-*) die "unknown option $1" ;;
	*)
		[ -z "$ver" ] || die "give exactly one version"
		ver="${1#v}"
		;;
	esac
	shift
done

[ -n "$ver" ] || usage 1

cd "$(git rev-parse --show-toplevel)"

# ---------------------------------------------------------------- checks

[[ $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] ||
	die "'$ver' is not a version like 0.3.4 or 0.4.0-rc1"

git diff-index --quiet HEAD -- ||
	die "working tree has uncommitted changes; commit or stash them first"

git rev-parse -q --verify "refs/tags/v$ver" >/dev/null &&
	die "tag v$ver already exists locally"

if git ls-remote --exit-code --tags origin "refs/tags/v$ver" >/dev/null 2>&1; then
	die "tag v$ver already exists on origin"
fi

current="$(sed -n '/^\[package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -1)"
[ -n "$current" ] || die "could not read the version from Cargo.toml"
[ "$current" != "$ver" ] || die "Cargo.toml is already at $ver"

grep -q '^## \[Unreleased\]' CHANGELOG.md ||
	die "CHANGELOG.md has no '## [Unreleased]' heading"

# What lands in the release body: everything between [Unreleased] and the
# version heading below it, which is exactly what the workflow will publish.
# Leading blank lines are dropped by sed, trailing ones by the substitution.
notes="$(awk '
	/^## \[Unreleased\]/ { inside = 1; next }
	inside && /^## \[/   { exit }
	inside               { print }
' CHANGELOG.md | sed -e '/./,$!d')"

[ -n "$notes" ] ||
	die "[Unreleased] is empty. Write the section for $ver first: what changed and why."

# ---------------------------------------------------------------- signing

# The public half of the release key ships inside every build; the private
# half lives only in the RELEASE_GPG_KEY secret. This script never reads a
# secret, it only checks that the committed public key is real.
key_file='packaging/release-signing-key.asc'
if grep -q 'BEGIN PGP PUBLIC KEY BLOCK' "$key_file"; then
	signing="key present ($key_file)"
	signing_ok=1
else
	signing="PLACEHOLDER in $key_file, the release workflow would refuse to sign; see docs/RELEASE_SIGNING.md"
	signing_ok=0
fi

# The updater demands a signature from SIGNED_SINCE on. A build shipped under
# a lower version would accept unsigned updates until then, so refuse it.
since="$(sed -n 's/^pub const SIGNED_SINCE: &str = "\([^"]*\)";/\1/p' src/updater.rs | head -1)"
[ -n "$since" ] || die "could not read SIGNED_SINCE from src/updater.rs"
core="${ver%%-*}"
if [ "$core" = "$since" ] && [ "$core" != "$ver" ]; then
	# A pre-release of SIGNED_SINCE itself sorts below it under semver.
	since_ok=0
elif [ "$(printf '%s\n%s\n' "$core" "$since" | sort -V | head -1)" = "$core" ] && [ "$core" != "$since" ]; then
	since_ok=0
else
	since_ok=1
fi

today="$(date -u +%F)"

echo "Release v$ver  (was $current)"
echo "  date       $today"
echo "  changelog  $(printf '%s\n' "$notes" | grep -c '^- ') bullet(s), $(printf '%s\n' "$notes" | wc -l) lines"
echo "  signing    $signing"
echo

if [ "$dry_run" -eq 1 ]; then
	echo "--- release body ---"
	printf '%s\n' "$notes"
	if [ "$signing_ok" -eq 0 ]; then
		echo "--- would refuse: the release signing key is still the placeholder ---"
	fi
	if [ "$since_ok" -eq 0 ]; then
		echo "--- would refuse: $ver is below SIGNED_SINCE ($since) ---"
	fi
	echo "--- (dry run, nothing changed) ---"
	exit 0
fi

[ "$signing_ok" -eq 1 ] ||
	die "$key_file is still the placeholder, so the release workflow cannot sign v$ver. docs/RELEASE_SIGNING.md says how to create the key."

[ "$since_ok" -eq 1 ] ||
	die "v$ver is below SIGNED_SINCE ($since in src/updater.rs); a build under this number would accept unsigned updates. Tag $since or later, or lower SIGNED_SINCE deliberately."

# ---------------------------------------------------------------- bump

# Only the [package] version, never a dependency's.
awk -v v="$ver" '
	/^\[/                        { in_pkg = ($0 == "[package]") }
	in_pkg && /^version *= *"/   { sub(/"[^"]*"/, "\"" v "\"") }
	{ print }
' Cargo.toml >Cargo.toml.new && mv Cargo.toml.new Cargo.toml

# The lockfile records the crate's own version too; patch it in place rather
# than making a release depend on cargo being able to reach the network.
awk -v v="$ver" '
	/^name = "neutronsync"$/     { hit = 1; print; next }
	hit && /^version = "/        { print "version = \"" v "\""; hit = 0; next }
	{ print }
' Cargo.lock >Cargo.lock.new && mv Cargo.lock.new Cargo.lock

grep -q "^version = \"$ver\"$" Cargo.lock ||
	die "failed to update Cargo.lock; check it by hand"

# ---------------------------------------------------------------- changelog

awk -v v="$ver" -v d="$today" '
	/^## \[Unreleased\]/ {
		print
		print ""
		print "## [" v "] - " d
		next
	}
	{ print }
' CHANGELOG.md >CHANGELOG.md.new && mv CHANGELOG.md.new CHANGELOG.md

# ---------------------------------------------------------------- commit + tag

msg="$(mktemp)"
trap 'rm -f "$msg"' EXIT

if [ -n "$subject" ]; then
	printf 'Release %s: %s\n\n' "$ver" "$subject" >"$msg"
else
	printf 'Release %s: \n\n' "$ver" >"$msg"
fi
printf '%s\n' "$notes" >>"$msg"

git add -A -- Cargo.toml Cargo.lock CHANGELOG.md

if [ -n "$subject" ]; then
	git commit -q -F "$msg"
else
	# No subject given, so finish it in the editor. The changelog section is
	# already in the body to trim down.
	git commit -q -e -F "$msg"
fi

# tag.gpgsign is set in this repo, so -a signs.
git tag -a "v$ver" -F <(printf 'NeutronSync v%s\n\n%s\n' "$ver" "$notes")

if git cat-file tag "v$ver" | grep -q 'BEGIN PGP SIGNATURE'; then
	signed='signed'
else
	signed='UNSIGNED, tag.gpgsign is off'
fi

echo
echo "Committed $(git rev-parse --short HEAD) and tagged v$ver ($signed)."
echo
echo "Nothing has been pushed. To ship it:"
echo "    git push origin $(git branch --show-current) && git push origin v$ver"
echo
echo "It lands as a pre-release. Promote it once it has proven itself:"
echo "    scripts/promote.sh $ver"
