# Software factory

Tessera milestones are delivered by a fixed roster of project agents in `.claude/agents/`, run as a Claude Code
agent team: the lead session owns the shared task list and delegates each issue to a teammate. Each agent has one
responsibility, a pinned model and effort, and a tool allowlist that matches that responsibility. Change the roster
by editing the agent file; the lead refers to agents by name only.

## Roster

| Agent | Responsibility | Model | Effort | Writes code | Commits |
|---|---|---|---|---|---|
| `milestone-lead` | Wave queues (`wave:<n>` labels), task briefs, pre-assigned ADR numbers/deps/modules, dispute arbitration | opus | medium | no | no |
| `spec-researcher` | Spec → test table/fixture during planning, before "Done when" is final | sonnet | high | fixtures only | no |
| `rust-implementer` | One Rust issue in its own worktree, gate green; fixes its own branch on `revise` | opus | high | yes | branch |
| `swift-implementer` | One macOS app issue in its own worktree; fixes its own branch on `revise` | opus | high | yes | branch |
| `rust-critic` | Independent ship/revise/escalate verdict for crates/ branches; owns the "Done when" judgement | opus | high (rounds 3+: medium) | no | no |
| `swift-critic` | Independent ship/revise/escalate verdict for apps/macos branches; Swift 6 concurrency, AppKit, FFI ownership | opus | high (rounds 3+: medium) | no | no |
| `code-critic` | Independent ship/revise/escalate verdict for infra and docs branches (workflows, justfile, scripts, ADRs) | opus | medium (rounds 3+: low) | no | no |
| `adversarial-tester` | Breaks ship-ready branches with hostile input and real flows; reproduced bugs only | opus | xhigh (cycle 2: high) | scratch only | no |
| `integrator` | Serialized: one conventional commit on `origin/main` per verified task, lockfile conflicts, PR body (wave mode: squash into local main) | sonnet | medium | conflicts only | task branch / main |
| `docs-writer` | ADRs, CONTRIBUTING, README, generated references after merges | sonnet | medium | docs | branch |
| `release-engineer` | CI/release workflows, justfile, packaging, tag/version guards | opus | medium | infra | branch |
| `flow-analyst` | Post-wave retro from journals and `scripts/factory-stats.sh`: cost/value per role, proposed changes | sonnet | high | retro only | no |

Model aliases map to the current Claude 5 family (`fable` = claude-fable-5-1, `opus` = claude-opus-5-5,
`sonnet` = claude-sonnet-5-5, `haiku` = claude-haiku-5-5). Fable is not used (token cost); planning, implementation,
critique, adversarial testing and release engineering run on Opus. Pin a full id in the frontmatter if a role must not follow alias updates.

**Effort by round.** Critique rounds 1 and 2 and verify cycle 1 run at the frontmatter effort: they find the
defects (wave 3: 6 majors in round 1, 2 in round 2 from benchmarks and differential fuzzing, none in round 3; both
verify failures in cycle 1). Later runs mostly confirm fixes, so the lead spawns critique rounds 3+ and verify
cycle 2 with the Agent tool's `effort: "lower"`, one step below the frontmatter. Every round from 2 on gets a scoped
brief (below). Any defect a lower-effort run misses that a later gate catches goes into the next retro; that is the
signal to revert. `code-critic` and `release-engineer` run at medium by the user's decision of 2026-10-28 (no retro
data yet: neither ran in wave 3); revisit in the next retro that has runs of either.

**Mechanical checks are scripts, not agents.** `scripts/verify-branch.sh` replaced the haiku `quality-gate` agent
(0 failures in 7 wave-3 runs, one false-positive flag); `revision-implementer` was removed after 0 uses in wave 3,
because teammates fix their own branches with their context warm.

## Pipeline per issue

```
milestone-lead ── draft brief ──► spec-researcher (spec-heavy issues, in parallel) ──► final brief
                                                        │
                                                        ▼
                                      implementer (rust-/swift-implementer, release-engineer)
                                                        │
                    ┌──────── revise ◄──────────────────┤
                    ▼                                   ▼
            same implementer ────────────────► {rust|swift|code}-critic ── escalate ──► lead decides
                    ▲                                   │ ship (adversarial may start during round 2)
                    │ fail (≤2 cycles)                  ▼
                    └──────────── verify-branch.sh ∥ adversarial-tester
                                                        │ pass
                                                        ▼
                                 integrator (serialized): one commit on origin/main
                                                        │
                                                        ▼
                         lead pushes task branch, one PR per task, squash-merge, resync
                                                        │ merged
                                                        ▼
                                  next queued issue starts; after the wave, flow-analyst retro
```

- Critique loop is capped at 5 rounds; waves 1 and 3 converged in ≤3.
- The critic is chosen by area: `rust-critic` for crates/, `swift-critic` for apps/macos, `code-critic` for infra and docs.
- Verification is two lenses: `scripts/verify-branch.sh` (mechanical) and `adversarial-tester` (judgement). The
  critic owns "Done when"; the adversarial tester stays at the highest effort for cycle 1 because it found 7 of the
  9 post-ship majors in wave 1 and both verify failures in wave 3.
- Any major finding from any gate fails that gate.
- Every branch merges current main before critique and before verification so verifiers never report stale-base
  problems as findings.

## Shared tooling

Agents use these instead of rebuilding the setup in every run:

- **Compiler cache.** `.cargo/config.toml` routes rustc through `scripts/rustc-wrapper.sh`, which uses `sccache`
  when it is installed (`brew install sccache`) so every worktree shares one cache for dependencies; without it
  the wrapper runs rustc directly. `TESSERA_NO_SCCACHE=1` turns it off. sccache hashes every `CARGO_*`
  variable, so never set `CARGO_TARGET_DIR` for normal builds: each worktree's default `target/` shares the cache
  (measured: a second worktree hit 18 of 26 dependency compilations; crates with build scripts miss).
- **Gate pass stamps.** `scripts/quality-gate.sh` records a pass on a clean tree per commit, toolchain and gate
  script, so the implementer, critic and verifiers pay for the gate once per commit. `--force` re-runs it.
- **A/B probes.** `cargo xtask ab init --base <rev> --crate <name> --out <scratch dir>` builds a probe crate that
  depends on each crate at the base revision (`base_<name>`) and in the worktree (`head_<name>`). Write cases in
  the probe's `src/cases.rs`, then `cargo xtask ab bench <dir> --case <c> [--max-ratio <bar>]` (alternating
  base/head processes; best-of-N wall time, instructions retired and cycles net of the measured process overhead,
  gating on instructions because a loaded machine distorts wall time; a case too small to measure or whose sides
  compute different checksums fails the bar) and `cargo xtask ab diff <dir> --case <c> --seeds <n>` (differential fuzzing).
  Implementers measure performance bars with it, critics re-measure and diff-fuzz fast paths, the adversarial
  tester uses it for its own probes.
- **Branch verification.** `scripts/verify-branch.sh <wt> [--base <ref>] [--allow <path>]... [--session <url>]
  [--coauthor <line>] [--load <crate>:<target>]... [--no-load] [--parallel <1-8>] [--list-targets]` runs the gate
  and checks the clean tree, ancestry, Conventional Commit subjects and trailers, scope against the brief's paths
  (a warning, for the lead), ADR number collisions, and timing-sensitive tests under load (10 runs each).
  `--parallel N` caps the load check at N test runs in flight, per target and in total (targets run one
  after another); the default is 3 so a busy machine is not exhausted, and the lead may raise it up to 8 on a quiet
  one. Do not improvise extra load runs beside it. `--list-targets` prints the effective cap and the load targets
  derived from the branch (`crate:kind`, one per line) and exits without building; `just verify-branch-test`
  checks that derivation and the cap under bash and zsh. The script is zsh: always brace a name before a colon
  (`${name}:lib`), because zsh reads `$name:lib` as the `:l` modifier and `$name:tests` as `:t`.
- **Wave stats.** `scripts/factory-stats.sh --since <t> --until <t> <session dir>` prints the per-role and
  per-agent cost tables (output tokens, cache reads, tool calls, span and active minutes) for the retro.
- **Scratch directories.** Every subagent works in its own `<scratchpad>/<agent type>-<issue>`; the scratchpad
  is shared, and a probe file left in a worktree under review becomes a false finding.

## The PR gate

A branch is pushed and a pull request opened only after the whole local pipeline has passed: critic `ship`
(or a round the lead accepts under step 4 of "Running a wave"), `scripts/verify-branch.sh` pass (including its load
check of timing-sensitive tests), `adversarial-tester` pass for product code (`crates/`, `apps/macos/`; test-only
changes are exempt), and a green `scripts/quality-gate.sh` on the integrated commit. CI is a confirmation, not a
reviewer. A red CI run means the local pipeline missed something: fix the cause locally through the same loop, then
push again. Never open a PR to find out what CI thinks, and never push a branch mid-review.

"The lead" in this document is the main Claude Code session that runs the team. It is the only one that pushes,
opens PRs and merges them. `milestone-lead` is the planner and arbiter it delegates to; like every other agent it
never pushes.

**Merging.** From wave 5 on, every PR is squash-merged with a conventional title, so each task's commit reaches
`git cliff`: `gh pr merge <pr> --squash --subject "<conventional title> (#<pr>)"`. Wave 4 and earlier ship as one
wave PR with rebase-merge; the rules for those are under "Wave PRs" below. The 0.1.0 wave PRs #67 and #70 were
squash-merged before that rule; a `commit_preprocessors` entry in `cliff.toml` maps their `<version> wave <n>:`
titles to `feat:` so they still appear in the notes.

**Resync.** After a PR merges, bring local main back to `origin/main` before the next integration or branch:
`git fetch origin && git switch main && git merge --ff-only origin/main`. Local main never carries unpushed
commits under per-task PRs, so this always fast-forwards.

**Base moved.** `main` is protected with strict up-to-date checks, so an open PR cannot merge after `origin/main`
moves. Per-task PRs are merged one at a time, so this only happens when a change outside the wave lands first:
`gh pr update-branch <pr>`, wait for CI, and merge.

**Wave PRs (wave 4 and earlier).** A wave PR is rebase-merged (enable it under Settings → General → Pull Requests
→ Allow rebase merging; `gh api repos/{owner}/{repo} --jq .allow_rebase_merge` must print `true`).

- Integrate: `integrator` in wave mode squash-merges each verified branch into local main, one at a time.
- Ship: when every task in the wave is integrated and the gate is green on local main, the lead runs
  `git push origin main:refs/heads/wave/<milestone>-<n>`, then
  `gh pr create --base main --head wave/<milestone>-<n> --title "<milestone> wave <n>: <summary>" --body-file
  <file>`, with one `Closes #<issue>` line per task in the body file. When CI is green, the lead merges it with
  `gh pr merge <pr> --rebase`.

- Resync (rebase-merge rewrites SHAs, so `--ff-only` fails by design): `git fetch origin && git switch main &&
  git cherry origin/main main`. Every line must start with `-` (each local commit has an equivalent patch
  upstream); only then run `git reset --hard origin/main`. Any `+` line means a local commit is not upstream: stop
  and investigate instead of resetting.
  A `+` line together with an empty `git diff origin/main main` and `gh pr view <pr> --json state` showing
  `MERGED` (for example a wave PR squash-merged by mistake) is safe to reset; anything else is not.
- Base moved: never use GitHub's "Update branch" (it moves the wave branch away from local main). Instead run
  `git fetch origin && git switch main && git rebase origin/main`, re-run `scripts/quality-gate.sh` on main, then
  `git push --force-with-lease origin main:refs/heads/wave/<milestone>-<n>`, wait for CI, and rebase-merge. If
  the rebase stops on a conflict, resolve it or run `git rebase --abort` and escalate; never push a partially
  rebased main.
- If a squash PR lands while local main carries integrated wave commits, do not run the `--ff-only` resync (it
  fails); rebase local main onto `origin/main` as in the bullet above, before pushing the wave.

## Running a wave

Waves run as an agent team (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, set in `.claude/settings.local.json`).
Scripted workflows are not used for waves: they cannot apply judgement when a step fails or a critic and an
implementer disagree, and they hide progress from the user. A wave is a planning batch and an ordered queue, not a
shipping unit: up to five tasks are in flight, and each ships as soon as it is verified.

0. **Pre-flight.** Before planning, the lead checks that local main equals `origin/main`
   (`git fetch origin && git rev-list --count origin/main..main` prints `0`) and that `sccache` is on `PATH`
   (otherwise every worktree builds its dependencies from scratch; ask the user to `brew install sccache`).
   Commands the user types with `!` may not run in the session; when a step needs the user, give them the exact
   command to run in a separate terminal, or run it from the lead with their go-ahead.
1. **Plan.** `milestone-lead` turns open issues into a wave: an ordered queue of parallel-safe issues, each labelled
   `wave:<n>`. Wave numbers are global and never reset across milestones: the next wave is the highest existing
   `wave:<n>` plus one, or 3 when no `wave:<n>` label exists (0.1.0 used waves 1 and 2 before labels existed). It
   returns one brief per issue (files, API names, ADR numbers, deps and do-not-touch lists pre-assigned). Known
   flaky tests in crates the wave touches go to the head of the queue. Decisions that constrain a later issue, and
   findings deferred to it, go in a comment on that issue. There is no task-board file in the repo.
   Planning is two passes: briefs for spec-heavy issues carry `assignments.spec`; the lead runs `spec-researcher`
   on all of them in parallel, and `milestone-lead` finalises their "Done when" from the fixtures before any
   implementer starts (wave 3: the #6 brief named the wrong vttest menu and contradicted xterm in five places, and
   8-10 lead decisions were appended mid-flight). If a researcher fails, the lead decides whether to retry it or
   finalise "Done when" from the issue text. Where two issues change the same function, the planner marks
   one upstream and one downstream and queues the downstream one to start after the upstream merges. Hot-path
   issues carry a performance bar with a named corpus measured against main with `cargo xtask ab`.
2. **Task list.** The lead session creates one shared task per issue in the wave, carrying the final brief, and
   keeps up to five in progress. When no task-list tool exists, the lead keeps a status table in its messages.
3. **Implement.** One teammate per issue claims its task and implements it in its own worktree
   (`../tessera-worktrees/wave-<n>/task-<issue>`, branch `task/<issue>-<slug>`) as `rust-implementer`,
   `swift-implementer` or `release-engineer` by area, with the spec fixture as its first test. Before reporting it
   runs the critics' standing checks on its own branch (mutation of tolerance tests, flood of per-byte events,
   fast paths against their reference) and ticks off every lead decision.
4. **Critique.** For every round the lead spawns a fresh one-shot critic subagent (`rust-critic`, `swift-critic`
   or `code-critic` by area). Rounds 1 and 2 run at the frontmatter effort, rounds 3+ with `effort: "lower"`.
   Round 1 gets the brief and the branch; rounds 2+ get a scoped brief: the previous round's findings, the
   implementer's response to each, and the commit range since the reviewed commit. The critic checks the findings
   first, then the new commits, then the rest of the diff for regressions. On `revise` the owning teammate fixes or refutes each finding. On
   `revise` with no blocker, where every major is a doc line or an item already recorded as a lead decision, the
   teammate fixes it and the lead checks the fix in the diff instead of spawning another critic; the round counts
   as `ship` and verification follows. On `escalate` the lead (or `milestone-lead`) decides and records the
   decision in the brief, under a dated `## Lead decisions (<date>)` heading, before the next round; escalations
   do not count against the cap. The loop is capped at 5 rounds.
5. **Verify.** After `ship`, the lead runs
   `scripts/verify-branch.sh <wt> --allow <brief paths>... --session <url> --coauthor <line>`
   (in the background) and the `adversarial-tester` as a one-shot subagent, in parallel. The adversarial brief
   carries the critic's `coverage` section so it does not repeat the critic's benchmarks and fuzzing.
   **Early start:** when round 1's findings contain no blocker and no major in a hot path, fast path, concurrency
   or a structural design, the lead may start the adversarial run at the same time as critique round 2 against
   the round-2 branch; if round 2 then changes code the adversarial run covered, the "Changes after `ship`" rules
   apply. Any major finding fails the gate; at most 2 verify cycles. Verify cycle 2 runs the adversarial tester
   with `effort: "lower"`, the cycle-1 repros and the fix commits, scoped to the fixes and their callers. A
   structural rewrite during a verify cycle gets a fresh critic before verification resumes.
6. **Integrate.** Verified branches go to `integrator` one at a time, first verified first integrated (a downstream
   task after its upstream). It squashes the branch into one conventional commit, rebases it onto `origin/main`,
   re-runs the gate and writes the PR body. Conflicts it resolved outside Cargo.lock go to the lead, who applies
   the "Changes after `ship`" rules.
7. **Ship.** The lead pushes the task branch (`git push -u origin task/<issue>-<slug>`), opens its PR
   (`gh pr create --base main --head task/<issue>-<slug> --title "<conventional title>" --body-file <file>`),
   and when CI is green merges it with `gh pr merge <pr> --squash --subject "<conventional title> (#<pr>)"`. Then
   it resyncs local main, removes the worktree, and starts the next queued issue. The next integration waits for
   this merge, so `origin/main` is always the base.
8. **Close out.** When the wave's queue is empty, the lead comments deferred findings on their owning issues and
   hands the wave's transcripts and `scripts/factory-stats.sh` output to `flow-analyst`, whose retro lands in
   `docs/retros/`.

**Changes after `ship`.** Fixes the lead sends after a critic's `ship` or after a verify pass are classified:

- Doc-only and test-only changes go to the teammate; the lead reads the diff. No fresh critic.
- Any non-test change to a hot path or a fast path gets a fresh critic (it may be scoped to the changed region)
  or an `adversarial-tester` run scoped to the changed region whose prompt asks for a differential fuzz against
  the reference path (`cargo xtask ab diff`), before integration.
- Other non-test code changes: the lead's judgement; a structural rewrite takes a fresh critic as in step 5.

Critics and verifiers are always one-shot subagents, never long-lived teammates, so every review starts from a
clean context.

## Changes outside a wave

Hotfixes, CI fixes and factory changes take the same loop at a smaller size, still before any push:

1. Precondition: `git fetch origin && git rev-list --count origin/main..main` prints `0`. If it does not, either
   the wave PR already merged (run the wave resync rule, then continue), or local main carries commits of an open
   wave PR (wave 4 and earlier): then branch from `origin/main` in its own worktree and name `origin/main` as the
   base in every critic brief and in `scripts/verify-branch.sh --base origin/main`. Otherwise resync and branch
   from local main in its own worktree, so every verifier's `main...HEAD` is exactly the change.
2. Implement with the agent for the area (`rust-implementer`, `swift-implementer`, `release-engineer`,
   `docs-writer`), or directly by the lead for agent definitions and docs.
3. A fresh critic for the area; revise until `ship`.
4. `scripts/verify-branch.sh`. Add `adversarial-tester` when the change touches product code (`crates/`,
   `apps/macos/`) other than tests; skip it for docs, agent definitions and test-only changes.
5. `git push -u origin <branch>`, then
   `gh pr create --base main --head <branch> --title "<conventional title>" --body-file <file>` (with
   `Closes #<issue>` where one exists). When CI is green, the lead merges it with
   `gh pr merge <pr> --squash --subject "<conventional title> (#<pr>)"`, then resyncs.

## Running the gate

Every agent that runs `scripts/quality-gate.sh` reads the real exit code: redirect output to a file and read `$?`
(`scripts/quality-gate.sh <wt> > <log> 2>&1; echo $?`). A status read through a pipe is not evidence. The same
holds for `scripts/verify-branch.sh`.

Agent definitions change only through a retro proposal or an explicit decision; record the reason in the commit
message. The `factory` project skill in `.claude/skills/factory/` points lead sessions at this document.
