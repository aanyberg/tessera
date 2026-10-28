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
milestone-lead ── brief ──► [spec-researcher] ──► implementer (rust-/swift-implementer, release-engineer)
                                                        │
                    ┌──────── revise ◄──────────────────┤
                    ▼                                   ▼
          revision-implementer ────────────────► {rust|swift|code}-critic ── escalate ──► lead decides
                    ▲                                   │ ship
                    │ fail (≤2 cycles)                  ▼
                    └───────────────── quality-gate ∥ adversarial-tester
                                                        │ pass
                                                        ▼
                                              integrator (serialized)
                                                        │ whole wave integrated, gate green
                                                        ▼
                                lead pushes wave/<milestone>-<n>, one PR, rebase-merge
                                                        │ merged
                                                        ▼
                                         resync main ──► flow-analyst retro
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
code (`crates/`, `apps/macos/`; test-only changes are exempt), and a green `scripts/quality-gate.sh` on the
integrated tree. CI is a confirmation, not a reviewer. A red CI run means the local pipeline missed something:
fix the cause locally through the same loop, then push again. Never open a PR to find out what CI thinks, and
never push a branch mid-review.

"The lead" in this document is the main Claude Code session that runs the team. It is the only one that pushes,
opens PRs and merges them. `milestone-lead` is the planner and arbiter it delegates to; like every other agent it
never pushes.

**Merging.** Wave PRs are rebase-merged so each task's conventional commit reaches `git cliff`: a squash merge
would collapse the wave into one commit whose title is not conventional. Every other PR is squash-merged with a
conventional title. Rebase-merge must be enabled in the repository settings (Settings → General → Pull Requests
→ Allow rebase merging). The 0.1.0 wave PRs #67 and #70 were squash-merged before this rule; a
`commit_preprocessors` entry in `cliff.toml` maps their `<version> wave <n>:` titles to `feat:` so they still
appear in the notes.

**Resync.** After a PR merges, bring local main back to `origin/main` before the next integration or branch:

- Squash-merged PR: `git fetch origin && git switch main && git merge --ff-only origin/main`.
- Wave PR (rebase-merge rewrites SHAs, so `--ff-only` fails by design): `git fetch origin && git switch main &&
  git cherry origin/main main`. Every line must start with `-` (each local commit has an equivalent patch
  upstream); only then run `git reset --hard origin/main`. Any `+` line means a local commit is not upstream: stop
  and investigate instead of resetting.
  A `+` line together with an empty `git diff origin/main main` and `gh pr view <pr> --json state` showing
  `MERGED` (for example a wave PR squash-merged by mistake) is safe to reset; anything else is not.

**Base moved.** `main` is protected with strict up-to-date checks, so an open PR cannot merge after `origin/main`
moves.

- Wave PR: never use GitHub's "Update branch" (it moves the wave branch away from local main). Instead run
  `git fetch origin && git switch main && git rebase origin/main`, re-run `scripts/quality-gate.sh` on main, then
  `git push --force-with-lease origin main:refs/heads/wave/<milestone>-<n>`, wait for CI, and rebase-merge. If
  the rebase stops on a conflict, resolve it or run `git rebase --abort` and escalate; never push a partially
  rebased main.
- Squash PR: `gh pr update-branch <pr>`, wait for CI, and merge.
- If a squash PR lands while local main carries integrated wave commits, do not run the `--ff-only` resync (it
  fails); rebase local main onto `origin/main` as in the wave PR bullet above, before pushing the wave.

## Running a wave

Waves run as an agent team (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, set in `.claude/settings.local.json`).
Scripted workflows are not used for waves: they cannot apply judgement when a step fails or a critic and an
implementer disagree, and they hide progress from the user.

0. **Pre-flight.** Before planning, the lead checks that
   `gh api repos/<owner>/<repo> --jq .allow_rebase_merge` prints `true`. If it does not, stop and ask the user to
   enable rebase merging before the wave starts, not at merge time. Commands the user types with `!` may not run
   in the session; when a step needs the user, give them the exact command to run in a separate terminal, or run
   the `gh api` call from the lead with their go-ahead.
1. **Plan.** `milestone-lead` turns open issues into waves of five parallel-safe issues and labels each issue
   `wave:<n>`. Wave numbers are global and never reset across milestones: the next wave is the highest existing
   `wave:<n>` plus one, or 3 when no `wave:<n>` label exists (0.1.0 used waves 1 and 2 before labels existed). It
   returns one brief per issue (files, API names, ADR numbers, deps and do-not-touch lists pre-assigned). Decisions
   that constrain a later issue, and findings deferred to it, go in a comment on that issue. There is no task-board
   file in the repo. Where two issues change the same function, the planner marks one upstream and one
   downstream (or splits them across waves); the downstream task's critique waits for the upstream merge, or is
   limited to a `git merge-tree` check against the upstream branch until then. Hot-path issues carry a
   performance bar with a named corpus measured against main.
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
   subagents. Any major finding fails the gate; at most 2 verify cycles. A structural rewrite during a verify
   cycle gets a fresh critic before verification resumes.
6. **Integrate.** Verified branches go to `integrator` one at a time, first verified first merged (not a fixed
   plan order, except that a downstream task merges after its upstream); it squash-merges each into local main as
   one conventional commit with the session's attribution trailer, re-runs the gate and removes the worktree.
7. **Ship.** When every task in the wave is integrated and the gate is green on local main, the lead pushes and
   opens one PR:
   `git push origin main:refs/heads/wave/<milestone>-<n>`, then
   `gh pr create --base main --head wave/<milestone>-<n> --title "<milestone> wave <n>: <summary>" --body-file
   <file>`, with one `Closes #<issue>` line per task in the body file. When CI is green, the lead merges it with
   `gh pr merge <pr> --rebase`.
8. **Close out.** After the PR merges, the lead resyncs local main (wave PR rule above), comments deferred
   findings on their owning issues, starts the next wave, and hands the wave's transcripts to `flow-analyst`,
   whose retro lands in `docs/retros/`.

**Changes after `ship`.** Fixes the lead sends after a critic's `ship` or after a verify pass are classified:

- Doc-only and test-only changes go to the teammate; the lead reads the diff. No fresh critic.
- When a critic's only remaining major is a doc line or an item the brief already records as a lead decision,
  the lead checks the fix in the diff instead of spending a critique round.
- Any other non-test code change, and every change to a hot path or a fast path, gets a fresh critic (it may be
  scoped to the changed region) or a re-run of the adversarial differential on that region before integration.

Critics and verifiers are always one-shot subagents, never long-lived teammates, so every review starts from a
clean context.

## Changes outside a wave

Hotfixes, CI fixes and factory changes take the same loop at a smaller size, still before any push:

1. Precondition: `git fetch origin && git rev-list --count origin/main..main` prints `0`. If it does not, either
   land the open wave PR first or, if it already merged, run the wave resync rule. Never cut a hotfix from a main
   that carries unmerged or unresynced wave commits. Then resync with the rule for the PR that last merged, and
   branch from local main in its own worktree, so every verifier's `main...HEAD` is exactly the change.
2. Implement with the agent for the area (`rust-implementer`, `swift-implementer`, `release-engineer`,
   `docs-writer`), or directly by the lead for agent definitions and docs.
3. A fresh critic for the area; revise until `ship`.
4. `quality-gate`. Add `adversarial-tester` when the change touches product code (`crates/`, `apps/macos/`)
   other than tests; skip it for docs, agent definitions and test-only changes.
5. `git push -u origin <branch>`, then
   `gh pr create --base main --head <branch> --title "<conventional title>" --body-file <file>` (with
   `Closes #<issue>` where one exists). When CI is green, the lead merges it with
   `gh pr merge <pr> --squash --subject "<conventional title> (#<pr>)"`, then resyncs.

## Running the gate

Every agent that runs `scripts/quality-gate.sh` reads the real exit code: redirect output to a file and read `$?`
(`scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`). A status read through a pipe is not evidence.

Agent definitions change only through a retro proposal or an explicit decision; record the reason in the commit
message. The `factory` project skill in `.claude/skills/factory/` points lead sessions at this document.
