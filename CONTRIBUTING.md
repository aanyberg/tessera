# Contributing

## Workflow

- `main` is protected. Work on a branch, open a PR, CI must be green.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org):
  `feat(grid): reflow on resize`, `fix(vt): handle empty OSC`, `docs: …`.
  Scopes are crate names without the `tessera-` prefix, or `app` for Swift.
- PRs are squash-merged with an explicit Conventional Commit subject
  (`gh pr merge <pr> --squash --subject "<type>(<scope>): <title> (#<pr>)"`).
  The agent team ships each task as its own squash-merged PR from wave 5
  on; wave PRs (`wave/<milestone>-<n>`, wave 4 and earlier) are
  rebase-merged instead so each task's commit reaches the release notes.
  See `docs/software-factory.md`.
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

## Debugging: screen dumps

`tessera-devtool` runs a command in a headless terminal and prints what the
emulator drew, without the app:

```sh
cargo run -p tessera-devtool -- run 'ls --color' --cols 80 --rows 24
cargo run -p tessera-devtool -- run 'printf "a\tb\n"' --cols 20 --rows 3 --format text
```

The command string runs as `/bin/sh -c -- <command>` in a real PTY (so quote
it as one argument; a leading `-` is part of the command), with the environment every Tessera session gets
(`TERM=xterm-256color`, `COLORTERM=truecolor`, …) and the current directory.
Nothing is written to its input. Output feeds `tessera_term::Terminal`; when
the command exits, the final grid is printed in one of three formats:

- `--format ansi` (default): the screen re-encoded as SGR colours and
  attributes, for viewing in a real terminal. Each row ends with a reset.
- `--format text`: plain text, one line per row, trailing blanks trimmed, a
  `\` after a row that soft-wraps. These are the rules of the emulator's
  snapshot helper (`render_screen`, which always marks the cursor; add
  `--cursor` to match it), so a dump can be pasted into a test.
- `--format cells`: one line per cell, `row:col "text" markers style`, to see
  wide-character spacers, grapheme clusters and exact colours.

`--scrollback` prints the history above the screen (as negative rows in
`cells`); `--cursor` marks the cursor (a `|` after its cell in `text`,
`<cursor` in `cells`).

The tool waits about `--timeout` seconds (default 10, fractions allowed),
even while the command floods the terminal; the timeout also covers the
emulator processing the output, so a command that exits while the tool is
still behind on its output counts as timed out. When the timeout expires the
tool sends `SIGHUP` to the command's shell, gives it 200 ms to exit, then
sends `SIGKILL`, keeps reading for up to one more second, prints the grid and
exits with `124`. Deadlines are checked between slices of about 512 bytes of
output, so at extreme sizes (a 2000-column `a ESC[65535b`, DECALN on a
2000x2000 screen) a single slice can take seconds and stretch the bound. Only
the shell is signalled: `tessera-pty` has no process-group signal, so
processes the command started are left to the hangup the kernel sends when
the shell (the session leader) dies. Otherwise the tool exits with the
command's status (`128 + signal` if a signal ended it); a closed stdout
(`| head`) does not change that. `125` means the tool itself failed.

Memory grows with `--cols`: the default 10 000 lines of scrollback are kept
at full width even without `--scrollback` (about 1 MiB per line at 65535
columns), and `--format cells` builds its whole output in memory. Inherited
`COLUMNS` and `LINES` are passed to the command unchanged, and programs that
read them believe them over `--cols`/`--rows`; unset them for such programs.

The formatters are a library too (`tessera_devtool::dump::{ansi, text,
cells}`), so tests can dump a `Grid` they built by hand. The crate is
dev-only: `publish = false`, and a test checks that `tessera-ffi` and
`tessera-app-core` never link it.

## Swift

- Swift 6 language mode, strict concurrency.
- The app talks to Rust only through `TesseraKit`, the Swift wrapper over
  the generated header (ADR 0012); only `apps/macos/Sources/TesseraKit`
  imports `TesseraCore`. No business logic in Swift that could live in Rust.

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
