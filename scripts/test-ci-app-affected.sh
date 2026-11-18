#!/usr/bin/env bash
# Exercise scripts/ci-app-affected.sh (#417): every skip rule, every always-run rule, and the edge cases (rename, delete,
# empty diff, spaces in a path, docs-and-src mix, git failure), plus the real crate list against this checkout.
#
#   scripts/test-ci-app-affected.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/ci-app-affected.sh"
failures=0
export TESSERA_CI_CRATES='crates/core in
crates/leaf out
crates/plugins/deep in
crates/plugins/deepleaf out
xtask out'

check() { # <expected> <description> <paths...>
    local want="$1" desc="$2" got; shift 2
    got="$(printf '%s\n' "$@" | "$script" --stdin 2>/dev/null)"
    if [[ "$got" == "$want" ]]; then echo "ok   $desc"; else echo "FAIL $desc: want $want, got $got"; failures=$((failures + 1)); fi
}

# skip-eligible
check false "docs"                         docs/releasing.md docs/adr/0001-x.md docs/config.md
check false "markdown anywhere"            README.md CONTRIBUTING.md crates/core/README.md
check false "licences"                     LICENSE LICENSE-MIT LICENSE-APACHE
check false "xtask"                        xtask/src/main.rs xtask/Cargo.toml
check false "fuzz"                         fuzz/fuzz_targets/a.rs fuzz/Cargo.toml fuzz/seeds/x
check false "issue templates"              .github/ISSUE_TEMPLATE/bug.yml
check false "tests of a closure crate"     crates/core/tests/it.rs crates/core/tests/c/surface.c
check false "benches of a closure crate"   crates/core/benches/b.rs
check false "tests of a nested crate"      crates/plugins/deep/tests/t.rs
check false "scripts the app job ignores"  scripts/quality-gate.sh scripts/test-ci-app-affected.sh scripts/ffi-c-test.sh
check false "crate outside the closure"    crates/leaf/src/lib.rs crates/leaf/Cargo.toml crates/plugins/deepleaf/src/x.rs
check false "empty diff"
check false "only blank lines"             ""
check false "a path with spaces (docs)"    "docs/a b/c d.md"
check false "a path with spaces (leaf)"    "crates/leaf/src/with space.rs"

# app-affecting
check true  "src of a closure crate"       crates/core/src/lib.rs
check true  "src of a nested closure crate" crates/plugins/deep/src/lib.rs
check true  "closure crate manifest"       crates/core/Cargo.toml
check true  "closure crate build.rs"       crates/core/build.rs
check true  "closure crate cbindgen.toml"  crates/core/cbindgen.toml
check true  "src/tests module in a closure crate" crates/core/src/tests/mod.rs
check true  "apps"                         apps/macos/Sources/X.swift
check true  "apps markdown"                apps/macos/README.md
check true  "justfile"                     justfile
check true  "root manifest"                Cargo.toml
check true  "lockfile"                     Cargo.lock
check true  "ci workflow"                  .github/workflows/ci.yml
check true  "other workflow"               .github/workflows/release.yml
check true  "toolchain file"               rust-toolchain.toml
check true  "cargo config"                 .cargo/config.toml
check true  "watchdog script"              scripts/app-test-watchdog.sh
check true  "rustc wrapper"                scripts/rustc-wrapper.sh
check true  "shell-integration"            shell-integration/zsh/x.zsh
check true  "unknown top-level file"       deny.toml
check true  "dot-dot path"                 docs/../apps/x
check true  "docs and src mix"             docs/a.md crates/core/src/lib.rs
check true  "docs and src mix, src first"  crates/core/src/lib.rs docs/a.md
check true  "leaf and app mix"             crates/leaf/src/lib.rs apps/macos/project.yml
check true  "a path with spaces (src)"     "crates/core/src/with space.rs"
check true  "crate dir prefix is not a match" crates/core2/src/lib.rs
check true  "a LICENSE-named file in a closure crate src" crates/core/src/LICENSE_x.rs
check true  "markdown in a closure crate src (include_str)" crates/core/src/notes.md
check false "markdown in a leaf crate src"  crates/leaf/src/notes.md
check true  "a quoted control-character path (git quotes it)" '"docs/a\tb.md"'

# git modes: rename, delete, empty diff, bad ref
work="$(mktemp -d "${TMPDIR:-/tmp}/ci-app-affected-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
(cd "$work" && git init -q -b main . && mkdir -p crates/core/src crates/leaf/src docs "dir with space"
    : > crates/core/src/a.rs; : > crates/leaf/src/b.rs; : > docs/x.md; : > "dir with space/f.md"
    git add -A && git commit -q -m base)
git_check() { # <expected> <description> <setup commands run in the work tree>
    local want="$1" desc="$2" got
    (cd "$work" && git checkout -q main && git checkout -q -B topic && eval "$3" && git add -A && git commit -q --allow-empty -m change)
    got="$(cd "$work" && "$script" --base main --head topic 2>/dev/null)"
    if [[ "$got" == "$want" ]]; then echo "ok   $desc"; else echo "FAIL $desc: want $want, got $got"; failures=$((failures + 1)); fi
}
git_check false "git: empty diff"                      "true"
git_check false "git: docs edit"                       "echo x >> docs/x.md"
git_check false "git: delete a docs file"              "git rm -q docs/x.md"
git_check true  "git: delete a src file of a closure crate" "git rm -q crates/core/src/a.rs"
git_check false "git: delete a src file of a leaf crate"    "git rm -q crates/leaf/src/b.rs"
git_check true  "git: rename src -> docs (old path is src)" "git mv crates/core/src/a.rs docs/a.md"
git_check true  "git: rename docs -> src (new path is src)" "git mv docs/x.md crates/core/src/x.rs"
git_check false "git: rename within docs"              "git mv docs/x.md docs/y.md"
git_check false "git: path with spaces, docs"          "echo y >> 'dir with space/f.md'"
git_check true  "git: docs and src together"           "echo y >> docs/x.md; echo y >> crates/core/src/a.rs"
got="$(cd "$work" && "$script" --base no-such-ref 2>/dev/null)"
if [[ "$got" == true ]]; then echo "ok   git: unknown base fails safe"; else echo "FAIL git: unknown base: got $got"; failures=$((failures + 1)); fi

# The real crate list: the ffi and its dependencies run the app, the cli does not (needs cargo and jq; skipped without)
unset TESSERA_CI_CRATES
if command -v cargo >/dev/null && command -v jq >/dev/null; then
    root="$(cd "$(dirname "$script")/.." && pwd)"
    real() { (cd "$root" && printf '%s\n' "$@" | "$script" --stdin 2>/dev/null); }
    real_check() { # <expected> <description> <path>
        local got; got="$(real "$3")"
        if [[ "$got" == "$1" ]]; then echo "ok   real: $2"; else echo "FAIL real: $2: want $1, got $got"; failures=$((failures + 1)); fi
    }
    real_check true  "tessera-ffi src"                crates/tessera-ffi/src/lib.rs
    real_check true  "tessera-vt src (a dependency)"  crates/tessera-vt/src/lib.rs
    real_check false "tessera-cli src (not linked)"   crates/tessera-cli/src/main.rs
else
    echo "skip real crate list: cargo or jq missing"
fi

if (( failures )); then echo "$failures failure(s)"; exit 1; fi
echo "all passed"
