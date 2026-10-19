#!/usr/bin/env bash
# Tests for scripts/check-glibc.sh: a binary under the floor passes, one above
# it fails naming the file and its version, and a non-ELF input fails.
#   scripts/check-glibc-test.sh
set -euo pipefail
here="$(cd -- "$(dirname -- "$0")" && pwd)"
check="$here/check-glibc.sh"

# A real dynamically linked glibc binary from this host, whatever its floor is.
bin="$(command -v ls)"
actual="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -V | tail -1 | cut -d. -f1,2)"
[ -n "$actual" ] || { echo "no glibc binary to test with"; exit 2; }

fail=0
pass_case() { echo "ok   $1"; }
fail_case() { echo "FAIL $1"; fail=1; }

# 1. At or under the floor: exit 0, prints the version.
if out="$("$check" "$actual" "$bin")" && grep -q "GLIBC_$actual" <<<"$out"; then
	pass_case "under the floor passes ($actual)"
else
	fail_case "under the floor should pass: $out"
fi

# 2. Above the floor: exit 1, message names the file and the version.
if out="$("$check" 2.0 "$bin" 2>&1)"; then
	fail_case "above the floor should fail"
elif grep -q "$bin" <<<"$out" && grep -q "needs GLIBC_$actual" <<<"$out" && grep -q "above the 2.0 floor" <<<"$out"; then
	pass_case "above the floor fails and names the file and version"
else
	fail_case "wrong message for above the floor: $out"
fi

# 3. Not an ELF file: exit 1, says so.
if out="$("$check" 99.0 "$check" 2>&1)"; then
	fail_case "a non-ELF input should fail"
elif grep -q "not an ELF binary" <<<"$out"; then
	pass_case "non-ELF input fails"
else
	fail_case "wrong message for non-ELF: $out"
fi

# 4. One bad file among good ones still fails, and every file is reported.
if out="$("$check" 99.0 "$bin" "$check" 2>&1)"; then
	fail_case "a mixed list should fail"
elif grep -q "GLIBC_$actual" <<<"$out" && grep -q "not an ELF" <<<"$out"; then
	pass_case "mixed list fails and reports every file"
else
	fail_case "mixed list: $out"
fi

# 5. Usage errors.
if "$check" 2.35 >/dev/null 2>&1; then fail_case "missing files should be a usage error"; else pass_case "usage error without files"; fi
if "$check" abc "$bin" >/dev/null 2>&1; then fail_case "bad MAX should be a usage error"; else pass_case "usage error on a bad MAX"; fi

exit "$fail"
