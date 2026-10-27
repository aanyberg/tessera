# Contributing

## Workflow

- `main` is protected. Work on a branch, open a PR, CI must be green.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org):
  `feat(grid): reflow on resize`, `fix(vt): handle empty OSC`, `docs: …`.
  Scopes are crate names without the `tessera-` prefix, or `app` for Swift.
- PRs are squash-merged; the PR title becomes the commit message and must be a Conventional Commit. Wave PRs
  from the agent team (`wave/<milestone>-<n>`) are rebase-merged instead so each task's commit reaches the
  release notes; see `docs/software-factory.md`.
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

## Releases

Releases are cut by pushing a tag; `.github/workflows/release.yml` does the
rest. Tags are SemVer `vMAJOR.MINOR.PATCH` with an optional pre-release suffix
(`v0.1.0-rc.1`; no leading zeros); anything else fails the workflow. A suffix
marks the GitHub Release as a pre-release.

1. Make sure `main` is green and the `[workspace.package]` version in
   `Cargo.toml` (and `MARKETING_VERSION` in `apps/macos/project.yml`) matches
   the release without its pre-release suffix (`0.1.0` for `v0.1.0-rc.1`).
   The workflow refuses a tag whose commit is not on `main` or whose version
   disagrees with `Cargo.toml`.
2. Preview the notes: `just changelog v0.1.0-rc.1` (git-cliff, `cliff.toml`,
   Conventional Commits; unconventional and merge commits are dropped,
   `chore(deps)` and `chore(release)` are skipped; a commit of any type that
   carries a `Security: <summary>` trailer in its footer lands under
   *Security*). `just changelog` previews the commits *not yet tagged*, so run
   it before tagging; after tagging use `just release-notes <tag>`.
3. Tag and push: `git tag -a v0.1.0-rc.1 -m "v0.1.0-rc.1" && git push origin v0.1.0-rc.1`.

**Promoting a release candidate.** When an rc is good, tag the *same* commit
with the final version and push that tag; nothing is rebuilt from a different
commit:

```sh
git tag -a v0.1.0 -m "v0.1.0" v0.1.0-rc.3^{}   # ^{} = the commit, not the rc tag object
git push origin v0.1.0
```

The notes for `v0.1.0` cover everything since the previous *final* release
(the rc tags on the way are ignored; the first final release covers the whole
history), whereas the notes for an rc cover everything since the nearest
release tag of any kind. The GitHub Release for a final tag is not marked as a
pre-release. After tagging, `just release-notes v0.1.0` shows exactly what the
workflow will attach; `scripts/release-notes-test.sh` exercises the tag
selection in a throwaway repository.

The workflow then, on a `macos-15` runner:

- `just xcframework` – universal (arm64 + x86_64) `TesseraCore.xcframework`.
- `just archive <short> <build>` – `xcodebuild archive`, Release configuration,
  ad-hoc signed (`CODE_SIGN_IDENTITY=-`, hardened runtime). `<short>` is the
  tag without `v` and pre-release suffix (`CFBundleShortVersionString`), `<build>`
  is the workflow run number (`CFBundleVersion`).
- `just package <tag>` – `apps/macos/build/Tessera-<tag>-macos-universal.zip`
  via `ditto` so bundle metadata survives.
- A smoke test unzips the artifact, sets and strips `com.apple.quarantine`,
  verifies the signature, architectures and version, and launches the app.
- `scripts/release-notes.sh <tag>` renders the notes for the pushed tag with
  an explicit `git-cliff --tag` and commit range (previous release tag to
  `HEAD`), ignoring any other release tag on the same commit, so a promoted rc
  and backport tags each get their own heading and "Full diff" link;
  `gh release create` (or `edit` + `upload --clobber` on a re-run) attaches the
  zip and its `.sha256`.

The same steps run locally:

```sh
just xcframework
just archive 0.1.0 1
just package v0.1.0-rc.1
just release-notes v0.1.0-rc.1   # after tagging HEAD
```

**Not signed, not notarized.** Developer ID signing and notarization are
tracked for 0.2.0. Until then Gatekeeper rejects the downloaded app; users must
run `xattr -dr com.apple.quarantine Tessera.app` once after unzipping (the
release notes say so). A failed release can be re-run from the Actions tab; it
updates the existing GitHub Release in place.
