---
name: factory
description: Tessera software factory. Use in the lead session when planning or running a milestone wave, delegating issues to the project agents, or shipping any change (hotfix, CI fix, factory change) to a pull request in this repo.
---

# Tessera software factory

The full runbook is `docs/software-factory.md`. Read it before you delegate work. These rules are not optional:

1. **PR gate.** Push a branch and open a PR only after the local pipeline has passed: critic `ship` (or a round the
   lead accepts under runbook step 4), `scripts/verify-branch.sh` pass, `adversarial-tester` pass for product code,
   and a green `scripts/quality-gate.sh` on the integrated commit. CI only confirms. Never push mid-review.
2. **Waves run as an agent team.** The lead owns the shared task list and keeps up to five tasks in flight from
   the wave's queue; each task ships as its own PR as soon as it is verified. One teammate per issue implements in
   its own worktree and fixes its own branch on `revise`. Do not use the Workflow tool for waves.
3. **Fresh reviewers.** Every critique round and every adversarial run is a new one-shot subagent
   (`rust-critic`, `swift-critic`, `code-critic`, `adversarial-tester`). Never a teammate. Critique rounds 2+ get a
   scoped brief (previous findings, responses, commit range); rounds 3+ and verify cycle 2 are spawned with
   `effort: "lower"`. The adversarial brief carries the critic's `coverage`.
4. **Caps.** Up to 5 critique rounds (escalations do not count) and 2 verify cycles. On `escalate`, the lead
   decides and records the decision in the brief.
5. **Plan on GitHub.** `milestone-lead` labels issues `wave:<n>` and comments decisions and deferred findings on
   the issue they affect. There is no task-board file.
6. **Exit codes.** Run the gate as `scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`. Never trust a status read
   through a pipe.
7. **Integration and merging.** `integrator` turns one verified branch at a time into a single conventional
   commit on `origin/main`; the lead pushes it, opens its PR, squash-merges it with a conventional title and
   resyncs local main before the next integration. Wave PRs from wave 4 and earlier follow "Wave PRs" in the
   runbook. Changes outside a wave follow "Changes outside a wave".
8. **Labels.** Wave numbers are global and never reset across milestones.
9. **Tooling, not ad-hoc setups.** Benchmarks and differential fuzzing use `cargo xtask ab`; mechanical checks
   use `scripts/verify-branch.sh`; retro numbers come from `scripts/factory-stats.sh`. Pre-flight checks that
   `sccache` is installed.
