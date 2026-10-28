#!/bin/sh
# Cargo's rustc wrapper (set in .cargo/config.toml). Compiles through sccache when it is installed, so every
# worktree shares one compiler cache instead of building its dependencies from scratch; otherwise it runs the
# compiler directly. Only plain rustc invocations are cached: Cargo passes a workspace wrapper such as
# clippy-driver as the first argument, and those run uncached. TESSERA_NO_SCCACHE=1 turns the cache off.
case "$1" in
    */rustc | rustc) ;;
    *) exec "$@" ;;
esac
if [ -z "${TESSERA_NO_SCCACHE:-}" ] && command -v sccache > /dev/null 2>&1; then
    exec sccache "$@"
fi
exec "$@"
