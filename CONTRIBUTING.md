# Contributing

## Workflow

- `main` is protected. Work on a branch, open a PR, CI must be green.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org):
  `feat(grid): reflow on resize`, `fix(vt): handle empty OSC`, `docs: …`.
  Scopes are crate names without the `tessera-` prefix, or `app` for Swift.
- PRs are squash-merged; the PR title becomes the commit message.
- Architecture changes get an ADR in `docs/adr/` in the same PR.

## Code

- Rust stable, edition 2024. No nightly features.
- `just check` must pass: `rustfmt`, `clippy -D warnings` (pedantic subset),
  `nextest`, `cargo-deny`.
- `#![forbid(unsafe_code)]` everywhere except `tessera-pty` and `tessera-ffi`.
  Every `unsafe` block carries a `// SAFETY:` comment.
- Public items are documented. Crate roots start with a purpose paragraph.
- Dependencies point downward only (see `docs/architecture.md`). A crate that
  needs something from above it is a design smell; raise it in the PR.
- Prefer snapshot tests (`insta`) for emulator behaviour and property tests
  for parsers.

## Swift

- Swift 6 language mode, strict concurrency.
- The app talks to Rust only through `TesseraCore` (the generated header).
  No business logic in Swift that could live in Rust.
