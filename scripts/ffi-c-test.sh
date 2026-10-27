#!/usr/bin/env zsh
# Builds tessera-ffi, compiles crates/tessera-ffi/tests/c/lifecycle.c against
# the generated TesseraCore.h and the static library, and runs it.
# Usage: scripts/ffi-c-test.sh [worktree-dir]   (defaults to the repo root)
set -euo pipefail
dir="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$dir"

cargo build -p tessera-ffi
# build.rs always writes the header under the workspace's own target/; the
# library follows CARGO_TARGET_DIR when it is set.
target_dir="${CARGO_TARGET_DIR:-$dir/target}"
include_dir="$dir/target/include"
out_dir="$target_dir/ffi-c-test"
mkdir -p "$out_dir"

# Native libraries the static library needs beyond what cc links by
# default, from `cargo rustc -p tessera-ffi --lib --crate-type staticlib --
# --print native-static-libs` (libSystem, libc and libm come for free).
case "$(uname -s)" in
    Darwin) native_libs=(-liconv) ;;
    *) native_libs=(-lutil -ldl -lm) ;;
esac

"${CC:-cc}" -std=c11 -Wall -Wextra -Werror -pedantic \
    -I "$include_dir" \
    crates/tessera-ffi/tests/c/lifecycle.c \
    "$target_dir/debug/libtessera_ffi.a" \
    $native_libs -lpthread \
    -o "$out_dir/lifecycle"
"$out_dir/lifecycle"
