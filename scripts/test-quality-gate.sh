#!/usr/bin/env zsh
# Exercise scripts/quality-gate.sh's nextest watchdog and release timing retry without running the gate (#166).
#
#   scripts/test-quality-gate.sh          (`just quality-gate-test`)
#
# Every case runs the gate under zsh with TESSERA_GATE_SELFTEST=1 and TESSERA_GATE_TEST_STEP, so it runs one step and
# never reads or writes a pass stamp. A fake `cargo` on PATH records each call with the name of its parent process
# and, for `cargo nextest run` without --no-run, passes, fails or hangs as FAKE_CARGO_RUN says. Cases:
#   - the test binaries are built (`--no-run`, same arguments) before, and outside, the watchdog;
#   - the watchdog's alarm stops a hanging run and the gate reports it (the control for the next case);
#   - a SIGKILLed gate leaves no watchdog behind: it exits within 5 s and its alarm never stops the run;
#   - the release timing retry: failure then pass under load passes ("failed under load ..., retried"), two failures
#     under load fail, one failure below the core count fails without a retry;
#   - without TESSERA_GATE_SELFTEST=1 the hooks are inert (a full gate run with every cargo call faked, in a
#     throwaway repository so no real pass stamp is written).
# Requires git, perl and ps on PATH.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
gate="$here/quality-gate.sh"
root="${here:h}"

work="$(mktemp -d "${TMPDIR:-/tmp}/quality-gate-test.XXXXXX")"
cleanup() {
    # A hanging fake left behind by the orphan case: its process group is its own pid (the watchdog's setpgrp).
    if [[ -s "$work/running" ]]; then
        kill -KILL -- "-$(cut -d' ' -f1 "$work/running")" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

failures=0
ok() { print -r -- "ok   $1"; }
not_ok() { print -r -- "FAIL $1"; failures=$((failures + 1)); }

mkdir -p "$work/bin"
cat > "$work/bin/cargo" <<'EOF'
#!/bin/sh
parent="$(ps -o comm= -p "$PPID" | sed 's|.*/||')"
printf '%s|%s\n' "$parent" "$*" >> "$FAKE_WORK/cargo.calls"
[ "${1:-} ${2:-}" = "nextest run" ] || exit 0
case " $* " in
    *" --no-run "*) exit 0 ;;
    *" --release "*) mode="${FAKE_CARGO_RELEASE:-pass}" ;;
    *) mode="${FAKE_CARGO_RUN:-pass}" ;;
esac
case "$mode" in
    pass) exit 0 ;;
    fail) exit 100 ;;
    hang)
        trap 'echo fired > "$FAKE_WORK/fired"; exit 1' TERM
        echo "$$ $PPID" > "$FAKE_WORK/running"
        while :; do sleep 0.2; done ;;
esac
EOF
chmod +x "$work/bin/cargo"
export FAKE_WORK="$work" PATH="$work/bin:$PATH"

# Runs one gate step with the self-test hooks on; extra environment as `NAME=value` arguments.
run_step() {
    local step="$1"
    shift
    env TESSERA_GATE_SELFTEST=1 TESSERA_GATE_TEST_STEP="$step" "$@" zsh "$gate" "$root"
}

# Waits up to $2 seconds (in tenths) for the command in $1 to succeed.
wait_for() {
    local tries=$(( $2 * 10 ))
    while (( tries-- > 0 )); do
        eval "$1" && return 0
        sleep 0.1
    done
    return 1
}

# --- the build runs before and outside the watchdog
: > "$work/cargo.calls"
if run_step tests FAKE_CARGO_RUN=pass > "$work/out" 2>&1; then
    calls=("${(@f)$(< "$work/cargo.calls")}")
    if [[ "${calls[1]}" == "zsh|nextest run --no-run --color never --workspace" \
        && "${calls[2]}" == "perl|nextest run --color never --workspace" && ${#calls} == 2 ]]; then
        ok "nextest builds the tests (--no-run, same arguments) outside the watchdog, then runs them under it"
    else
        not_ok "build-then-run order: ${(j:; :)calls}"
    fi
    if grep -n -e '-- building the tests' -e '-- running the tests under the watchdog' "$work/out" \
        | cut -d: -f2- | tr '\n' ' ' | grep -q '^-- building.*-- running'; then
        ok "the build line precedes the watchdog start in the output"
    else
        not_ok "build and watchdog lines: $(< "$work/out")"
    fi
else
    not_ok "tests step with a passing fake failed: $(< "$work/out")"
fi

# --- the alarm fires on a hang (control for the orphan case)
rm -f "$work/fired" "$work/running"
if run_step tests FAKE_CARGO_RUN=hang TESSERA_GATE_TEST_WATCHDOG_SECONDS=2 > "$work/out" 2>&1; then
    not_ok "a hanging run passed the gate"
elif [[ -e "$work/fired" ]] && grep -q 'killed by the gate watchdog' "$work/out"; then
    ok "the watchdog stops a hanging run after its limit and the gate names it"
else
    not_ok "watchdog alarm: fired=$([[ -e "$work/fired" ]] && echo yes || echo no) $(< "$work/out")"
fi
rm -f "$work/running"

# --- a SIGKILLed gate leaves no watchdog behind
rm -f "$work/fired" "$work/running"
env TESSERA_GATE_SELFTEST=1 TESSERA_GATE_TEST_STEP=tests FAKE_CARGO_RUN=hang TESSERA_GATE_TEST_WATCHDOG_SECONDS=3 \
    zsh "$gate" "$root" > "$work/out" 2>&1 &
# env execs zsh, so this is the gate's own pid.
gate_pid=$!
if wait_for '[[ -s "$work/running" ]]' 20; then
    fake_pid="$(cut -d' ' -f1 "$work/running")"
    watchdog_pid="$(cut -d' ' -f2 "$work/running")"
    watchdog_parent="$(ps -o ppid= -p "$watchdog_pid" | tr -d ' ')"
    kill -KILL "$gate_pid"
    wait "$gate_pid" 2>/dev/null || true
    if [[ "$watchdog_parent" != "$gate_pid" ]]; then
        not_ok "the watchdog ($watchdog_pid) is not the gate's ($gate_pid) child but $watchdog_parent's"
    elif wait_for '! kill -0 "$watchdog_pid" 2>/dev/null' 5; then
        ok "the watchdog exits within 5 s of its gate being SIGKILLed"
    else
        not_ok "the watchdog $watchdog_pid outlived its gate by 5 s"
    fi
    # Past the 3 s alarm: an orphaned watchdog that kept its alarm would have stopped the fake by now.
    sleep 2
    if [[ ! -e "$work/fired" ]] && kill -0 "$fake_pid" 2>/dev/null; then
        ok "the orphaned watchdog's alarm never stops the run"
    else
        not_ok "the orphaned watchdog's alarm fired"
    fi
else
    kill -KILL "$gate_pid" 2>/dev/null || true
    not_ok "the hanging fake never started: $(< "$work/out")"
fi

# --- release timing retry
ncpu="$(sysctl -n hw.ncpu)"
high=$(( ncpu + 1 ))
timing() { run_step release-timing TESSERA_GATE_TEST_TIMING_RESULTS="$1" TESSERA_GATE_TEST_LOAD="$2"; }

if timing 1,0 "$high" > "$work/out" 2>&1 && grep -q "failed under load $high, retried" "$work/out"; then
    ok "failure then pass under load: the step passes after one retry"
else
    not_ok "failure then pass under load: $(< "$work/out")"
fi
if ! timing 1,1 "$high" > "$work/out" 2>&1 && grep -q "retried" "$work/out" \
    && [[ "$(grep -c -- '-- release timing run' "$work/out")" == 2 ]]; then
    ok "two failures under load: the step fails after one retry"
else
    not_ok "two failures under load: $(< "$work/out")"
fi
if ! timing 1,0 0 > "$work/out" 2>&1 && ! grep -q "retried" "$work/out" \
    && [[ "$(grep -c -- '-- release timing run' "$work/out")" == 1 ]]; then
    ok "one failure below the core count: the step fails without a retry"
else
    not_ok "one failure below the core count: $(< "$work/out")"
fi
if timing 0 "$high" > "$work/out" 2>&1 && ! grep -q "retried" "$work/out"; then
    ok "a pass is not retried"
else
    not_ok "a pass: $(< "$work/out")"
fi

# --- without TESSERA_GATE_SELFTEST=1 every hook is inert: a full gate run (every cargo call faked) in a throwaway
# repository runs every step from fmt on and the real release timing run, whose failure the stub results would hide.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/fuzz"
: > "$repo/Cargo.lock"
print -r -- 'exit 0' > "$repo/scripts/ffi-c-test.sh"
chmod +x "$repo/scripts/ffi-c-test.sh"
git -C "$repo" init -q
git -C "$repo" add -A
git -C "$repo" -c user.name=test -c user.email=test@example.com commit -q -m init
if env -u TESSERA_GATE_SELFTEST TESSERA_GATE_TEST_STEP=release-timing TESSERA_GATE_TEST_TIMING_RESULTS=0 \
    TESSERA_GATE_TEST_LOAD=0 TESSERA_GATE_TEST_WATCHDOG_SECONDS=x FAKE_CARGO_RELEASE=fail \
    zsh "$gate" --force "$repo" > "$work/out" 2>&1; then
    not_ok "hooks without TESSERA_GATE_SELFTEST=1: the gate passed: $(< "$work/out")"
elif grep -q '== fmt' "$work/out" && grep -q '== ffi C tests' "$work/out" \
    && grep -q 'quality gate FAILED: nextest run failed' "$work/out" && ! grep -q 'self-test' "$work/out"; then
    ok "without TESSERA_GATE_SELFTEST=1 the hooks are inert: full gate, real release timing run"
else
    not_ok "hooks without TESSERA_GATE_SELFTEST=1: $(< "$work/out")"
fi

if (( failures )); then
    print -r -- "$failures check(s) failed"
    exit 1
fi
print -r -- "all checks passed"
