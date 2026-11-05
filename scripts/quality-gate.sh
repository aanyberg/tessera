#!/usr/bin/env zsh
# The mechanical quality gate every task branch must pass before review, verification and merge.
# Usage: scripts/quality-gate.sh [--force] [worktree-dir]   (defaults to the repo root)
#
# A pass on a clean tree is recorded per commit and toolchain in the repository's shared git dir, so the
# implementer, critic and verifiers running the gate on the same commit pay for it once. A dirty tree is never
# recorded or skipped. The key also covers the toolchain and this script, so a changed gate runs again. A reused pass
# does not refresh cargo-deny's advisory database or re-sample flaky tests; CI does both on every PR.
# --force (or QUALITY_GATE_FORCE=1) runs the gate even when the commit already passed.
#
# Self-test hooks (scripts/test-quality-gate.sh, `just quality-gate-test`; #166) are honoured only when
# TESSERA_GATE_SELFTEST=1, and a run without it ignores every TESSERA_GATE_TEST_* variable:
#   TESSERA_GATE_TEST_STEP             run one step and exit, without the stamp or any other step:
#                                      `tests` (the workspace nextest run), `release-timing` or `ffi-c-tests`
#   TESSERA_GATE_TEST_TIMING_RESULTS   comma-separated exit codes returned by successive release timing runs
#                                      instead of running nextest
#   TESSERA_GATE_TEST_LOAD             the 1-minute load average the release timing retry reads
#   TESSERA_GATE_TEST_WATCHDOG_SECONDS the watchdog limit in seconds instead of TESSERA_GATE_NEXTEST_MINUTES
set -euo pipefail
force="${QUALITY_GATE_FORCE:-}"
dir=""
for arg in "$@"; do
    case "$arg" in
        --force) force=1 ;;
        -*) print -u2 "usage: scripts/quality-gate.sh [--force] [worktree-dir]"; exit 2 ;;
        *) [[ -z "$dir" ]] || { print -u2 "usage: scripts/quality-gate.sh [--force] [worktree-dir]"; exit 2; }; dir="$arg" ;;
    esac
done
gate_hash="$(shasum < "$0" | cut -c1-12)"
dir="${dir:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$dir"
step() { print -P "%F{cyan}== $1%f"; }
selftest=""
[[ "${TESSERA_GATE_SELFTEST:-}" == 1 ]] && selftest=1

# .config/nextest.toml terminates a test after its slow-timeout (300 s by default) so a hung test cannot stall the gate
# (#137). nextest then fails the run; say so plainly instead of leaving a bare exit code. Below the test level a
# watchdog kills nextest and its descendants after TESSERA_GATE_NEXTEST_MINUTES (default 30; macOS has no `timeout`).
# The test binaries are built first, outside the watchdog, so its window covers the run only (#166).
# nextest runs with --color never so the TIMEOUT grep also works under CARGO_TERM_COLOR=always. The status is read from
# the pipeline (pipestatus), never from tee.
nextest_minutes="${TESSERA_GATE_NEXTEST_MINUTES:-30}"
[[ "$nextest_minutes" == <1-> ]] || { print -u2 "TESSERA_GATE_NEXTEST_MINUTES must be a positive integer, got '$nextest_minutes'"; exit 2; }
watchdog_seconds=$(( nextest_minutes * 60 ))
watchdog_var=TESSERA_GATE_NEXTEST_MINUTES  # the variable that set the limit, named when the watchdog fires
if [[ -n "$selftest" && -n "${TESSERA_GATE_TEST_WATCHDOG_SECONDS:-}" ]]; then
    watchdog_seconds="$TESSERA_GATE_TEST_WATCHDOG_SECONDS"
    watchdog_var=TESSERA_GATE_TEST_WATCHDOG_SECONDS
    [[ "$watchdog_seconds" == <1-> ]] || { print -u2 "TESSERA_GATE_TEST_WATCHDOG_SECONDS must be a positive integer, got '$watchdog_seconds'"; exit 2; }
fi
nextest_log="$(mktemp)"
trap 'rm -f "$nextest_log"' EXIT
# `build` while the test binaries compile, `run` while they run; report_nextest_failure reads it.
nextest_phase=""

# Runs <command> in its own process group and kills the group and its descendants after <seconds>. Output goes to
# the terminal and the log. Exit status: the command's own, or 124 when the watchdog fired. The watchdog polls
# getppid() and exits without acting once the gate is gone (a SIGKILLed gate leaves it orphaned, #166), so an
# orphaned alarm never kills a later run's processes.
# Usage: watchdog <seconds> <command> [args]...
watchdog() {
    local st
    : > "$nextest_log"
    set +e
    # Through tee, nextest's stdout is not a tty, so its live progress bar is hidden: only result lines appear.
    perl -e '
        use POSIX ":sys_wait_h";
        my ($secs, @cmd) = @ARGV;
        my $parent = getppid();
        my $pid = fork() // die "fork: $!";
        if (!$pid) { setpgrp(0, 0); exec @cmd or die "exec $cmd[0]: $!"; }
        sub descendants {
            my %kids; open(my $ps, "-|", "ps", "-A", "-o", "pid=,ppid=") or return ();
            while (<$ps>) { my ($c, $p) = split; push @{$kids{$p}}, $c; }
            my @todo = ($_[0]); my @all;
            while (@todo) { my $n = shift @todo; push @all, $n; push @todo, @{$kids{$n} // []}; }
            return @all;
        }
        sub stop {
            my @all = descendants($pid);
            kill "TERM", -$pid;
            for (1 .. 100) { last if waitpid($pid, WNOHANG) == $pid; select(undef, undef, undef, 0.1); }
            kill "KILL", -$pid, @all;
            waitpid($pid, 0) if kill 0, $pid;
        }
        my $fired = 0;
        $SIG{ALRM} = sub { $fired = 1; stop(); };
        $SIG{INT} = $SIG{TERM} = sub { stop(); exit 130; };
        alarm($secs);
        my $r;
        while (1) {
            $r = waitpid($pid, WNOHANG);
            last if $r == $pid || $r == -1 || $fired;
            if (getppid() != $parent) { alarm(0); exit 1; }
            select(undef, undef, undef, 0.2);
        }
        if ($fired) { exit 124; }
        exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
    ' "$@" 2>&1 | tee -a "$nextest_log"
    st=${pipestatus[1]}
    set -e
    return $st
}

# Builds the test binaries for `cargo nextest run <args>` (`--no-run`, same arguments), then runs them under the
# watchdog. Exit status: cargo's or nextest's own, or 124 when the watchdog fired.
run_nextest() {
    nextest_phase=build
    print -r -- "-- building the tests: cargo nextest run --no-run $*"
    cargo nextest run --no-run --color never "$@" || return $?
    nextest_phase=run
    print -r -- "-- running the tests under the watchdog (${watchdog_seconds} s)"
    watchdog "$watchdog_seconds" cargo nextest run --color never "$@"
}

# Prints why a nextest run failed. $1 is its exit status.
report_nextest_failure() {
    if [[ "$nextest_phase" == build ]]; then
        print -u2 -P "%F{red}quality gate FAILED: building the tests failed%f"
    elif [[ "$1" == 124 ]]; then
        print -u2 -P "%F{red}quality gate FAILED: nextest was killed by the gate watchdog after ${watchdog_seconds} s (${watchdog_var})%f"
    elif grep -q -E '^ *TIMEOUT ' "$nextest_log"; then
        print -u2 -P "%F{red}quality gate FAILED: nextest terminated hung test(s) after their slow-timeout (.config/nextest.toml):%f"
        grep -E '^ *TIMEOUT ' "$nextest_log" | sort -u >&2
    else
        print -u2 -P "%F{red}quality gate FAILED: nextest run failed%f"
    fi
}

tests_step() {
    run_nextest --workspace || { report_nextest_failure $?; exit 1; }
}

# The C programs run under the same watchdog as nextest (a hung C test would otherwise stall the gate; #235).
ffi_c_tests_step() {
    # `watchdog` truncates $nextest_log: nothing in it survives this step, so no later step may rely on it.
    local st=0
    watchdog "$watchdog_seconds" scripts/ffi-c-test.sh "$dir" || st=$?
    (( st == 0 )) && return 0
    if (( st == 124 )); then
        print -u2 -P "%F{red}quality gate FAILED: the ffi C tests were killed by the gate watchdog after ${watchdog_seconds} s (${watchdog_var})%f"
    else
        print -u2 -P "%F{red}quality gate FAILED: the ffi C tests failed (exit $st)%f"
    fi
    exit 1
}

# Timing bars that only run in release builds (#114, #175): the reader lock hold, the view metrics under an output
# flood, scrolling through a long history and captures under a flood while scrolled back. They are in the `timing`
# test group (one at a time). A failure under load (1-min load average >= the core count) is retried once; a second
# failure, any failure below that load and any failure of the build (`--no-run`) fail the gate.
timing_stub=(${(s:,:)${TESSERA_GATE_TEST_TIMING_RESULTS:-}})
release_timing() {
    if [[ -n "$selftest" && -n "${TESSERA_GATE_TEST_TIMING_RESULTS:-}" ]]; then
        (( ${#timing_stub} )) || { print -u2 "TESSERA_GATE_TEST_TIMING_RESULTS has no result left"; return 2; }
        local st="${timing_stub[1]}"
        shift timing_stub
        nextest_phase=run
        : > "$nextest_log"
        print -r -- "-- release timing run (self-test): exit $st"
        return "$st"
    fi
    run_nextest --release -p tessera-app-core -E 'kind(lib) & (test(reader_lock_hold_is_bounded) | test(view_metrics_are_fast_while_output_floods_the_terminal) | test(scrolling_through_a_long_history_is_smooth) | test(a_flood_under_a_scrolled_view_keeps_captures_short))'
}
load_1min() {
    if [[ -n "$selftest" && -n "${TESSERA_GATE_TEST_LOAD:-}" ]]; then
        print -r -- "$TESSERA_GATE_TEST_LOAD"
    else
        sysctl -n vm.loadavg | awk '{print $2}'
    fi
}
release_timing_step() {
    local release_status load
    release_timing && release_status=0 || release_status=$?
    if (( release_status != 0 )); then
        load="$(load_1min)"
        # A failed build is not a timing failure: load does not explain it, so it is never retried.
        if [[ "$nextest_phase" != build ]] && (( load >= $(sysctl -n hw.ncpu) )); then
            print -P "%F{yellow}release timing tests failed under load ${load}, retried%f"
            release_timing || { report_nextest_failure $?; exit 1; }
        else
            report_nextest_failure "$release_status"
            exit 1
        fi
    fi
}

if [[ -n "$selftest" && -n "${TESSERA_GATE_TEST_STEP:-}" ]]; then
    case "$TESSERA_GATE_TEST_STEP" in
        tests) step "tests"; tests_step ;;
        release-timing) step "release timing tests"; release_timing_step ;;
        ffi-c-tests) step "ffi C tests"; ffi_c_tests_step ;;
        *) print -u2 "TESSERA_GATE_TEST_STEP must be tests, release-timing or ffi-c-tests, got '$TESSERA_GATE_TEST_STEP'"; exit 2 ;;
    esac
    print -P "%F{green}quality gate self-test step $TESSERA_GATE_TEST_STEP passed%f"
    exit 0
fi

stamp=""
if [[ -z "$(git status --porcelain)" ]]; then
    head="$(git rev-parse HEAD)"
    stamp="$(git rev-parse --path-format=absolute --git-common-dir)/quality-gate-passed/$head-$(rustc -V | shasum | cut -c1-12)-$gate_hash"
    if [[ -z "$force" && -e "$stamp" ]]; then
        print -P "%F{green}quality gate already passed for ${head:0:7} with this toolchain (--force re-runs it)%f"
        exit 0
    fi
fi

step "fmt";     cargo fmt --all --check
step "clippy";  cargo clippy --workspace --all-targets -- -D warnings
step "tests";   tests_step
step "rustdoc"; RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
step "deny";    cargo deny check
step "ffi release build"; cargo build --release -p tessera-ffi
# The C programs in crates/tessera-ffi/tests/c against the generated header (#175); surface.c skips itself when
# TESSERA_SKIP_GPU_TESTS is set.
step "ffi C tests"; ffi_c_tests_step
step "release timing tests"; release_timing_step
# fuzz/ is not a workspace member (#14). Its lock file is the workspace's, copied in before every build so the
# fuzzers test the workspace's dependency versions (fuzz/Cargo.lock is ignored; cargo only adds libfuzzer-sys and its dependencies).
# Format and stable clippy always run, so a rename breaks here before merge; the libFuzzer build needs a nightly
# toolchain and cargo-fuzz, optional for contributors (the nightly fuzz workflow always builds it).
cp Cargo.lock fuzz/Cargo.lock
step "fuzz fmt";    cargo fmt --manifest-path fuzz/Cargo.toml --check
step "fuzz clippy"; cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
if [[ "$(rustup toolchain list)" == (|*$'\n')nightly* ]] && cargo +nightly fuzz --version > /dev/null 2>&1; then
    step "fuzz build"; cargo +nightly fuzz build
else
    print -P "%F{yellow}== fuzz build skipped: needs a nightly toolchain and cargo-fuzz (see docs/software-factory.md)%f"
fi

# Record the pass only when the tree and HEAD are still what the gate started on.
if [[ -n "$stamp" && -z "$(git status --porcelain)" && "$(git rev-parse HEAD)" == "$head" ]]; then
    mkdir -p "${stamp:h}" && : > "$stamp"
fi
print -P "%F{green}quality gate passed in $dir%f"
