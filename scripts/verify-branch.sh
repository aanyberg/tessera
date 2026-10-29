#!/usr/bin/env zsh
# Mechanical verification of a task branch before integration: the facts a reviewer should not spend reasoning on.
# Usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>]
#                                            [--load <crate>:<lib|bins|bin/<name>|tests|test-binary>]... [--no-load]
#                                            [--parallel <1-8>] [--list-targets]
#        scripts/verify-branch.sh --help
#
# Checks, each printed as PASS, WARN or FAIL:
#   gate      scripts/quality-gate.sh exits 0 (reuses a recorded pass for the same commit, toolchain and gate script)
#   clean     the worktree has no uncommitted or untracked changes
#   base      the branch contains <base> (default: main)
#   commits   every commit in <base>..HEAD has a Conventional Commit subject and the Co-Authored-By and
#             Claude-Session trailers (with --coauthor / --session, those exact lines)
#   scope     with --allow: changed paths outside every allowed file or directory (WARN: the lead decides)
#   adr       no ADR added or renamed on the branch shares its number with another ADR on the branch or on <base>
#             (paths the branch deletes or renames away do not count)
#   load      timing-sensitive tests pass 10 runs each, at most --parallel at a time. Triggers: a nextest `timing`
#             override in a changed package; a changed test file (tests/*.rs; a src file with #[cfg(test)], or
#             src/**/tests.rs and src/**/tests/*.rs) that mentions wall-clock time anywhere in it (that test binary;
#             every test binary for a shared helper under tests/<dir>/; the lib, or every bin for src/main.rs,
#             src/bin/ and bins-only packages); and every --load target. A target that matches no test suite FAILs.
#
# --parallel <N>   load check: at most N test runs in flight, per target and in total (targets run one
#                  after another). Default 3 so a busy machine is not exhausted; 1-8 accepted. A run that records
#                  no exit status FAILs. Ctrl-C (or TERM/HUP) stops the runs in flight and exits 130 (143/129).
# --list-targets   print the effective parallelism as a `# parallel: N` line, then the derived load targets
#                  (`crate:kind`, one per line, sorted), and exit 0. Runs no check and builds nothing.
# Exit 0 when nothing FAILs, 1 otherwise, 2 on a usage error. Logs go to the directory printed on the first line.
# The script is zsh; started from bash or sh it re-executes itself under zsh.
[ -n "${ZSH_VERSION:-}" ] || exec zsh "$0" "$@"
set -uo pipefail

self="${0:A}"  # inside a function $0 is the function's name
help() { sed -n '2,/^[^#]/s/^# \{0,1\}//p' "$self"; }
usage() {
    print -ru2 -- "usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>] [--load <crate>:<target>]... [--no-load] [--parallel <1-8>] [--list-targets]  (--help for details)"
    exit 2
}
[[ "${1:-}" == (-h|--help) ]] && { help; exit 0; }
(( $# >= 1 )) || usage
wt="$1"; shift
base=main session="" coauthor="" no_load="" list_targets="" parallel=3 runs_per_test=10 max_parallel=8
typeset -a allow loads
while (( $# )); do
    case "$1" in
        --base|--allow|--session|--coauthor|--load|--parallel) (( $# >= 2 )) || usage ;;
    esac
    case "$1" in
        --base) base="$2"; shift 2 ;;
        --allow) allow+=("$2"); shift 2 ;;
        --session) session="$2"; shift 2 ;;
        --coauthor) coauthor="$2"; shift 2 ;;
        --load) loads+=("$2"); shift 2 ;;
        --no-load) no_load=1; shift ;;
        --parallel)
            [[ "$2" == [1-${max_parallel}] ]] || { print -ru2 -- "--parallel takes 1-${max_parallel}, got '$2'"; usage; }
            parallel="$2"; shift 2 ;;
        --list-targets) list_targets=1; shift ;;
        -h|--help) help; exit 0 ;;
        *) usage ;;
    esac
done
cd "$wt" || exit 2
script_dir="${self:h}"
if [[ -n "$list_targets" ]] && ! git rev-parse --verify -q "${base}^{commit}" > /dev/null; then
    print -ru2 -- "unknown base '$base'"; exit 2
fi

changed=("${(@f)$(git diff --name-only "$base...HEAD")}")
changed=(${changed:#})

# Load targets ("<crate>:<kind>" keys). Always brace the name before a colon: zsh reads `$name:lib` as `${name:l}`
# followed by "ib", and `$name:tests` as `${name:t}` followed by "ests".
timing='Instant|Duration::|elapsed\(|recv_timeout|thread::sleep|deadline'
# "<package dir> <package name>" for a path, empty outside any package.
package_of() {
    local d="${1:h}"
    while [[ "$d" != "." && "$d" != "/" ]]; do
        if [[ -f "$d/Cargo.toml" ]] && grep -q '^\[package\]' "$d/Cargo.toml"; then
            print -r -- "$d $(sed -n 's/^name *= *"\(.*\)"/\1/p' "$d/Cargo.toml" | head -1)"
            return
        fi
        d="${d:h}"
    done
}
typeset -A targets touched
for t in $loads; do targets[$t]=1; done
for f in $changed; do
    pkg="$(package_of "$f")"
    [[ -n "$pkg" ]] || continue
    dir="${pkg%% *}" name="${pkg#* }"
    touched[$name]=1
    [[ -f "$f" ]] || continue
    rel="${f#${dir}/}"
    if [[ "$rel" == tests/*/* ]]; then
        grep -qE "$timing" "$f" && targets[${name}:tests]=1
    elif [[ "$rel" == tests/*.rs ]]; then
        grep -qE "$timing" "$f" && targets[${name}:${${rel:t}%.rs}]=1
    elif [[ "$rel" == src/*.rs ]] && grep -qE "$timing" "$f" \
        && { grep -q '#\[cfg(test)\]' "$f" || [[ "${rel:t}" == tests.rs || "$rel" == */tests/*.rs ]]; }; then
        if [[ "$rel" == src/main.rs || "$rel" == src/bin/* || ! -f "$dir/src/lib.rs" ]]; then
            targets[${name}:bins]=1
        else
            targets[${name}:lib]=1
        fi
    fi
done
if [[ -f .config/nextest.toml ]]; then
    while IFS= read -r line; do
        p="$(print -r -- "$line" | sed -n 's/.*package(\([^)]*\)).*/\1/p')"
        [[ -n "$p" && -n "${touched[$p]:-}" ]] || continue
        b="$(print -r -- "$line" | sed -n 's/.*binary(\([^)]*\)).*/\1/p')"
        if [[ -n "$b" ]]; then targets[${p}:${b}]=1; else targets[${p}:lib]=1; fi
    done < <(grep '^filter' .config/nextest.toml)
fi

if [[ -n "$list_targets" ]]; then
    print -r -- "# parallel: $parallel"
    (( ${#targets} )) && print -rl -- ${(ko)targets}
    exit 0
fi

# An explicit template: macOS `mktemp -d` alone ignores TMPDIR.
logs="$(mktemp -d "${${TMPDIR:-/tmp}%/}/verify-branch.XXXXXX")"
print "logs: $logs"
failed=0
pass() { print -P "%F{green}PASS%f $1"; }
warn() { print -P "%F{yellow}WARN%f $1"; }
fail() { print -P "%F{red}FAIL%f $1"; failed=1; }

# gate
"$script_dir/quality-gate.sh" "$PWD" > "$logs/gate.log" 2>&1
rc=$?
if (( rc == 0 )); then pass "gate"; else fail "gate (exit $rc): $logs/gate.log"; tail -20 "$logs/gate.log"; fi

# clean
if [[ -z "$(git status --porcelain)" ]]; then pass "clean"; else fail "clean:"; git status --porcelain; fi

# base
if git merge-base --is-ancestor "$base" HEAD; then pass "base contains $base"; else fail "base: HEAD does not contain $base (merge it first)"; fi

# commits
conventional='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([a-z0-9._/-]+\))?!?: .+'
bad=()
for sha in $(git rev-list --no-merges "$base..HEAD"); do
    subject="$(git log -1 --format=%s "$sha")"
    body="$(git log -1 --format=%B "$sha")"
    problems=()
    [[ "$subject" =~ $conventional ]] || problems+=("subject not conventional")
    if [[ -n "$coauthor" ]]; then
        print -r -- "$body" | grep -qxF "$coauthor" || problems+=("no '$coauthor'")
    else
        print -r -- "$body" | grep -q '^Co-Authored-By: ' || problems+=("no Co-Authored-By")
    fi
    if [[ -n "$session" ]]; then
        print -r -- "$body" | grep -qxF "Claude-Session: $session" || problems+=("no 'Claude-Session: $session'")
    else
        print -r -- "$body" | grep -q '^Claude-Session: ' || problems+=("no Claude-Session")
    fi
    (( ${#problems} )) && bad+=("${sha:0:7} $subject (${(j:, :)problems})")
done
if (( ${#bad} )); then fail "commits:"; print -l -- "  "${^bad}; else pass "commits"; fi

# scope
if (( ${#allow} )); then
    outside=()
    for f in $changed; do
        ok=""
        for prefix in $allow; do
            [[ "$f" == "${prefix%/}" || "$f" == "${prefix%/}/"* ]] && { ok=1; break; }
        done
        [[ -n "$ok" ]] || outside+=("$f")
    done
    if (( ${#outside} )); then warn "scope: outside the allowed paths:"; print -l -- "  "${^outside}; else pass "scope"; fi
fi

# adr
adr_num() { local n="${1:t}"; print -r -- "${n%%-*}"; }
gone=( ${(f)"$(git diff --name-status -M "$base...HEAD" -- docs/adr/ | awk '$1 == "D" || $1 ~ /^R/ { print $2 }')"} )
known=()
for k in ${(u)${(f)"$(git ls-tree --name-only HEAD docs/adr/; git ls-tree --name-only "$base" docs/adr/)"}}; do
    (( ${gone[(Ie)$k]} )) || known+=("$k")
done
collisions=()
for f in $(git diff --name-only --diff-filter=AR "$base...HEAD" -- 'docs/adr/*.md'); do
    num="$(adr_num "$f")"
    same=()
    for k in $known; do [[ "$(adr_num "$k")" == "$num" && "$k" != "$f" ]] && same+=("$k"); done
    (( ${#same} )) && collisions+=("$f shares $num with ${(j:, :)same}")
done
if (( ${#collisions} )); then fail "adr:"; print -l -- "  "${^collisions}; else pass "adr"; fi

# load
if [[ -z "$no_load" ]]; then
    if (( ${#targets} == 0 )); then
        pass "load (not applicable)"
    else
        typeset -A ran
        typeset -a pids
        # Background runs ignore SIGINT in a non-interactive shell, so Ctrl-C would leave them running: stop each
        # in-flight run and the test binary it is waiting for, then exit.
        stop_runs() {
            local p
            for p in $pids; do pkill -TERM -P "$p" 2>/dev/null; kill -TERM "$p" 2>/dev/null; done
            print -ru2 -- "interrupted: stopped ${#pids} load run(s); logs: $logs"
            exit "$1"
        }
        trap 'stop_runs 130' INT
        trap 'stop_runs 143' TERM
        trap 'stop_runs 129' HUP
        for target in ${(ko)targets}; do
            crate="${target%%:*}" what="${target#*:}"
            d="$logs/load-${crate}-${what//\//-}"; mkdir -p "$d"
            if ! cargo nextest list -p "$crate" --message-format json > "$d/list.json" 2> "$d/list.err"; then
                fail "load ${target}: cannot list tests: $d/list.err"; continue
            fi
            case "$what" in
                lib) want="$crate" kind="" ;;
                tests) want="" kind="test" ;;
                bins) want="" kind="bin" ;;
                *) want="${crate}::${what}" kind="" ;;
            esac
            suites=( ${(f)"$(jq -r --arg want "$want" --arg kind "$kind" --arg crate "$crate" '."rust-suites" | to_entries[]
                | select(if $want != "" then .key == $want
                         else .value.kind == $kind and (.key | startswith($crate + "::")) end)
                | "\(.value."binary-path")\t\(.value.cwd)"' "$d/list.json")"} )
            suites=(${suites:#})
            if (( ${#suites} == 0 )); then fail "load ${target}: no test suite matches in $crate"; continue; fi
            # One pool per target: before each start, wait for the oldest run while $parallel are in flight.
            pids=() expected=0
            for suite in $suites; do
                B="${suite%%$'\t'*}" C="${suite#*$'\t'}"
                [[ -n "${ran[$B]:-}" ]] && continue
                ran[$B]=1
                for t in $("$B" --list 2>/dev/null | sed -n 's/: test$//p'); do
                    for (( i = 1; i <= runs_per_test; i++ )); do
                        if (( ${#pids} >= parallel )); then wait "${pids[1]}"; shift pids; fi
                        (cd "$C" && "$B" "$t" --exact > "$d/${B:t}.${t}.${i}.log" 2>&1; print $? > "$d/${B:t}.${t}.${i}.rc") &
                        pids+=($!)
                        (( ++expected ))
                    done
                done
            done
            wait
            pids=()
            runs=( "$d"/*.rc(N) )
            # A run killed before it recorded its status leaves no .rc file: that is a failure, not a smaller sample.
            if (( ${#runs} < expected )); then
                fail "load ${target}: $(( expected - ${#runs} )) of $expected runs recorded no exit status (killed?): $d"
                continue
            fi
            # Never call grep without files: it would read stdin and hang.
            if (( ${#runs} == 0 )); then pass "load ${target} (not applicable: no tests)"; continue; fi
            bad_runs=( ${(f)"$(grep -L '^0$' $runs < /dev/null 2>/dev/null)"} )
            bad_runs=(${bad_runs:#})
            if (( ${#bad_runs} )); then fail "load ${target}: ${#bad_runs}/${#runs} runs failed:"; print -l -- "  "${^bad_runs}
            else pass "load ${target} (${#runs} runs, at most $parallel at a time)"; fi
        done
    fi
fi

exit $failed
