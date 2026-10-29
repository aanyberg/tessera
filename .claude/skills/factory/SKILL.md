---
name: factory
description: Tessera software factory. Use in the lead session when planning or running a milestone wave, delegating issues to the project agents, or shipping any change (hotfix, CI fix, factory change) to a pull request in this repo.
---

# Tessera software factory

The full runbook is `docs/software-factory.md`. Read it before you delegate work. These rules are not optional:

1. **PR gate.** Push a branch and open a PR only after the local pipeline has passed: critic `ship` (or the blocker
   fix commit the review rules clear without a second round, or the applied lead decision after a round-2
   `escalate`), `scripts/verify-branch.sh` pass,
   `adversarial-tester` pass for product code (`crates/`, `apps/macos/`; docs, infra, tooling and test-only changes
   are exempt), and a green `scripts/quality-gate.sh` on the integrated commit. CI only confirms. Never push
   mid-review.
2. **Waves run as an agent team.** The lead owns the shared task list and keeps up to five tasks in flight from
   the wave's queue; each task ships as its own PR as soon as it is verified. One teammate per issue implements in
   its own worktree and fixes its own branch on `revise`. Do not use the Workflow tool for waves.
3. **Fresh reviewers.** Every critique round and every adversarial run is a new one-shot subagent
   (`rust-critic`, `swift-critic`, `code-critic`, `adversarial-tester`). Never a teammate. The adversarial brief
   carries the critic's `coverage` (checks run versus only read).
4. **Review rules** (runbook "Review rules"). Round 1 is one strict full review; every finding is a BLOCKER (wrong
   behaviour, spec or "Done when" miss, data loss or safety, flaky or hanging test, unmet perf bar, breaks a
   sibling) or a MINOR (docs, wording, non-critical test gaps, polish). The implementer fixes blockers only, in one
   fix commit with no new behaviour, and reports a mutation check per blocker. Minors go into one follow-up issue
   in the milestone, labelled for the lowest wave that has not started, filed by the lead; they do not hold the
   merge (the lead may order a cheap docs or test minor fixed now). A refuted or deferred blocker is a disagreement:
   the lead (or `milestone-lead`) decides and records it. The lead re-runs each named mutation check against the
   fix's parent. A second critic round runs only if the fix added new behaviour or code paths, or a blocker
   was a design problem; it is scoped to the fix range, spawned with `effort: "lower"`, and the last: remaining
   blockers, old or new, go to the lead as `escalate`, never round 3; round 2 never returns `revise`. The adversarial tester runs once after `ship` for product
   code; a second cycle only if the first failed, scoped to the repro, at `effort: "lower"`; a cycle-2 failure goes to the lead. For low-risk product
   changes (list in the runbook) the critic and the adversarial tester may run in parallel; if the critic then asks
   for a change, only the new range is re-run. On `escalate`, the lead decides and records the decision in the brief.

   | | Before | After |
   |---|---|---|
   | Critic rounds | up to 5 | 1, plus a scoped `lower` round 2 only for new behaviour or a design blocker |
   | Severity | blocker / major / minor | BLOCKER / MINOR; minors never block |
   | Fix commit | any scope | blockers only, no new behaviour |
   | Adversarial | up to 2 cycles, early start | once after `ship`; cycle 2 only after a failure; docs, infra, tooling, test-only exempt |

5. **Plan on GitHub.** `milestone-lead` labels issues `wave:<n>` and comments decisions and deferred findings on
   the issue they affect. There is no task-board file.
6. **Exit codes.** Run the gate as `scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`. Never trust a status read
   through a pipe.
7. **Integration and merging.** `integrator` turns one verified branch at a time into a single conventional
   commit on `origin/main`; the lead pushes it, opens its PR, squash-merges it with a conventional title and
   resyncs local main before the next integration. Wave PRs from wave 4 and earlier follow "Wave PRs" in the
   runbook. Changes outside a wave follow "Changes outside a wave".
8. **Labels.** Wave numbers are global and never reset across milestones.
9. **Short reports, resumable lead.** Teammates and subagents report a verdict, blockers and a path or issue link;
   detail goes to a file or the GitHub issue. Between waves a fresh lead resumes from GitHub state; mid-wave the
   briefs and decisions live in the lead's task list, so keep the lead (runbook "Session hygiene").
10. **Tooling, not ad-hoc setups.** Benchmarks and differential fuzzing use `cargo xtask ab`; mechanical checks
    use `scripts/verify-branch.sh`; retro numbers come from `scripts/factory-stats.sh`. Pre-flight checks that
    `sccache` is installed.
