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
#   - a TSan hang, also in macOS 15's shape (the runtime yielding through libsystem_pthread), reruns only the hung
#     test, once, with the other arguments kept and the run's -only-testing replaced
#     ("TSan hang in ..., retried"), and passes when that rerun passes; a TSan hang plus another failure fails without
#     a rerun; a second hang in the rerun fails, keeping both samples; a TSan hang that xcodebuild lists under no
#     failure (`hang-unlisted`, as with -test-iterations) is rerun too (#443).
#   - a host that outlives the run's last suite line is sampled and killed after the limit, and the run fails
#     (`hang-after`); a `sample` that never returns is killed after TESSERA_APP_TEST_SAMPLE_SECONDS (3 in that case, 60 otherwise);
#   - a host that keeps forking is stopped before it is killed, so none of its children outlives it (`fork`);
#   - SIGINT, SIGTERM, SIGHUP and SIGALRM to the wrapper alone kill the hosts and remove its state directory;
#   - a process under the products path whose PID is reused by a newer process (a fake `ps` shifts its start time
#     after the first call) is a test host and is killed.
# Timing bounds grow with the machine's load (`slack`, measured below), and each case's alarm is 120 s.
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
    hang-after) ;;
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
    hang-after)
        # All tests ended, the host does not exit.
        echo "Test Suite 'Selected tests' passed at 2026-11-03 12:00:01.000."
        "$dd/Build/Products/Debug/FakeHost" &
        wait
        echo "** TEST SUCCEEDED **"
        exit 0 ;;
    hang-unlisted)
        # As with -test-iterations (#443): the restarted host repeats the test and passes, so no test is listed failed.
        echo "Test Case '-[FakeTests.HangTests testHangs]' started (Iteration 1 of 1)."
        echo "Test Case '-[FakeTests.HangTests testHangs]' passed (0.001 seconds)."
        echo "Test Suite 'Selected tests' passed at 2026-11-03 12:00:01.000."
        echo "** TEST SUCCEEDED **"
        exit 0 ;;
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
# A host that ignores SIGTERM and, once its sample has begun, forks a `sleep 20` every millisecond or so (up to 600), so
# a kill that does not stop it first leaves children (the fake `sample` pauses FAKE_SAMPLE_PAUSE s, mid-storm).
cat > "$work/bin/ForkHost" <<'EOF'
#!/usr/bin/perl
$SIG{TERM} = "IGNORE";
open(my $h, ">>", "$ENV{FAKE_WORK}/hosts") or die; print $h "$$\n"; close $h;
select(undef, undef, undef, 0.001) until -e "$ENV{FAKE_WORK}/sampled";
for (1 .. 600) {
    my $p = fork();
    if (!$p) { exec "sleep", "20"; exit 1; }
    open(my $s, ">>", "$ENV{FAKE_WORK}/sleeps") or die; print $s "$p\n"; close $s;
    select(undef, undef, undef, 0.001);
}
sleep 1 while 1;
EOF
# `sample <pid> <seconds> -file <path>`: a call graph shaped like a real TSan host's. Both variants have the runtime's
# background thread and a thread blocked in an interceptor (`__tsan::` frames, as in every TSan process); the main
# thread hangs inside the runtime (`__tsan::SlotLock`) when FAKE_SAMPLE_TSAN gives this call 1, in a condition wait
# otherwise; with 15 the main thread waits and another thread is stuck in the runtime as on macOS 15 (CI, PR #294),
# yielding through libsystem_pthread's `cthread_yield`, while a third thread waits in fork's atfork handler (#443).
cat > "$work/bin/sample" <<'EOF'
#!/bin/sh
echo "$1" >> "$FAKE_WORK/sampled"
[ -z "$FAKE_SAMPLE_PAUSE" ] || sleep "$FAKE_SAMPLE_PAUSE"
# A sample that never returns.
if [ -n "$FAKE_SAMPLE_HANG" ]; then echo $$ >> "$FAKE_WORK/sample-pids"; exec sleep 300; fi
# FAKE_SAMPLE_TSAN is a comma-separated list: the n-th call uses the n-th value, or the last one.
n=$(wc -l < "$FAKE_WORK/sampled" | tr -d " ")
tsan="$(echo "${FAKE_SAMPLE_TSAN:-0}" | cut -d, -f"$n")"
[ -n "$tsan" ] || tsan="$(echo "${FAKE_SAMPLE_TSAN:-0}" | awk -F, '{ print $NF }')"
extra=""
if [ "$tsan" = 15 ]; then
    # macOS 15 (CI's PR #294 sample): the runtime semaphore yields through libsystem_pthread's cthread_yield.
    tsan=0
    extra="    5974 Thread_4   DispatchQueue_99: NSPersistentUI Work  (serial)
    + 5974 start_wqthread  (in libsystem_pthread.dylib) + 8  [0x0]
    +   5974 _pthread_wqthread  (in libsystem_pthread.dylib) + 292  [0x0]
    +     5974 _dispatch_client_callout  (in libdispatch.dylib) + 16  [0x0]
    +       5974 __tsan::dispatch_callback_wrap(void*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 672  [0x0]
    +         5974 __tsan::invoke_and_release_block(void*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 32  [0x0]
    +           5974 ___NSPersistentUIDispatchQueueAsync_block_invoke  (in AppKit) + 76  [0x0]
    +             5974 -[NSPersistentUIWindowInfo initWithWindowID:]  (in AppKit) + 60  [0x0]
    +               5974 wrap_arc4random_buf  (in libclang_rt.tsan_osx_dynamic.dylib) + 212  [0x0]
    +                 5974 ccrng_crypto_generate  (in libcorecrypto.dylib) + 292  [0x0]
    +                   5974 wrap_os_unfair_lock_unlock  (in libclang_rt.tsan_osx_dynamic.dylib) + 192  [0x0]
    +                     5974 __tsan::Release(__tsan::ThreadState*, unsigned long, unsigned long)  (in libclang_rt.tsan_osx_dynamic.dylib) + 48  [0x0]
    +                       5974 __tsan::SlotLock(__tsan::ThreadState*)  (in libclang_rt.tsan_osx_dynamic.dylib) + 140  [0x0]
    +                         5973 __sanitizer::Semaphore::Wait()  (in libclang_rt.tsan_osx_dynamic.dylib) + 40  [0x0]
    +                         ! 5971 cthread_yield  (in libsystem_pthread.dylib) + 36  [0x0]
    +                         ! : 5970 swtch_pri  (in libsystem_kernel.dylib) + 8  [0x0]
    +                         ! : 1 DYLD-STUB\$\$swtch_pri  (in libsystem_pthread.dylib) + 0  [0x0]
    +                         ! 2 cthread_yield  (in libsystem_pthread.dylib) + 28  [0x0]
    +                         1 __sanitizer::Semaphore::Wait()  (in libclang_rt.tsan_osx_dynamic.dylib) + 40  [0x0]
    5974 Thread_5: tessera-spawn
    + 5974 thread_start  (in libsystem_pthread.dylib) + 8  [0x0]
    +   5974 wrap_fork  (in libclang_rt.tsan_osx_dynamic.dylib) + 128  [0x0]
    +     5974 fork  (in libsystem_c.dylib) + 36  [0x0]
    +       5974 libSystem_atfork_prepare  (in libSystem.B.dylib) + 56  [0x0]
    +         5974 ccrng_process_atfork_prepare  (in libcorecrypto.dylib) + 28  [0x0]
    +           5974 wrap_os_unfair_lock_lock  (in libclang_rt.tsan_osx_dynamic.dylib) + 188  [0x0]
    +             5974 _os_unfair_lock_lock_slow  (in libsystem_platform.dylib) + 176  [0x0]
    +               5974 __ulock_wait2  (in libsystem_kernel.dylib) + 8  [0x0]"
fi
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
$extra

Total number in stack (recursive counted multiple, when >=5):
SAMPLE
EOF
# A process under the products path that predates the run, as a developer's `just run` Tessera.
cat > "$work/bin/UserTessera" <<'EOF'
#!/bin/sh
while :; do sleep 1; done
EOF
# A `ps` for the PID-reuse and fork cases. From its second call on it reports the start time of the pid in $FAKE_WORK/user
# as another one (PID reuse), and prints its snapshot FAKE_PS_DELAY s late, so a process table read just before a kill
# is as stale as a slow machine makes it.
mkdir -p "$work/psbin"
cat > "$work/psbin/ps" <<'EOF'
#!/bin/sh
first=1
[ -e "$FAKE_WORK/ps-called" ] && first=0
: > "$FAKE_WORK/ps-called"
out="$FAKE_WORK/ps-out.$$"
/bin/ps "$@" > "$out"
[ "$first" = 1 ] || [ -z "$FAKE_PS_DELAY" ] || sleep "$FAKE_PS_DELAY"
if [ "$first" = 1 ] || [ ! -s "$FAKE_WORK/user" ]; then
    cat "$out"
else
    REUSED="$(cat "$FAKE_WORK/user")" perl -pe 's/^(\s*$ENV{REUSED}\s+\d+\s+)\w{3}\s+\w{3}\s+\d+\s+[\d:]{8}\s+\d{4}/${1}Mon Jan  1 00:00:00 2001/' "$out"
fi
rm -f "$out"
EOF
chmod +x "$work/psbin/ps" "$work/bin/ForkHost"
chmod +x "$work/bin/xcodebuild" "$work/bin/sample" "$work/bin/FakeHost" "$work/bin/UserTessera"
export PATH="$work/bin:$PATH" TESSERA_APP_TEST_HANG_SECONDS=5

# Seconds added to every timing bound: 10 on an idle machine, 10 per multiple of the CPU count the 1-minute load average
# reaches, or per 0.5 s that ten `ps -A` calls take (a measured no-op), at most 100.
slack=$(( 10 * $(perl -MTime::HiRes=time -e '
    my $n = `sysctl -n hw.ncpu` + 0; my ($l) = `sysctl -n vm.loadavg` =~ /([\d.]+)/;
    my $t = time; system("ps -A -o pid=,ppid=,lstart=,args= >/dev/null") for 1 .. 10;
    my $f = $l / $n; my $m = (time - $t) * 2; $f = $m if $m > $f; $f = 1 if $f < 1; $f = 10 if $f > 10;
    printf "%d\n", $f + 0.999;') ))
limit=$TESSERA_APP_TEST_HANG_SECONDS

# Starts case $1 in the background in $work/$1 (its FAKE_WORK and derived data): the watchdog on the fake xcodebuild
# with FAKE_RUNS=$2 and FAKE_SAMPLE_TSAN=$3, through the app-test recipe's pipeline
# (`set -o pipefail; ... 2>&1 | tee <log> | tail -5 || { ...; exit 1 }`). An alarm ends a watchdog that has not
# returned after 120 s; its monitor then stops the fake on its own. The cases run at once, each with its own hosts.
# $4 holds options: `user` starts a UserTessera under the case's products path first, in a process group of its own;
# `fork` makes the host a ForkHost and `ps` slow; `ps` puts the fake `ps` on the watchdog's PATH; `samplehang` makes `sample` hang.
start_case() {
    local w="$work/$1" opts=" ${4:-} " case_path="$PATH"
    mkdir -p "$w/DerivedData/Build/Products/Debug"
    cp "$work/bin/FakeHost" "$w/DerivedData/Build/Products/Debug/FakeHost"
    [[ "$opts" != *" fork "* ]] || cp "$work/bin/ForkHost" "$w/DerivedData/Build/Products/Debug/FakeHost"
    [[ "$opts" != *" user "* ]] || start_user "$w"
    [[ "$opts" != *" ps "* ]] || case_path="$work/psbin:$PATH"
    local hang_sample="" pause="" ps_delay="" sample_secs=60
    [[ "$opts" != *" samplehang "* ]] || { hang_sample=1; sample_secs=3; }
    [[ "$opts" != *" fork "* ]] || { pause=0.2; ps_delay=0.2; }
    (
        local start=$SECONDS st=0
        FAKE_WORK="$w" FAKE_RUNS="$2" FAKE_SAMPLE_TSAN="$3" FAKE_SAMPLE_HANG="$hang_sample" FAKE_SAMPLE_PAUSE="$pause" FAKE_PS_DELAY="$ps_delay" TESSERA_APP_TEST_SAMPLE_SECONDS="$sample_secs" PATH="$case_path" zsh -cu "set -o pipefail; perl -e 'alarm shift; exec @ARGV' 120 \
            zsh ${(q)watchdog} --sample-log ${(q)w}/sample.log xcodebuild test -scheme Fake -only-testing:FakeTests -skip-testing:FakeTests/SlowTests -derivedDataPath ${(q)w}/DerivedData \
            2>&1 | tee ${(q)w}/out | tail -5 || { echo 'xcodebuild test failed' >&2; exit 1 }" > "$w/tail" 2>&1 || st=$?
        print -r -- "$st $(( SECONDS - start ))" > "$w/result"
    ) &
}

start_user() {
    cp "$work/bin/UserTessera" "$1/DerivedData/Build/Products/Debug/UserTessera"
    perl -e 'setpgrp(0, 0); exec @ARGV' "$1/DerivedData/Build/Products/Debug/UserTessera" &!
    print -r -- $! > "$1/user"
    # The watchdog snapshots the processes under the products path when it starts: wait for the exec, or a slow machine
    # snapshots the bare fork and the process is taken for a new host.
    local tries=200
    while ! ps -o args= -p "$(< "$1/user")" | grep -q UserTessera && (( tries-- > 0 )); do sleep 0.1; done
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

# Cases `sig-<signal>`: the watchdog alone (not its group) gets the signal once the fake host runs; its TMPDIR is
# $work/<case>/tmp, where the state directory must not outlive it.
start_signal() {
    local w="$work/sig-$1"
    mkdir -p "$w/DerivedData/Build/Products/Debug" "$w/tmp"
    cp "$work/bin/FakeHost" "$w/DerivedData/Build/Products/Debug/FakeHost"
    (
        local start=$SECONDS st=0 tries=100
        FAKE_WORK="$w" FAKE_RUNS=hang FAKE_SAMPLE_TSAN=0 TMPDIR="$w/tmp" perl -e '$SIG{INT} = "DEFAULT"; setpgrp(0, 0); alarm shift; exec @ARGV' 120 \
            zsh "$watchdog" --sample-log "$w/sample.log" xcodebuild test -scheme Fake -derivedDataPath "$w/DerivedData" \
            > "$w/out" 2>&1 &
        local wd=$!
        while [[ ! -s "$w/hosts" ]] && (( tries-- > 0 )); do sleep 0.1; done
        sleep 0.5
        kill -"$1" "$wd"
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
start_signal INT
start_signal TERM
start_signal HUP
start_signal ALRM
start_case hang-after hang-after 0
start_case fork hang,pass 0 "fork ps"
start_case sample-hang hang,pass 0 samplehang
start_case reuse hang,pass 0 "user ps"
start_case continue hang-continue 0
start_case stuck hang-stuck 0
start_case tsan-pass hang,pass 1
start_case tsan-macos15 hang,pass 15
start_case tsan-other hang-fail,pass 1
start_case tsan-unlisted hang-unlisted,pass 1
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
if (( st != 0 && secs <= limit + slack )); then
    ok "a hang that ignores SIGTERM ends within the limit + ${slack} s (${secs} s, exit $st)"
else
    not_ok "a hang: exit $st after ${secs} s (limit ${limit} s + ${slack} s)"
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
if (( st != 0 && secs <= limit + slack )) && [[ "$(hosts)" == 1 ]]; then
    ok "SIGINT to the watchdog's process group ends the run (exit $st, ${secs} s)"
else
    not_ok "SIGINT: exit $st after ${secs} s, $(hosts) hosts: $out"
fi
check_no_strays "SIGINT"
check_user_survives "SIGINT"

# --- SIGINT, SIGTERM, SIGHUP, SIGALRM to the wrapper alone: hosts killed, state directory removed
for pair in INT:130 TERM:143 HUP:129 ALRM:142; do
    sig="${pair%%:*}" code="${pair##*:}"
    load_case "sig-$sig"
    leftover=("$w"/tmp/*(N))
    if (( st == code && secs <= limit + slack )) && [[ "$(hosts)" == 1 && ${#leftover} == 0 ]]; then
        ok "SIG$sig to the wrapper alone exits $code, kills the host and removes the state directory (${secs} s)"
    else
        not_ok "SIG$sig: exit $st (want $code) after ${secs} s, $(hosts) hosts, left in TMPDIR: ${leftover}: $out"
    fi
    check_no_strays "SIG$sig to the wrapper alone"
done

# --- a host that outlives the run's last suite line
load_case hang-after
if (( st != 0 && secs <= limit + slack )) && [[ "$(runs)" == 1 && "$out" != *retried* && "$out" != *rerunning* \
    && "$tail" == *"FAILED: the test host kept running for more than 5 s after the run's last test result (TESSERA_APP_TEST_HANG_SECONDS=5)"* ]] \
    && grep -q "Analysis of sampling FakeHost (pid $(head -1 "$w/hosts"))" "$w/sample.log"; then
    ok "a host still alive more than the limit after the last suite line is sampled and killed, and the run fails (${secs} s)"
else
    not_ok "a host that outlives the run: exit $st after ${secs} s, $(runs) runs: $out"
fi
check_no_strays "a host that outlives the run"

# --- a host that keeps forking: stopped before it is killed
load_case fork
nsleeps="$(wc -l < "$w/sleeps" | tr -d ' ')"
if (( st != 0 && nsleeps >= 50 )) && [[ "$tail" == *"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s"* ]]; then
    ok "a host that forks every millisecond while it is sampled ($nsleeps children) is ended with none left"
else
    not_ok "a forking host: exit $st, $nsleeps children: $out"
fi
check_no_strays "a forking host"

# --- a sample that never returns is killed
load_case sample-hang
sample_pid="$(head -1 "$w/sample-pids" 2>/dev/null)"
if (( st != 0 && secs <= limit + 3 + slack )) && [[ -n "$sample_pid" ]] && ! kill -0 "$sample_pid" 2>/dev/null \
    && grep -q "sample timed out after 3 s" "$w/sample.log" && [[ "$tail" == *"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s"* ]]; then
    ok "a sample that does not return is killed after 3 s and the log says so (${secs} s)"
else
    not_ok "a hanging sample: exit $st after ${secs} s, sample pid '$sample_pid': $out"
fi
check_no_strays "a hanging sample"

# --- a snapshotted PID reused by a newer process (another start time) is a test host
load_case reuse
user_pid="$(< "$w/user")"
if (( st != 0 )) && ! kill -0 "$user_pid" 2>/dev/null && grep -qx "$user_pid" "$w/sampled"; then
    ok "a process whose PID matches the start snapshot but whose start time does not is sampled and killed"
else
    not_ok "a reused PID: exit $st, process $user_pid alive: $(kill -0 "$user_pid" 2>/dev/null && echo yes || echo no), sampled: $(cat "$w/sampled" 2>/dev/null | tr '\n' ' '): $out"
fi
check_no_strays "a reused PID"

# --- xcodebuild restarts the killed host and goes on: the run is not cut short
load_case continue
if (( st != 0 && secs <= limit + 6 + slack )) && [[ "$(runs)" == 1 && "$(hosts)" == 2 \
    && "$out" == *"testAfterTheRestart6]' passed"*"FAILED: -[FakeTests.HangTests testHangs] ran longer than 5 s"* \
    && "$out" != *"no test started"* ]]; then
    ok "after the kill xcodebuild's restarted run goes on past the limit to its end, and the run fails naming the hang"
else
    not_ok "a hang then a restarted run: exit $st after ${secs} s, $(runs) runs, $(hosts) hosts: $out"
fi
check_no_strays "a hang then a restarted run"

# --- xcodebuild starts no test after the kill: it and the restarted host (outside its process group) are killed
load_case stuck
if (( st != 0 && secs <= 2 * limit + slack )) && [[ "$(hosts)" == 2 \
    && "$out" == *"no test started within 5 s after the host was killed"*"FAILED: -[FakeTests.HangTests testHangs]"* ]]; then
    ok "a run that starts no test after the kill is stopped within twice the limit + ${slack} s (${secs} s)"
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

load_case tsan-macos15
if (( st == 0 )) && [[ "$(runs)" == 2 && "$tail" == *"TSan hang in -[FakeTests.HangTests testHangs] (#278), retried"* \
    && "$out" == *"Thread_4"*"NSPersistentUI Work"*"stuck in the TSan runtime"*"cthread_yield"* \
    && "$out" == *"Thread_5: tessera-spawn is forking"*"ccrng_process_atfork_prepare"* ]]; then
    ok "a macOS 15 TSan hang (the runtime semaphore yielding through cthread_yield) is classified TSan and rerun, naming the forking thread"
else
    not_ok "a macOS 15 TSan hang: exit $st, $(runs) runs: $out"
fi
check_no_strays "a macOS 15 TSan hang"

load_case tsan-unlisted
if (( st == 0 )) && [[ "$(runs)" == 2 && "$tail" == *"TSan hang in -[FakeTests.HangTests testHangs] (#278), retried"* ]]; then
    ok "a TSan hang that xcodebuild does not list as failed (-test-iterations) is still rerun"
else
    not_ok "a TSan hang with no failed test listed: exit $st, $(runs) runs: $out"
fi
check_no_strays "a TSan hang with no failed test listed"

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
