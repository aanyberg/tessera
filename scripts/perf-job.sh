#!/usr/bin/env bash
# The logic of the scheduled performance and memory job (.github/workflows/perf.yml, #612, ADR 0049).
#
#   scripts/perf-job.sh check  --state <dir> [--head <sha>]
#       Prints `skip=true` when <dir>/perf.jsonl already has measurements of <head> (nothing new on main since the last
#       run), `skip=false` otherwise; also appended to $GITHUB_OUTPUT when that is set. Exit 0 either way.
#   scripts/perf-job.sh run    --state <dir> [--head <sha>]
#       Measures the fixed case set (scripts/perf-cases.rs) of <head> against the last measured SHA with no regression
#       and appends one JSON line per case to <dir>/perf.jsonl. A case past its bar opens or comments on a `type:bug`
#       issue naming the SHA range. Exit 0 when the measurements were taken, even when a bar was missed (the issue is
#       the signal); nonzero when they could not be taken.
#   scripts/perf-job.sh timing-packages | timing-filter
#       The `-p <crate>` arguments and the nextest filter of the quality gate's release timing step (read from
#       scripts/quality-gate.sh, so the job runs the gate's bars, the #216 byte budgets included, except the startup
#       wall-time budget a runner cannot meet; see runner_skips).
#   scripts/perf-job.sh failed --step <name> [--head <sha>] [--state <dir>]
#       Files the same issue for a failed step of the job (the release timing bars, the budget tests).
#
# One line per case: {ts, sha, base, case, instructions, base_instructions, ratio, heap_bytes, base_heap_bytes, bar,
# status, note}; status is ok, regressed or noisy (the instrument could not back a verdict: a loaded machine, or one
# without instruction counters such as a GitHub macOS runner, where `ratio` is the wall-time ratio and the heap cases
# still bind). A line is a number for
# the case's own kind (instructions for feed, scroll and search; heap bytes for heap_idle and heap_scrollback) and
# null for the other.
#
# The baseline is the newest SHA none of whose lines is `regressed`, so a regression that stays does not become the
# new normal. Bars: instructions head/base <= PERF_JOB_BAR (1.02) in at least one of PERF_JOB_BUILDS (3) independent
# probe builds; heap head <= base * PERF_JOB_BAR.
#
# Test hooks (scripts/test-perf-job.sh): PERF_JOB_AB (default `cargo xtask ab`), PERF_JOB_PROBE (default the built
# probe binary; called `<case> <side>`, prints the checksum), PERF_JOB_GH (default `gh`), PERF_JOB_WORK (scratch).
#
# Bash 3.2 compatible (the macOS system bash).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
ab="${PERF_JOB_AB:-cargo xtask ab}"
gh_cmd="${PERF_JOB_GH:-gh}"
bar="${PERF_JOB_BAR:-1.02}"
builds="${PERF_JOB_BUILDS:-3}"
pairs="${PERF_JOB_PAIRS:-15}"
instr_cases="feed scroll search"
heap_cases="heap_idle heap_scrollback"
issue_title="perf: the scheduled performance job found a regression"
failed_title="perf: the scheduled performance job failed"

# Tests that bind an absolute wall time which only a local Mac meets (the GitHub runner took 99.7 ms at best for the
# 19.645 ms startup budget, #682); `just check` and the gate enforce them. The other timing tests have bars loose enough
# for the runner, and the byte budgets are not machine dependent, so they stay.
runner_skips="test(=startup_to_the_first_frame_stays_within_its_budget)"

die() { echo "perf-job: $*" >&2; exit 2; }

sub="${1:-}"; [[ -n "$sub" ]] || die "usage: perf-job.sh check|run|failed|timing-packages|timing-filter ..."
shift

# The gate's release timing command line: `run_nextest --release -p a -p b -E '<filter>'`.
gate_timing_line() {
    local found
    found="$(grep -F 'run_nextest --release ' "$root/scripts/quality-gate.sh" | head -n 1)"
    [[ -n "$found" ]] || die "scripts/quality-gate.sh has no 'run_nextest --release' line"
    printf '%s\n' "$found"
}
case "$sub" in
timing-packages)
    gate_timing_line | grep -oE -- '-p [a-z0-9-]+' | tr '\n' ' ' | sed 's/ $//'; echo
    exit 0 ;;
timing-filter)
    # The gate's filter minus the tests with an absolute wall-time budget measured on a local Mac (see runner_skips).
    gate="$(gate_timing_line | sed -n "s/.* -E '\(.*\)'\$/\1/p" | grep .)" || die "no -E filter in the gate's release timing line"
    printf '(%s) & !(%s)\n' "$gate" "$runner_skips"
    exit 0 ;;
esac
state="" head="" step=""
while (( $# )); do
    case "$1" in
        --state) [[ $# -ge 2 ]] || die "--state needs a directory"; state="$2"; shift 2 ;;
        --head) [[ $# -ge 2 ]] || die "--head needs a sha"; head="$2"; shift 2 ;;
        --step) [[ $# -ge 2 ]] || die "--step needs a name"; step="$2"; shift 2 ;;
        *) die "unknown argument '$1'" ;;
    esac
done
[[ -n "$head" ]] || head="$(git -C "$root" rev-parse HEAD)"
[[ "$head" =~ ^[0-9a-f]{7,40}$ ]] || die "'$head' is not a commit sha"
if [[ "$sub" != failed ]]; then [[ -n "$state" ]] || die "--state is required"; fi
jsonl="$state/perf.jsonl"

# The SHA of the newest line, or empty.
last_measured() { [[ -s "$jsonl" ]] && tail -n 1 "$jsonl" | jq -r '.sha' || true; }

# The newest SHA with lines and none of them regressed, or empty.
last_good() {
    [[ -s "$jsonl" ]] || return 0
    jq -rs '
        (map(select(.status == "regressed") | .sha) | unique) as $bad
        | [.[] | .sha | select(. as $s | $bad | index($s) | not)] | last // empty' "$jsonl"
}

emit_output() {
    echo "skip=$1"
    if [[ -n "${GITHUB_OUTPUT:-}" ]]; then echo "skip=$1" >> "$GITHUB_OUTPUT"; fi
}

# Opens an issue titled $2, or comments on the open one with that title. Args: body file, title.
file_issue() {
    local body="$1" title="$2" number
    number="$($gh_cmd issue list --label type:bug --state open --search "in:title \"$title\"" \
        --json number --jq '.[0].number // empty')"
    if [[ -n "$number" ]]; then
        $gh_cmd issue comment "$number" --body-file "$body"
    else
        $gh_cmd issue create --title "$title" --label type:bug --body-file "$body"
    fi
}

run_link() {
    if [[ -n "${GITHUB_RUN_ID:-}" && -n "${GITHUB_REPOSITORY:-}" ]]; then
        echo "Run: ${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"
    fi
}

# One JSON line. Args: case base instructions base_instructions ratio heap base_heap status note
line() {
    jq -nc --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg sha "$head" --arg base "$2" --arg case "$1" \
        --arg i "$3" --arg bi "$4" --arg r "$5" --arg h "$6" --arg bh "$7" --arg bar "$bar" \
        --arg status "$8" --arg note "$9" '
        def num: if . == "" then null else tonumber end;
        {ts: $ts, sha: $sha, base: $base, case: $case, instructions: ($i | num), base_instructions: ($bi | num),
         ratio: ($r | num), heap_bytes: ($h | num), base_heap_bytes: ($bh | num), bar: ($bar | tonumber),
         status: $status, note: $note}'
}

case "$sub" in
check)
    mkdir -p "$state"
    last="$(last_measured)"
    if [[ -n "$last" && "$last" == "$head" ]]; then
        echo "perf-job: main is still at $head, the last measured SHA: nothing to do" >&2
        emit_output true
    else
        echo "perf-job: measuring $head (last measured: ${last:-none})" >&2
        emit_output false
    fi
    ;;

failed)
    [[ -n "$step" ]] || die "--step is required"
    body="$(mktemp)"
    # Removed however the script ends: a failing `gh` call no longer leaves it in $TMPDIR (#698).
    trap 'rm -f "$body"' EXIT
    {
        echo "The scheduled performance job's step \`$step\` failed at \`$head\`."
        echo
        run_link
    } > "$body"
    file_issue "$body" "$failed_title"
    rm -f "$body"
    ;;

run)
    mkdir -p "$state"
    if [[ -n "${PERF_JOB_WORK:-}" ]]; then
        work="$PERF_JOB_WORK"
    else
        # A work directory of its own (a local run; the workflow sets PERF_JOB_WORK) goes when the run succeeds; a
        # failed run keeps it for its logs and names it last, instead of leaving every run's in $TMPDIR (#698).
        work="$(mktemp -d)"
        trap 'rc=$?; if (( rc == 0 )); then rm -rf "$work"; else echo "perf-job: logs kept in $work" >&2; fi' EXIT
    fi
    mkdir -p "$work"
    base="$(last_good)"
    note=""
    if [[ -z "$base" ]]; then base="$head"; note="baseline: no earlier measurement"; fi

    results="$work/results.jsonl"; : > "$results"
    todo="$instr_cases"

    # best_<case> files hold: ratio base_instr head_instr status
    attempt=1
    probe_dir=""
    first_probe=""
    while (( attempt <= builds )) && [[ -n "$todo" ]]; do
        probe_dir="$work/probe-$attempt"
        rm -rf "$probe_dir"
        measure_failed=0
        $ab init --base "$base" --crate tessera-term --crate tessera-grid --crate tessera-search --out "$probe_dir" \
            > "$work/init-$attempt.log" 2>&1 || measure_failed=1
        if (( measure_failed == 0 )); then
            cp "$root/scripts/perf-cases.rs" "$probe_dir/probe/src/cases.rs"
        fi
        still=""
        for c in $todo; do
            out="$work/bench-$c-$attempt.log"
            if (( measure_failed == 0 )) && $ab bench "$probe_dir" --case "$c" --pairs "$pairs" > "$out" 2>&1; then
                row="$(awk '$1 == "instructions" { print $2, $3, $4 }' "$out" | head -n 1)"
                problems="$(grep -c '^problem:' "$out" || true)"
                # shellcheck disable=SC2086 # intentional split of the three fields of $row
                set -- $row
                # No usable instruction counters (a virtual machine has none: `/usr/bin/time -l` omits the line or
                # reports 0): the bench ran, but it cannot back an instruction bar. Say so, keep the wall ratio for
                # the trend, and do not retry another build, which would not change that.
                if [[ -z "$row" || "${2:-}" == 0 || "${3:-}" == 0 ]] || ! awk -v r="${3:-x}" 'BEGIN { exit !(r + 0 > 0) }'; then
                    if ! grep -q '^wall ns ' "$out"; then measure_failed=1; break; fi
                    awk '$1 == "wall" && $2 == "ns" { print $5 }' "$out" | head -n 1 > "$work/wall-$c.txt"
                    echo "perf-job: no instruction counters for $c on this machine (see $out); recording the wall ratio only" >&2
                    continue
                fi
                if [[ "$problems" != 0 ]]; then verdict=noisy
                elif awk -v r="$3" -v b="$bar" 'BEGIN { exit !(r <= b) }'; then verdict=ok
                else verdict=regressed; fi
                echo "$3 $1 $2 $verdict" >> "$work/best-$c.txt"
                [[ "$verdict" == ok ]] || still="$still $c"
            else
                measure_failed=1
                break
            fi
        done
        if (( measure_failed )); then
            if [[ "$base" != "$head" ]]; then
                echo "perf-job: the case set did not build or run at $base; measuring $head against itself" >&2
                base="$head"; note="baseline: the case set does not build at the last measured SHA"
                rm -f "$work"/best-*.txt
                continue
            fi
            die "the case set could not be measured at $head (logs in $work)"
        fi
        [[ -n "$first_probe" ]] || first_probe="$probe_dir"
        todo="$still"
        attempt=$((attempt + 1))
    done

    probe_bin="${PERF_JOB_PROBE:-$first_probe/target/release/ab-probe}"
    for c in $instr_cases; do
        if [[ -s "$work/wall-$c.txt" ]]; then
            line "$c" "$base" "" "" "$(cat "$work/wall-$c.txt")" "" "" noisy \
                "no instruction counters on this machine: wall ratio only, no verdict; $note" >> "$results"
            continue
        fi
        # The lowest ratio over the builds is the verdict: one build under the bar clears the case.
        best="$(sort -g "$work/best-$c.txt" | head -n 1)"
        # shellcheck disable=SC2086 # intentional split of the four fields of $best
        set -- $best
        status="$4"
        # A case that was never ok keeps the status of its last build: regressed only when every build agrees.
        if [[ "$status" != ok ]] && grep -q ' noisy$' "$work/best-$c.txt"; then status=noisy; fi
        line "$c" "$base" "$3" "$2" "$1" "" "" "$status" "$note" >> "$results"
    done
    for c in $heap_cases; do
        head_heap="$($probe_bin bench "$c" head | sed -n 's/.*sum=\([0-9][0-9]*\).*/\1/p')"
        base_heap="$($probe_bin bench "$c" base | sed -n 's/.*sum=\([0-9][0-9]*\).*/\1/p')"
        [[ -n "$head_heap" && -n "$base_heap" ]] || die "no heap reading for $c"
        if awk -v h="$head_heap" -v b="$base_heap" -v r="$bar" 'BEGIN { exit !(h <= b * r) }'; then status=ok
        else status=regressed; fi
        ratio="$(awk -v h="$head_heap" -v b="$base_heap" 'BEGIN { printf "%.4f", (b > 0 ? h / b : 1) }')"
        line "$c" "$base" "" "" "$ratio" "$head_heap" "$base_heap" "$status" "$note" >> "$results"
    done

    cat "$results" >> "$jsonl"
    cat "$results"
    if jq -e 'select(.status == "regressed")' "$results" > /dev/null; then
        body="$work/issue.md"
        {
            echo "The scheduled performance job measured \`$head\` against \`$base\` (\`git log --oneline $base..$head\`)"
            echo "and a case is past its bar (instructions or heap at most ${bar}x the baseline)."
            echo
            echo "| case | status | head/base | instructions (head) | heap bytes (head) |"
            echo "| --- | --- | --- | --- | --- |"
            jq -r '"| \(.case) | \(.status) | \(.ratio) | \(.instructions // "-") | \(.heap_bytes // "-") |"' "$results"
            echo
            echo "Policy: docs/testing.md, Performance and memory bars. Trend: the \`perf-trend\` artifact of the run."
            run_link
        } > "$body"
        file_issue "$body" "$issue_title"
    fi
    ;;
*) die "unknown subcommand '$sub'" ;;
esac
