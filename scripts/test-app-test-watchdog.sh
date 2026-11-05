#!/usr/bin/env zsh
# Exercise scripts/app-test-watchdog.sh with a fake xcodebuild and a fake `sample` (#228).
#
#   scripts/test-app-test-watchdog.sh          (`just app-test-watchdog-test`)
#
# The fake xcodebuild prints xcodebuild-style test lines and records its arguments. Each run takes the next mode from
# FAKE_RUNS: `pass`, or `hang`: it starts '-[FakeTests.HangTests testHangs]' (in the "started (Iteration n of m)."
# form of -test-iterations) and a fake test host under <derivedDataPath>/Build/Products/ that ignores SIGTERM and keeps
# a `sleep` child, waits for it, then lists the test under "Failing tests:" as xcodebuild does for a killed host.
# `hang-fail` also fails '-[FakeTests.OtherTests testOther]'; `hang-hang` then hangs in
# '-[FakeTests.HangTests testHangsToo]' too. A run with -only-testing:FakeTests/HangTests/testHangs
# runs that test alone (`pass` or `hang`). The fake `sample` writes a call graph whose main thread hangs inside the
# TSan runtime (`__tsan::SlotLock`) with FAKE_SAMPLE_TSAN=1 and in a condition wait otherwise; both carry the
# `__tsan::` frames every TSan process has. TESSERA_APP_TEST_HANG_SECONDS is 5. Cases:
#   - a run whose tests all pass exits 0 and leaves no sample, also when writing its results takes longer than the
#     limit (`slow-results`);
#   - a hang outside the TSan runtime: the host is killed within the limit + 10 s, the run fails naming the test and the
#     limit, the sample log exists and there is no retry;
#   - after the kill, a restarted run that goes on is left to finish and a restarted host it leaves behind is
#     killed; a run that starts no test within the limit is stopped, with the restarted host outside its group;
#   - a TSan hang and a hang outside the runtime in one attempt fail without a rerun, in either order;
#   - a process under the products path that predates the run survives a hang and a SIGINT to the watchdog's group;
#   - a TSan hang reruns only the hung test, once, with the other arguments kept and the run's -only-testing replaced
#     ("TSan hang in ..., retried"), and passes when that rerun passes; a TSan hang plus another failure fails without
#     a rerun; a second hang in the rerun fails, keeping both samples.
# Every case runs through the app-test recipe's pipeline (`set -o pipefail; ... | tee | tail`) and checks that no fake
# host or its `sleep` is left behind.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
watchdog="$here/app-test-watchdog.sh"

work="$(mktemp -d "${TMPDIR:-/tmp}/app-test-watchdog-test.XXXXXX")"
work="${work:A}"
cleanup() {
    setopt localoptions noerrexit
    local pids
    pids=($(ps -A -o pid=,args= | awk -v p="$work/" 'index($0, p) && $2 != "awk" { print $1 }'))
    (( ${#pids} )) && kill -KILL $pids 2>/dev/null
    pids=($(cat "$work"/*/sleeps 2>/dev/null))
    (( ${#pids} )) && kill -KILL $pids 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

failures=0
ok() { print -r -- "ok   $1"; }
not_ok() { print -r -- "FAIL $1"; failures=$((failures + 1)); }

mkdir -p "$work/bin"
cat > "$work/bin/xcodebuild" <<'EOF'
#!/bin/sh
echo "$*" >> "$FAKE_WORK/args"
dd="" only=""
for a in "$@"; do
    [ "$prev" = -derivedDataPath ] && dd="$a"
    [ "$a" = -only-testing:FakeTests/HangTests/testHangs ] && only=1
    prev="$a"
done
n=$(( $(cat "$FAKE_WORK/runs" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$FAKE_WORK/runs"
mode="$(echo "$FAKE_RUNS" | cut -d, -f"$n")"
hang() {
    echo "Test Case '-[FakeTests.HangTests testHangs]' started (Iteration 1 of 1)."
    "$dd/Build/Products/Debug/FakeHost" &
    wait
    echo "Restarting after unexpected exit, crash, or test timeout; summary will include totals from previous launches."
}
failing() {
    echo "Failing tests:"
    for t in "$@"; do printf '\t%s\n' "$t"; done
    echo
    echo "** TEST FAILED **"
    exit 65
}
echo "Test Suite 'Selected tests' started at 2026-11-03 12:00:00.000."
if [ -n "$only" ]; then
    if [ "$mode" = hang ]; then hang; failing "HangTests.testHangs()"; fi
    echo "Test Case '-[FakeTests.HangTests testHangs]' started."
    echo "Test Case '-[FakeTests.HangTests testHangs]' passed (0.001 seconds)."
    echo "** TEST SUCCEEDED **"
    exit 0
fi
echo "Test Case '-[FakeTests.PassTests testPasses]' started."
echo "Test Case '-[FakeTests.PassTests testPasses]' passed (0.001 seconds)."
case "$mode" in
    hang*) hang ;;
esac
case "$mode" in
    hang)
        failing "HangTests.testHangs()" ;;
    hang-hang)
        echo "Test Case '-[FakeTests.HangTests testHangsToo]' started."
        "$dd/Build/Products/Debug/FakeHost" &
        wait
        echo "Restarting after unexpected exit, crash, or test timeout; summary will include totals from previous launches."
        failing "HangTests.testHangs()" "HangTests.testHangsToo()" ;;
    hang-fail)
        echo "Test Case '-[FakeTests.OtherTests testOther]' started."
        echo "Test Case '-[FakeTests.OtherTests testOther]' failed (0.001 seconds)."
        failing "HangTests.testHangs()" "OtherTests.testOther()" ;;
    hang-continue)
        # The restarted host, in a process group of its own as launchd parents the real one, outlives this fake;
        # the restarted run goes on for longer than the limit: each test resets the clock.
        perl -e 'setpgrp(0, 0); exec @ARGV' "$dd/Build/Products/Debug/FakeHost" &
        for t in 1 2 3 4 5 6; do
            echo "Test Case '-[FakeTests.PassTests testAfterTheRestart$t]' started."
            sleep 1
            echo "Test Case '-[FakeTests.PassTests testAfterTheRestart$t]' passed (1.0 seconds)."
        done
        failing "HangTests.testHangs()" ;;
    hang-stuck)
        # The restarted host in a process group of its own, as launchd parents the real one; then no test starts.
        perl -e 'setpgrp(0, 0); exec @ARGV' "$dd/Build/Products/Debug/FakeHost" &
        while :; do sleep 1; done ;;
esac
echo "Test Suite 'Selected tests' passed at 2026-11-03 12:00:01.000."
# Writing the results after the last test can take longer than the limit.
[ "$mode" = slow-results ] && sleep 7
echo "** TEST SUCCEEDED **"
EOF
# Each case copies it into its own <derivedDataPath>/Build/Products/Debug, where the watchdog looks for hosts.
cat > "$work/bin/FakeHost" <<'EOF'
#!/bin/sh
trap '' TERM
echo $$ >> "$FAKE_WORK/hosts"
while :; do
    sleep 1 &
    echo $! >> "$FAKE_WORK/sleeps"
    wait $! 2>/dev/null
done
EOF
# `sample <pid> <seconds> -file <path>`: a call graph shaped like a real TSan host's. Both variants have the runtime's
# background thread and a thread blocked in an interceptor (`__tsan::` frames, as in every TSan process); the main
# thread hangs inside the runtime (`__tsan::SlotLock`) when FAKE_SAMPLE_TSAN gives this call 1, in a condition wait
# otherwise.
cat > "$work/bin/sample" <<'EOF'
#!/bin/sh
echo "$1" >> "$FAKE_WORK/sampled"
# FAKE_SAMPLE_TSAN is a comma-separated list: the n-th call uses the n-th value, or the last one.
n=$(wc -l < "$FAKE_WORK/sampled" | tr -d " ")
tsan="$(echo "${FAKE_SAMPLE_TSAN:-0}" | cut -d, -f"$n")"
[ -n "$tsan" ] || tsan="$(echo "${FAKE_SAMPLE_TSAN:-0}" | awk -F, '{ print $NF }')"
if [ "$tsan" = 1 ]; then
    hang="    +             900 __tsan::SlotLock(__tsan::ThreadState*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 96  [0x1]
    +               900 __sanitizer::FutexWait(__sanitizer::atomic_uint32_t*, unsigned int)  (in libclang_rt.tsan_osx_dynamic.dylib) + 40  [0x2]
    +                 900 __ulock_wait  (in libsystem_kernel.dylib) + 8  [0x3]"
else
    hang="    +             900 __tsan::call_pthread_cancel_with_cleanup(int (*)(void*), void (*)(void*), void*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 72  [0x1]
    +               900 _pthread_cond_wait  (in libsystem_pthread.dylib) + 980  [0x2]
    +                 900 __psynch_cvwait  (in libsystem_kernel.dylib) + 8  [0x3]"
fi
cat > "$4" <<SAMPLE
Analysis of sampling FakeHost (pid $1) every 1 millisecond
Process:         FakeHost [$1]

Call graph:
    900 Thread_1   DispatchQueue_1: com.apple.main-thread  (serial)
    + 900 start  (in dyld) + 6652  [0x0]
    +   900 main  (in FakeHost) + 40  [0x0]
    +     900 -[FakeTests.HangTests testHangs]  (in FakeTests) + 52  [0x0]
    +       900 TesseraSession.close()  (in FakeHost) + 52  [0x0]
    +         900 tessera_session_close  (in FakeHost) + 52  [0x0]
    +           900 wrap_pthread_cond_wait  (in libclang_rt.tsan_osx_dynamic.dylib) + 20  [0x0]
$hang
    900 Thread_2
    + 900 thread_start  (in libsystem_pthread.dylib) + 8  [0x0]
    +   900 _pthread_start  (in libsystem_pthread.dylib) + 136  [0x0]
    +     898 __tsan::BackgroundThread(void*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 220  [0x0]
    +     ! 898 usleep  (in libsystem_c.dylib) + 68  [0x0]
    +     !   898 __semwait_signal  (in libsystem_kernel.dylib) + 8  [0x0]
    +     2 __tsan::BackgroundThread(void*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 224  [0x0]
    +       2 __sanitizer::NanoTime()  (in libclang_rt.tsan_osx_dynamic.dylib) + 0  [0x0]
    900 Thread_3: pty-reader-1
    + 900 thread_start  (in libsystem_pthread.dylib) + 8  [0x0]
    +   900 _pthread_start  (in libsystem_pthread.dylib) + 136  [0x0]
    +     900 __tsan_thread_start_func  (in libclang_rt.tsan_osx_dynamic.dylib) + 144  [0x0]
    +       900 wrap_read  (in libclang_rt.tsan_osx_dynamic.dylib) + 364  [0x0]
    +         900 read  (in libsystem_kernel.dylib) + 8  [0x0]

Total number in stack (recursive counted multiple, when >=5):
SAMPLE
EOF
# A process under the products path that predates the run, as a developer's `just run` Tessera.
cat > "$work/bin/UserTessera" <<'EOF'
#!/bin/sh
while :; do sleep 1; done
EOF
chmod +x "$work/bin/xcodebuild" "$work/bin/sample" "$work/bin/FakeHost" "$work/bin/UserTessera"
export PATH="$work/bin:$PATH" TESSERA_APP_TEST_HANG_SECONDS=5

# Starts case $1 in the background in $work/$1 (its FAKE_WORK and derived data): the watchdog on the fake xcodebuild
# with FAKE_RUNS=$2 and FAKE_SAMPLE_TSAN=$3, through the app-test recipe's pipeline
# (`set -o pipefail; ... 2>&1 | tee <log> | tail -5 || { ...; exit 1 }`). An alarm ends a watchdog that has not
# returned after 60 s; its monitor then stops the fake on its own. The cases run at once, each with its own hosts.
# With $4 = user, a UserTessera under the case's products path is started first, in a process group of its own.
start_case() {
    local w="$work/$1"
    mkdir -p "$w/DerivedData/Build/Products/Debug"
    cp "$work/bin/FakeHost" "$w/DerivedData/Build/Products/Debug/FakeHost"
    [[ "${4:-}" != user ]] || start_user "$w"
    (
        local start=$SECONDS st=0
        FAKE_WORK="$w" FAKE_RUNS="$2" FAKE_SAMPLE_TSAN="$3" zsh -cu "set -o pipefail; perl -e 'alarm shift; exec @ARGV' 60 \
            zsh ${(q)watchdog} --sample-log ${(q)w}/sample.log xcodebuild test -scheme Fake -only-testing:FakeTests -skip-testing:FakeTests/SlowTests -derivedDataPath ${(q)w}/DerivedData \
            2>&1 | tee ${(q)w}/out | tail -5 || { echo 'xcodebuild test failed' >&2; exit 1 }" > "$w/tail" 2>&1 || st=$?
        print -r -- "$st $(( SECONDS - start ))" > "$w/result"
    ) &
}

start_user() {
    cp "$work/bin/UserTessera" "$1/DerivedData/Build/Products/Debug/UserTessera"
    perl -e 'setpgrp(0, 0); exec @ARGV' "$1/DerivedData/Build/Products/Debug/UserTessera" &!
    print -r -- $! > "$1/user"
}

# Fails the case unless its UserTessera is still running.
check_user_survives() {
    local pid="$(< "$w/user")"
    if kill -0 "$pid" 2>/dev/null && ps -o args= -p "$pid" | grep -q UserTessera; then
        ok "$1: the process that predates the run under the products path survives"
    else
        not_ok "$1: the process that predates the run under the products path ($pid) was killed"
    fi
}

# Case `interrupt`: a UserTessera, then the watchdog in a process group of its own on a hanging fake; once the fake
# host runs, SIGINT goes to the watchdog's group, as Ctrl-C in a terminal does.
start_interrupt() {
    local w="$work/interrupt"
    mkdir -p "$w/DerivedData/Build/Products/Debug"
    cp "$work/bin/FakeHost" "$w/DerivedData/Build/Products/Debug/FakeHost"
    start_user "$w"
    (
        local start=$SECONDS st=0 tries=100
        FAKE_WORK="$w" FAKE_RUNS=hang FAKE_SAMPLE_TSAN=0 perl -e 'setpgrp(0, 0); alarm shift; exec @ARGV' 60 \
            zsh "$watchdog" --sample-log "$w/sample.log" xcodebuild test -scheme Fake -derivedDataPath "$w/DerivedData" \
            > "$w/out" 2>&1 &
        local wd=$!
        while [[ ! -s "$w/hosts" ]] && (( tries-- > 0 )); do sleep 0.1; done
        sleep 0.5
        kill -INT -- "-$wd"
        wait $wd || st=$?
        : > "$w/tail"
        print -r -- "$st $(( SECONDS - start ))" > "$w/result"
    ) &
}

# Loads case $1's results: $w (its directory), $st (the pipeline's status), $secs, $out (the full log), $tail.
load_case() {
    w="$work/$1"
    read -r st secs < "$w/result"
    out="$(< "$w/out")"
    tail="$(< "$w/tail")"
    if [[ "$out" == *" at -e line "* ]]; then
        not_ok "$1: the watchdog printed a perl diagnostic: $out"
    fi
}

# Fails the case unless every fake host and `sleep` it started is gone.
check_no_strays() {
    local pid
    for pid in $(cat "$w/hosts" "$w/sleeps" 2>/dev/null); do
        if kill -0 "$pid" 2>/dev/null && ps -o args= -p "$pid" | grep -q -E "FakeHost|sleep"; then
            not_ok "$1: process $pid ($(ps -o args= -p "$pid")) left behind"
            return
        fi
    done
    ok "$1: no fake host or sleep left behind"
}

runs() { cat "$w/runs" 2>/dev/null || echo 0; }
hosts() { wc -l < "$w/hosts" | tr -d ' '; }

start_case pass pass 0
start_case slow-results slow-results 0
start_case hang hang,pass 0 user
start_interrupt
start_case continue hang-continue 0
start_case stuck hang-stuck 0
start_case tsan-pass hang,pass 1
start_case tsan-other hang-fail,pass 1
start_case tsan-stuck hang-stuck,pass 1
start_case tsan-twice hang,hang 1
start_case mixed hang-hang,pass 1,0
start_case mixed-reversed hang-hang,pass 0,1
wait

# --- all tests pass
load_case pass
if (( st == 0 )) && [[ ! -e "$w/sample.log" && "$out" == *"** TEST SUCCEEDED **"* && "$out" != *watchdog* ]]; then
    ok "a passing run exits 0, passes the output through and leaves no sample"
else
    not_ok "a passing run: exit $st, sample log $([[ -e $w/sample.log ]] && echo exists || echo absent): $out"
fi

load_case slow-results
if (( st == 0 && secs >= 7 )) && [[ ! -e "$w/sample.log" && "$out" != *watchdog* ]]; then
    ok "the clock stops at the run's last suite result: writing the results for longer than the limit is no hang"
else
    not_ok "results written after the limit: exit $st after ${secs} s: $out"
fi

# --- a hang outside the TSan runtime: killed, named, sampled, not retried
load_case hang
if (( st != 0 && secs <= TESSERA_APP_TEST_HANG_SECONDS + 10 )); then
    ok "a hang that ignores SIGTERM ends within the limit + 10 s (${secs} s, exit $st)"
else
    not_ok "a hang: exit $st after ${secs} s (limit ${TESSERA_APP_TEST_HANG_SECONDS} s + 10 s)"
fi
if [[ "$tail" == *"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s"*"TESSERA_APP_TEST_HANG_SECONDS=5"* ]]; then
    ok "the recipe fails and its tail names the hung test and the limit"
else
    not_ok "the recipe's tail does not name '-[FakeTests.HangTests testHangs]' and the limit: $tail"
fi
if [[ -s "$w/sample.log" ]] && grep -q -- "-\[FakeTests.HangTests testHangs\]" "$w/sample.log" \
    && grep -q "Analysis of sampling FakeHost (pid $(head -1 "$w/hosts"))" "$w/sample.log"; then
    ok "the sample log holds the host's sample under the test's name"
else
    not_ok "no sample of the host in the sample log: $(cat "$w/sample.log" 2>/dev/null)"
fi
if [[ "$(runs)" == 1 && "$out" != *retried* ]]; then
    ok "a hang outside the TSan runtime (with the __tsan:: frames of every TSan process) is not retried"
else
    not_ok "a hang outside the TSan runtime ran $(runs) times: $out"
fi
check_no_strays "a hang"
check_user_survives "a hang"

# --- Ctrl-C: the hosts are killed, a process that predates the run is not
load_case interrupt
if (( st != 0 && secs <= 15 )) && [[ "$(hosts)" == 1 ]]; then
    ok "SIGINT to the watchdog's process group ends the run (exit $st, ${secs} s)"
else
    not_ok "SIGINT: exit $st after ${secs} s, $(hosts) hosts: $out"
fi
check_no_strays "SIGINT"
check_user_survives "SIGINT"

# --- xcodebuild restarts the killed host and goes on: the run is not cut short
load_case continue
if (( st != 0 && secs <= TESSERA_APP_TEST_HANG_SECONDS + 16 )) && [[ "$(runs)" == 1 && "$(hosts)" == 2 \
    && "$out" == *"testAfterTheRestart6]' passed"*"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s"* \
    && "$out" != *"no test started"* ]]; then
    ok "after the kill xcodebuild's restarted run goes on past the limit to its end, and the run fails naming the hang"
else
    not_ok "a hang then a restarted run: exit $st after ${secs} s, $(runs) runs, $(hosts) hosts: $out"
fi
check_no_strays "a hang then a restarted run"

# --- xcodebuild starts no test after the kill: it and the restarted host (outside its process group) are killed
load_case stuck
if (( st != 0 && secs <= 2 * TESSERA_APP_TEST_HANG_SECONDS + 10 )) && [[ "$(hosts)" == 2 \
    && "$out" == *"no test started within 5 s after the host was killed"*"FAILED: -[FakeTests.HangTests testHangs]"* ]]; then
    ok "a run that starts no test after the kill is stopped within twice the limit + 10 s (${secs} s)"
else
    not_ok "a run stuck after the kill: exit $st after ${secs} s, $(hosts) hosts: $out"
fi
check_no_strays "a run stuck after the kill"

# --- a hang inside the TSan runtime: only the hung test is rerun, once
load_case tsan-pass
rerun="$(sed -n 2p "$w/args")"
if (( st == 0 )) && [[ "$(runs)" == 2 && "$tail" == *"TSan hang in -[FakeTests.HangTests testHangs] (#278), retried"* ]] \
    && [[ "$out" == *"Thread_1"*"stuck in the TSan runtime"*"__tsan::SlotLock"* ]] \
    && grep -q "__tsan::SlotLock" "$w/sample.log"; then
    ok "a TSan hang is retried once, naming the stuck thread, and passes when the rerun passes, keeping the sample"
else
    not_ok "a TSan hang then a pass: exit $st, $(runs) runs: $out"
fi
if [[ " $rerun " == *" -only-testing:FakeTests/HangTests/testHangs "* && " $rerun " == *" -scheme Fake "* \
    && " $rerun " == *" -derivedDataPath "* && " $rerun " != *" -only-testing:FakeTests "* && "$rerun" != *-skip-testing* ]]; then
    ok "the rerun keeps the arguments and replaces -only-testing/-skip-testing with the hung test alone"
else
    not_ok "the rerun's arguments: $rerun"
fi
check_no_strays "a TSan hang then a pass"

load_case tsan-other
if (( st != 0 )) && [[ "$(runs)" == 1 \
    && "$tail" == *"FAILED: TSan hang in -[FakeTests.HangTests testHangs] (#278), and other tests failed: OtherTests.testOther; not retried"* ]]; then
    ok "a TSan hang in a run with another failure fails without a rerun, naming that failure"
else
    not_ok "a TSan hang and another failure: exit $st, $(runs) runs: $out"
fi
check_no_strays "a TSan hang and another failure"

load_case tsan-stuck
if (( st != 0 )) && [[ "$(runs)" == 1 && "$tail" == *"FAILED: TSan hang in"*"the run did not finish after the kill; not retried"* ]]; then
    ok "a TSan hang whose run is stopped after the kill fails without a rerun"
else
    not_ok "a TSan hang whose run is stopped: exit $st, $(runs) runs: $out"
fi
check_no_strays "a TSan hang whose run is stopped"

load_case tsan-twice
if (( st != 0 )) && [[ "$(runs)" == 2 \
    && "$tail" == *"FAILED: TSan hang in -[FakeTests.HangTests testHangs] (#278), retried, and -[FakeTests.HangTests testHangs] ran longer than 5 s"* ]] \
    && [[ "$(grep -c '^=== .* attempt [12]:' "$w/sample.log")" == 2 ]]; then
    ok "a second TSan hang, in the rerun, fails, keeping both samples"
else
    not_ok "two TSan hangs: exit $st, $(runs) runs: $out"
fi
check_no_strays "two TSan hangs"

# --- a TSan hang and a hang outside the runtime in one attempt: each is classified on its own sample
load_case mixed
if (( st != 0 )) && [[ "$(runs)" == 1 && "$out" != *retried* && "$out" != *rerunning* \
    && "$tail" == *"FAILED: -[FakeTests.HangTests testHangsToo] ran longer than 5 s (TESSERA_APP_TEST_HANG_SECONDS=5)"* ]]; then
    ok "a TSan hang then a hang outside the runtime fails without a rerun, naming the second"
else
    not_ok "a TSan hang then another hang: exit $st, $(runs) runs: $out"
fi
check_no_strays "a TSan hang then another hang"

load_case mixed-reversed
if (( st != 0 )) && [[ "$(runs)" == 1 && "$out" != *rerunning* \
    && "$tail" == *"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s (TESSERA_APP_TEST_HANG_SECONDS=5)"* ]]; then
    ok "a hang outside the runtime then a TSan hang fails without a rerun, naming the first"
else
    not_ok "another hang then a TSan hang: exit $st, $(runs) runs: $out"
fi
check_no_strays "another hang then a TSan hang"

if (( failures )); then
    print -r -- "$failures check(s) failed"
    exit 1
fi
print -r -- "all checks passed"
