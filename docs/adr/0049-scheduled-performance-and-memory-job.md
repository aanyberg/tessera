# ADR 0049: A scheduled performance and memory job

**Status:** accepted · 2026-11-25

## Context

CI runs `cargo nextest run --workspace` in debug. The release-only timing bars and the `cargo xtask ab` probes run
only in the local gate and in factory review, so a speed or memory regression that skips them, or lands in code that
has no bar, ships unnoticed (#612). Memory is a first-class goal next to speed (`docs/testing.md`, Performance and
memory bars), and #216 added budgets for the heap of a session that nothing checks after the day they were set.

## Decision

**D1. Cadence.** `.github/workflows/perf.yml` runs on cron `0 5 */2 * *` (odd days of the month; two runs fall on
consecutive days across a 31-day month's end) and on `workflow_dispatch`. Its first step ends the run early, with the
rest skipped, when main's head is the SHA the last run measured, so a quiet week costs a checkout and a cache
restore. A dispatch input `force` bypasses that. It runs only on `main`.

**D2. Case set.** Two kinds, both on the `xcode-27` runner so instructions retired (`/usr/bin/time -l`) exist.
- The gate's release timing step, whose packages and filter `scripts/perf-job.sh` reads out of
  `scripts/quality-gate.sh`; the #216 budget tests are in it. One list, two users.
- The fixed set in `scripts/perf-cases.rs`, run through `cargo xtask ab` with the last measured SHA as base: `feed`
  (parse and print, no scrollback), `scroll` (into a scrollback of 10 000 lines), `search` (literal and regex over
  it) as instructions; `heap_idle` and `heap_scrollback` as live heap bytes of a `Terminal`, counted by the probe's
  allocator. The corpus is generated from a fixed seed. Changing it moves every number once, so it is a trend
  break to note in the PR that changes it.

**D3. Bars.** Instructions head/base at most 1.02 in at least one of three independent probe builds (the noise-floor
rule of the factory: a probe build alone moves the base by about 1 %), so a case is rebuilt only while it is over.
Heap at most 1.02 of the baseline's. A `problem:` line from `ab` (case too small for the machine's load, checksums
differ) makes the line `noisy`, never `regressed`.

**D4. Baseline.** The newest SHA none of whose lines is `regressed`. A regression that stays on main is compared to
the last good SHA on every run instead of becoming the new normal. When the case set does not build at that SHA (an
API of the crates changed), the run measures head against itself and says so in the line's `note`.

**D5. Trend format.** One JSON object per line and case in `perf.jsonl`: `ts`, `sha`, `base`, `case`,
`instructions`, `base_instructions`, `ratio`, `heap_bytes`, `base_heap_bytes`, `bar`, `status` (`ok`, `regressed`,
`noisy`), `note`. The numbers of the other kind are `null`. The file is uploaded as the `perf-trend` artifact of every
measuring run, so a trend across runs is `jq` over the artifacts.

**D6. State.** The file is carried between runs in the Actions cache (`perf-state-<run id>`, restored by prefix, saved
only by a run whose steps all passed), not read back from the previous run's artifact: downloading another run's
artifact needs `actions: read`, and D7 forbids it. A cache lost to eviction (unused for seven days) makes the next
run a baseline run, one comparison missed.

**D7. Token.** `contents: read` and `issues: write`, at the workflow level and nothing else; the workflow runs no
code from a pull request and has no secrets beyond `GITHUB_TOKEN`. A case past its bar, or a failed timing step,
opens an issue labelled `type:bug` titled "perf: the scheduled performance job found a regression", or comments on
the open one; the body names `git log <base>..<head>` and the run.

**D8. Why it does not gate PRs.** A 2 % instruction bar and a timing bar are noisy on a shared hosted runner, a
release build with a full probe build costs far more than a PR's CI budget, and a gate that fails at random is
ignored. The PR-time bars stay with `cargo xtask ab` in the implementer's and the critic's hands (the policy in
`docs/testing.md`); this job catches what skipped them and gives a bisectable range of at most two days of merges.

## Consequences

- A regression surfaces within two days, with a SHA range, instead of at release.
- The case set is a second place that uses the crates' public API; an API change in `tessera-term`, `-grid` or
  `-search` that breaks `scripts/perf-cases.rs` fails the job's case-set step (an issue is filed); a local `just perf-job <dir>` shows
  it first. The self-test stubs the probe, so it does not catch this.
- Hosted-runner variance can still produce a false `regressed` line (three builds make it unlikely, not impossible);
  such an issue is closed with the evidence.
