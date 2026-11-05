#!/usr/bin/env zsh
# Run the tests of the crates a change touches and of every crate that depends on them: the inner-loop check, much
# cheaper than the workspace run. It does not replace scripts/quality-gate.sh, which every branch still passes before
# a push or a PR.
# Usage: scripts/affected-tests.sh [--base <ref>] [--clippy] [--list] [-- <extra cargo nextest args>]
#
#   --base <ref>  compare against the merge base with <ref> (default: main)
#   --clippy      also run `cargo clippy -p <crate>... --all-targets -- -D warnings` before the tests
#   --list        print the affected crates, one per line, and what else the change needs; run nothing
#
# Changed files = committed changes since the merge base, plus staged, unstaged and untracked files. A file maps to
# the workspace package whose directory contains it. Affected = those packages plus every package that depends on
# one of them, directly or transitively (normal, build and dev dependencies; dependencies only point downward, so
# the set is the part of the workspace above the change). The whole workspace is affected when a root manifest,
# Cargo.lock, the toolchain pin, the nextest config or the lint/deny config changed.
#
# Not covered, and said so in the output:
#   apps/macos          the Swift app: `just app-test` (needs Xcode)
#   fuzz/               not a workspace member: `scripts/quality-gate.sh` runs its fmt and clippy
#   everything else     docs, scripts, CI: no Rust test is affected; run the check that matches the file
#                       (actionlint, `just terminfo-check`, `just config-docs-check`)
# shell-integration/ is read by Rust tests (the scripts by tessera-shell-integration, the terminfo entry by
# tessera-term), so a change there selects both crates and their dependents.
# Exit status: the clippy or nextest status, 2 on a usage error or when git/jq/cargo fail.
set -euo pipefail

base=main
clippy=""
list=""
extra=()
while (( $# )); do
    case "$1" in
        --base) (( $# >= 2 )) || { print -u2 "--base needs a ref"; exit 2; }; base="$2"; shift 2 ;;
        --clippy) clippy=1; shift ;;
        --list) list=1; shift ;;
        --) shift; extra=("$@"); break ;;
        -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) print -u2 "usage: scripts/affected-tests.sh [--base <ref>] [--clippy] [--list] [-- <nextest args>]"; exit 2 ;;
    esac
done

root="$(git rev-parse --show-toplevel)"
cd "$root"
root="$(pwd -P)"
mb="$(git merge-base "$base" HEAD)" || { print -u2 "no merge base with '$base'"; exit 2; }

files=("${(@f)$( { git -c core.quotePath=false diff --no-renames --name-only "$mb"; git -c core.quotePath=false ls-files --others --exclude-standard; } | sort -u )}")
files=("${(@)files:#}")
if (( ${#files} == 0 )); then
    print -u2 "no changes since $base (${mb:0:7})"
    exit 0
fi

# name|directory (relative to the root)|comma-separated workspace dependencies
typeset -A pkg_dir rdeps
meta="$(cargo metadata --format-version 1 --no-deps)"
while IFS='|' read -r name dir deps; do
    pkg_dir[$name]="$dir"
    for d in ${(s:,:)deps}; do rdeps[$d]="${rdeps[$d]:-} $name"; done
done < <(print -r -- "$meta" | jq -r --arg root "$root/" '
    ([.packages[].name]) as $names
    | .packages[]
    | [.name,
       (.manifest_path | sub("/Cargo.toml$"; "") | ltrimstr($root)),
       ([.dependencies[].name | select(. as $n | $names | index($n))] | unique | join(","))]
    | join("|")')

# The package whose directory is the longest prefix of the file, so a nested package wins over the one above it.
owner_of() {
    local f="$1" best="" best_len=0 n d
    for n in ${(k)pkg_dir}; do
        d="${pkg_dir[$n]}"
        if [[ "$f" == "$d"/* ]] && (( ${#d} > best_len )); then best="$n"; best_len=${#d}; fi
    done
    print -r -- "$best"
}

typeset -A hit
whole=""
app="" fuzz="" other=()
for f in $files; do
    case "$f" in
        Cargo.toml|Cargo.lock|rust-toolchain|rust-toolchain.toml|clippy.toml|.clippy.toml|rustfmt.toml|.rustfmt.toml|deny.toml|.config/nextest.toml|.cargo/*)
            whole=1 ;;
        # Rust tests read these files: the shell scripts (tessera-shell-integration) and the terminfo entry (tessera-term).
        shell-integration/*)
            for n in tessera-shell-integration tessera-term; do [[ -n "${pkg_dir[$n]:-}" ]] && hit[$n]=1; done ;;
        apps/macos/*) app=1 ;;
        fuzz/*) fuzz=1 ;;
        *)
            o="$(owner_of "$f")"
            if [[ -n "$o" ]]; then hit[$o]=1; else other+=("$f"); fi ;;
    esac
done

typeset -A affected
if [[ -n "$whole" ]]; then
    for n in ${(k)pkg_dir}; do affected[$n]=1; done
else
    queue=(${(k)hit})
    while (( ${#queue} )); do
        n="${queue[1]}"; queue=("${(@)queue[2,-1]}")
        [[ -n "${affected[$n]:-}" ]] && continue
        affected[$n]=1
        queue+=(${=rdeps[$n]:-})
    done
fi
crates=(${(ko)affected})

notes=()
[[ -n "$whole" ]] && notes+=("a workspace-wide file changed: every crate is affected")
[[ -n "$app" ]] && notes+=("apps/macos changed: also run 'just app-test' (needs Xcode)")
[[ -n "$fuzz" ]] && notes+=("fuzz/ changed: scripts/quality-gate.sh runs its fmt and clippy")
if (( ${#other} )); then
    more=""
    (( ${#other} > 5 )) && more=" (+$(( ${#other} - 5 )) more)"
    notes+=("no crate owns: ${(j:, :)other[1,5]}${more}; run the check that matches the file")
fi
notes+=("before a push or PR: scripts/quality-gate.sh")

if [[ -n "$list" ]]; then
    (( ${#crates} )) && print -rl -- $crates
    for n in $notes; do print -u2 -r -- "note: $n"; done
    exit 0
fi

if (( ${#crates} == 0 )); then
    print -P "%F{yellow}no Rust crate is affected%f"
    for n in $notes; do print -r -- "note: $n"; done
    exit 0
fi

pargs=()
for c in $crates; do pargs+=(-p "$c"); done
print -P "%F{cyan}== affected crates (${#crates}): ${(j: :)crates}%f"
if [[ -n "$clippy" ]]; then
    print -P "%F{cyan}== clippy%f"
    cargo clippy "${pargs[@]}" --all-targets -- -D warnings
fi
print -P "%F{cyan}== tests%f"
rc=0
cargo nextest run "${pargs[@]}" --no-tests=pass "${extra[@]}" || rc=$?
for n in $notes; do print -r -- "note: $n"; done
exit $rc
