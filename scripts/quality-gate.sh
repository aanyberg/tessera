#!/usr/bin/env zsh
# The mechanical quality gate every task branch must pass before review, verification and merge.
# Usage: scripts/quality-gate.sh [--force] [worktree-dir]   (defaults to the repo root)
#
# A pass on a clean tree is recorded per commit and toolchain in the repository's shared git dir, so the
# implementer, critic and verifiers running the gate on the same commit pay for it once. A dirty tree is never
# recorded or skipped. The key also covers the toolchain and this script, so a changed gate runs again. A reused pass
# does not refresh cargo-deny's advisory database or re-sample flaky tests; CI does both on every PR.
# --force (or QUALITY_GATE_FORCE=1) runs the gate even when the commit already passed.
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
step "tests"
# .config/nextest.toml terminates a test after its slow-timeout (300 s by default) so a hung test cannot stall the gate
# (#137). nextest then fails the run; say so plainly instead of leaving a bare exit code. Below the test level a
# watchdog kills nextest and its descendants after TESSERA_GATE_NEXTEST_MINUTES (default 30; macOS has no `timeout`).
# nextest runs with --color never so the TIMEOUT grep also works under CARGO_TERM_COLOR=always. The status is read from
# the pipeline (pipestatus), never from tee.
nextest_minutes="${TESSERA_GATE_NEXTEST_MINUTES:-30}"
[[ "$nextest_minutes" == <1-> ]] || { print -u2 "TESSERA_GATE_NEXTEST_MINUTES must be a positive integer, got '$nextest_minutes'"; exit 2; }
nextest_log="$(mktemp)"
trap 'rm -f "$nextest_log"' EXIT

# Runs `cargo nextest run <args>` in its own process group under the watchdog; output goes to the terminal and the log.
# Exit status: nextest's own, or 124 when the watchdog fired.
run_nextest() {
    local st
    : > "$nextest_log"
    set +e
    perl -e '
        use POSIX ":sys_wait_h";
        my ($secs, @cmd) = @ARGV;
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
        alarm($secs * 60);
        my $r;
        while (1) { $r = waitpid($pid, 0); last if $r == $pid || $fired; }
        if ($fired) { exit 124; }
        exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
    ' "$nextest_minutes" cargo nextest run --color never "$@" 2>&1 | tee -a "$nextest_log"
    st=${pipestatus[1]}
    set -e
    return $st
}

# Prints why a nextest run failed. $1 is its exit status.
report_nextest_failure() {
    if [[ "$1" == 124 ]]; then
        print -u2 -P "%F{red}quality gate FAILED: nextest was killed by the gate watchdog after ${nextest_minutes} minutes (TESSERA_GATE_NEXTEST_MINUTES)%f"
    elif grep -q -E '^ *TIMEOUT ' "$nextest_log"; then
        print -u2 -P "%F{red}quality gate FAILED: nextest terminated hung test(s) after their slow-timeout (.config/nextest.toml):%f"
        grep -E '^ *TIMEOUT ' "$nextest_log" | sort -u >&2
    else
        print -u2 -P "%F{red}quality gate FAILED: nextest run failed%f"
    fi
}

run_nextest --workspace || { report_nextest_failure $?; exit 1; }
step "rustdoc"; RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
step "deny";    cargo deny check
step "ffi release build"; cargo build --release -p tessera-ffi
# Timing bars that only run in release builds (#114): the reader lock hold and the view metrics under an output flood.
# They are in the `timing` test group (one at a time). A failure under load (1-min load average >= the core count) is
# retried once; a second failure, or any failure below that load, fails the gate.
step "release timing tests"
release_timing() {
    run_nextest --release -p tessera-app-core -E 'kind(lib) & (test(reader_lock_hold_is_bounded) | test(view_metrics_are_fast_while_output_floods_the_terminal))'
}
release_timing && release_status=0 || release_status=$?
if (( release_status != 0 )); then
    load="$(sysctl -n vm.loadavg | awk '{print $2}')"
    if (( load >= $(sysctl -n hw.ncpu) )); then
        print -P "%F{yellow}release timing tests failed under load ${load}, retried%f"
        release_timing || { report_nextest_failure $?; exit 1; }
    else
        report_nextest_failure "$release_status"
        exit 1
    fi
fi
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
