#!/usr/bin/env bash
# Decide whether a change can affect the macOS app job of .github/workflows/ci.yml (#417): print `true` (run the job)
# or `false` (skip it). Fail-safe: anything unknown, any error and any path not on the skip list prints `true`.
#
# Usage: scripts/ci-app-affected.sh --base <ref> [--head <ref>]   changed files = `git diff --no-renames <base>...<head>`
#        scripts/ci-app-affected.sh --stdin                       changed paths, one per line, on stdin
#        (--stdin is also what the test script uses; an empty list prints false: nothing changed, nothing to break)
#
# The app links the xcframework built from tessera-ffi, so a Rust change anywhere in the crates that tessera-ffi
# depends on can break the hosted Swift tests (an FFI JSON shape did, wave 21 #452). The job is skipped ONLY when
# every changed path is skip-eligible:
#   docs/**, *.md, LICENSE* (not under a src/ directory), xtask/**, fuzz/**, .github/ISSUE_TEMPLATE/**
#   <crate>/tests/**, <crate>/benches/** for every workspace crate
#   scripts/** except scripts/app-test-watchdog.sh (just app-test runs it) and scripts/rustc-wrapper.sh (every cargo
#       build runs it through .cargo/config.toml)
#   every file of a workspace crate that is NOT in the dependency closure of tessera-ffi (computed from Cargo.lock;
#       the crate directories come from `cargo metadata --no-deps`)
# Always app-affecting (checked first): apps/**, justfile, Cargo.toml, Cargo.lock, .cargo/**, rust-toolchain*,
# .github/workflows/**. Everything not listed above (shell-integration/, src/ of a closure crate, deny.toml, ...) too.
# A rename is two paths (--no-renames), so moving a file out of src/ is still seen.
#
#   TESSERA_CI_CRATES  test hook: lines `<dir> in|out` replacing the cargo metadata / Cargo.lock computation
#   --list-crates      print that computed `<dir> in|out` list (for the PR description) and exit
set -euo pipefail

mode="" base="" head="HEAD"
while (( $# )); do
    case "$1" in
        --base) [[ $# -ge 2 ]] || { echo "--base needs a ref" >&2; exit 2; }; mode=base; base="$2"; shift 2 ;;
        --head) [[ $# -ge 2 ]] || { echo "--head needs a ref" >&2; exit 2; }; head="$2"; shift 2 ;;
        --stdin) mode=stdin; shift ;;
        --list-crates) mode=list; shift ;;
        -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "usage: scripts/ci-app-affected.sh --base <ref> [--head <ref>] | --stdin | --list-crates" >&2; exit 2 ;;
    esac
done
[[ -n "$mode" ]] || { echo "usage: scripts/ci-app-affected.sh --base <ref> [--head <ref>] | --stdin | --list-crates" >&2; exit 2; }

failsafe() { echo "ci-app-affected: $1; running the app job" >&2; echo true; exit 0; }

# `<dir> in|out` per workspace crate, `in` = tessera-ffi or one of its (transitive, any kind) dependencies.
crates() {
    if [[ -n "${TESSERA_CI_CRATES:-}" ]]; then printf '%s\n' "$TESSERA_CI_CRATES"; return; fi
    local root meta closure
    root="$(git rev-parse --show-toplevel)" || return 1
    meta="$(cd "$root" && cargo metadata --format-version 1 --no-deps --offline)" || return 1
    # Reachable package names from tessera-ffi over Cargo.lock (names only: a duplicate name over-approximates).
    closure="$(awk '
        /^\[\[package\]\]/ { name = ""; indeps = 0; next }
        /^name = / { gsub(/^name = "|"$/, ""); name = $0; next }
        /^dependencies = \[/ { indeps = 1; next }
        indeps && /^\]/ { indeps = 0; next }
        indeps { d = $0; gsub(/^ *"|",? *$/, "", d); split(d, p, " "); edges[name] = edges[name] " " p[1]; next }
        END {
            n = 1; queue[1] = "tessera-ffi"; seen["tessera-ffi"] = 1
            for (i = 1; i <= n; i++) {
                m = split(edges[queue[i]], ds, " ")
                for (j = 1; j <= m; j++) if (!(ds[j] in seen)) { seen[ds[j]] = 1; queue[++n] = ds[j] }
            }
            for (k in seen) print k
        }' "$root/Cargo.lock")" || return 1
    grep -qx tessera-ffi <<<"$closure" || return 1
    jq -r --arg root "$root/" --arg closure "$closure" '
        ($closure | split("\n")) as $c
        | .packages[]
        | (.manifest_path | sub($root; "") | sub("/?Cargo.toml$"; "")) + " " + (if (.name as $n | $c | index($n)) then "in" else "out" end)
    ' <<<"$meta"
}

if [[ "$mode" == list ]]; then crates; exit; fi

paths=()
if [[ "$mode" == base ]]; then
    out="$(git -c core.quotePath=false diff --no-renames --name-only "$base...$head")" || failsafe "git diff $base...$head failed"
    while IFS= read -r line; do paths+=("$line"); done <<<"$out"
else
    while IFS= read -r line || [[ -n "$line" ]]; do paths+=("$line"); done
fi

# Drop empty lines; nothing left means an empty diff.
clean=()
for p in ${paths[@]+"${paths[@]}"}; do [[ -n "$p" ]] && clean+=("$p"); done
if (( ${#clean[@]} == 0 )); then echo false; exit 0; fi

# The crate list (cargo metadata and jq) is computed only when a path needs it: a change of docs and scripts alone never
# runs cargo, so a runner that cannot satisfy the toolchain cannot turn a docs-only PR into a full macOS run (#489,
# 21.0d m1). Any failure to compute it still prints `true`.
cr="" cr_loaded=""
load_crates() {
    [[ -z "$cr_loaded" ]] || return 0
    cr="$(crates)" || failsafe "could not compute the crate list"
    [[ -n "$cr" ]] || failsafe "empty crate list"
    cr_loaded=1
}

# Skip-eligible? 0 = yes (cannot affect the app), 1 = no.
skippable() {
    local p="$1"
    case "$p" in
        /*|../*|*/../*|..) return 1 ;;
        apps/*|justfile|Cargo.toml|Cargo.lock|.cargo/*|rust-toolchain*|.github/workflows/*) return 1 ;;
        scripts/app-test-watchdog.sh|scripts/rustc-wrapper.sh) return 1 ;;
        docs/*|xtask/*|fuzz/*|.github/ISSUE_TEMPLATE/*|scripts/*) return 0 ;;
        # A markdown or LICENSE file inside a crate's src/ can be `include_str!`ed or `include_bytes!`ed: it goes through
        # the crate rules below (the job runs for a closure crate), never through the docs rule (21.0d m2).
        */src/*) ;;
        *.md|LICENSE*|*/LICENSE*) return 0 ;;
    esac
    load_crates
    local line dir state best="" best_state=""
    while IFS= read -r line; do
        dir="${line% *}"; state="${line##* }"
        [[ -n "$dir" && "$dir" != "." ]] || continue
        if [[ "$p" == "$dir"/* && ${#dir} -gt ${#best} ]]; then best="$dir"; best_state="$state"; fi
    done <<<"$cr"
    [[ -n "$best" ]] || return 1
    [[ "$best_state" == out ]] && return 0
    case "${p#"$best"/}" in tests/*|benches/*) return 0 ;; esac
    return 1
}

for p in "${clean[@]}"; do
    if ! skippable "$p"; then echo true; exit 0; fi
done
echo false
