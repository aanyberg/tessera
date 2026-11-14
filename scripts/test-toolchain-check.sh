#!/usr/bin/env zsh
# Exercise scripts/toolchain-check.sh with stub `rustc` and `rustup` on PATH: older local version warns (and exits 0),
# the same or a newer version passes, 1.99 vs 1.100 compares as numbers, and a missing rustup or an offline
# `rustup check` is skipped with exit 0.
#
#   scripts/test-toolchain-check.sh
set -uo pipefail

zsh="${commands[zsh]}"
script="$(cd "$(dirname "$0")" && pwd)/toolchain-check.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/toolchain-check-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
failures=0
ok() { print -r -- "ok   $1"; }
not_ok() { print -r -- "FAIL $1"; failures=$((failures + 1)); }

mkdir -p "$work/bin" "$work/nobin"
cat > "$work/bin/rustc" <<'STUB'
#!/bin/sh
echo "rustc $FAKE_RUSTC (58cf8f9c3 2026-10-13)"
STUB
cat > "$work/bin/rustup" <<'STUB'
#!/bin/sh
case "$FAKE_RUSTUP" in
    update) echo "stable-aarch64-apple-darwin - update available: $FAKE_RUSTC (58cf8f9c3 2026-10-13) -> 1.100.0 (0ef1bc386 2026-11-09)"
            echo "nightly-aarch64-apple-darwin - update available: 1.101.0-nightly (f9ceb4a4d 2026-10-31) -> 1.102.0-nightly (5f4effd57 2026-11-13)"; exit 100 ;;
    current) echo "stable-aarch64-apple-darwin - Up to date : 1.100.0 (0ef1bc386 2026-11-09)"; exit 0 ;;
    offline) echo "error: could not download manifest" >&2; exit 1 ;;
esac
STUB
chmod +x "$work/bin/rustc" "$work/bin/rustup"
ln -s /usr/bin/sed "$work/nobin/sed"; ln -s /usr/bin/grep "$work/nobin/grep"; ln -s /usr/bin/head "$work/nobin/head"
ln -s "$work/bin/rustc" "$work/nobin/rustc"

# check <name> <rustc version> <rustup mode> <expected substring> [PATH]
check() {
    local name="$1" rv="$2" mode="$3" want="$4" search="${5:-$work/bin:$PATH}" out rc=0
    out="$(env PATH="$search" FAKE_RUSTC="$rv" FAKE_RUSTUP="$mode" "$zsh" "$script" 2>&1)" || rc=$?
    if [[ "$rc" == 0 && "$out" == *"$want"* ]]; then ok "$name"; else not_ok "$name: exit $rc, '$out', want '$want'"; fi
}
check "an older rustc warns and exits 0" 1.99.1 update "WARN"
check "the warning names both versions" 1.99.1 update "local rustc 1.99.1 is behind the current stable 1.100.0"
check "1.99.1 is older than 1.100.0 as numbers" 1.99.1 update "behind"
check "an equal version passes" 1.100.0 current "PASS"
check "a newer local version passes" 1.101.0 current "PASS"
check "an offline rustup check is skipped" 1.99.1 offline "check skipped"
check "no rustup is skipped" 1.99.1 update "rustup not found" "$work/nobin"
out="$(FAKE_RUSTC=1.99.1 FAKE_RUSTUP=update PATH="$work/bin:$PATH" zsh "$script" extra 2>&1)"; rc=$?
[[ "$rc" == 2 ]] && ok "an argument is a usage error" || not_ok "argument: exit $rc, '$out'"
# The script never updates the toolchain.
if ! grep -qE "^[[:space:]]*(command )?rustup (update|toolchain install|default)" "$script"; then ok "the script never runs rustup update"; else not_ok "the script runs a rustup update"; fi

(( failures == 0 )) || { print -u2 "$failures failure(s)"; exit 1; }
print "all toolchain-check cases passed"
