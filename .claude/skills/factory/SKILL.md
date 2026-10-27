---
name: factory
description: Tessera software factory. Use in the lead session when planning or running a milestone wave, delegating issues to the project agents, or shipping any change (hotfix, CI fix, factory change) to a pull request in this repo.
---

# Tessera software factory

The full runbook is `docs/software-factory.md`. Read it before you delegate work. These rules are not optional:

1. **PR gate.** Push a branch and open a PR only after the local pipeline has passed: critic `ship`, `quality-gate`
   pass, `adversarial-tester` pass for product code, and a green `scripts/quality-gate.sh` on the integrated tree.
   CI only confirms. Never push mid-review.
2. **Waves run as an agent team.** The lead owns the shared task list. One teammate per issue implements in its
   own worktree. Do not use the Workflow tool for waves.
3. **Fresh reviewers.** Every critique round and every verification run is a new one-shot subagent
   (`rust-critic`, `swift-critic`, `code-critic`, `quality-gate`, `adversarial-tester`). Never a teammate.
4. **Caps.** Up to 5 critique rounds, 2 verify cycles (escalations do not count). On `escalate`, the lead decides and records the decision in
   the brief.
5. **Plan on GitHub.** `milestone-lead` labels issues `wave:<n>` and comments decisions and deferred findings on
   the issue they affect. There is no task-board file.
6. **Exit codes.** Run the gate as `scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`. Never trust a status read
   through a pipe.
7. **Integration and merging.** `integrator` merges one branch at a time into local main. Wave PRs are
   rebase-merged, all other PRs squash-merged with a conventional title. Resync local main with `origin/main` after
   every merge. Changes outside a wave follow "Changes outside a wave" in the runbook.
8. **Labels.** Wave numbers are global and never reset across milestones.
