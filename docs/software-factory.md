# Software factory

Tessera milestones are delivered by a fixed roster of project agents in `.claude/agents/`, run as a Claude Code
agent team: the lead session owns the shared task list and delegates each issue to a teammate. Each agent has one
responsibility, a pinned model and effort, and a tool allowlist that matches that responsibility. Change the roster
by editing the agent file; the lead refers to agents by name only.

## Roster

| Agent | Responsibility | Model | Effort | Writes code | Commits |
|---|---|---|---|---|---|
| `milestone-lead` | Waves, task briefs, pre-assigned ADR numbers/deps/modules, task board, dispute arbitration | fable | medium | no | no |
| `spec-researcher` | Spec → test table/fixture before spec-heavy implementation | sonnet | high | fixtures only | no |
| `rust-implementer` | One Rust issue in its own worktree, gate green | opus | high | yes | branch |
| `swift-implementer` | One macOS app issue in its own worktree | opus | high | yes | branch |
| `revision-implementer` | Applies findings from critic/verifiers; fixes or refutes with evidence | opus | medium | yes | branch |
| `rust-critic` | Independent ship/revise verdict for crates/ branches; owns the "Done when" judgement | opus | high | no | no |
| `swift-critic` | Independent ship/revise verdict for apps/macos branches; Swift 6 concurrency, AppKit, FFI ownership | opus | high | no | no |
| `code-critic` | Independent ship/revise verdict for infra and docs branches (workflows, justfile, scripts, ADRs) | opus | high | no | no |
| `quality-gate` | Mechanical: `scripts/quality-gate.sh`, clean tree, contains main, trailers, scope, ADR collisions | haiku | low | no | no |
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
milestone-lead ─ brief ─► [spec-researcher] ─► rust-|swift-|release-implementer ─► {rust|swift|code}-critic ─┐
        ▲                                                                   revise ◄──┘ ship
        │ findings/disputes                           revision-implementer ◄───────────┘   │
        │                                                                                  ▼
        │                                   quality-gate ∥ adversarial-tester  (fail → revision-implementer, ≤2 cycles)
        │                                                                                  │ pass
        └──────────────────────── flow-analyst (after wave) ◄── integrator (serialized) ◄──┘
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

## Running a wave

Waves run as an agent team (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, set in `.claude/settings.local.json`).
Scripted workflows are not used for waves: they cannot apply judgement when a step fails or a critic and an
implementer disagree, and they hide progress from the user.

1. **Plan.** `milestone-lead` turns open issues into waves of five parallel-safe issues and writes the task board
   `TASKS.md` with one brief per issue (files, API names, ADR numbers, deps and do-not-touch lists pre-assigned).
2. **Task list.** The lead session creates one shared task per issue in the wave, carrying the brief.
3. **Implement.** One teammate per issue claims its task and implements it in its own worktree
   (`../tessera-worktrees/wave-<n>/task-<issue>`, branch `task/<issue>-<slug>`) as `rust-implementer`,
   `swift-implementer` or `release-engineer` by area. Spec-heavy issues get a `spec-researcher` pass first; if the
   researcher fails, the lead decides whether to retry or proceed from the issue text.
4. **Critique.** For every round the lead spawns a fresh one-shot critic subagent (`rust-critic`, `swift-critic`
   or `code-critic` by area) with no memory of earlier rounds. On `revise` the owning teammate, or a
   `revision-implementer`, fixes or refutes each finding. The loop is capped at 5 rounds; disputes go to
   `milestone-lead`, whose decision is added to the brief.
5. **Verify.** After `ship`, the lead runs `quality-gate` and `adversarial-tester` in parallel as one-shot
   subagents. Any major finding fails the gate; at most 2 fix cycles.
6. **Merge.** Verified branches go to `integrator` one at a time; it squash-merges into local main with the
   session's attribution trailer, re-runs the gate and removes the worktree. Nothing is pushed without the user.
7. **Close out.** The lead updates `TASKS.md`, starts the next wave, and hands the wave's transcripts to
   `flow-analyst`, whose retro lands in `docs/retros/`.

Critics and verifiers are always one-shot subagents, never long-lived teammates, so every review starts from a
clean context. Agent definitions change only through a retro proposal or an explicit decision; record the reason
in the commit message.
