#!/usr/bin/env bash
# Exercise scripts/perf-job.sh (#612) offline: the early exit on an unchanged SHA, one JSON line per case (SHA,
# instructions, heap bytes), the three-build rule, the baseline that skips a regressed SHA, and the issue request
# (open, or comment on an open one) that names the SHA range. `cargo xtask ab`, the probe binary and `gh` are stubbed.
#
#   scripts/test-perf-job.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/perf-job.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
expect() { # description, command...
    local what="$1"; shift
    "$@" > /dev/null 2>&1 || fail "$what"
}

A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
C=cccccccccccccccccccccccccccccccccccccccc

# The stubs. Each `ratio-<case>` file is a queue: one ratio per bench call of that case, 1.000 when empty.
stub="$tmp/stub"; mkdir -p "$stub"
cat > "$tmp/ab" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
stub="$PERF_STUB"
case "$1" in
init)
    out=""; base=""
    while (( $# )); do case "$1" in --out) out="$2";; --base) base="$2";; esac; shift; done
    echo "init base=$base" >> "$stub/ab.log"
    if [[ -f "$stub/no-build-at" && "$base" == "$(cat "$stub/no-build-at")" ]]; then echo "does not build" >&2; exit 1; fi
    mkdir -p "$out/probe/src"
    ;;
bench)
    case_name=""
    while (( $# )); do case "$1" in --case) case_name="$2";; esac; shift; done
    q="$stub/ratio-$case_name"
    ratio=1.000
    if [[ -s "$q" ]]; then ratio="$(head -n 1 "$q")"; sed -i.bak '1d' "$q"; rm -f "$q.bak"; fi
    echo "case \`$case_name\`, 15 pairs, load averages: 1.00 1.00 1.00"
    echo "metric              base best      head best head/base    base median    head median"
    echo "wall ns              79386334       89326917     1.125       87168167       89713375"
    head=$(awk -v r="$ratio" 'BEGIN { printf "%d", 1000000000 * r }')
    if [[ ! -f "$stub/no-counters" ]]; then
        echo "instructions       1000000000     $head     $ratio     1000000000     $head"
    fi
    if [[ -f "$stub/problem-$case_name" ]]; then echo "problem: case too small"; fi
    ;;
esac
STUB
cat > "$tmp/probe" <<'STUB'
#!/usr/bin/env bash
# probe bench <case> <side>
heap=1000; [[ "$3" == head ]] && heap="$(cat "$PERF_STUB/heap-head-$2" 2>/dev/null || echo 1000)"
echo "ns=1 sum=$heap"
STUB
cat > "$tmp/gh" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$PERF_STUB/gh.log"
if [[ "$1 $2" == "issue list" && -f "$PERF_STUB/existing" ]]; then cat "$PERF_STUB/existing"; fi
for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == --body-file ]]; then j=$((i + 1)); cat "${!j}" > "$PERF_STUB/body-$(wc -l < "$PERF_STUB/gh.log" | tr -d ' ').md"; fi
done
STUB
# rss <probe> <case> <side>: the empty case's 500 under a case's 1000500 (rss-head-<case> overrides the head's; a
# queue rss-queue-<side>-<case> gives one reading per call first).
cat > "$tmp/rss" <<'STUB'
#!/usr/bin/env bash
[[ "$2" == __empty ]] && { echo 500; exit 0; }
[[ -f "$PERF_STUB/rss-fail" ]] && exit 1
q="$PERF_STUB/rss-queue-$3-$2"
if [[ -s "$q" ]]; then head -n 1 "$q"; sed -i.bak '1d' "$q"; rm -f "$q.bak"; exit 0; fi
rss=1000500; [[ "$3" == head ]] && rss="$(cat "$PERF_STUB/rss-head-$2" 2>/dev/null || echo 1000500)"
echo "$rss"
STUB
chmod +x "$tmp/ab" "$tmp/probe" "$tmp/gh" "$tmp/rss"
export PERF_STUB="$stub" PERF_JOB_AB="$tmp/ab" PERF_JOB_PROBE="$tmp/probe" PERF_JOB_GH="$tmp/gh" PERF_JOB_RSS="$tmp/rss"
unset GITHUB_OUTPUT GITHUB_RUN_ID GITHUB_REPOSITORY

reset() { rm -rf "$stub"; mkdir -p "$stub"; : > "$stub/gh.log"; : > "$stub/ab.log"; }
run() { PERF_JOB_WORK="$tmp/work-$RANDOM" "$script" run --state "$1" --head "$2" > "$tmp/out.log" 2>&1; }

# 1. The early exit.
reset
s="$tmp/s1"
[[ "$("$script" check --state "$s" --head "$A")" == "skip=false" ]] || fail "an empty state must not skip"
run "$s" "$A" || fail "baseline run failed: $(cat "$tmp/out.log")"
[[ "$("$script" check --state "$s" --head "$A")" == "skip=true" ]] || fail "an unchanged SHA must skip"
[[ "$("$script" check --state "$s" --head "$B")" == "skip=false" ]] || fail "a new SHA must not skip"
GITHUB_OUTPUT="$tmp/gho" "$script" check --state "$s" --head "$A" > /dev/null
[[ "$(cat "$tmp/gho")" == "skip=true" ]] || fail "check must write skip=true to GITHUB_OUTPUT"

# 2. One JSON line per case, with the SHA, instructions and heap bytes; no issue for a clean run.
all_cases="feed scroll search cat rewrap vtebench heap_idle heap_scrollback heap_lines_plain heap_lines_mixed rss_1m"
[[ "$(wc -l < "$s/perf.jsonl" | tr -d ' ')" == 11 ]] || fail "expected 11 lines, got $(wc -l < "$s/perf.jsonl")"
for c in $all_cases; do
    jq -e --arg c "$c" --arg a "$A" 'select(.case == $c and .sha == $a and .status == "ok")' "$s/perf.jsonl" > /dev/null \
        || fail "no ok line for $c at $A"
done
jq -e 'select(.case == "feed") | .instructions == 1000000000 and .ratio == 1' "$s/perf.jsonl" > /dev/null || fail "feed instructions"
jq -e 'select(.case == "heap_idle") | .heap_bytes == 1000 and .base_heap_bytes == 1000' "$s/perf.jsonl" > /dev/null || fail "heap bytes"
jq -e 'select(.case == "rss_1m") | .heap_bytes == 1000000 and .base_heap_bytes == 1000000 and .instructions == null' \
    "$s/perf.jsonl" > /dev/null || fail "resident bytes are the case's peak over the empty probe's"
jq -e 'select(.case == "vtebench") | .instructions == 1000000000' "$s/perf.jsonl" > /dev/null || fail "vtebench instructions"
jq -e 'select(.case == "feed") | .note | startswith("baseline")' "$s/perf.jsonl" > /dev/null || fail "the first run says it is a baseline"
[[ ! -s "$stub/gh.log" ]] || fail "a clean run must not call gh"
grep -q "init base=$A" "$stub/ab.log" || fail "the baseline run compares $A with itself"

# 3. A clean second run: base is the previous SHA, no issue.
run "$s" "$B" || fail "clean run failed"
grep -q "init base=$A" "$stub/ab.log" || fail "the second run must compare against $A"
[[ ! -s "$stub/gh.log" ]] || fail "a clean second run must not call gh"

# 4. The three-build rule: over the bar once, under it in a later build, is not a regression.
reset
printf '1.050\n1.010\n' > "$stub/ratio-feed"
run "$s" "$C" || fail "noisy run failed"
jq -e --arg c "$C" 'select(.sha == $c and .case == "feed") | .status == "ok" and .ratio == 1.01' "$s/perf.jsonl" > /dev/null \
    || fail "one build under the bar clears the case"
[[ ! -s "$stub/gh.log" ]] || fail "no issue when a later build is under the bar"
[[ "$(grep -c '^init' "$stub/ab.log")" == 2 ]] || fail "only the case over the bar is rebuilt, got $(grep -c '^init' "$stub/ab.log") probe builds"

# 5. A regression over every build: an issue is opened naming the SHA range; a heap past the bar likewise.
reset
s="$tmp/s5"
run "$s" "$A" || fail "baseline run failed"
reset
printf '1.080\n1.070\n1.090\n' > "$stub/ratio-search"
echo 1030 > "$stub/heap-head-heap_scrollback"
echo 1030500 > "$stub/rss-head-rss_1m"
run "$s" "$B" || fail "regression run failed: $(cat "$tmp/out.log")"
jq -e --arg b "$B" 'select(.sha == $b and .case == "rss_1m") | .status == "regressed" and .heap_bytes == 1030000 and .ratio == 1.03' \
    "$s/perf.jsonl" > /dev/null || fail "a resident set 3 % over must be regressed"
grep -q '| rss_1m | regressed' "$stub"/body-*.md || fail "the issue must list the resident case"
jq -e --arg b "$B" 'select(.sha == $b and .case == "search") | .status == "regressed"' "$s/perf.jsonl" > /dev/null || fail "search must be regressed"
jq -e --arg b "$B" 'select(.sha == $b and .case == "heap_scrollback") | .status == "regressed" and .heap_bytes == 1030' "$s/perf.jsonl" > /dev/null || fail "heap must be regressed"
jq -e --arg b "$B" 'select(.sha == $b and .case == "feed") | .status == "ok"' "$s/perf.jsonl" > /dev/null || fail "feed must stay ok"
grep -q '^issue list --label type:bug' "$stub/gh.log" || fail "the job must look for an open issue"
grep -q '^issue create --title .* --label type:bug --body-file' "$stub/gh.log" || fail "the job must open an issue"
if ! grep -q "$A..$B" "$stub"/body-*.md 2>/dev/null; then fail "the issue must name the range $A..$B"; fi
grep -q '| search | regressed' "$stub"/body-*.md || fail "the issue must list the case"
[[ "$(grep -c '^init' "$stub/ab.log")" == 3 ]] || fail "a regressed case is measured in three probe builds"

# 6. An open issue gets a comment, not a second issue; the baseline skips the regressed SHA.
reset
echo 77 > "$stub/existing"
printf '1.080\n1.070\n1.090\n' > "$stub/ratio-search"
run "$s" "$C" || fail "comment run failed"
grep -q "init base=$A" "$stub/ab.log" || fail "the baseline must stay at $A while $B is regressed"
grep -q '^issue comment 77 --body-file' "$stub/gh.log" || fail "an open issue must get a comment"
grep -q '^issue create' "$stub/gh.log" && fail "no second issue"
grep -q "$A..$C" "$stub"/body-*.md || fail "the comment must name the range $A..$C"

# 7. The case set does not build at the baseline: measure against itself, say so.
reset
s="$tmp/s7"
run "$s" "$A" || fail "baseline run failed"
echo "$A" > "$stub/no-build-at"
run "$s" "$B" || fail "fallback run failed: $(cat "$tmp/out.log")"
jq -e --arg b "$B" 'select(.sha == $b and .case == "feed") | .base == $b and (.note | test("does not build"))' "$s/perf.jsonl" > /dev/null \
    || fail "a case set that does not build at the baseline falls back to a baseline run"

# 7b. A machine without instruction counters (a virtual machine): the case set is still taken, the instruction cases
# are `noisy` with the wall ratio and a note (no failure, no retry), and the heap cases still bind.
reset
s="$tmp/s7b"
touch "$stub/no-counters"
echo 1030 > "$stub/heap-head-heap_idle"
run "$s" "$A" || fail "a run without counters must not fail: $(cat "$tmp/out.log")"
[[ "$(wc -l < "$s/perf.jsonl" | tr -d ' ')" == 11 ]] || fail "no counters: expected 11 lines"
jq -e 'select(.case == "feed") | .status == "noisy" and .instructions == null and .ratio == 1.125 and (.note | test("no instruction counters"))' \
    "$s/perf.jsonl" > /dev/null || fail "no counters: feed must be noisy with the wall ratio and a note"
[[ "$(grep -c '^init' "$stub/ab.log")" == 1 ]] || fail "no counters: no second probe build"
jq -e 'select(.case == "heap_idle") | .status == "regressed"' "$s/perf.jsonl" > /dev/null || fail "no counters: the heap bar still binds"

# 7b2. The highest of three resident readings counts: one low reading on the base side does not hide a regression,
# and one low reading on the head side is not a gain.
reset
s="$tmp/s7b2"
printf '1000500\n960500\n1000500\n' > "$stub/rss-queue-base-rss_1m"
printf '1030500\n1000500\n1000500\n' > "$stub/rss-queue-head-rss_1m"
run "$s" "$A" || fail "resident runs failed: $(cat "$tmp/out.log")"
jq -e 'select(.case == "rss_1m") | .status == "regressed" and .heap_bytes == 1030000 and .base_heap_bytes == 1000000' \
    "$s/perf.jsonl" > /dev/null || fail "the highest of three resident readings per side must count"

# 7c. A resident reading that cannot be taken fails the run (no line is written for a half-measured set).
reset
s="$tmp/s7c"
touch "$stub/rss-fail"
expect "a failed resident reading fails the run" bash -c "! PERF_JOB_WORK='$tmp/work-7c' '$script' run --state '$s' --head $A"
[[ ! -s "$s/perf.jsonl" ]] || fail "a failed resident reading must not append lines"

# 8. The gate's release timing step is what the job runs: its packages and filter come out of quality-gate.sh.
pk="$("$script" timing-packages)"
[[ "$pk" == *"-p tessera-ffi"* && "$pk" != *"'"* ]] || fail "timing-packages: '$pk'"
flt="$("$script" timing-filter)"
[[ "$flt" == *"binary(=budgets)"* && "$flt" != *"'"* ]] || fail "timing-filter does not carry the #216 budgets: '$flt'"

# 9. A failed step files its own issue, titled apart from a regression (they must not share a thread).
reset
"$script" failed --step "release timing and budget tests" --head "$A" > /dev/null 2>&1 || fail "failed subcommand"
grep -q '^issue create --title perf: the scheduled performance job failed --label' "$stub/gh.log" || fail "a failed step must open an issue titled 'failed': $(cat "$stub/gh.log")"
grep -q 'found a regression' "$stub/gh.log" && fail "a failed step must not use the regression title"
grep -q 'in:title "perf: the scheduled performance job failed"' "$stub/gh.log" || fail "the failed issue is looked up by its own title"
grep -q "release timing and budget tests" "$stub"/body-*.md || fail "the failed issue names the step"

[[ "$flt" == *"!(test(=startup_to_the_first_frame_stays_within_its_budget))" ]] || fail "timing-filter must skip the startup wall-time budget: '$flt'"
[[ "$flt" == *"(package(tessera-ffi) & binary(=budgets))"* ]] || fail "timing-filter must keep the byte budgets: '$flt'"
grep -q -- '--no-fail-fast' "$(dirname "$script")/../.github/workflows/perf.yml" || fail "the workflow must run nextest with --no-fail-fast"
wf_file="$(dirname "$script")/../.github/workflows/perf.yml"
if ! grep -q 'upload-artifact' "$wf_file" || ! grep -q 'name: perf-logs' "$wf_file"; then
    fail "the workflow must upload the logs"
fi
# The upload is logs only: the probe build trees (target/, base.tar, many GB) live outside the uploaded directory.
if ! grep -q '^ *path: perf-logs$' "$wf_file"; then fail "the logs artifact must upload exactly perf-logs"; fi
if grep 'PERF_JOB_WORK:' "$wf_file" | grep -q 'perf-logs'; then
    fail "PERF_JOB_WORK must be outside perf-logs, or the artifact carries the probe builds"
fi
grep -q 'PERF_JOB_WORK"/\*\.log' "$wf_file" || fail "the workflow must copy the case-set *.log files into perf-logs"

# 10. The workflow keeps measuring and saving after a red timing step: the case set, the cache save and the upload
# do not depend on `success()` of the steps before them (a red release timing step would skip them).
wf="$(cd "$(dirname "$script")/.." && pwd)/.github/workflows/perf.yml"
step_if() { awk -v n="$1" '$0 ~ "- name: " n {f=1} f && /^ *if:/ {print; exit}' "$wf"; }
for st in "Case set against" "Save the state" "Upload the trend"; do
    step_if "$st" | grep -q '!cancelled()' || fail "workflow step '$st' must run after a red timing step (if: !cancelled() ...)"
    step_if "$st" | grep -q 'success()' && fail "workflow step '$st' must not wait on success()"
done

# 11. An accepted step (scripts/perf-steps.txt, ADR 0049 D9): a line head has and base has not raises that case's bar
# for that comparison only; the next comparison, whose base has the line, is back at the default bar.
reset
s="$tmp/s11"
steps="$tmp/steps"; rm -rf "$steps"; mkdir -p "$steps"
export PERF_JOB_STEPS_DIR="$steps"
printf '# case bar reason\n' > "$steps/$A"
printf '# case bar reason\nscroll 1.30 ADR 0051, packed scrollback (#51)\nheap_idle 1.05 a test step\n' > "$steps/$B"
cp "$steps/$B" "$steps/$C"
run "$s" "$A" || fail "step baseline run failed: $(cat "$tmp/out.log")"
reset
printf '1.250\n' > "$stub/ratio-scroll"
printf '1.250\n1.250\n1.250\n' > "$stub/ratio-cat"
echo 1040 > "$stub/heap-head-heap_idle"
run "$s" "$B" || fail "step run failed: $(cat "$tmp/out.log")"
jq -e --arg b "$B" 'select(.sha == $b and .case == "scroll") | .status == "ok" and .bar == 1.3 and (.note | test("accepted step.*#51"))' \
    "$s/perf.jsonl" > /dev/null || fail "a step's line at head and not at base raises that case's bar"
jq -e --arg b "$B" 'select(.sha == $b and .case == "heap_idle") | .status == "ok" and .bar == 1.05' "$s/perf.jsonl" > /dev/null \
    || fail "a step raises a heap case's bar too"
jq -e --arg b "$B" 'select(.sha == $b and .case == "cat") | .status == "regressed" and .bar == 1.02' "$s/perf.jsonl" > /dev/null \
    || fail "a step raises no other case's bar"
reset
printf '1.250\n' > "$stub/ratio-cat"
run "$s" "$C" || fail "step follow-up run failed: $(cat "$tmp/out.log")"
grep -q "init base=$A" "$stub/ab.log" || fail "the baseline stays at $A while $B is regressed"
jq -e --arg c "$C" 'select(.sha == $c and .case == "scroll") | .bar == 1.3' "$s/perf.jsonl" > /dev/null \
    || fail "the step applies while the base still lacks its line"
reset
s="$tmp/s11b"
run "$s" "$B" || fail "step baseline run at B failed"
reset
printf '1.250\n1.250\n1.250\n' > "$stub/ratio-scroll"
run "$s" "$C" || fail "step run after the step failed"
jq -e --arg c "$C" 'select(.sha == $c and .case == "scroll") | .status == "regressed" and .bar == 1.02' "$s/perf.jsonl" > /dev/null \
    || fail "a step whose line the base has no longer raises the bar"
grep -qE '\| scroll \| regressed \| 1\.250? \| 1\.02 \|' "$stub"/body-*.md || fail "the issue lists each case's bar"
unset PERF_JOB_STEPS_DIR

# 12. Bad input.
expect "a non-sha head is refused" bash -c "! '$script' check --state '$tmp/s8' --head 'main; rm -rf /'"
expect "an unknown subcommand is refused" bash -c "! '$script' frobnicate"

if (( failures )); then echo "$failures failure(s)" >&2; exit 1; fi
echo "perf-job: all checks passed"
