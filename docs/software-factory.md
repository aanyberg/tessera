# Software factory

Tessera milestones are delivered by a fixed roster of project agents in `.claude/agents/`, orchestrated by the
`milestone-wave` workflow in `.claude/workflows/`. Each agent has one responsibility, a pinned model and effort,
and a tool allowlist that matches that responsibility. Change the roster by editing the agent file; the workflow
refers to agents by name only.

## Roster

| Agent | Responsibility | Model | Effort | Writes code | Commits |
|---|---|---|---|---|---|
| `milestone-lead` | Waves, task briefs, pre-assigned ADR numbers/deps/modules, task board, dispute arbitration | fable | high | no | no |
| `spec-researcher` | Spec → test table/fixture before spec-heavy implementation | sonnet | high | fixtures only | no |
| `rust-implementer` | One Rust issue in its own worktree, gate green | fable | high | yes | branch |
| `swift-implementer` | One macOS app issue in its own worktree | fable | high | yes | branch |
| `revision-implementer` | Applies findings from critic/verifiers; fixes or refutes with evidence | fable | medium | yes | branch |
| `code-critic` | Independent ship/revise verdict; owns the "Done when" judgement | fable | high | no | no |
| `quality-gate` | Mechanical: `scripts/quality-gate.sh`, clean tree, contains main, trailers, scope, ADR collisions | haiku | low | no | no |
| `adversarial-tester` | Breaks ship-ready branches with hostile input and real flows; reproduced bugs only | fable | xhigh | scratch only | no |
| `integrator` | Serialized squash-merge into local main, lockfile conflicts, worktree cleanup | sonnet | medium | conflicts only | main |
| `docs-writer` | ADRs, CONTRIBUTING, README, generated references after merges | sonnet | medium | docs | branch |
| `release-engineer` | CI/release workflows, justfile, packaging, tag/version guards | fable | high | infra | branch |
| `flow-analyst` | Post-wave retro from journals: cost/value per role, proposed changes | sonnet | high | retro only | no |

Model aliases map to the current Claude 5 family (`fable` = claude-fable-5-1, `sonnet` = claude-sonnet-5-5,
`haiku` = claude-haiku-5-5). Pin a full id in the frontmatter if a role must not follow alias updates.

## Pipeline per issue

```
milestone-lead ─ brief ─► [spec-researcher] ─► rust-|swift-|release-implementer ─► code-critic ─┐
        ▲                                                                   revise ◄──┘ ship
        │ findings/disputes                           revision-implementer ◄───────────┘   │
        │                                                                                  ▼
        │                                   quality-gate ∥ adversarial-tester  (fail → revision-implementer, ≤2 cycles)
        │                                                                                  │ pass
        └──────────────────────── flow-analyst (after wave) ◄── integrator (serialized) ◄──┘
```

- Critique loop is capped at 5 rounds; wave 1 converged in ≤3.
- Verification runs two lenses, not three: the acceptance lens from wave 1 never failed a task while passing
  branches with major findings, and overlapped the critic. The critic now owns "Done when"; the mechanical gate is
  a haiku agent around a script; the adversarial tester stays at the highest effort because it found 7 of the 9
  post-ship majors in wave 1.
- Any major finding from any gate fails that gate.
- Every branch merges current main before critique and before verification so verifiers never report stale-base
  problems as findings.

## Running a wave

```
Workflow name: milestone-wave
args: { repo, wtRoot, attribution, tasks: [brief, ...] }   # briefs come from milestone-lead; attribution = the session's commit trailer lines
```

The wave result is the input for `flow-analyst`, whose retro lands in `docs/retros/`. Agent definitions change
only through a retro proposal or an explicit decision; record the reason in the commit message.
