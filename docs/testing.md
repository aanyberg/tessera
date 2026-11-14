# Testing

Which tests to run while you work, how tests stay away from your home directory, and how to add an
escape-sequence test. The contributor workflow is in [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Which tests to run

Pick the cheapest check that covers the change while you work; the full gate runs once before a push.

| Stage | Command | Covers |
|---|---|---|
| Iterating | `cargo nextest run -p <crate>` | one crate |
| After a change | `just test-affected` (`--clippy` to lint too; `--base <ref>`; `-- <nextest args>`) | every crate your change touches since the merge base with `main` (committed, staged, unstaged and untracked), plus every crate that depends on them |
| Swift or `tessera-ffi` surface | `just app-test` | the XCTest suites (Xcode, Thread Sanitizer); the gate does not run them, CI does |
| Docs, scripts, CI | the matching check: `actionlint`, `just config-docs-check`; for the factory scripts `just factory-test`, `just verify-branch-test`, `just quality-gate-test` | no Rust test is affected |
| `shell-integration/` | `just test-affected` (selects those two crates and their dependents: `tessera-shell-integration` and `tessera-term` read it), plus `just terminfo-check` for the terminfo entry | the shell scripts and the terminfo entry |
| Before any push or PR | `scripts/quality-gate.sh <worktree>` | everything: fmt, clippy, workspace tests, rustdoc, deny, FFI build and C tests, release timing tests, fuzz fmt and clippy |

`just test-affected --list` prints the affected crates without running anything. When local `main` or `origin/main` is
stale, commits from other branches show up as changed files: `git fetch` and use `--base origin/main`. A changed file
under `crates/tessera-config/` also prints a note to run `just config-docs-check`, because the generated
`docs/config.md` can change with it and no Rust test checks that. A change to a root manifest,
`Cargo.lock`, the toolchain pin or the nextest, clippy, rustfmt or deny config affects every crate. Dependencies point
downward only, so a change in `tessera-vt` or `tessera-grid` reaches most of the workspace and a change in
`tessera-cli` only itself. The targeted run is a feedback loop, not a gate: it can miss what only the whole workspace
shows (build scripts, the FFI header, the Swift consumers).

**A worktree's built app is from its last run.** `just app` and `just app-test` build into the worktree's own
`apps/macos/DerivedData`, and `just run` opens whatever is there: the app from the last build in that worktree, not
from the branch you have checked out now. Before an eye check, rebuild (`just app`) or look at the branch the
DerivedData app was built from (`git -C <worktree> branch --show-current` and the build time of
`apps/macos/DerivedData/Build/Products/Debug/Tessera.app`); a check against a stale app proves nothing about the branch.

## Tests and the home directory

Tests never read or write the real home directory. Tests that create an
`App` pass it a temporary configuration root; the FFI tests, whose
`tessera_app_new` uses `~/Library/Application Support/Tessera`, point
`HOME` at a temporary home first (`crates/tessera-ffi/tests/hermetic`), and
`tests/home.rs` fails if a test file forgets or if the real
`Application Support/Tessera` changes. Changing the environment is only
sound with one test per process, so run them with `cargo nextest` (as `just
test` and the gate do), not plain `cargo test`.

The app's tests do not read the machine's Accessibility settings either:
the test bundle's principal class (`HostDisplaySettings`) pins Reduce
Motion, Reduce Transparency and Increase Contrast off before the first
test, and a test that turns one on puts the pin back in its tearDown. A
view's VoiceOver monitor is the machine's until a test replaces it, so a
test that needs a client attached sets a detached fake first.

## Adding an escape-sequence test

Emulator tests in `crates/tessera-term/tests/` feed bytes into a fresh
`Terminal` and compare what it drew. The harness is `tests/common/mod.rs`
(scripts, rendering) and `tests/spec/mod.rs` (fixture reader).

**Scripts.** A script is one case per file under `tests/scripts/<group>/`:

```text
# Comment lines start with `#`; blank lines are skipped.
@size 25x80
@set auto_wrap false
\x1b[3;6Hab\x1b[2D\x1b[K
```

- `@size <rows>x<cols>` comes once, before anything else.
- `@set auto_wrap|insert_mode true|false` switches a mode through the
  `Terminal` API, for modes whose escape sequence is not wired up yet.
- Every other line is one payload, fed with one `advance` call. Its escapes
  are decoded by `decode`: `\xNN` (one raw byte), `\r`, `\n`, `\t`, `\\`
  and `\u{H..}` (one scalar value, UTF-8 encoded); everything else is
  literal UTF-8, and any other escape fails the test. The line break ending
  a payload is not part of it, so write `\r\n` where the stream has one.

A group's test file loads its scripts, runs each whole and byte by byte
(the two must agree), and snapshots the render (the `common` module docs
describe the screen format: `|` after the cursor's cell, `\` after a
soft-wrapped row).

**Fixtures.** Behaviour that a specification pins goes into a fixture in
`tests/fixtures/` (`csi_spec.txt`, `esctest_spec.txt` and `decstr_spec.txt`
share one layout), which `spec::parse_fixture` reads:

```text
### E_CUP_rowOnly
size: 25x80
step: feed \x1b[3;6H
step: feed \x1b[2H
source: esctest2 esctest/tests/cup.py test_CUP_RowOnly
screen:
>
> |
>                    (one `>` row per terminal row, `|` after the cursor)
cursor: (1,0)        # 0-based (row,col)
pending_wrap: false
requests: []         # or [Bell, ...]
scrollback: len=0    # or `not asserted`, or `len=N first=("text", wrapped=false)`
```

`step:` takes `feed <payload>` (decoded as above) or `set <mode> true|false`.
`cite:`, `note:`, `flags:` and `needs:` are documentation; a test file may
read extra keys itself (`attr:` cell styles in `esctest.rs`, `pen:` in
`decstr.rs`). `spec::check_all_in(fixture, select)` checks every selected
case whole and byte by byte and reports all mismatches at once.

Rules for a new case:

- It cites its source: `source:` names the test it ports (esctest file and
  function, vttest screen and capture), or `cite:` the spec section (xterm
  ctlseqs, DEC STD 070). The expected screen comes from that source or from
  an independent model of xterm, never from running tessera.
- A case tessera cannot pass yet is not a skipped test: write it as a `#>`
  comment block that names the issue that will make it pass
  (`#> E_HPR_defaultParams -> #93 ...`). The PR that fixes the issue turns
  the block into a case.
- A bug a case reveals is not fixed in the test PR: record it as a `#>`
  block with a new issue number.

Fixtures are generated by the `spec-researcher` agent with throwaway tools
(an xterm model in Python, a pty capture of vttest); the tools are not
committed. To change a case, edit the fixture by hand and say where the new
expectation comes from, or regenerate the section with the researcher and
commit only the generated fixture.

**Snapshots.** Run the tests; a new or changed render is written as a
`.snap.new` file next to the snapshots in `tests/snapshots/`. Review and
accept it with `cargo insta review` (from `cargo install cargo-insta`), or
without that tool with
`INSTA_UPDATE=always cargo nextest run -p tessera-term --test <file>`
(scope it to the test file: `always` also rewrites snapshots of tests that
fail for another reason), then read the new `.snap` files and check
`git status` before committing.
