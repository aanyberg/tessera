#!/usr/bin/env zsh
# Builds tessera-ffi, compiles each C test in crates/tessera-ffi/tests/c
# (lifecycle.c, input.c, scroll.c, then surface.c) against the generated TesseraCore.h
# and the static library, and runs it. surface.c draws into a bare
# CAMetalLayer and skips itself when TESSERA_SKIP_GPU_TESTS is set.
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
# --print native-static-libs` (libSystem, libc and libm come for free). On
# macOS the renderer (wgpu's Metal backend, through objc2) needs the
# Objective-C runtime and five system frameworks; every test links them
# because every test pulls in the whole library.
case "$(uname -s)" in
    Darwin) native_libs=(-liconv -lobjc
        -framework CoreFoundation -framework Foundation -framework CoreGraphics
        -framework QuartzCore -framework Metal) ;;
    *) native_libs=(-lutil -ldl -lm) ;;
esac

# tessera_app_new reads and watches its configuration directory (#100): the
# tests use a scratch one, never the user's.
config_dir="$(mktemp -d)"
trap 'rm -rf "$config_dir"' EXIT
export TESSERA_CONFIG_DIR="$config_dir"

for test in lifecycle input scroll surface; do
    "${CC:-cc}" -std=c11 -Wall -Wextra -Werror -pedantic \
        -I "$include_dir" \
        "crates/tessera-ffi/tests/c/$test.c" \
        "$target_dir/debug/libtessera_ffi.a" \
        $native_libs -lpthread \
        -o "$out_dir/$test"
    "$out_dir/$test"
done
