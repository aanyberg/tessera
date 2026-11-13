# Contributing

Thanks for helping. This page is the short version: how to work on a change and what a PR needs. Longer reference
material lives in `docs/`:

| Need | Read |
|---|---|
| Build from source | [README](README.md#building-from-source) |
| Which tests to run, adding an escape-sequence test | [`docs/testing.md`](docs/testing.md) |
| Seeing what the emulator draws, the development window | [`docs/debugging.md`](docs/debugging.md) |
| How the code is organised and why | [`docs/architecture.md`](docs/architecture.md), [`docs/adr/`](docs/adr/) |
| Trust boundaries, resource caps, security rules | [`docs/security.md`](docs/security.md) |
| Cutting and signing a release (maintainers) | [`docs/releasing.md`](docs/releasing.md) |
| How agent teams ship milestones | [`docs/software-factory.md`](docs/software-factory.md) |

## Workflow

- `main` is protected. Work on a branch, open a PR, CI must be green.
- Commits and PR titles follow [Conventional Commits](https://www.conventionalcommits.org):
  `feat(grid): reflow on resize`, `fix(vt): handle empty OSC`, `docs: …`. Scopes are crate names without the
  `tessera-` prefix, or `app` for Swift.
- PRs are squash-merged with the conventional title as the subject
  (`gh pr merge <pr> --squash --subject "<type>(<scope>): <title> (#<pr>)"`).
- Architecture changes get an ADR in `docs/adr/` in the same PR. A later change to an accepted ADR is an amendment at
  the end of that ADR, headed `## Amendment: <topic> (#<issue>[, #<issue>], <YYYY-MM-DD>)`; older headings are left as
  they are.
- A change to `tessera-config` types regenerates `docs/config.md` with `just config-docs`; CI fails when it is stale.
- Changes to `tessera-vt`, `tessera-grid` or `tessera-term` can be fuzzed with `just fuzz <target>`
  (four libFuzzer targets in `fuzz/`; setup under "Fuzzing" in `docs/software-factory.md`).

Before you push, run `just check` (fmt, clippy, tests, cargo-deny); add `just app-test` when you touched Swift or
`tessera-ffi`. The full gate is `scripts/quality-gate.sh <worktree>`, described in
[`docs/testing.md`](docs/testing.md#which-tests-to-run).

## Code

- Rust stable, edition 2024. No nightly features.
- `just check` must pass: `rustfmt`, `clippy -D warnings` (pedantic subset), `nextest`, `cargo-deny`.
- `#![forbid(unsafe_code)]` everywhere except `tessera-pty` and `tessera-ffi`. Every `unsafe` block carries a
  `// SAFETY:` comment. The exceptions outside them are the counting test allocators in
  `crates/tessera-render/tests/alloc.rs` (ADR 0016), `crates/tessera-grid/tests/reset.rs`,
  `crates/tessera-term/tests/osc8_alloc.rs`, `crates/tessera-term/tests/mouse_alloc.rs`,
  `crates/tessera-term/tests/screen_clears_alloc.rs`, `crates/tessera-term/tests/rect_alloc.rs` and
  `crates/tessera-app-core/tests/keybindings.rs`. `scripts/verify-branch.sh` checks this mechanically: its `unsafe`
  check fails an added `unsafe` or `#[global_allocator]` line outside those two crates unless the file is listed in
  `scripts/unsafe-exceptions.txt`, and `just verify-branch-test` fails when that list and `docs/security.md` S7 differ.
- The rules in [`docs/security.md`](docs/security.md) hold for every change: nothing the child sends is echoed into
  the input or read back, and every buffer filled from untrusted input has a named cap.
- Public items are documented. Crate roots start with a purpose paragraph.
- Dependencies point downward only (see [`docs/architecture.md`](docs/architecture.md)). A crate that needs
  something from above it is a design smell; raise it in the PR.
- Prefer snapshot tests (`insta`) for emulator behaviour and property tests for parsers.

## Swift

- Swift 6 language mode, strict concurrency.
- The app talks to Rust only through `TesseraKit`, the Swift wrapper over the generated header (ADR 0012); only
  `apps/macos/Sources/TesseraKit` imports `TesseraCore`. No business logic in Swift that could live in Rust.
