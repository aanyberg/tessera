#!/usr/bin/env bash
# docs/security.md S7 and scripts/unsafe-exceptions.txt name the same files: verify-branch.sh's `unsafe` check is
# mechanical, so the text must not drift from the list. Fails when they differ, and proves the comparison itself fails
# for a path missing on either side. Runs as `just factory-test`.
#
#   scripts/test-unsafe-exceptions.sh
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/unsafe-exceptions-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
failures=0
ok() { printf 'ok   %s\n' "$1"; }
not_ok() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }

security="$here/../docs/security.md"
s7_paths() { sed -n '/^- \*\*S7\./,/^- \*\*S8\./p' "$1" | grep -oE '(crates|apps)/[A-Za-z0-9_./-]+\.rs' | sort -u; }
listed_paths() { grep -vE '^[[:space:]]*(#|$)' "$1" | sort -u; }
sync_check() { # <security.md> <exceptions file>
    [ -n "$(s7_paths "$1")" ] && [ "$(s7_paths "$1")" = "$(listed_paths "$2")" ]
}
if sync_check "$security" "$here/unsafe-exceptions.txt"; then
    ok "docs/security.md S7 and scripts/unsafe-exceptions.txt name the same files"
else
    not_ok "docs/security.md S7 and scripts/unsafe-exceptions.txt differ: $(diff <(s7_paths "$security") <(listed_paths "$here/unsafe-exceptions.txt") || true)"
fi
grep -v 'crates/tessera-grid/tests/reset.rs' "$security" > "$work/security-fewer.md"
if sync_check "$work/security-fewer.md" "$here/unsafe-exceptions.txt"; then
    not_ok "the sync check passes with a path missing from S7"
else
    ok "the sync check fails when S7 lacks a listed path"
fi
{ cat "$here/unsafe-exceptions.txt"; echo crates/tessera-vt/tests/extra.rs; } > "$work/exceptions-more.txt"
if sync_check "$security" "$work/exceptions-more.txt"; then
    not_ok "the sync check passes with a path missing from S7"
else
    ok "the sync check fails when the list has a path S7 lacks"
fi

if [ "$failures" -ne 0 ]; then
    printf '%s check(s) failed\n' "$failures"
    exit 1
fi
printf 'all unsafe-exceptions cases passed\n'
