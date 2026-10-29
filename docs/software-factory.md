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
| `rust-implementer` | One Rust issue in its own worktree, gate green; fixes blockers only on `revise` | opus | high | yes | branch |
| `swift-implementer` | One macOS app issue in its own worktree; fixes blockers only on `revise` | opus | high | yes | branch |
| `rust-critic` | One strict blocker/minor review of crates/ branches, then ship/revise/escalate; owns the "Done when" judgement | opus | high (scoped round 2: medium) | no | no |
| `swift-critic` | One strict blocker/minor review of apps/macos branches, then ship/revise/escalate; Swift 6 concurrency, AppKit, FFI ownership | opus | high (scoped round 2: medium) | no | no |
| `code-critic` | One strict blocker/minor review of infra and docs branches (workflows, justfile, scripts, ADRs) | sonnet | medium (scoped round 2: low) | no | no |
| `adversarial-tester` | Breaks ship-ready branches with hostile input and real flows; reproduced bugs only | opus | high (risky changes: xhigh; repro re-run: medium) | scratch only | no |
| `integrator` | Serialized: one conventional commit on `origin/main` per verified task, lockfile conflicts, PR body (wave mode: squash into local main) | sonnet | medium | conflicts only | task branch / main |
| `docs-writer` | ADRs, CONTRIBUTING, README, generated references after merges | sonnet | medium | docs | branch |
| `release-engineer` | CI/release workflows, justfile, packaging, tag/version guards | sonnet | medium | infra | branch |
| `flow-analyst` | Post-wave retro from journals and `scripts/factory-stats.sh`: cost/value per role, proposed changes | sonnet | high | retro only | no |

Model aliases map to the current Claude 5 family (`fable` = claude-fable-5-1, `opus` = claude-opus-5-5,
`sonnet` = claude-sonnet-5-5, `haiku` = claude-haiku-5-5). Fable is not used (token cost); planning, implementation
and critique of product code, and adversarial testing run on Opus (Sonnet per spawn for low-risk changes); infra and
docs review and release engineering run on Sonnet. Pin a full id in the frontmatter if a role must not follow alias updates.

**Effort by round.** Round 1 and adversarial cycle 1 run at the frontmatter effort: they find the defects (wave 3:
6 majors in round 1, 2 in round 2 from benchmarks and differential fuzzing, none in round 3; both verify failures in cycle 1; waves 5 and 6: critics found
most serious defects in round 1, which is why round 1 must run those benchmarks and diff-fuzzes itself). The only other critic round, the scoped round 2 (below), is spawned with the Agent
tool's `effort: "lower"`, one step below the frontmatter, because it checks a fix range rather than a branch.
Adversarial cycle 2 (only after a cycle-1 failure) is spawned the same way. Any defect a lower-effort run misses that
a later gate catches goes into the next retro; that is the signal to revert. `code-critic` and `release-engineer` run
at medium by the user's decision of 2026-10-28 (no retro data yet: neither ran in wave 3), and on Sonnet from
2026-10-29 to cut token cost; revisit in the next retro that has runs of either.

**Adversarial cost levers (2026-10-29).** The frontmatter is `high` with `maxTurns: 80`. The lead raises a cycle-1
run with the Agent tool's `effort: "higher"` (xhigh) when the change is risky: concurrency, FFI or render path, a
hot or fast path, parser or pty input handling, or user-visible emulation behaviour (any one of these is risky; a
change is low-risk only when it meets all of the low-risk list under "Review rules"). It spawns the run with
`model: "sonnet"` for low-risk changes. Cycle 2 is `effort: "lower"` (medium) on the cycle-1 model. If a later gate
or a release
finds a bug an `adversarial-tester` run at the lower setting should have caught, record it in the next retro and
revert that setting.

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
                                                        ▼
                      {rust|swift|code}-critic, round 1: one strict full review, BLOCKER / MINOR ── escalate ──► lead decides
                                      │ revise                                          │ ship
                                      ▼                                                 │
                  same implementer: one fix commit, blockers only, no new behaviour     │
                                      │                                                 │
                  new behaviour / new code paths, or a design blocker?                  │
                      no │            │ yes                                             │
                         │            ▼                                                 │
                         │   critic round 2: fix range only, effort lower, the last     │
                         │   round; blockers left ──► lead (escalate), never round 3    │
                         ▼            ▼                                                 ▼
                      verify-branch.sh ∥ adversarial-tester (once; product code only; low-risk: may start with round 1)
                                                        │ pass (fail: fix, then one scoped re-run on the repro)
                                                        ▼
                                 integrator (serialized): one commit on origin/main
                                                        │
                                                        ▼
                         lead pushes task branch, one PR per task, squash-merge, resync
                         (the round's minors are already filed as one follow-up issue, linked in the PR body)
                                                        │ merged
                                                        ▼
                                  next queued issue starts; after the wave, flow-analyst retro
```

- The critic is chosen by area: `rust-critic` for crates/, `swift-critic` for apps/macos, `code-critic` for infra and docs.
- Verification is two lenses plus a gate: `scripts/verify-branch.sh` (mechanical), `adversarial-tester` (judgement)
  and the quality gate. The critic owns "Done when"; the adversarial tester runs at xhigh for risky changes in cycle 1
  (see "Adversarial cost levers") because it found 7 of the 9 post-ship majors in wave 1, both verify failures in
  wave 3, and the bugs the critics missed in waves 5 and 6.
- Any blocker from any gate fails that gate. Minors never fail a gate.
- Every branch merges current main before critique and before verification so verifiers never report stale-base
  problems as findings.

## Review rules

The review loop was "up to 5 critique rounds and 2 verify cycles". In waves 5 and 6 the cost came from the rounds
that fix commits caused by adding new code: #100 needed 3 critic rounds because its fixes added a poll thread and
lazy config creation, and each of those rounds re-reviewed code the previous round had not seen. Meanwhile the
critics found most serious defects in round 1, and the adversarial tester found what they missed (DECCOLM width
desync in #8, a racy paste test in #74, HFS+ edit loss in #100). The rules below keep the strict first look and the
adversarial run, and replace the extra rounds with mechanical checks.

| | Before | After |
|---|---|---|
| Critic rounds | up to 5, each re-reviewing the new commits | round 1 full and strict; round 2 only if a fix added new behaviour or code paths, or a blocker was a design problem; round 2 is the last |
| Finding severity | blocker / major / minor; any major blocked `ship` | BLOCKER or MINOR only; only blockers block |
| Fix commits | fix or refute every finding, may widen as needed | blockers only, one commit, no new behaviour |
| Minors | fixed in later rounds, or ad hoc | one follow-up issue labelled for the lowest wave that has not started, does not hold the merge |
| Round 3+ | spawned at `effort: "lower"` | none: remaining blockers go to the lead (`escalate`) |
| Re-checking fixes | another critic round | `scripts/verify-branch.sh` with its load targets, the quality gate, and the mutation check the implementer reports for each fixed blocker |
| Adversarial tester | cycle 1, up to 2 cycles; early start with critique round 2 | once after `ship` for product code; a second cycle only if the first fails, scoped to the repro; docs, infra, tooling and test-only changes exempt |
| Critic and adversarial run | sequential, with an early-start exception | in parallel for low-risk product changes (below) |

**Round 1: one strict, full review.** The critic reads the whole diff, runs its checks, and sorts every finding:

- BLOCKER: wrong behaviour; a spec or "Done when" miss; data loss or a safety problem; a flaky or hanging test; an
  unmet performance bar; a change that breaks a sibling task.
- MINOR: docs, wording, test gaps for non-critical behaviour, polish.

The verdict is `ship` (no blockers), `revise` (blockers) or `escalate` (a blocker that cannot be fixed inside the
task's scope; unchanged). The report ends with `coverage`: the checks the critic ran (benchmarks, fuzz, mutations,
builds, with counts) and the ones it only read. The adversarial brief carries it.

**The fix commit.** On `revise` the implementer fixes the blockers only, in one `fix(<scope>):` commit, and adds no
behaviour in it: no new public item, thread, I/O path, state or option. For every fixed blocker the report names the
mutation check: the test that fails without the fix. A blocker that cannot be fixed without new behaviour is
reported as a design problem instead of being widened silently; the lead authorises the wider fix and records it as
a lead decision, and round 2 then reviews it. A blocker the implementer refutes or defers is a disagreement with the
critic: the lead (or `milestone-lead`) reads both arguments and the spec, decides, and records the decision under
`## Lead decisions (<date>)`; or the lead spawns round 2 for it. The report also lists the minors it saw.

**Minors.** The lead files one GitHub issue per task with every minor of the round (file, line, what, suggested fix),
in the task's milestone and labelled for the lowest wave that has not started (`wave:<m>`; labels may already exist
well ahead of the running wave, and the next wave number is never the highest label plus one for this), and links it
in the PR body, so the issue is filed before the integrator writes the body. `milestone-lead` pulls open issues
already labelled for the wave it plans into that wave and writes their briefs. The issue does not hold the merge.
The lead may order one specific minor fixed now when it is a cheap docs or test fix; it goes into the same fix
commit, or, when there is no fix commit because the critic shipped, it follows "Changes after `ship`".

**Round 2 (rare, scoped, last).** A second critic round runs only if the fix commit added new behaviour or new code
paths (a safety net: the fix commit is not allowed to), or a blocker was a design problem. It is a fresh one-shot
critic spawned with `effort: "lower"`. The brief
carries round 1's blockers, the implementer's response to each, and the fix range; the critic checks the blockers
first, then reviews the fix range in full, and does not re-review code outside it unless the fix touches it. It is
the last round and never returns `revise`: every blocker it reports, old or new, comes back as `escalate`, and
there is no round 3. The lead decides and records it in the brief under `## Lead decisions (<date>)`; the teammate
applies the decision, the lead reads the diff, and the mechanical checks and, for product code, the adversarial run
cover the result. That clears the PR gate.

**Mechanical checks, not rounds.** Everything else about a fix is checked by scripts and by the lead's reading:
`scripts/verify-branch.sh` with its load targets (timing-sensitive tests under load) and `scripts/quality-gate.sh`
are scripts; the mutation check per fixed blocker is the implementer's report, and the lead re-runs each named test
against the fix's parent (`git checkout <fix>^ -- <source files>`, run, then `git checkout HEAD -- <source files>`; only once the teammate has stopped working) before accepting it. A fix commit that adds only a test or a doc line gets no critic: the lead reads the diff.

**Adversarial tester, once.** After `ship` the `adversarial-tester` runs once for product code (`crates/`,
`apps/macos/`). A second cycle runs only if the first failed (a reproduced blocker), at `effort: "lower"`, scoped to
the original repro, the fix commits and their callers. If cycle 2 fails, the lead decides (fix, descope or drop the
task) and records it in the brief; there is no cycle 3. Docs, infra, tooling (`scripts/`, `justfile`, `xtask`,
workflows) and test-only changes are exempt.

**Low-risk product changes: critic and adversarial in parallel.** The lead may start the adversarial run at the same
time as round 1 when the change is low-risk, meaning all of:

- small: a handful of non-test lines in a few files (roughly 150 changed lines or fewer);
- local: inside one module or crate, no public API change that another crate or task consumes;
- no concurrency: no thread, channel, lock, atomic, async, signal or Drop-order logic;
- no FFI or render path: not `tessera-ffi`, `tessera-render`, the TesseraCore wrapper or frame pacing;
- no user-visible behaviour: no change to what the terminal emulates, draws, accepts as input or reads from
  configuration;
- no hot or fast path.

With no critic `coverage` yet, the tester runs its full plan. If the critic then asks for a change to product code,
the adversarial run is repeated only on the new range (the fix commit), not on the whole branch, and that repeat is
not the second cycle. If the critic ships unchanged, the parallel run is the run.

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
- **Fuzzing.** `fuzz/` holds four cargo-fuzz (libFuzzer) targets outside the workspace: `parser` (raw bytes
  into `tessera_vt::Parser`; no panic, CSI sub-parameter masks and DCS framing consistent), `terminal` (raw bytes
  into `Terminal::advance` at a size taken from the first two bytes; `Grid::check_invariants` after every chunk,
  at most 64 full checks per input), `chunking` (whole versus chunked input through `Terminal` give the same
  screen, cursor, modes and hidden state) and `resize` (`0xFE rows cols` in the input calls `Terminal::resize`,
  invariants checked around it). They need a nightly toolchain and cargo-fuzz, installed once with
  `rustup toolchain install nightly --profile minimal` and `cargo install cargo-fuzz --locked` (stable stays the
  default). `just fuzz <target> <minutes>` runs one target (default `terminal`, 5 minutes) on its corpus in
  `fuzz/corpus/<target>` plus the checked-in seeds in `fuzz/seeds/<target>`; a crash lands in
  `fuzz/artifacts/<target>`, and `cargo +nightly fuzz tmin <target> <file>` minimises it. `fuzz/Cargo.lock` is
  not committed: the gate, `just fuzz` and the workflow copy the workspace's `Cargo.lock` in before building, so
  the fuzzers test the workspace's dependency versions. The gate format-checks and clippies `fuzz/` on stable and
  builds the targets when nightly and cargo-fuzz are installed. `.github/workflows/fuzz.yml` runs each target for
  5 minutes every night (03:00 UTC, or on demand with a length) on a pinned nightly (bumping it is described in
  the workflow), with the corpus kept in the Actions cache, after replaying `fuzz/regressions/<target>/`. A crash
  is filed as an issue with the minimised input attached and the input is added under
  `fuzz/regressions/<target>/`; fix it in a branch that owns the crate. `cargo deny` does not cover `fuzz/`, and
  `libfuzzer-sys` is `(MIT OR Apache-2.0) AND NCSA`, a license outside `deny.toml`'s allow list: accepted by the
  lead (2026-10-29) because the fuzzers are a development tool that is never distributed.
- **Wave stats.** `scripts/factory-stats.sh --since <t> --until <t> <session dir>` prints the per-role and
  per-agent cost tables (output tokens, cache reads, tool calls, span and active minutes) for the retro.
- **Scratch directories.** Every subagent works in its own `<scratchpad>/<agent type>-<issue>`; the scratchpad
  is shared, and a probe file left in a worktree under review becomes a false finding.

## The PR gate

A branch is pushed and a pull request opened only after the whole local pipeline has passed: critic `ship` (round 1,
or the fix commit after a `revise` that "Review rules" clears without a second round, or, after a round-2 `escalate`,
the recorded lead decision applied and checked as in "Review rules"), `scripts/verify-branch.sh` pass (including
its load check of timing-sensitive tests), `adversarial-tester` pass for product code (`crates/`, `apps/macos/`;
docs, infra, tooling and test-only changes are exempt), and a green `scripts/quality-gate.sh` on the integrated
commit. CI is a confirmation, not a
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

**Session hygiene.** The lead's context grows with every teammate report. Teammates and subagents return a verdict, the
blockers and a path or issue link, not the full analysis; detail goes into a file or the GitHub issue. Between waves,
all state that matters is on GitHub (`wave:<n>` labels, merged PRs, cross-issue decisions and deferred findings in
issue comments), so a lead session can end after a wave and a fresh one resumes by listing the milestone's open
issues and `wave:<n>` labels and re-running pre-flight. Mid-wave state is not: the final briefs, the lead decisions
appended to them and each task's round or cycle live in the lead session's task list, so do not replace the lead
while tasks are in flight. Stop idle teammates, and `/clear` between unrelated tasks.

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
4. **Critique.** The lead spawns a fresh one-shot critic (`rust-critic`, `swift-critic` or `code-critic` by area)
   at the frontmatter effort for round 1, with the brief and the branch. It reviews once, strictly, and sorts every
   finding into BLOCKER or MINOR (see "Review rules"). On `revise` the owning teammate fixes the blockers only, in
   one fix commit with no new behaviour, and reports the mutation check for each. The lead files the round's minors
   as one issue, in the milestone and labelled for the lowest wave that has not started; it does not hold the merge,
   and the lead may order a cheap docs or test minor fixed in the same fix commit. The lead reads the fix commit and
   re-runs each named mutation check against the fix's parent. A blocker the implementer refutes or defers is
   decided by the lead (or `milestone-lead`) and recorded like an escalation. A second critic round runs only if the fix added new
   behaviour or new code paths, or a blocker was a design problem: a fresh critic at `effort: "lower"`, scoped to the
   fix range, and the last round. It never returns `revise`: every blocker it reports comes back as `escalate`, and
   there is no round 3. On `escalate` the lead
   (or `milestone-lead`) decides and records the decision in the brief, under a dated
   `## Lead decisions (<date>)` heading; the teammate applies it in the fix commit, and the round-2 rule above
   decides whether another critic looks.
5. **Verify.** After `ship` (or after the fix commit that clears round 1), the lead runs
   `scripts/verify-branch.sh <wt> --allow <brief paths>... --session <url> --coauthor <line>`
   (in the background, with its load targets) and, for product code, the `adversarial-tester` as a one-shot
   subagent, in parallel. The adversarial brief carries the critic's `coverage` section so it does not repeat the
   critic's benchmarks and fuzzing. The adversarial tester runs once; docs, infra, tooling and test-only changes skip
   it. For a low-risk product change (see "Review rules") the lead may start it at the same time as critique round 1;
   if the critic then asks for a change, the run is repeated only on the new range. A blocker from either lens
   fails the gate. Only then does a second adversarial cycle run: `effort: "lower"`, the cycle-1 repros and the fix
   commits, scoped to the fixes and their callers; if it fails too, the lead decides and there is no cycle 3. A structural rewrite during a verify cycle gets a fresh critic
   (the lead's judgement; it is not round 2 and does not permit a round 3) before verification resumes.
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

- Doc-only and test-only changes go to the teammate; the lead reads the diff. No fresh critic, no adversarial run.
- Any non-test change to a hot path or a fast path gets a fresh critic (it may be scoped to the changed region)
  or an `adversarial-tester` run scoped to the changed region whose prompt asks for a differential fuzz against
  the reference path (`cargo xtask ab diff`), before integration.
- Other non-test code changes: the lead's judgement; a structural rewrite takes a fresh critic as in step 5. A change
  after `ship` or after a verify pass is not a critic round.

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
3. A fresh critic for the area, one strict round under "Review rules": the implementer fixes the blockers in one
   commit, and a second scoped round runs only on the conditions listed there. Minors go to one follow-up issue.
4. `scripts/verify-branch.sh`. Add `adversarial-tester`, once, when the change touches product code (`crates/`,
   `apps/macos/`) other than tests; skip it for docs, infra, tooling, agent definitions and test-only changes.
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
