# ADR 0049: A scheduled performance and memory job

**Status:** accepted · 2026-11-25 · amended 2026-11-27 (#51: accepted steps, the baselines after packed scrollback)

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
  allocator. #51 added `cat`, `rewrap` and `vtebench` (instructions), `heap_lines_plain` and `heap_lines_mixed` (heap)
  and `rss_1m` (resident bytes); see "Case set of #51" below. The corpus is generated from a fixed seed. Changing it moves every number once, so it is a trend
  break to note in the PR that changes it.

**D3. Bars.** Instructions head/base at most 1.02 in at least one of three independent probe builds (the noise-floor
rule of the factory: a probe build alone moves the base by about 1 %), so a case is rebuilt only while it is over. The
job clears a case when any one build is under the bar and flags it only when all three are over; this is the reverse
of the review rule (docs/software-factory.md, `cargo xtask ab`), under which a bar tighter than 1.02 counts as met
only when all three builds are under it. Review proves a change is fast enough; the job only reports a change it is
sure of. Heap at most 1.02 of the baseline's. A `problem:` line from `ab` (case too small against the process
overhead, checksums differ) makes the line `noisy`, never `regressed`.

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

## Case set of #51 (2026-11-26)

#51 (scrollback memory) added six cases, a trend break for nothing older: the five cases above keep their corpora.

- `cat`: 8 MiB of plain ASCII source-like lines (0 to 119 characters, about a third wrap at 80 columns; 256 KiB of
  lines repeated) fed in 4 KiB reads into a 24x80 terminal with the default 10 000 lines of scrollback.
- `rewrap`: the `scroll` corpus filling the default scrollback, then twenty width changes between 80 and 100 columns.
- `vtebench`: the ten benchmarks of vtebench 0.3.1 (alacritty) at 80x24, each repeated to vtebench's 1 MiB, after a
  RIS, once each. The shell scripts are reproduced in Rust byte for byte (checked against the scripts' own output with
  `onlcr` applied, FNV-1a equal for all ten), so the job needs neither vtebench nor its scripts. vtebench itself
  installs (`cargo install vtebench --version 0.3.1 --locked`) and runs headless under `tessera-devtool run`, which
  drains the PTY into a `Terminal`; its numbers are wall times of a write into the PTY, so they are recorded below
  once, not run by the job (a hosted runner's wall time backs no bar, D8).
- `heap_lines_plain`, `heap_lines_mixed`: the heap of 10 000 lines of scrollback, full 80-column lines of printable
  ASCII, or of coloured words (8- and 256-colour SGR, bold), CJK, a combining sequence and an emoji; the difference
  between the same terminal fed the same lines with the default scrollback and with none. Divide by 10 000 for the
  bytes per line that #51's bar (256 bytes per plain 80-column cold line) is measured in.
- `rss_1m`: a terminal holding 1 000 000 lines of plain 80-column scrollback, the lines generated as they are fed.
  The job runs the probe under `/usr/bin/time -l` and records the peak resident set minus that of the probe's empty
  case in the heap fields (bar as for heap: at most 1.02x). The highest of three runs per side counts: the system can
  only take pages away from a run, and one reading of the job's first local run came out 1.4 % low.

Baselines at `6ad4b0c0` (main when #51 started), measured with `cargo xtask ab` and the probe on the M-series Mac of
the factory (release build, best of 5 pairs; the 1-minute load beside each number):

| case | measure | baseline | load |
| --- | --- | --- | --- |
| `cat` | instructions | 2 248 343 201 (about 267 per byte) | 13.5 |
| `cat` | wall, best | 79.9 ms for 8 389 504 bytes, about 105 MB/s | 13.5 |
| `vtebench` | instructions | 3 779 487 031 | 13.5 |
| `vtebench` | wall, best | 199.8 ms for 11 071 011 bytes, about 55 MB/s | 13.5 |
| `rewrap` | instructions | 1 814 612 296 (about 73 000 000 per re-wrap of 10 024 lines) | 13.5 |
| `rewrap` | wall, best | 91.5 ms; net of the fill (about 17 ms) 3.7 ms per re-wrap, 0.37 ms per 1 000 lines of this corpus | 13.5 |
| `feed` | instructions | 1 464 526 932 | 12.8 |
| `scroll` | instructions | 1 007 668 964 (about 25 000 per line) | 12.8 |
| `search` | instructions | 417 843 017 | 12.8 |
| `heap_idle` | heap bytes | 40 304 | 12.1 |
| `heap_scrollback` | heap bytes | 17 256 611 | 12.1 |
| `heap_lines_plain` | heap bytes | 16 640 000: **1 664 bytes per plain 80-column line** | 12.1 |
| `heap_lines_mixed` | heap bytes | 17 809 034: 1 781 bytes per mixed line | 12.1 |
| `rss_1m` | resident bytes | 1 894 825 984 (1 807 MiB, 1 895 bytes per line; three runs per side within 0.3 %) | 11.2-11.8 |

The case's instruction counts include generating its corpus: 10.6 M for `cat`, 110 M for `vtebench`, 27 M for the
`scroll` corpus of `rewrap`. The heap cases count requested bytes; `rss_1m` shows what the allocator rounds them to
(1 600 requested bytes of cells take a 1 792-byte block on macOS, see `crates/tessera-grid/src/scrollback.rs`).

vtebench 0.3.1 run headless (`tessera-devtool run 'vtebench -s --max-secs 3 --dat …' --cols 80 --rows 24`, release
build, load 12.8 at the start and 12.1 at the end): median milliseconds per sample of at least 1 MiB and the
throughput that makes.

| benchmark | samples | median ms | MB/s |
| --- | --- | --- | --- |
| cursor_motion | 114 | 17 | 75.0 |
| dense_cells | 364 | 9 | 149.8 |
| light_cells | 313 | 8 | 131.2 |
| scrolling | 49 | 61 | 17.2 |
| scrolling_bottom_region | 45 | 49 | 21.4 |
| scrolling_bottom_small_region | 39 | 57 | 18.4 |
| scrolling_fullscreen | 207 | 13 | 80.7 |
| scrolling_top_region | 56 | 51 | 20.6 |
| scrolling_top_small_region | 57 | 49 | 21.4 |
| unicode | 202 | 13 | 85.1 |

## Amendment: accepted steps and the baselines after packed scrollback (#51, 2026-11-27)

Packed scrollback (ADR 0051) trades instructions for memory on purpose: `scroll` 1.256, `cat` 1.167, `search` 1.263
and `rewrap` 1.718 of the phase 1 baseline, for 11x less heap per history line. Under D3 and D4 alone the first run
after it would be `regressed` on four cases, and because D4 never makes a regressed SHA the baseline, so would every
run after it.

**D9. Accepted steps.** `scripts/perf-steps.txt` holds one line per accepted step: `<case> <bar> <reason>`. A line
that the measured head has and the baseline has not (`git show <sha>:scripts/perf-steps.txt`, compared line by line)
raises that case's bar to `<bar>` for that comparison; the line's `bar` field and its `note` say so, and the issue
body lists each case's bar. The first run whose cases are all `ok` becomes the baseline (D4); it has the line, so the
case is back at 1.02 from the next run on. A step is added only for a regression accepted under `docs/testing.md`,
with its justification in the PR and the area's ADR; lines are never edited or removed. While the baseline lags (a
regression elsewhere in the range), the step keeps applying, so it can hide up to its own margin of another change to
that case in the same range; the bar is the measured ratio plus the 2 % noise margin, which keeps that margin small.

The steps of #51 are `scroll` 1.282, `cat` 1.191, `search` 1.289 and `rewrap` 1.753 (ADR 0051 D7, D8); `feed`, the
heap cases and `rss_1m` keep 1.02. After #51 the memory cases measure (the instruction baselines are the job's first
run past it):

| case | measure | before | after |
| --- | --- | --- | --- |
| `heap_lines_plain` | heap bytes per line | 1 664 | 151 |
| `heap_lines_mixed` | heap bytes per line | 1 781 | 319 |
| `rss_1m` | resident bytes per line | 1 895 | about 159 (11.9x lower) |
