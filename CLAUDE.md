# Tessera

macOS terminal emulator: thin Swift app (`apps/macos`) over a modular Rust core (`crates/`, dependencies point
downward only). Architecture: `docs/architecture.md`; decisions: `docs/adr/`; contributing and release flow:
`CONTRIBUTING.md`.

## Checks

- `just check` runs fmt-check, clippy, nextest and cargo-deny; `scripts/quality-gate.sh <worktree>` is the full gate
  and `scripts/verify-branch.sh <worktree>` the mechanical branch checks.
- Read gate exit codes from a redirected log (`scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`), never through a
  pipe.
- Commits and PR titles are conventional (`feat(term): ...`); PRs are squash-merged.

## Factory

Milestone work, hotfixes and any push or PR go through the software factory: `docs/software-factory.md` (invoke the
`factory` skill in the lead session). Nothing is pushed before the local critic, verify and (product code only)
adversarial pipeline has passed.

- Keep agent and teammate reports short: verdict, blockers, a path or issue link for detail. Write long output to a
  file or the GitHub issue, not into the lead's context.
- Between waves, state lives on GitHub (`wave:<n>` labels, issue comments), so a fresh lead can resume from there;
  mid-wave, briefs and decisions are in the lead's task list, so keep that session. `/clear` between unrelated tasks.
