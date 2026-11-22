#!/usr/bin/env zsh
# Builds tessera-ffi, compiles each C test in crates/tessera-ffi/tests/c
# (lifecycle.c, input.c, keymap.c, scroll.c, clipboard.c, remote.c, selection.c, selection_rich.c, search.c, prompts.c, foreground.c, config.c, appearance.c, plugins.c, accessibility.c, notify.c, then surface.c) against the generated TesseraCore.h
# and the static library, and runs it. surface.c draws into a bare
# CAMetalLayer and skips itself when TESSERA_SKIP_GPU_TESTS is set.
# Usage: scripts/ffi-c-test.sh [worktree-dir]   (defaults to the repo root)
set -euo pipefail
dir="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$dir"

# Scratch directories of this run, removed on every way out. The EXIT trap alone is not enough: zsh skips it when the
# script is killed with TERM (the gate's watchdog does that), so TERM, INT and HUP run the cleanup and exit with the
# conventional 128 + signal status. The traps are set before the first directory is made, so a failure between the
# mktemp and a later line leaves nothing behind either (#489, 21.1 n2).
out_dir="" config_dir=""
cleanup() {
    [[ -z "$out_dir" ]] || rm -rf "$out_dir"
    [[ -z "$config_dir" ]] || rm -rf "$config_dir"
}
trap cleanup EXIT
trap 'cleanup; trap - EXIT; exit 143' TERM
trap 'cleanup; trap - EXIT; exit 130' INT
trap 'cleanup; trap - EXIT; exit 129' HUP

cargo build -p tessera-ffi
# build.rs always writes the header under the workspace's own target/; the
# library follows CARGO_TARGET_DIR when it is set.
target_dir="${CARGO_TARGET_DIR:-$dir/target}"
include_dir="$dir/target/include"
# One directory per run, so two runs on one target never link over each other.
mkdir -p "$target_dir"
out_dir="$(mktemp -d "$target_dir/ffi-c-test.XXXXXX")"

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
export TESSERA_CONFIG_DIR="$config_dir"

for test in lifecycle input keymap scroll clipboard remote selection selection_rich search prompts foreground config appearance plugins accessibility notify surface; do
    "${CC:-cc}" -std=c11 -Wall -Wextra -Werror -pedantic \
        -I "$include_dir" \
        "crates/tessera-ffi/tests/c/$test.c" \
        "$target_dir/debug/libtessera_ffi.a" \
        $native_libs -lpthread \
        -o "$out_dir/$test"
    "$out_dir/$test"
done
