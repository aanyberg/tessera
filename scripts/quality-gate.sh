#!/usr/bin/env zsh
# The mechanical quality gate every task branch must pass before review, verification and merge.
# Usage: scripts/quality-gate.sh [--force] [worktree-dir]   (defaults to the repo root)
#
# A pass on a clean tree is recorded per commit and toolchain in the repository's shared git dir, so the
# implementer, critic and verifiers running the gate on the same commit pay for it once. A dirty tree is never
# recorded or skipped. The key also covers the toolchain and this script, so a changed gate runs again. A reused pass
# does not refresh cargo-deny's advisory database or re-sample flaky tests; CI does both on every PR.
# --force (or QUALITY_GATE_FORCE=1) runs the gate even when the commit already passed.
set -euo pipefail
force="${QUALITY_GATE_FORCE:-}"
dir=""
for arg in "$@"; do
    case "$arg" in
        --force) force=1 ;;
        -*) print -u2 "usage: scripts/quality-gate.sh [--force] [worktree-dir]"; exit 2 ;;
        *) [[ -z "$dir" ]] || { print -u2 "usage: scripts/quality-gate.sh [--force] [worktree-dir]"; exit 2; }; dir="$arg" ;;
    esac
done
gate_hash="$(shasum < "$0" | cut -c1-12)"
dir="${dir:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$dir"
step() { print -P "%F{cyan}== $1%f"; }

stamp=""
if [[ -z "$(git status --porcelain)" ]]; then
    head="$(git rev-parse HEAD)"
    stamp="$(git rev-parse --path-format=absolute --git-common-dir)/quality-gate-passed/$head-$(rustc -V | shasum | cut -c1-12)-$gate_hash"
    if [[ -z "$force" && -e "$stamp" ]]; then
        print -P "%F{green}quality gate already passed for ${head:0:7} with this toolchain (--force re-runs it)%f"
        exit 0
    fi
fi

step "fmt";     cargo fmt --all --check
step "clippy";  cargo clippy --workspace --all-targets -- -D warnings
step "tests";   cargo nextest run --workspace
step "rustdoc"; RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
step "deny";    cargo deny check
step "ffi release build"; cargo build --release -p tessera-ffi
# fuzz/ is not a workspace member (#14). Its lock file is the workspace's, copied in before every build so the
# fuzzers test the workspace's dependency versions (fuzz/Cargo.lock is ignored; cargo only adds libfuzzer-sys and its dependencies).
# Format and stable clippy always run, so a rename breaks here before merge; the libFuzzer build needs a nightly
# toolchain and cargo-fuzz, optional for contributors (the nightly fuzz workflow always builds it).
cp Cargo.lock fuzz/Cargo.lock
step "fuzz fmt";    cargo fmt --manifest-path fuzz/Cargo.toml --check
step "fuzz clippy"; cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
if [[ "$(rustup toolchain list)" == (|*$'\n')nightly* ]] && cargo +nightly fuzz --version > /dev/null 2>&1; then
    step "fuzz build"; cargo +nightly fuzz build
else
    print -P "%F{yellow}== fuzz build skipped: needs a nightly toolchain and cargo-fuzz (see docs/software-factory.md)%f"
fi

# Record the pass only when the tree and HEAD are still what the gate started on.
if [[ -n "$stamp" && -z "$(git status --porcelain)" && "$(git rev-parse HEAD)" == "$head" ]]; then
    mkdir -p "${stamp:h}" && : > "$stamp"
fi
print -P "%F{green}quality gate passed in $dir%f"
