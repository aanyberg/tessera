# Testing

Which tests to run while you work, how tests stay away from your home directory, and how to add an
escape-sequence test. The contributor workflow is in [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Which tests to run

Pick the cheapest check that covers the change while you work; the full gate runs once before a push.

| Stage | Command | Covers |
|---|---|---|
| Iterating | `cargo nextest run -p <crate>` | one crate |
| After a change | `just test-affected` (`--clippy` to lint too; `--base <ref>`; `-- <nextest args>`) | every crate your change touches since the merge base with `main` (committed, staged, unstaged and untracked), plus every crate that depends on them |
| Swift or `tessera-ffi` surface | `just app-test` (all suites) or `just app-test-only <scheme> <Target/Class[/test]>...` (a test, a class, a mutation run) | the XCTest suites (Xcode, Thread Sanitizer); the gate does not run them, CI does |
| Docs, scripts, CI | the matching check: `actionlint`, `just config-docs-check`; for the factory scripts `just factory-test`, `just verify-branch-test`, `just quality-gate-test`, `just perf-job-test` | no Rust test is affected |
| `shell-integration/` | `just test-affected` (selects those two crates and their dependents: `tessera-shell-integration` and `tessera-term` read it), plus `just terminfo-check` for the terminfo entry | the shell scripts and the terminfo entry |
| Before any push or PR | `scripts/quality-gate.sh <worktree>` | everything: fmt, clippy, workspace tests, rustdoc, deny, FFI build and C tests, release timing tests, fuzz fmt and clippy |

`just test-affected --list` prints the affected crates without running anything. When local `main` or `origin/main` is
stale, commits from other branches show up as changed files: `git fetch` and use `--base origin/main`. A changed file
under `crates/tessera-config/` also prints a note to run `just config-docs-check`, because the generated
`docs/config.md` can change with it and no Rust test checks that. A change to a root manifest,
`Cargo.lock`, the toolchain pin or the nextest, clippy, rustfmt or deny config affects every crate. Dependencies point
downward only, so a change in `tessera-vt` or `tessera-grid` reaches most of the workspace and a change in
`tessera-cli` only itself. The targeted run is a feedback loop, not a gate: it can miss what only the whole workspace
shows (build scripts, the FFI header, the Swift consumers).

**The app's tests run on a core with test hooks.** `just app-test` and `just app-test-only` build the xcframework from
a core with tessera-ffi's `test-hooks` feature, so that the hosted tests can call its test hooks (#650; declared in
`apps/macos/Tests/TesseraTests/CoreTestHooks.h`, never in `TesseraCore.h`). `just xcframework`, `just app` and the
release workflow build it without features, and `just archive` refuses an xcframework whose core has any (it reads
`TesseraCore.xcframework/features`). A test that needs a hook fails, naming the cause, on a host without it, for
example after a test run from Xcode on a core from `just xcframework`.

**A worktree's built app is from its last run.** `just app` and `just app-test` build into the worktree's own
`apps/macos/DerivedData`, and `just run` opens whatever is there: the app from the last build in that worktree, not
from the branch you have checked out now. Before an eye check, rebuild (`just app`) or look at the branch the
DerivedData app was built from (`git -C <worktree> branch --show-current` and the build time of
`apps/macos/DerivedData/Build/Products/Debug/Tessera.app`); a check against a stale app proves nothing about the branch.

**A hang under Thread Sanitizer is usually the fork deadlock of #278.** The session spawn forks on the main thread, and
TSan's `fork` interceptor holds the runtime's slot locks while libSystem's atfork handlers take their own locks
(corecrypto's `ccrng`, the Objective-C runtime). Another thread that holds one of those locks and then enters the
runtime (`__tsan::SlotLock`) deadlocks the host. Every other thread that touches the runtime then parks in `SlotLock`
too, the font index's `close` in `Coverage::build` among them (#443): those threads are bystanders. In the watchdog's
sample (`apps/macos/build/app-test-hang-sample.log`), look for the thread it reports as "forking" and for the thread
that holds the atfork lock. In #443 that was AppKit's `NSPersistentUI` queue (`arc4random_buf` from
`-[NSPersistentUIWindowInfo initWithWindowID:]`); other samples show a CoreSpotlight queue. On macOS 27 (CI run
44917832924) the main thread waited in libobjc's atfork handler while 71 workqueue threads waited in the runtime's
allocator (`__tsan::ProcCreate` and `__sanitizer::InternalAlloc` on an unfair lock), and the retry hung the same way;
the watchdog classifies that shape too. Terminal windows are not
restorable by AppKit (the window state store restores them), which removes the `NSPersistentUI` partner but not the
others. `scripts/app-test-watchdog.sh` kills such a host and reruns the hung test once; the run fails anyway when
another test failed in it. The watchdog does not see the same hang in a child copy of the host, so
`TerminationSignalTests` handles it itself (#525). A loaded child that is still not ready 20 s after launch is
sampled, and again every 20 s after that. If one thread spent the whole sample in `wrap_fork` with an atfork prepare
handler or `__tsan::` frames on the same stack (only the sample is read, never the child's output), the child is killed
and started once more. The deadlock never resolves, so a retry costs about 25 s and the test stays within the default
120 s allowance and the watchdog's 150 s limit. A second hang fails the test as the second try. A child that stays at
the stage "the test bundle started" for 20 s without that fork hang is also killed and started once more, and
a second such stall fails the test (#640). Under load, such a child's main thread and AppKit's
`NSPersistentUILocalStorageClient` queue had spun in TSan's `dispatch_once` for 60 s while another thread sat in the
runtime's allocator (`__tsan::MetaMap::AllocBlock`). The child now starts with `-ApplePersistence NO`, so AppKit
keeps no restorable state there. xcodebuild's `-ApplePersistenceIgnoreState YES` only moves that state to a
`<bundle id>.savedState` in the temporary directory, which every host and child of the bundle id shares. Other
partners may remain, hence the relaunch. A test that waits for
a child's output should wait for all of it and allow 30 s, so that it does not fail on its own on a loaded host. To
reproduce locally, run the affected classes under TSan with `-test-iterations 20` through the watchdog (`taskpolicy -b`
and a concurrent `cargo build`). On main that gave 1 hang in 60 iterations of `TabVisibilityTests` and
`PromptNavigationTests`, and none in 40 with terminal windows not restorable.

**A SEGV inside Thread Sanitizer's `munmap` interceptor is a runtime defect, not a test failure (#609, #660).** In CI
run 45038033580 the test host crashed about 0.1 s into `AppAppearanceTests.testEachValueReachesEveryWindowTheAppOpens`:
the `tessera-font-index` thread unmapped a font file and the runtime faulted in its own bookkeeping for the range
(`wrap_munmap` → `__tsan::UnmapShadow` → `__tsan::MetaMap::ResetRange`), then aborted on a recursive lock in its
crash handler, so the log has `ThreadSanitizer:DEADLYSIGNAL` and no stack, and only the `.ips` names the thread. The
same family as #496. It does not reproduce locally (0 of 40 runs). Each core loads its fonts on its first surface,
scanning (and fontdb mapping) every system font and starting an index build, and tests replace the core hundreds of
times a run, so the mapping churn was the exposure. Since #691 the index reads font files and maps none. The test
host also turns on `tessera_test_share_font_index` before the first test (`HostDisplaySettings`), so every core in
the process shares one scan and one index: a full run builds the index once, plus once for the #594 test that holds
it (a load while the indexes are held builds its own). Production keeps one per core. The appearance test asserts
its seven cores build at most one. A `ThreadSanitizer: SEGV on unknown address 0x0 ... in __pthread_kill` from
`TerminationSignalChild.mark` belongs to a passing test: `TerminationSignalTests`' children raise `SIGSEGV` on
purpose (#625) and the log quotes their reports. The test that failed is in the log's `Failing tests:` list.

## Hung runs: the test watchdog

Every XCTest run goes through `scripts/app-test-watchdog.sh` (`just app-test`, `just app-test-only`; `just test` wraps
nextest the same way). Never run a bare `xcodebuild test`: one ran for 9.5 hours after its test host crashed, because
launchd, not xcodebuild, parents the host. The watchdog passes the output and the exit code through and ends a run that
hangs, with exit **97** and a line starting `app-test watchdog: HANG` (or `FAILED:` for a test that outran
`TESSERA_APP_TEST_HANG_SECONDS`, 150). Three clocks, each overridable by environment variable:

| Variable | Default | Trips when |
|---|---|---|
| `TESSERA_APP_TEST_HANG_SECONDS` | 150 | a started test has had no test line for that long (xcodebuild's own allowance is 120 s), or at once when xcodebuild prints that the test `exceeded execution time allowance`: xcodebuild then samples and kills the host, so the watchdog samples it first (#644) |
| `TESSERA_APP_TEST_IDLE_SECONDS` | 900 | no output and no CPU progress (above 50 ms) in the command, its descendants or its hosts, also when the host died before any test started |
| `TESSERA_APP_TEST_HARD_SECONDS` | 5400 | the run has lasted that long (a normal suite is about 7 minutes; 5400 s leaves 7x for a loaded machine) |

It kills only the command's own process tree and the hosts under this run's `-derivedDataPath` that started after it
did, so runs in other worktrees are untouched. `just app-test-watchdog-test` checks it with fake commands.

## App tests that skip

A hosted test that cannot exercise what it checks on this host skips with the reason in its `XCTSkip` message.

- **The real-`NSSlider` tests in `SettingsTests`** (the ones that find the slider through `hostedOpacitySlider`, #564)
  skip on CI and in any local build with Xcode 27. SwiftUI's `Slider` is an `NSSlider` only in an app linked against
  the macOS 15 SDK (Xcode 16); linked against SDK 27, as CI and releases are since #584, it is SwiftUI's own control
  with no AppKit control to drive. They stay as coverage of older-SDK builds (#583). The binding tests in the same
  file drive the field's `doubleBinding` and `editingChanged` directly and cover the hold and hand-edit logic on every
  SDK. On the macOS 15 runner most of them skipped too, because its synthetic drags did not move the knob.
- **Tests that need a visible window or a full screen transition** (`FullScreenToggleTests`, `ResizeFootprintTests`)
  skip on a locked console, where nothing is drawn and no transition finishes; CI runs them.

A Rust test that cannot run here prints `SKIP <test>: <reason>` and returns; for the #555 font index renderer tests
a `success-output` override in `.config/nextest.toml` puts that line in the log (#590).

## Tests and the home directory

Tests never read or write the real home directory. Tests that create an
`App` pass it a temporary configuration root; the FFI tests, whose
`tessera_app_new` uses `~/Library/Application Support/Tessera`, point
`HOME` at a temporary home first (`crates/tessera-ffi/tests/hermetic`), and
`tests/home.rs` fails if a test file forgets or if the real
`Application Support/Tessera` changes. Changing the environment is only
sound with one test per process, so run them with `cargo nextest` (as `just
test` and the gate do), not plain `cargo test`.

The app's tests do not read the machine's Accessibility settings either:
the test bundle's principal class (`HostDisplaySettings`) pins Reduce
Motion, Reduce Transparency and Increase Contrast off before the first
test, and a test that turns one on puts the pin back in its tearDown. A
view's VoiceOver monitor is the machine's until a test replaces it, so a
test that needs a client attached sets a detached fake first. It also
turns AppKit's window open and close animations off
(`NSAutomaticWindowAnimationsEnabled`, in the volatile argument domain):
in some console states (seen locally, cause unknown) a closing window's
animation never ends and holds a dispatch worker, and once a run has held
64 the quit tests hang in `NSApp.terminate` (#637).

## Timing tests and the `timing` group

A test with a wall-clock bar (a latency, a lock hold, a ratio of two costs) joins the `timing` test group in
`.config/nextest.toml`, so nextest runs it alone and a loaded machine cannot fail it by running two of them at once.
Other work on the machine still preempts the test, so a bar on the cost of code measures the thread's CPU time
(`CLOCK_THREAD_CPUTIME_ID` through `nix::time`, see `thread_cpu_time` in the test helpers of `tessera-term` and
`tessera-app-core`) or counts work, and compares costs taken side by side, since the cores differ in speed (#520, #521).
A latency bar on a watcher, which a stall of the watcher's thread breaks whatever the group, is asserted on the fastest
of a few rounds while every round asserts the events themselves (`tests/watch_themes.rs`, #525;
`tests/profile_files.rs`, #656). The startup budget of `tessera-ffi`'s `tests/budgets.rs` is both: the calling thread's
CPU time from `tessera_app_new` to the first frame, best of five fresh processes, since that thread does about nine
tenths of the work the first frame waits for (#672). Its bar keeps #655's 19.6 ms, which is above main's 13.3 ms plus
20 % for a regression and as much again for load (19.1 ms): load still moves CPU time a little (lower clocks with every
core busy, efficiency cores). The reasoning and the measurements are beside the constant. Add an override with a
`filter` that names the package, the binary and the test, and a one-line comment that says what the bar is and which
issue set it. The groups today cover the config reload (with the profile-file bound, #656), shaping, close and writer
bounds, the DECSCUSR flood, the theme-edit bound, the search caps (including
`the_kept_copy_and_its_spares_stay_within_the_cap`, which failed at load 35 before it fixed its budget), the plugin host
throughput and the rectangle cost bars: the `b02_*` tests of `tests/rect.rs` and the two cluster bars of
`tests/rect_fixes.rs`. Release-only bars are also listed in the quality gate's release timing step (`release_timing` in
`scripts/quality-gate.sh`); a bar added to one place belongs in the other, and
`cargo nextest list --release --run-ignored all -E 'test(<name>)'` shows that the filter finds it.
`scripts/verify-branch.sh` runs the suites such an override names, in a changed package, ten times each, so a new bar is
exercised under load before it merges.

## Performance and memory bars

Memory efficiency is a first-class goal next to speed. When a brief names no bar, these are the defaults:

- **Instructions.** A hot path may not retire more than 2 % more instructions than main: `cargo xtask ab bench
  --max-ratio 1.02` on a named corpus. The 1.02 bar counts as met only when three independent probe builds (a fresh
  `ab init` each) all measure at or under 1.02 (the noise-floor rule in `docs/software-factory.md`).
- **Memory.** The live heap of an idle session, and of a session per 10 000 scrollback lines, may not grow more than
  2 % over the budgets of `crates/tessera-ffi/tests/budgets.rs` (#216), as measured on main when the change started.
- **A larger regression** needs an explicit justification in the PR description and a note in the ADR of the area (or
  a new one); raising a budget constant in the same change without that is a blocker.
- **A new hot path gets an absolute bar** (a time, an instruction count or a byte count on a named corpus) in the same
  change, so the next change has something to compare against.
- **Briefs.** `milestone-lead` puts a memory bar in every brief that touches per-session, per-line or per-cell
  storage.

These bars bind at review time. The scheduled `perf.yml` job (ADR 0049, `docs/software-factory.md`) catches what
slips past them: every other day it measures main against the last measured SHA on a fixed case set (feed, scroll,
search, `cat` throughput, rewrap, vtebench's ten streams; idle heap, full-scrollback heap, heap per plain and per mixed
scrollback line, the resident set of 1 000 000 lines; the heap cases measure a `Terminal` only, about a third of an
idle session, and session-level growth stays with the #216 budgets; the baselines are in ADR 0049) and runs the release timing bars, and a case past its bar opens or comments
on a `type:bug` issue naming the SHA range. It never gates a PR. `just perf-job-test` checks its logic offline.

What the job enforces on its GitHub runner, and what it leaves to `just check` and local runs (#682): the absolute
wall-time budgets come from a local Mac, so the startup budget (`startup_to_the_first_frame_stays_within_its_budget`,
99.7 ms on the runner against 19.6 ms) is skipped there; the other release timing tests and the #216 byte budgets run
(`--no-fail-fast`, so one red test hides none). The ratio bars (instructions at most 1.02x the baseline, heap at most
1.02x) are comparable on any machine and are enforced, except that a runner without instruction counters (a virtual
machine) cannot back an instruction bar: those cases are recorded as `noisy` with the wall ratio and a note, and only
the heap bars bind there. Logs of every measuring run are uploaded as the `perf-logs` artifact. A dispatch from a
branch measures that branch, prints the issue requests instead of filing them, and saves no state.

## Adding an escape-sequence test

Emulator tests in `crates/tessera-term/tests/` feed bytes into a fresh
`Terminal` and compare what it drew. The harness is `tests/common/mod.rs`
(scripts, rendering) and `tests/spec/mod.rs` (fixture reader).

**Scripts.** A script is one case per file under `tests/scripts/<group>/`:

```text
# Comment lines start with `#`; blank lines are skipped.
@size 25x80
@set auto_wrap false
\x1b[3;6Hab\x1b[2D\x1b[K
```

- `@size <rows>x<cols>` comes once, before anything else.
- `@set auto_wrap|insert_mode true|false` switches a mode through the
  `Terminal` API, for modes whose escape sequence is not wired up yet.
- Every other line is one payload, fed with one `advance` call. Its escapes
  are decoded by `decode`: `\xNN` (one raw byte), `\r`, `\n`, `\t`, `\\`
  and `\u{H..}` (one scalar value, UTF-8 encoded); everything else is
  literal UTF-8, and any other escape fails the test. The line break ending
  a payload is not part of it, so write `\r\n` where the stream has one.

A group's test file loads its scripts, runs each whole and byte by byte
(the two must agree), and snapshots the render (the `common` module docs
describe the screen format: `|` after the cursor's cell, `\` after a
soft-wrapped row).

**Fixtures.** Behaviour that a specification pins goes into a fixture in
`tests/fixtures/` (`csi_spec.txt`, `esctest_spec.txt` and `decstr_spec.txt`
share one layout), which `spec::parse_fixture` reads:

```text
### E_CUP_rowOnly
size: 25x80
step: feed \x1b[3;6H
step: feed \x1b[2H
source: esctest2 esctest/tests/cup.py test_CUP_RowOnly
screen:
>
> |
>                    (one `>` row per terminal row, `|` after the cursor)
cursor: (1,0)        # 0-based (row,col)
pending_wrap: false
requests: []         # or [Bell, ...]
scrollback: len=0    # or `not asserted`, or `len=N first=("text", wrapped=false)`
```

`step:` takes `feed <payload>` (decoded as above) or `set <mode> true|false`.
`cite:`, `note:`, `flags:` and `needs:` are documentation; a test file may
read extra keys itself (`attr:` cell styles in `esctest.rs`, `pen:` in
`decstr.rs`). `spec::check_all_in(fixture, select)` checks every selected
case whole and byte by byte and reports all mismatches at once.

Rules for a new case:

- It cites its source: `source:` names the test it ports (esctest file and
  function, vttest screen and capture), or `cite:` the spec section (xterm
  ctlseqs, DEC STD 070). The expected screen comes from that source or from
  an independent model of xterm, never from running tessera.
- A case tessera cannot pass yet is not a skipped test: write it as a `#>`
  comment block that names the issue that will make it pass
  (`#> E_HPR_defaultParams -> #93 ...`). The PR that fixes the issue turns
  the block into a case.
- A bug a case reveals is not fixed in the test PR: record it as a `#>`
  block with a new issue number.

Fixtures are generated by the `spec-researcher` agent with throwaway tools
(an xterm model in Python, a pty capture of vttest); the tools are not
committed. To change a case, edit the fixture by hand and say where the new
expectation comes from, or regenerate the section with the researcher and
commit only the generated fixture.

**Snapshots.** Run the tests; a new or changed render is written as a
`.snap.new` file next to the snapshots in `tests/snapshots/`. Review and
accept it with `cargo insta review` (from `cargo install cargo-insta`), or
without that tool with
`INSTA_UPDATE=always cargo nextest run -p tessera-term --test <file>`
(scope it to the test file: `always` also rewrites snapshots of tests that
fail for another reason), then read the new `.snap` files and check
`git status` before committing.
