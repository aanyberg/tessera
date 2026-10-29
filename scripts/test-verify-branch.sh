#!/usr/bin/env bash
# Exercise scripts/verify-branch.sh in a throwaway repository: the load targets it derives from a branch's changes
# (--list-targets) and the --parallel cap of its load check.
#
#   scripts/test-verify-branch.sh          (also runs as `zsh scripts/test-verify-branch.sh`)
#
# verify-branch.sh is a zsh script (it re-executes itself under zsh when started from bash or sh); every case runs
# it directly, through `bash` and through `zsh`, and fails if any derived target is mangled the way zsh mangles an
# unbraced `$name:lib` (`<crate>ib`) or `$name:tests` (`<crate>ests`). A fake `cargo` on PATH proves that
# --list-targets builds nothing and lets the load check run against a fake test binary that records how many of
# its runs overlap, loses a run's exit status, or hangs until the check is interrupted. Requires git, jq, zsh and
# pgrep on PATH.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/verify-branch.sh"

work="$(mktemp -d "${TMPDIR:-/tmp}/verify-branch-test.XXXXXX")"
trap 'pkill -f "$work/bin/fake-test" 2>/dev/null || true; rm -rf "$work"' EXIT
# verify-branch.sh keeps its logs in `mktemp -d`; keep them inside $work.
mkdir -p "$work/tmp"
TMPDIR="$work/tmp"
export TMPDIR

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

failures=0
ok() { printf 'ok   %s\n' "$1"; }
not_ok() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }

# Fake tools: `cargo` records every call; `cargo nextest list` describes one lib suite backed by fake-test.
mkdir -p "$work/bin" "$work/active"
cat > "$work/bin/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_WORK/cargo.calls"
if [ "${1:-}" = nextest ] && [ "${2:-}" = list ]; then
    printf '{"rust-suites":{"tessera-app-core":{"binary-path":"%s/bin/fake-test","cwd":"%s","kind":"lib"}}}\n' \
        "$FAKE_WORK" "$FAKE_WORK"
    exit 0
fi
exit 1
EOF
# Two tests; each run marks itself active, samples how many runs are active, and holds for a moment.
# FAKE_KILL_PARENT: the first run kills the shell that would record its exit status.
# FAKE_HANG: runs block (in this process, so a stray run is visible to pgrep) until they are killed.
cat > "$work/bin/fake-test" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --list ]; then printf 'a: test\nb: test\n'; exit 0; fi
if [ -n "${FAKE_KILL_PARENT:-}" ] && mkdir "$FAKE_WORK/killed" 2>/dev/null; then kill -9 "$PPID"; fi
: > "$FAKE_WORK/active/$$"
ls "$FAKE_WORK/active" | wc -l | tr -d ' ' >> "$FAKE_WORK/samples"
if [ -n "${FAKE_HANG:-}" ]; then read -t 60 line <> "$FAKE_WORK/fifo"; else sleep 0.2; fi
rm -f "$FAKE_WORK/active/$$"
EOF
mkfifo "$work/fifo"
chmod +x "$work/bin/cargo" "$work/bin/fake-test"
export FAKE_WORK="$work"
PATH="$work/bin:$PATH"
export PATH

# Fixture repository: four packages and two nextest overrides on main.
repo="$work/repo"
mkdir -p "$repo"
cd "$repo"
git init -q -b main
mkdir -p .config
cat > .config/nextest.toml <<'EOF'
[[profile.default.overrides]]
filter = 'package(tessera-config) & binary(reload)'
test-group = 'timing'

[[profile.default.overrides]]
filter = 'package(tessera-term) & kind(lib) & test(slow)'
test-group = 'timing'
EOF
package() { # <name> <file under src>
    mkdir -p "crates/$1/src"
    printf '[package]\nname = "%s"\nversion = "0.0.0"\n' "$1" > "crates/$1/Cargo.toml"
    printf '// %s\n' "$1" > "crates/$1/src/$2"
}
package tessera-app-core lib.rs
package tessera-render lib.rs
package tessera-devwin main.rs
package tessera-config lib.rs
package tessera-term lib.rs
git add -A
git commit -qm 'chore: fixture'

timed_test_module='#[cfg(test)]
mod tests {
    use std::time::Instant;
}'
change() { # <path> <content>: append to (or create) a file
    mkdir -p "$(dirname "$1")"
    printf '%s\n' "$2" >> "$1"
}
branch() { # <name>: a fresh branch from main
    git switch -q main
    git switch -qc "$1"
}
commit() { git add -A; git commit -qm "test: $(git branch --show-current)"; }

branch app-core-lib
change crates/tessera-app-core/src/lib.rs "$timed_test_module"
commit

branch render-golden
change crates/tessera-render/tests/golden.rs 'fn bounded() { let _ = std::time::Duration::from_millis(5); }'
commit

branch render-helper
change crates/tessera-render/tests/common/mod.rs 'pub fn settle() { std::thread::sleep(std::time::Duration::ZERO); }'
commit

branch devwin-bin
change crates/tessera-devwin/src/main.rs "$timed_test_module"
commit

branch test-only-untimed
change crates/tessera-render/tests/plain.rs 'fn plain() { assert_eq!(1 + 1, 2); }'
change crates/tessera-app-core/src/clock.rs 'pub fn now() -> std::time::Instant { std::time::Instant::now() }'
commit

branch overrides
change crates/tessera-config/README.md 'notes'
change crates/tessera-term/src/lib.rs '// untimed change'
commit

branch everything
for b in app-core-lib render-golden render-helper devwin-bin overrides; do
    git merge -q --no-edit "$b"
done

# expect <case> <expected output> <verify-branch args>...: the same output when started directly, via bash, via zsh.
expect() {
    name="$1" want="$2"
    shift 2
    for runner in direct bash zsh; do
        out="$work/out" rc=0
        case "$runner" in
            direct) "$script" "$repo" "$@" > "$out" 2> "$work/err" || rc=$? ;;
            *) "$runner" "$script" "$repo" "$@" > "$out" 2> "$work/err" || rc=$? ;;
        esac
        got="$(cat "$out")"
        if [ "$rc" -ne 0 ]; then
            not_ok "$name ($runner): exit $rc: $(cat "$work/err")"
        elif [ "$got" != "$want" ]; then
            not_ok "$name ($runner): got [$got], want [$want]"
        elif grep -qE 'renderib|coreib' "$out" || grep -vq -e '^#' -e ':' "$out"; then
            not_ok "$name ($runner): mangled target in [$got]"
        else
            ok "$name ($runner)"
        fi
    done
}

list() { git -C "$repo" switch -q "$1"; }
nl='
'
header='# parallel: 3'

list app-core-lib;      expect 'app-core src change' "$header${nl}tessera-app-core:lib" --list-targets
list render-golden;     expect 'render test file' "$header${nl}tessera-render:golden" --list-targets
list render-helper;     expect 'render test helper' "$header${nl}tessera-render:tests" --list-targets
list devwin-bin;        expect 'bin-only crate' "$header${nl}tessera-devwin:bins" --list-targets
list test-only-untimed; expect 'untimed test-only change' "$header" --list-targets
list overrides;         expect 'nextest overrides' "$header${nl}tessera-config:reload${nl}tessera-term:lib" --list-targets
list everything
all="tessera-app-core:lib${nl}tessera-config:reload${nl}tessera-devwin:bins${nl}tessera-render:golden${nl}tessera-render:tests${nl}tessera-term:lib"
expect 'every trigger' "$header${nl}$all" --list-targets
expect 'explicit --load and --parallel' "# parallel: 8${nl}tessera-app-core:lib${nl}tessera-app-core:lib-extra${nl}${all#*"$nl"}" \
    --list-targets --parallel 8 --load tessera-app-core:lib-extra --load tessera-app-core:lib

if [ -e "$work/cargo.calls" ]; then not_ok "--list-targets called cargo: $(cat "$work/cargo.calls")"; else ok "--list-targets builds nothing"; fi

# Usage errors exit 2.
for bad in 0 9 12 x ''; do
    rc=0
    "$script" "$repo" --list-targets --parallel "$bad" > /dev/null 2>&1 || rc=$?
    if [ "$rc" -eq 2 ]; then ok "--parallel '$bad' rejected"; else not_ok "--parallel '$bad': exit $rc, want 2"; fi
done
rc=0
"$script" "$repo" --list-targets --parallel > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 2 ]; then ok "--parallel without a value rejected"; else not_ok "--parallel without a value: exit $rc, want 2"; fi
rc=0
"$script" "$repo" --list-targets --base no-such-ref > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 2 ]; then ok "--list-targets with an unknown base rejected"; else not_ok "unknown base: exit $rc, want 2"; fi
rc=0
"$script" --help > "$work/help" 2>&1 || rc=$?
if [ "$rc" -eq 0 ] && grep -q -- '--parallel <N>' "$work/help" && grep -q -- '--list-targets' "$work/help"; then
    ok "--help documents --parallel and --list-targets"
else
    not_ok "--help: exit $rc or flags missing"
fi

# The load check runs 2 tests x 10 runs and never has more than --parallel of them in flight. The fixture's gate
# and commit trailers may pass or fail (exit 0 or 1); only the load line and the samples matter here.
list app-core-lib
for n in default 1 3; do
    rm -f "$work/samples"
    rc=0
    if [ "$n" = default ]; then
        "$script" "$repo" > "$work/load.log" 2>&1 || rc=$?
        cap=3
    else
        "$script" "$repo" --parallel "$n" > "$work/load.log" 2>&1 || rc=$?
        cap="$n"
    fi
    max="$(sort -n "$work/samples" 2>/dev/null | tail -1)"
    runs="$(wc -l < "$work/samples" 2>/dev/null | tr -d ' ')"
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ]; then
        not_ok "load --parallel $n: exit $rc, want 0 or 1: $(cat "$work/load.log")"
    elif ! grep -q "PASS.* load tessera-app-core:lib (20 runs, at most $cap at a time)\$" "$work/load.log"; then
        not_ok "load --parallel $n: no PASS line: $(cat "$work/load.log")"
    elif [ "$runs" != 20 ] || [ -z "$max" ] || [ "$max" -gt "$cap" ]; then
        not_ok "load --parallel $n: $runs runs, at most $max at once, cap $cap"
    else
        ok "load --parallel $n: 20 runs, at most $max at once (cap $cap)"
    fi
done

# A run killed before it records its exit status fails the load check instead of shrinking the sample.
rm -rf "$work/killed"
rc=0
FAKE_KILL_PARENT=1 "$script" "$repo" > "$work/kill.log" 2>&1 || rc=$?
if [ "$rc" -eq 1 ] && grep -q "FAIL.* load tessera-app-core:lib: 1 of 20 runs recorded no exit status" "$work/kill.log"; then
    ok "a run without an exit status fails the load check"
else
    not_ok "a run without an exit status: exit $rc: $(cat "$work/kill.log")"
fi

# SIGINT while the pool is full stops every run in flight and exits 130.
: > "$work/samples"
rm -rf "$work/active" && mkdir "$work/active"
FAKE_HANG=1 "$script" "$repo" > "$work/int.log" 2>&1 &
pid=$!
i=0
while [ "$(wc -l < "$work/samples" | tr -d ' ')" -lt 3 ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i + 1)); done
in_flight="$(wc -l < "$work/samples" | tr -d ' ')"
kill -INT "$pid"
# Bounded: a script that ignores the signal would otherwise wait out every hung run.
i=0
while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
kill -0 "$pid" 2>/dev/null && { printf 'SIGINT ignored after 10s; killing\n' >> "$work/int.log"; kill -KILL "$pid"; }
rc=0
wait "$pid" || rc=$?
sleep 0.5
stray="$(pgrep -f "$work/bin/fake-test" || true)"
if [ "$in_flight" = 3 ] && [ "$rc" -eq 130 ] && [ -z "$stray" ] \
    && grep -q 'interrupted: stopped 3 load run(s)' "$work/int.log"; then
    ok "SIGINT stops the 3 runs in flight and exits 130"
else
    not_ok "SIGINT: $in_flight in flight, exit $rc, stray pids [$stray]: $(cat "$work/int.log")"
fi
pkill -f "$work/bin/fake-test" 2>/dev/null || true

if [ "$failures" -ne 0 ]; then
    printf '%s check(s) failed\n' "$failures"
    exit 1
fi
printf 'all checks passed\n'
