# Software factory

Tessera milestones are delivered by a fixed roster of project agents in `.claude/agents/`, run as a Claude Code
agent team: the lead session owns the shared task list and delegates each issue to a teammate. Each agent has one
responsibility, a pinned model and effort, and a tool allowlist that matches that responsibility. Change the roster
by editing the agent file; the lead refers to agents by name only.

## Roster

| Agent | Responsibility | Model | Effort | Writes code | Commits |
|---|---|---|---|---|---|
| `milestone-lead` | Waves (`wave:<n>` labels), task briefs, pre-assigned ADR numbers/deps/modules, dispute arbitration | fable | medium | no | no |
| `spec-researcher` | Spec → test table/fixture before spec-heavy implementation | sonnet | high | fixtures only | no |
| `rust-implementer` | One Rust issue in its own worktree, gate green | opus | high | yes | branch |
| `swift-implementer` | One macOS app issue in its own worktree | opus | high | yes | branch |
| `revision-implementer` | Applies findings from critic/verifiers; fixes or refutes with evidence | opus | medium | yes | branch |
| `rust-critic` | Independent ship/revise/escalate verdict for crates/ branches; owns the "Done when" judgement | opus | high | no | no |
| `swift-critic` | Independent ship/revise/escalate verdict for apps/macos branches; Swift 6 concurrency, AppKit, FFI ownership | opus | high | no | no |
| `code-critic` | Independent ship/revise/escalate verdict for infra and docs branches (workflows, justfile, scripts, ADRs) | opus | high | no | no |
| `quality-gate` | Mechanical: `scripts/quality-gate.sh`, clean tree, contains main, trailers, scope, ADR collisions, timing tests under load | haiku | low | no | no |
| `adversarial-tester` | Breaks ship-ready branches with hostile input and real flows; reproduced bugs only | opus | xhigh | scratch only | no |
| `integrator` | Serialized squash-merge into local main, lockfile conflicts, worktree cleanup | sonnet | medium | conflicts only | main |
| `docs-writer` | ADRs, CONTRIBUTING, README, generated references after merges | sonnet | medium | docs | branch |
| `release-engineer` | CI/release workflows, justfile, packaging, tag/version guards | opus | high | infra | branch |
| `flow-analyst` | Post-wave retro from journals: cost/value per role, proposed changes | sonnet | high | retro only | no |

Model aliases map to the current Claude 5 family (`fable` = claude-fable-5-1, `opus` = claude-opus-5-5,
`sonnet` = claude-sonnet-5-5, `haiku` = claude-haiku-5-5). Fable is reserved for the lead; implementation,
critique, adversarial testing and release engineering run on Opus. Pin a full id in the frontmatter if a role must not follow alias updates.

## Pipeline per issue

```
milestone-lead ─ brief ─► [spec-researcher] ─► rust-|swift-implementer|release-engineer ─► {rust|swift|code}-critic ─┐
        ▲                                                        revise ◄──┘ escalate → lead decides │ ship
        │ findings/disputes                           revision-implementer ◄───────────┘   │
        │                                                                                  ▼
        │                                   quality-gate ∥ adversarial-tester  (fail → revision-implementer, ≤2 cycles)
        │                                                                                  │ pass
        └──────────────────────── flow-analyst (after wave) ◄── integrator (serialized) ◄──┘
                                                     │ whole wave integrated, gate green
                                                     ▼
                                      lead pushes wave/<milestone>-<n>, one PR, rebase-merge
```

- Critique loop is capped at 5 rounds; wave 1 converged in ≤3.
- The critic is chosen by area: `rust-critic` for crates/, `swift-critic` for apps/macos, `code-critic` for infra and docs.
- Verification runs two lenses, not three: the acceptance lens from wave 1 never failed a task while passing
  branches with major findings, and overlapped the critic. The critic now owns "Done when"; the mechanical gate is
  a haiku agent around a script; the adversarial tester stays at the highest effort because it found 7 of the 9
  post-ship majors in wave 1.
- Any major finding from any gate fails that gate.
- Every branch merges current main before critique and before verification so verifiers never report stale-base
  problems as findings.

## The PR gate

A branch is pushed and a pull request opened only after the whole local pipeline has passed: critic `ship`,
`quality-gate` pass (including its load check of timing-sensitive tests), `adversarial-tester` pass for product
code (`crates/`, `apps/macos/`), and a green `scripts/quality-gate.sh` on the integrated tree. CI is a confirmation, not a reviewer. A red
CI run means the local pipeline missed something: fix the cause locally through the same loop, then push again.
Never open a PR to find out what CI thinks, and never push a branch mid-review.

"The lead" in this document is the main Claude Code session that runs the team. It is the only one that pushes.
`milestone-lead` is the planner and arbiter it delegates to; like every other agent it never pushes.

**Merging.** Wave PRs are rebase-merged so each task's conventional commit reaches `git cliff`: a squash merge
would collapse the wave into one unconventional commit that `filter_unconventional` drops (this is what happened
to #67 and #70). Every other PR is squash-merged with a conventional title. Rebase-merge must be enabled in the
repository settings (Settings → General → Pull Requests → Allow rebase merging).

**Resync.** After any PR merges, fast-forward local main to `origin/main` before the next integration or branch:
`git fetch origin && git switch main && git merge --ff-only origin/main`. A rebase-merge rewrites SHAs, so after a
wave PR merges, confirm `git diff main origin/main` is empty and then `git reset --hard origin/main`.

## Running a wave

Waves run as an agent team (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, set in `.claude/settings.local.json`).
Scripted workflows are not used for waves: they cannot apply judgement when a step fails or a critic and an
implementer disagree, and they hide progress from the user.

1. **Plan.** `milestone-lead` turns open issues into waves of five parallel-safe issues, labels each issue
   `wave:<n>` (wave numbers are global and never reset across milestones, so labels cannot collide), and returns one brief per issue (files, API names, ADR numbers, deps and do-not-touch lists
   pre-assigned). Decisions that constrain a later issue, and findings deferred to it, go in a comment on that
   issue. There is no task-board file in the repo.
2. **Task list.** The lead session creates one shared task per issue in the wave, carrying the brief.
3. **Implement.** One teammate per issue claims its task and implements it in its own worktree
   (`../tessera-worktrees/wave-<n>/task-<issue>`, branch `task/<issue>-<slug>`) as `rust-implementer`,
   `swift-implementer` or `release-engineer` by area. Spec-heavy issues get a `spec-researcher` pass first; if the
   researcher fails, the lead decides whether to retry or proceed from the issue text.
4. **Critique.** For every round the lead spawns a fresh one-shot critic subagent (`rust-critic`, `swift-critic`
   or `code-critic` by area) with no memory of earlier rounds. On `revise` the owning teammate, or a
   `revision-implementer`, fixes or refutes each finding. On `escalate` the lead (or `milestone-lead`) decides and
   records the decision in the brief before the next round; escalations do not count against the cap. The loop
   is capped at 5 rounds.
5. **Verify.** After `ship`, the lead runs `quality-gate` and `adversarial-tester` in parallel as one-shot
   subagents. Any major finding fails the gate; at most 2 verify cycles. A structural rewrite during a fix cycle
   gets a fresh critic before verification resumes.
6. **Integrate.** Verified branches go to `integrator` one at a time; it squash-merges into local main with the
   session's attribution trailer, re-runs the gate and removes the worktree.
7. **Ship.** When every task in the wave is integrated and the gate is green on local main, the lead pushes one
   wave branch (`wave/<milestone>-<n>`) and opens one PR, rebase-merged once CI is green. See the PR gate above.
8. **Close out.** After the PR merges, the lead resyncs local main, comments deferred findings on their owning
   issues, starts the next wave, and hands the
   wave's transcripts to `flow-analyst`, whose retro lands in `docs/retros/`.

Critics and verifiers are always one-shot subagents, never long-lived teammates, so every review starts from a
clean context.

## Changes outside a wave

Hotfixes, CI fixes and factory changes take the same loop at a smaller size, still before any push:

1. Resync local main with `origin/main` (see Resync above; only possible when no wave is integrated but
   unpushed), then branch from local main in its own worktree, so every verifier's `main...HEAD` is exactly the
   change.
2. Implement with the agent for the area (`rust-implementer`, `swift-implementer`, `release-engineer`,
   `docs-writer`), or directly by the lead for agent definitions and docs.
3. A fresh critic for the area; revise until `ship`.
4. `quality-gate`. Add `adversarial-tester` when the change touches product code (`crates/`, `apps/macos/`);
   skip it for docs, agent definitions and test-only changes.
5. Push and open the PR with a conventional title. Squash-merge when CI is green, then resync.

## Running the gate

Every agent that runs `scripts/quality-gate.sh` reads the real exit code: redirect output to a file and read `$?`
(`scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`). A status read through a pipe is not evidence.

Agent definitions change only through a retro proposal or an explicit decision; record the reason in the commit
message. The `factory` project skill in `.claude/skills/factory/` points lead sessions at this document.
