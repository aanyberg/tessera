#!/usr/bin/env zsh
# Runs one `xcodebuild test` invocation and ends a hang below the test level (#228, #121, #177).
# Usage: scripts/app-test-watchdog.sh [--sample-log <file>] xcodebuild test ... -derivedDataPath <dir> ...
#
# xcodebuild fails a single hung test after its execution time allowance, but when the test host itself hangs
# (a Thread Sanitizer lock, a host that ignores SIGTERM) xcodebuild waits for it and the run stalls until CI's step
# limit. This wrapper follows the command's output (passed through unchanged). Once a test has started
# ("Test Case '-[X y]' started.") and no test line has followed for TESSERA_APP_TEST_HANG_SECONDS (default 150, 30 s
# past xcodebuild's 120 s allowance; with -test-iterations the line reads "started (Iteration n of m)."; the clock stops
# at the "Test Suite 'All tests'" (or 'Selected tests') result, after which xcodebuild only writes its results), it:
#   - writes a `sample` (10 s) of each test host to the sample log (default apps/macos/build/app-test-hang-sample.log,
#     appended to, never truncated: `just app-test` removes it before its first run);
#   - kill -9s the hosts and their descendants. xcodebuild then restarts the host and goes on with the next test, and
#     each later hang is handled the same way; when no test starts within the limit after a kill, the command's
#     process group and any host left are killed;
#   - if every hang's own sample has a thread stuck inside the Thread Sanitizer runtime (`__tsan::`/`__sanitizer::`
#     frames at its hot leaf; see tsan_hang: the fork deadlock of #228, whose cause is #278) and the hung tests are the
#     run's only failures, reruns only them once, after the run ends (same arguments, `-only-testing:<Target>/<Class>/<test>`
#     instead of any -only-testing/-skip-testing), and prints "TSan hang in <test> (#278), retried"; the run then
#     passes if the rerun does. A hang outside the TSan runtime, a run with other failures or that did not finish
#     after the kill, or a second hang exits 1 with a line naming the test and the limit.
# Test hosts are the processes whose arguments contain <derivedDataPath>/Build/Products/ (Tessera.app, which
# launchd parents), except those already running when the command started (a `just run` Tessera), and the command's
# `xctest` descendants. Otherwise the exit status is the command's own.
# `sample` is looked up on PATH (the self-test, scripts/test-app-test-watchdog.sh, puts a fake one there).
set -eu
usage() { print -u2 "usage: scripts/app-test-watchdog.sh [--sample-log <file>] <command> [args]..."; exit 2; }

root="$(cd "$(dirname "$0")/.." && pwd)"
sample_log="$root/apps/macos/build/app-test-hang-sample.log"
if [[ "${1:-}" == --sample-log ]]; then
    (( $# >= 2 )) || usage
    sample_log="$2"
    shift 2
fi
(( $# )) || usage
limit="${TESSERA_APP_TEST_HANG_SECONDS:-150}"
[[ "$limit" == <1-> ]] || { print -u2 "TESSERA_APP_TEST_HANG_SECONDS must be a positive integer, got '$limit'"; exit 2; }

products=""
args=("$@")
for (( i = 1; i < ${#args}; i++ )); do
    if [[ "${args[i]}" == -derivedDataPath ]]; then
        products="${args[i+1]:a}/Build/Products/"
    fi
done
[[ -n "$products" ]] || print -u2 "app-test watchdog: no -derivedDataPath; only xctest hosts can be found"

state="$(mktemp -d "${TMPDIR:-/tmp}/app-test-watchdog.XXXXXX")"
trap 'rm -rf "$state"' EXIT

# Runs the command once under the monitor. Exit status: the command's own, or 124 after a hang; then $state/test
# holds one "<test>\t<description>" line per hang, $state/hang-<n> the samples of the n-th hang and $state/sample all
# of them. $state/failed lists the failed tests as <Class>.<test>, $state/ended exists when xcodebuild printed its
# "** TEST ..." result and $state/stopped when the monitor stopped the command.
monitor() {
    rm -f "$state/test" "$state/sample" "$state/failed" "$state/ended" "$state/stopped" "$state"/hang-*(N)
    perl -e '
        use strict; use warnings;
        use POSIX qw(:sys_wait_h);
        use IO::Select; use Time::HiRes qw(time sleep);
        my ($limit, $products, $state, @cmd) = @ARGV;
        my $parent = getppid();
        $| = 1;
        # Processes under the products path that predate this run (a `just run` Tessera and its shells) are never hosts.
        my %pre;
        if ($products ne "") {
            my (undef, $a0) = table();
            for my $p (keys %$a0) { $pre{$p} = 1 if index($a0->{$p}, $products) >= 0; }
        }
        pipe(my $r, my $w) or die "pipe: $!";
        my $pid = fork() // die "fork: $!";
        if (!$pid) {
            close $r; setpgrp(0, 0);
            open STDOUT, ">&", $w or die; open STDERR, ">&", $w or die; close $w;
            exec @cmd or die "exec $cmd[0]: $!";
        }
        close $w;
        sub table {
            my (%kids, %args);
            open(my $ps, "-|", "ps", "-A", "-o", "pid=,ppid=,args=") or return ({}, {});
            while (<$ps>) {
                next unless /^\s*(\d+)\s+(\d+)\s?(.*)$/;
                push @{$kids{$2}}, $1; $args{$1} = $3;
            }
            return (\%kids, \%args);
        }
        sub below {
            my ($kids, @todo) = @_; my @all;
            while (@todo) { my $n = shift @todo; push @all, $n; push @todo, @{$kids->{$n} // []}; }
            return @all;
        }
        sub hosts {
            my ($kids, $args) = table();
            my %h;
            if ($products ne "") {
                for my $p (keys %$args) { $h{$p} = 1 if $p != $pid && $p != $$ && !$pre{$p} && index($args->{$p}, $products) >= 0; }
            }
            for my $p (below($kids, $pid)) {
                my ($exe) = split " ", ($args->{$p} // "");
                $h{$p} = 1 if $p != $pid && defined $exe && $exe =~ m{(^|/)xctest$};
            }
            return ($kids, $args, sort { $a <=> $b } keys %h);
        }
        # Stops the hosts and their descendants before killing them, so none starts a child that outlives it.
        sub kill_hosts {
            my ($kids, @h) = @_;
            return unless @h;
            kill "STOP", below($kids, @h);
            ($kids) = table();
            kill "KILL", below($kids, @h);
        }
        sub stop_group {
            kill "TERM", -$pid;
            for (1 .. 50) { last if waitpid($pid, WNOHANG) == $pid; sleep 0.1; }
            kill "KILL", -$pid;
            waitpid($pid, 0) if kill 0, $pid;
        }
        # Kills the test hosts and their descendants (a host that launchd parents is outside the command'"'"'s process
        # group), then the group.
        sub stop_all {
            my ($k, $a2, @h) = hosts(); kill_hosts($k, @h);
            stop_group();
        }
        $SIG{INT} = $SIG{TERM} = sub { stop_all(); exit 130; };

        my $sel = IO::Select->new($r);
        # $progress: a test started since the last host was killed.
        my ($buf, $current, $finished, $last, $fired, $progress, $failing) = ("", undef, 0, undef, 0, 0, 0);
        sub note { my ($file, $text) = @_; open(my $f, ">>", "$state/$file") or die; print $f "$text\n"; close $f; }
        my $open = 1;
        my ($status, $exited);
        while (1) {
            if ($open && $sel->can_read(0.5)) {
                my $n = sysread($r, my $chunk, 65536);
                if (!$n) { $open = 0; close $r; }
                else {
                    print $chunk;
                    $buf .= $chunk;
                    while ($buf =~ s/^([^\n]*)\n//) {
                        my $line = $1;
                        # Failed tests as <Class>.<test>: from "Test Case ... failed" and from the "Failing tests:"
                        # summary, which is the only place a test whose host was killed is listed.
                        if ($failing && $line =~ /^\s+(\S+?)(\(\))?\s*$/) {
                            my @parts = split /\./, $1; note("failed", join(".", @parts[-2 .. -1])) if @parts >= 2;
                        } else { $failing = 0; }
                        $failing = 1 if $line =~ /^Failing tests:/;
                        note("failed", "$1.$2") if $line =~ /^Test Case \x27-\[(?:[^ \]]*\.)?([^ .\]]+) ([^ \]]+)\]\x27 failed/;
                        note("ended", $line) if $line =~ /^\*\* TEST /;
                        if ($line =~ /^Test Case \x27(-\[.+\])\x27 started( \(Iteration \d+ of \d+\))?\./) {
                            ($current, $finished, $last, $progress) = ($1, 0, time, 1);
                        }
                        elsif ($line =~ /^Test Case \x27(-\[.+\])\x27 (passed|failed|skipped)/) { $finished = 1; $last = time if defined $last; }
                        # The run'"'"'s last tests ended: xcodebuild may take minutes to write its results after restarts.
                        elsif ($line =~ /^(\*\* TEST |Test session results|Test Suite \x27(All|Selected) tests\x27 (passed|failed))/) { $last = undef; }
                        elsif ($line =~ /^Test (Suite|Case) / && defined $last) { $last = time; }
                    }
                }
            } elsif (!$open) {
                sleep 0.2;
            }
            if (!defined $exited && waitpid($pid, WNOHANG) == $pid) { ($status, $exited) = ($?, time); }
            # A descendant that keeps the pipe open does not hold the run past the command'"'"'s exit.
            last if defined $exited && (!$open || time - $exited > 5);
            if (getppid() != $parent) { stop_all(); exit 1; }
            next unless defined $last && time - $last > $limit;
            if ($fired && !$progress) {
                print STDERR "app-test watchdog: no test started within $limit s after the host was killed; stopping the command\n";
                note("stopped", "no test started after the kill");
                stop_all(); last;
            }
            # xcodebuild restarts a killed host and goes on with the next test; the clock stays armed, so a run
            # that does not go on is stopped at the next check.
            ($fired, $progress) = ($fired + 1, 0);
            my $what = $finished ? "the test host stalled for more than $limit s after $current ended"
                                 : "$current ran longer than $limit s";
            open(my $t, ">>", "$state/test") or die; print $t "$current\t$what\n"; close $t;
            my ($kids, $args, @h) = hosts();
            # Each hang'"'"'s samples in a file of their own (hang-<n>, n as in $state/test), classified on their own.
            my $hang = "$state/hang-$fired";
            open(my $s, ">>", $hang) or die;
            print $s "no test host found (products path: $products)\n" unless @h;
            for my $h (@h) {
                print $s "== sample of pid $h: " . ($args->{$h} // "?") . "\n"; close $s;
                my $rc = system("sample", $h, "10", "-file", "$state/sample-$h") >> 8;
                open($s, ">>", $hang) or die;
                if (open(my $in, "<", "$state/sample-$h")) { print $s $_ while <$in>; close $in; }
                else { print $s "sample failed (exit $rc)\n"; }
            }
            close $s;
            open(my $all, ">>", "$state/sample") or die; open(my $one, "<", $hang) or die;
            print $all $_ while <$one>; close $one; close $all;
            ($kids) = table();
            kill_hosts($kids, @h);
            print STDERR "app-test watchdog: $what (TESSERA_APP_TEST_HANG_SECONDS); killed test host(s) @h\n";
            $last = time;
        }
        if ($fired) {
            my ($k, $a2, @h) = hosts(); kill_hosts($k, @h);
            exit 124;
        }
        $status = $? unless defined $status;
        exit($status & 127 ? 128 + ($status & 127) : $status >> 8);
    ' "$limit" "$products" "$state" "$@"
}

# Appends this attempt's samples to the sample log under a header naming its hangs.
keep_sample() {
    mkdir -p "${sample_log:h}"
    {
        print -r -- "=== $(date '+%Y-%m-%d %H:%M:%S') attempt $1: $(hangs)"
        cat "$state/sample"
        print
    } >> "$sample_log"
}

# This attempt's hangs, one description each, joined with "; ".
hangs() { cut -f2 "$state/test" | paste -s -d ';' - | sed 's/;/; /g'; }

# Succeeds when a thread in the samples file $1 is stuck inside the Thread Sanitizer runtime, and prints that thread
# and its innermost frames. Every TSan process has `__tsan::` frames (its background thread, the trampolines of each
# thread and blocking interceptor), so a match on the name alone would retry every hang. A thread counts when the leaf
# holding at least half its samples, without its trailing wait frames (libsystem_kernel and libsystem_platform, and
# libsystem_pthread's `cthread_yield` and its `swtch_pri` stub, through which macOS 15's runtime semaphore yields),
# ends in a `__tsan::` or `__sanitizer::` function: SlotLock, a runtime mutex, semaphore or futex wait, a fork hook.
# `_pthread_cond_wait` is not a wait frame here: under it sits the interceptor trampoline every blocked thread has.
tsan_hang() {
    perl -e '
        use strict; use warnings;
        my ($in, $thread, $total, @stack, $found) = (0, "", 0);
        sub leaf {
            my @path = @stack;
            return if !@path || $path[-1]{count} * 2 < $total;
            pop @path while @path && ($path[-1]{text} =~ /\(in libsystem_(kernel|platform)\.dylib\)/
                || $path[-1]{text} =~ /^(cthread_yield|DYLD-STUB\$\$\w+) .*\(in libsystem_pthread\.dylib\)/);
            return unless @path && $path[-1]{text} =~ /^(__tsan::|__sanitizer::|_ZN6__tsan|_ZN11__sanitizer)/;
            $found = 1;
            print "$thread stuck in the TSan runtime ($stack[-1]{count} of $total samples):\n";
            print "    $_->{text}\n" for reverse @stack[($#stack > 7 ? $#stack - 7 : 0) .. $#stack];
        }
        sub flush { leaf() if @stack && !$stack[-1]{kids}; @stack = (); }
        while (<>) {
            chomp;
            if (/^Call graph:/) { $in = 1; next; }
            if ($in && /^\s*$/) { flush(); $in = 0; next; }
            next unless $in && /^([\s+!:|]*)(\d+)\s+(.*?)\s*$/;
            my ($depth, $count, $text) = (length $1, $2, $3);
            if ($text =~ /^Thread_/) { flush(); ($thread, $total) = ($text, $count); next; }
            while (@stack && $stack[-1]{depth} >= $depth) { leaf() unless $stack[-1]{kids}; pop @stack; }
            $stack[-1]{kids} = 1 if @stack;
            push @stack, { depth => $depth, count => $count, text => $text };
        }
        flush();
        exit($found ? 0 : 1);
    ' "$1"
}

st=0
monitor "$@" || st=$?
(( st == 124 )) || exit $st
keep_sample 1
hung=("${(@f)$(cut -f1 "$state/test" | sort -u)}")
fail() {
    print -u2 -r -- "app-test watchdog FAILED: $1 (TESSERA_APP_TEST_HANG_SECONDS=$limit); stack sample(s) in $sample_log"
    exit 1
}
# Each hang is classified on its own samples: one outside the TSan runtime fails the run, whatever the others show.
tsan_reports=() outside=() n=0
while IFS=$'\t' read -r t desc; do
    (( ++n ))
    if report="$(tsan_hang "$state/hang-$n")"; then tsan_reports+=("$report"); else outside+=("$desc"); fi
done < "$state/test"
(( ${#outside} == 0 )) || fail "${(j:; :)outside}"
print -u2 -r -- "${(F)tsan_reports}"
what="TSan hang in ${(j:, :)hung} (#278)"
[[ ! -e "$state/stopped" && -e "$state/ended" ]] || fail "$what, and the run did not finish after the kill; not retried"
# The run passes only if the hung tests are its only failures and their rerun passes.
hung_names=() rerun=()
for t in $hung; do
    # -[Target.Class test] -> Class.test and -only-testing:Target/Class/test
    if [[ "$t" =~ '^-\[([^ .]+)\.([^ .]+) ([^ ]+)\]$' ]]; then
        hung_names+=("$match[2].$match[3]")
        rerun+=("-only-testing:$match[1]/$match[2]/$match[3]")
    else
        fail "$what, and $t names no test target to rerun; not retried"
    fi
done
failed=(${(f)"$(cat "$state/failed" 2>/dev/null)"})
others=(${(u)failed:|hung_names})
(( ${#others} == 0 )) || fail "$what, and other tests failed: ${(j:, :)others}; not retried"
args=()
for a in "$@"; do
    [[ "$a" == -only-testing:* || "$a" == -skip-testing:* ]] || args+=("$a")
done
print -u2 -r -- "app-test watchdog: $what; rerunning only ${(j:, :)rerun} once"
st=0
monitor "${args[@]}" "${rerun[@]}" || st=$?
if (( st == 124 )); then
    keep_sample 2
    fail "$what, retried, and $(hangs)"
fi
(( st == 0 )) || fail "$what, retried, and the rerun failed (exit $st)"
print -u2 -r -- "app-test watchdog: $what, retried: the rerun of ${(j:, :)hung} passed; first sample in $sample_log"
