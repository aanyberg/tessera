#!/usr/bin/env zsh
# The mechanical quality gate every task branch must pass before review, verification and merge.
# Usage: scripts/quality-gate.sh [worktree-dir]   (defaults to the repo root)
set -euo pipefail
dir="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$dir"
step() { print -P "%F{cyan}== $1%f"; }
step "fmt";     cargo fmt --all --check
step "clippy";  cargo clippy --workspace --all-targets -- -D warnings
step "tests";   cargo nextest run --workspace
step "rustdoc"; RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
step "deny";    cargo deny check
step "ffi release build"; cargo build --release -p tessera-ffi
print -P "%F{green}quality gate passed in $dir%f"
