#!/usr/bin/env zsh
# Mechanical verification of a task branch before integration: the facts a reviewer should not spend reasoning on.
# Usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>]
#                                            [--load <crate>:<lib|bins|bin/<name>|tests|test-binary>]... [--no-load]
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
#   load      timing-sensitive tests pass 10 parallel runs each. Triggers: a nextest `timing` override in a changed
#             package; a changed test file (tests/*.rs; a src file with #[cfg(test)], or src/**/tests.rs and
#             src/**/tests/*.rs) that mentions wall-clock time anywhere in it (that test binary; every test binary
#             for a shared helper under tests/<dir>/; the lib, or every bin for src/main.rs, src/bin/ and bin-only
#             packages); and every --load target. A target that matches no test suite FAILs.
# Exit 0 when nothing FAILs, 1 otherwise. Logs go to the directory printed on the first line.
set -uo pipefail

usage() {
    print -u2 "usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>] [--load <crate>:<target>]... [--no-load]"
    exit 2
}
(( $# >= 1 )) || usage
wt="$1"; shift
base=main session="" coauthor="" no_load=""
typeset -a allow loads
while (( $# )); do
    case "$1" in
        --base) base="$2"; shift 2 ;;
        --allow) allow+=("$2"); shift 2 ;;
        --session) session="$2"; shift 2 ;;
        --coauthor) coauthor="$2"; shift 2 ;;
        --load) loads+=("$2"); shift 2 ;;
        --no-load) no_load=1; shift ;;
        *) usage ;;
    esac
done
cd "$wt" || exit 2
script_dir="${0:A:h}"
logs="$(mktemp -d)"
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

changed=("${(@f)$(git diff --name-only "$base...HEAD")}")
changed=(${changed:#})

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
        rel="${f#$dir/}"
        if [[ "$rel" == tests/*/* ]]; then
            grep -qE "$timing" "$f" && targets[$name:tests]=1
        elif [[ "$rel" == tests/*.rs ]]; then
            grep -qE "$timing" "$f" && targets[$name:${${rel:t}%.rs}]=1
        elif [[ "$rel" == src/*.rs ]] && grep -qE "$timing" "$f" \
            && { grep -q '#\[cfg(test)\]' "$f" || [[ "${rel:t}" == tests.rs || "$rel" == */tests/*.rs ]]; }; then
            if [[ "$rel" == src/main.rs || "$rel" == src/bin/* || ! -f "$dir/src/lib.rs" ]]; then
                targets[$name:bins]=1
            else
                targets[$name:lib]=1
            fi
        fi
    done
    if [[ -f .config/nextest.toml ]]; then
        while IFS= read -r line; do
            p="$(print -r -- "$line" | sed -n 's/.*package(\([^)]*\)).*/\1/p')"
            [[ -n "$p" && -n "${touched[$p]:-}" ]] || continue
            b="$(print -r -- "$line" | sed -n 's/.*binary(\([^)]*\)).*/\1/p')"
            if [[ -n "$b" ]]; then targets[$p:$b]=1; else targets[$p:lib]=1; fi
        done < <(grep '^filter' .config/nextest.toml)
    fi
    if (( ${#targets} == 0 )); then
        pass "load (not applicable)"
    else
        typeset -A ran
        for target in ${(k)targets}; do
            crate="${target%%:*}" what="${target#*:}"
            d="$logs/load-$crate-${what//\//-}"; mkdir -p "$d"
            if ! cargo nextest list -p "$crate" --message-format json > "$d/list.json" 2> "$d/list.err"; then
                fail "load $target: cannot list tests: $d/list.err"; continue
            fi
            case "$what" in
                lib) want="$crate" kind="" ;;
                tests) want="" kind="test" ;;
                bins) want="" kind="bin" ;;
                *) want="$crate::$what" kind="" ;;
            esac
            suites=( ${(f)"$(jq -r --arg want "$want" --arg kind "$kind" --arg crate "$crate" '."rust-suites" | to_entries[]
                | select(if $want != "" then .key == $want
                         else .value.kind == $kind and (.key | startswith($crate + "::")) end)
                | "\(.value."binary-path")\t\(.value.cwd)"' "$d/list.json")"} )
            suites=(${suites:#})
            if (( ${#suites} == 0 )); then fail "load $target: no test suite matches in $crate"; continue; fi
            for suite in $suites; do
                B="${suite%%$'\t'*}" C="${suite#*$'\t'}"
                [[ -n "${ran[$B]:-}" ]] && continue
                ran[$B]=1
                for t in $("$B" --list 2>/dev/null | sed -n 's/: test$//p'); do
                    for i in {1..10}; do (cd "$C" && "$B" "$t" --exact > "$d/${B:t}.$t.$i.log" 2>&1; print $? > "$d/${B:t}.$t.$i.rc") & done
                    wait
                done
            done
            runs=( "$d"/*.rc(N) )
            # Never call grep without files: it would read stdin and hang.
            if (( ${#runs} == 0 )); then pass "load $target (not applicable: no tests)"; continue; fi
            bad_runs=( ${(f)"$(grep -L '^0$' $runs < /dev/null 2>/dev/null)"} )
            bad_runs=(${bad_runs:#})
            if (( ${#bad_runs} )); then fail "load $target: ${#bad_runs}/${#runs} runs failed:"; print -l -- "  "${^bad_runs}
            else pass "load $target (${#runs} runs)"; fi
        done
    fi
fi

exit $failed
