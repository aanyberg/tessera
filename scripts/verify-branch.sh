#!/usr/bin/env zsh
# Mechanical verification of a task branch before integration: the facts a reviewer should not spend reasoning on.
# Usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>]
#                                            [--load <crate>:<lib|bins|bin/<name>|tests|test-binary>]... [--no-load]
#                                            [--parallel <1-8>] [--list-targets] [--closes <n[,n...]>] [--body <file>]
#        scripts/verify-branch.sh --help
#
# Checks, each printed as PASS, WARN or FAIL:
#   gate      scripts/quality-gate.sh exits 0 (reuses a recorded pass for the same commit, toolchain and gate script)
#   clean     the worktree has no uncommitted or untracked changes
#   base      the branch contains <base> (default: main)
#   commits   the tip commit (HEAD) has a Conventional Commit subject and the Co-Authored-By and Claude-Session
#             trailers (with --coauthor / --session, those exact lines); the integrator squashes the branch into one
#             commit, so a problem in an earlier commit of <base>..HEAD is a WARN, not a FAIL
#   scope     with --allow: changed paths outside every allowed file or directory (WARN: the lead decides)
#   adr       no ADR added or renamed on the branch shares its number with another ADR on the branch or on <base>
#             (paths the branch deletes or renames away do not count)
#   bodies    GitHub closing references (close/closes/closed, fix/fixes/fixed, resolve/resolves/resolved, any case, an
#             optional colon, then `#N`, `owner/repo#N` or an issue URL) in the commit messages of <base>..HEAD and,
#             with --body, in that PR body file. With --closes: FAIL on a reference to a number outside the list, and
#             (with --body) FAIL when a listed number has no closing reference in the body. Without --closes: WARN on
#             any closing reference, because a task branch should close nothing the lead did not name.
#   unsafe    FAIL when a line the branch adds to a Rust file (outside a `//` comment) contains the word `unsafe` or
#             `#[global_allocator]`, in a crate other than tessera-pty and tessera-ffi, unless the file is listed in
#             scripts/unsafe-exceptions.txt (docs/security.md S7). `unsafe_code` in a lint attribute does not match.
#   load      timing-sensitive tests pass 10 runs each, at most --parallel at a time. Triggers: a nextest `timing`
#             override in a changed package; a changed test file (tests/*.rs; a src file with #[cfg(test)], or
#             src/**/tests.rs and src/**/tests/*.rs) that mentions wall-clock time anywhere in it (that test binary;
#             every test binary for a shared helper under tests/<dir>/; the lib, or every bin for src/main.rs,
#             src/bin/ and bins-only packages); and every --load target. A target that matches no test suite FAILs.
#
# --parallel <N>   load check: at most N test runs in flight, per target and in total (targets run one
#                  after another). Default 3 so a busy machine is not exhausted; 1-8 accepted. A run that records
#                  no exit status FAILs. Ctrl-C (or TERM/HUP) stops the runs in flight and exits 130 (143/129).
# --closes <list>  bodies check: the issue numbers the task may close (from the brief's PR lines), comma separated.
# --body <file>    bodies check: also scan this PR body file. Together with --closes, every listed number must have a
#                  closing reference in it. Re-run with `--body <file> --closes <list> --no-load` on the integrator's
#                  body before `gh pr create`: the gate is a recorded-pass hit then, so only the `bodies` line is new.
# --list-targets   print the effective parallelism as a `# parallel: N` line, then the derived load targets
#                  (`crate:kind`, one per line, sorted), and exit 0. Runs no check and builds nothing.
# Exit 0 when nothing FAILs, 1 otherwise, 2 on a usage error. Logs go to the directory printed on the first line.
# The script is zsh; started from bash or sh it re-executes itself under zsh.
[ -n "${ZSH_VERSION:-}" ] || exec zsh "$0" "$@"
set -uo pipefail

self="${0:A}"  # inside a function $0 is the function's name
help() { sed -n '2,/^[^#]/s/^# \{0,1\}//p' "$self"; }
usage() {
    print -ru2 -- "usage: scripts/verify-branch.sh <worktree> [--base <ref>] [--allow <path>]... [--session <url>] [--coauthor <line>] [--load <crate>:<target>]... [--no-load] [--parallel <1-8>] [--list-targets] [--closes <n[,n...]>] [--body <file>]  (--help for details)"
    exit 2
}
[[ "${1:-}" == (-h|--help) ]] && { help; exit 0; }
(( $# >= 1 )) || usage
wt="$1"; shift
base=main session="" coauthor="" no_load="" list_targets="" parallel=3 runs_per_test=10 max_parallel=8
body_file="" closes_given=""
typeset -a allow loads closes
while (( $# )); do
    case "$1" in
        --base|--allow|--session|--coauthor|--load|--parallel|--closes|--body) (( $# >= 2 )) || usage ;;
    esac
    case "$1" in
        --base) base="$2"; shift 2 ;;
        --allow) allow+=("$2"); shift 2 ;;
        --session) session="$2"; shift 2 ;;
        --coauthor) coauthor="$2"; shift 2 ;;
        --load) loads+=("$2"); shift 2 ;;
        --closes)
            [[ "$2" =~ '^[0-9]+(,[0-9]+)*$' ]] || { print -ru2 -- "--closes takes issue numbers separated by commas, got '$2'"; usage; }
            closes=( ${(s:,:)2} ); closes_given=1; shift 2 ;;
        --body)
            [[ -f "$2" ]] || { print -ru2 -- "--body: no such file '$2'"; exit 2; }
            body_file="${2:A}"; shift 2 ;;
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
typeset -A targets touched pkgdir
for t in $loads; do targets[$t]=1; done
for f in $changed; do
    pkg="$(package_of "$f")"
    [[ -n "$pkg" ]] || continue
    dir="${pkg%% *}" name="${pkg#* }"
    touched[$name]=1 pkgdir[$name]="$dir"
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
# A nextest override filter is an expression such as `(package(a) & binary(b) & test(x)) | (package(c) & test(y))`.
# Each top-level `|` alternative names its own package and suite, so a target pairs a package with the binary of the
# same alternative, never the last binary of the line with another alternative's package (#192). An alternative that
# names several binaries, `package(x) & (binary(a) | binary(b))`, is one alternative: each binary is a target (#273).
# An alternative with no binary(...) selects its package's lib, or its bins when the package has no src/lib.rs.
# Prints "<package> <binary or empty>" per binary, or one line with an empty binary, per alternative of a filter line.
filter_targets() {
    perl -e '
        my $e = $ARGV[0]; my ($d, $cur, @alt) = (0, "");
        for my $c (split //, $e) {
            $d++ if $c eq "("; $d-- if $c eq ")";
            if ($c eq "|" && $d == 0) { push @alt, $cur; $cur = ""; next }
            $cur .= $c;
        }
        push @alt, $cur;
        for my $a (@alt) {
            $a =~ s/^\s+|\s+$//g;
            my ($p) = $a =~ /package\(([^)]*)\)/ or next;
            my @b = $a =~ /binary\(([^)]*)\)/g;
            if (@b) { print "$p $_\n" for @b } else { print "$p \n" }
        }
    ' -- "$1"
}
if [[ -f .config/nextest.toml ]]; then
    while IFS= read -r line; do
        expr="${line#filter*=}"
        expr="${${expr## #}%% #}"
        expr="${expr#\'}" expr="${expr%\'}"
        while IFS=' ' read -r p b; do
            [[ -n "$p" && -n "${touched[$p]:-}" ]] || continue
            if [[ -n "$b" ]]; then targets[${p}:${b}]=1
            elif [[ -f "${pkgdir[$p]}/src/lib.rs" ]]; then targets[${p}:lib]=1
            else targets[${p}:bins]=1; fi
        done < <(filter_targets "$expr")
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

# commits: the integrator squashes the branch into one commit with the message from the brief, so only the tip (HEAD)
# is the commit that lands: its subject and trailers FAIL. The commits below it are squashed away; their problems are
# WARNs (a single-commit branch, such as the integrator's, has only the tip).
conventional='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([a-z0-9._/-]+\))?!?: .+'
head_sha="$(git rev-list --no-merges -1 "$base..HEAD")"  # the newest non-merge commit
bad=() squashed=()
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
    (( ${#problems} )) || continue
    if [[ "$sha" == "$head_sha" ]]; then bad+=("${sha:0:7} $subject (${(j:, :)problems})")
    else squashed+=("${sha:0:7} $subject (${(j:, :)problems})"); fi
done
if (( ${#bad} )); then fail "commits:"; print -l -- "  "${^bad}; else pass "commits"; fi
if (( ${#squashed} )); then warn "commits below the tip (the integrator squashes them; only the tip lands):"; print -l -- "  "${^squashed}; fi

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

# bodies: closing references in the commit messages (and the PR body) against what the brief lets the task close.
# Prints the issue number of every closing reference read from stdin, one per line.
closing_refs() {
    perl -ne '
        while (/(?<![A-Za-z0-9_])(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)(?![A-Za-z0-9_]):?[ \t]+(?:[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+#|#|https?:\/\/github\.com\/[^\/\s]+\/[^\/\s]+\/issues\/)(\d+)/gi) { print "$1\n" }'
}
commit_refs=( ${(u)${(f)"$(git log --format=%B "$base..HEAD" | closing_refs)"}} )
commit_refs=(${commit_refs:#})
body_refs=()
if [[ -n "$body_file" ]]; then
    body_refs=( ${(u)${(f)"$(closing_refs < "$body_file")"}} )
    body_refs=(${body_refs:#})
fi
problems=()
if [[ -n "$closes_given" ]]; then
    for n in $commit_refs; do (( ${closes[(Ie)$n]} )) || problems+=("a commit message closes #$n, which is not in --closes ${(j:,:)closes}"); done
    for n in $body_refs; do (( ${closes[(Ie)$n]} )) || problems+=("the PR body closes #$n, which is not in --closes ${(j:,:)closes}"); done
    if [[ -n "$body_file" ]]; then
        for n in $closes; do (( ${body_refs[(Ie)$n]} )) || problems+=("the PR body has no 'Closes #$n' line"); done
    fi
    if (( ${#problems} )); then fail "bodies:"; print -l -- "  "${^problems}; else pass "bodies"; fi
elif (( ${#commit_refs} + ${#body_refs} )); then
    warn "bodies: closing references to ${(j:, :)${(u)commit_refs}} ${(j:, :)${(u)body_refs}} found; pass --closes with the brief's PR lines to check them"
else
    pass "bodies"
fi

# unsafe: S7 made mechanical. Only a line the branch adds counts; comments and `unsafe_code` lint names do not.
exceptions=()
[[ -f scripts/unsafe-exceptions.txt ]] && exceptions=( ${(f)"$(grep -vE '^[[:space:]]*(#|$)' scripts/unsafe-exceptions.txt)"} )
exceptions=(${exceptions:#})
unsafe_hits=()
while IFS=$'\t' read -r file text; do
    [[ -n "$file" ]] || continue
    (( ${exceptions[(Ie)$file]} )) && continue
    pkg="$(package_of "$file")"
    [[ "${pkg#* }" == (tessera-pty|tessera-ffi) ]] && continue
    unsafe_hits+=("$file: ${text## #}")
done < <(git -c core.quotePath=false diff -U0 --no-color "$base...HEAD" -- '*.rs' | awk '
    /^\+\+\+ / { f = ($0 == "+++ /dev/null") ? "" : substr($0, 7); next }
    /^\+/ && f != "" {
        s = substr($0, 2)
        gsub(/\/\*[^*]*\*\//, "", s); sub(/\/\*.*/, "", s); sub(/\/\/.*/, "", s)
        if (s ~ /^[ \t]*\*/) next
        if (s ~ /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/ || s ~ /#\[global_allocator\]/) print f "\t" s
    }')
if (( ${#unsafe_hits} )); then
    fail "unsafe: outside tessera-pty, tessera-ffi and scripts/unsafe-exceptions.txt (S7):"; print -l -- "  "${^unsafe_hits}
else
    pass "unsafe"
fi

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
