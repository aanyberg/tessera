#!/usr/bin/env zsh
# Exercise scripts/affected-tests.sh in a throwaway workspace: a <- b <- c (c depends on b, b on a), d alone, plus an
# app directory and a docs file. A shim on PATH forwards `cargo metadata` to the real cargo and records every other
# cargo call, so no test or lint actually runs.
#
#   scripts/test-affected-tests.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/affected-tests.sh"
real_cargo="$(command -v cargo)"
work="$(mktemp -d "${TMPDIR:-/tmp}/affected-tests-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
mkdir -p "$work/bin" "$work/repo" && cd "$work/repo"
cat > "$work/bin/cargo" <<SHIM
#!/bin/sh
if [ "\${1:-}" = metadata ]; then exec "$real_cargo" "\$@"; fi
echo "\$*" >> "$work/calls"
SHIM
chmod +x "$work/bin/cargo"
export PATH="$work/bin:$PATH"

git init -q -b main .
print '[workspace]\nresolver = "3"\nmembers = ["crates/a", "crates/b", "crates/c", "crates/d", "crates/term", "crates/shell", "crates/tessera-config"]' > Cargo.toml
typeset -A pkgname=(a a b b c c d d term tessera-term shell tessera-shell-integration tessera-config tessera-config)
for n in ${(k)pkgname}; do
    mkdir -p crates/$n/src && : > crates/$n/src/lib.rs
    print "[package]\nname = \"${pkgname[$n]}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n" > crates/$n/Cargo.toml
done
print '[dependencies]\na = { path = "../a" }' >> crates/b/Cargo.toml
print '[dependencies]\nb = { path = "../b" }' >> crates/c/Cargo.toml
mkdir -p apps/macos docs shell-integration/terminfo && : > apps/macos/x.swift && : > docs/x.md && : > shell-integration/terminfo/t.terminfo
echo "// movable" > crates/d/src/movable.rs
git add -A && git commit -q -m base
git checkout -q -b topic

failures=0
ok() { print -r -- "ok   $1"; }
not_ok() { print -r -- "FAIL $1"; failures=$((failures + 1)); }
# expect <name> <expected stdout of --list, space-joined> [stderr substring]
expect() {
    local name="$1" want="$2" note="${3:-}" out err
    out="$("$script" --list 2> "$work/err" | tr '\n' ' ' | sed 's/ $//')"
    err="$(cat "$work/err")"
    if [[ "$out" == "$want" && "$err" == *"$note"* ]]; then ok "$name"; else not_ok "$name: got '$out' / '$err', want '$want' / '$note'"; fi
}
reset() { git checkout -q -- . && git clean -fdq && git reset -q --hard main 2>/dev/null || true; }

out="$("$script" 2>&1)" && [[ "$out" == *"no changes since main"* ]] && ok "no changes" || not_ok "no changes: $out"

echo '//' >> crates/a/src/lib.rs
expect "a change reaches its dependents transitively" "a b c"
reset
echo '//' >> crates/c/src/lib.rs
expect "a leaf crate affects only itself" "c"
reset
echo '//' >> crates/d/src/lib.rs
expect "an unrelated crate stays out" "d"
reset
echo '# x' >> Cargo.toml
expect "a root manifest affects every crate" "a b c d tessera-config tessera-shell-integration tessera-term" "workspace-wide"
reset
echo '//' >> crates/tessera-config/src/lib.rs
expect "a change under crates/tessera-config names config-docs-check" "tessera-config" "just config-docs-check"
reset
echo '//' >> crates/a/src/lib.rs
expect "a change elsewhere does not name config-docs-check" "a b c" ""
[[ "$("$script" --list 2>&1 >/dev/null)" != *config-docs-check* ]] && ok "no config-docs-check note without a tessera-config change" || not_ok "unexpected config-docs-check note"
reset
echo y >> apps/macos/x.swift
expect "an app change affects no crate and points at app-test" "" "just app-test"
reset
echo y >> docs/x.md
expect "a docs change affects no crate" "" "no crate owns: docs/x.md"
reset
echo y >> shell-integration/terminfo/t.terminfo
expect "a shell-integration change selects the crates that read it" "tessera-shell-integration tessera-term"
reset
git mv crates/d/src/movable.rs crates/c/src/moved.rs
expect "a file moved between crates selects both" "c d"
reset
echo '//' >> crates/b/src/lib.rs && git add -A && git commit -q -m b
expect "committed changes count" "b c"
echo '//' > crates/d/src/new.rs
expect "untracked files count" "b c d"
reset
git checkout -q -- . 2>/dev/null; git checkout -q main && git checkout -q topic

echo '//' >> crates/a/src/lib.rs
: > "$work/calls"
"$script" --clippy -- --no-fail-fast > /dev/null 2>&1 || true
calls="$(cat "$work/calls")"
[[ "$calls" == *"clippy -p a -p b -p c --all-targets -- -D warnings"* ]] && ok "--clippy runs clippy on the affected crates" || not_ok "--clippy: $calls"
[[ "$calls" == *"nextest run -p a -p b -p c --no-tests=pass --no-fail-fast"* ]] && ok "tests run on the affected crates with extra args" || not_ok "nextest: $calls"

(( failures == 0 )) || { print -u2 "$failures failure(s)"; exit 1; }
print "all affected-tests cases passed"
