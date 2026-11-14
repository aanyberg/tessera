# ADR 0039: Command marks and prompt navigation

**Status:** accepted · 2026-11-07 · amended 2026-11-08 (#45b: the
exit-status gutter, its hover and a yield between scan holds; see
"Amendment: #45b") · amended 2026-11-12 (#377, #378 M3: a whole-screen
clear retires the records it erased; see "Amendment: #377" at the end) ·
amended 2026-11-12 (#392: the capture-only cost is accepted; see "Amendment:
capture-only cost")

## Context

Issue #45a asks for ⌘↑ / ⌘↓ to jump between the shell's prompts, and for
Select Last Output, by the OSC 133 marks Tessera's shell integration already
sends (ADR 0022: `A` prompt start, `B` command start, `C` output start, `D`
command end with its exit status). The grid already keeps those marks on the
lines they were set on, through scrolling and re-wrapping. The exit-status
gutter that reads the same marks is #45b.

The behaviour is pinned by the spec-researcher's fixture
(`crates/tessera-app-core/tests/fixtures/prompts_spec.txt`, 84 rows from
iTerm2, kitty, WezTerm, Ghostty and Terminal.app). The lead adopted its
decisions with wave-13 D4, D8 and D14. The rows are tests in
`crates/tessera-term/tests/prompt_marks.rs` (layer T),
`crates/tessera-app-core/src/commands.rs` (layer R, fake clock) and
`crates/tessera-app-core/tests/prompts.rs` (layers N, S and E).

Marks come from untrusted output: a `cat`ed file can hold 100 000 of them
(security rules S5 and S6). The history holds up to 10 000 000 lines, and
the terminal lock is shared with the PTY reader and the frame capture.

## Decision

**Navigation and selection read the grid.** Previous / Next Prompt and
Select Last Output scan the lines' marks. They do not use a separate index,
so they survive re-wraps, eviction and the record ring's cap. A soft-wrapped
prompt is one stop, and so is a prompt redrawn on the same row. `A;k=s` (and
`k=c`, `k=r`) set no mark, so continuation prompts never stop navigation.

- **Previous Prompt** puts the nearest prompt above the view's top row at the
  top. At the oldest prompt it stops (`false`).
- **Next Prompt** puts the nearest prompt below the top row at the top, or
  returns to the live screen after the newest one (D-NEXT-END).
- **Fallback.** When the scrollback holds no prompt mark (a shell without
  integration, or output from before it loaded), the commands page like Page
  Up and Page Down instead (D4, D-FALLBACK). A mark on the live screen alone
  does not count, so ⌘↑ can still page into history from before the
  integration. Once the scrollback holds a mark, the history above the
  oldest prompt is reached with Page Up or Home, not ⌘↑, as in kitty. A
  session whose shell never reported a prompt (`A`) pages at once, without
  a scan, however long its history; the session keeps that one flag.
  In fallback mode the scroller flashes only when the view moved: ⌘↑ at the
  top of the history and ⌘↓ at the live screen no longer flash it (Page Up
  and Page Down from the menu still do).
- **Alternate screen.** Nothing moves there.

**Select Last Output** selects the output of the last *finished* command:
from its `C` line to the line before the next prompt, without trailing
blank lines, ending at the last non-blank cell. That end matters for zsh's
`PROMPT_SP`, which pads `foo%` into the prompt's row. The commands skip a
running command (D-RUNNING). The selection is an ordinary one, and the view
scrolls to show its start (D-SELECT-SCROLL). It returns `false`, and keeps
the earlier selection, in these cases:

- the command printed nothing (D-EMPTY-OUTPUT);
- its first output line has left the history (D-EVICTED-OUTPUT);
- there are no marks;
- the alternate screen is shown.

The look reads whole rows and the marks' order on each, which has two known
edge cases. A row that carries an `A` before a `C` (the shell sent `C` on
the prompt's own row) starts the output there, so the selection includes
the prompt and command line. And with bash without `PROMPT_SP`, a last
output line with no trailing newline shares the next prompt's row and is
left out of the selection.

**The ⌘↑ rule (N1).** ⌘↑ and ⌘↓ stay the default chords of `scroll_page_up`
and `scroll_page_down` in the registry (ADR 0036). `prompt_previous` and
`prompt_next` have no chord of their own. When the dispatcher sees the
registry default chord unchanged by `[keys]` (`Keymap::prompt_rule`), it runs
the prompt command; the fallback keeps paging where there are no marks. A
user's own binding of ⌘↑ is never upgraded. The menu items Previous Prompt,
Next Prompt and Select Last Output (View) have no key equivalents. The menu
shows ⌘↑ on Page Up, and choosing Page Up pages.

**Scan bounds (S5, S6).** One call looks at most at `MAX_PROMPT_SCAN_LINES`
(1 000 000) lines. When the shell has reported prompts, a prompt further
away than that is not found and nothing moves (N13): it may be there. When
the look for *any* prompt in the history (the second look, after none was
found in the direction asked) reaches the cap, the commands page, never do
nothing; and a session that never saw a prompt pages without a scan. The scan
takes the terminal lock for `PROMPT_SCAN_LINES_PER_LOCK` (256) lines at a
time, like a search step (D-SCAN-LOCK). It stops with nothing done if the
line ids moved or the alternate screen came up between holds.

**Mark notifications.** `tessera-term` emits `Request::Mark { mark, line }`
from the branch that sets the mark, so marks the grid refuses (alternate
screen, `k=s`) emit nothing. The line is an absolute id, `scrollback_pushed`
plus the cursor row: the line numbering of the selection (ADR 0025).
Requests are coalesced to the last mark of each kind per `advance` call, so
there are at most `MAX_MARK_REQUESTS` (4) whatever the input (D8). The
session coalesces them again per chunk of PTY output. They are never
replied to (S1, S2).

**Line epochs (D-REFLOW).** A resize to another size moves rows between the
screen and the history without counting them, height-only changes included.
A clear to the prompt (⌘K) that moves the kept rows up does the same. Both
bump `Terminal::line_epoch`. An id is meaningful only with its epoch. A
record from an older epoch keeps its status and times, but no lines: it
gets `None` from `command_at`.

**Command records.** The core keeps a ring of `MAX_COMMAND_RECORDS` (1000)
records per session in `commands.rs`. Each record holds metadata only: the
prompt, command, output and end lines; the start and finish `Instant`; the
exit status (`None` when unknown); and the state. It holds no command text.
The Pump applies a chunk's marks with one clock read and emits
`SessionEvent::CommandStarted` and `CommandFinished { exit_code, duration }`.
Those events are internal for now; the plugin fan-out is #48, and the FFI
and devwin ignore them.

The state machine follows the fixture's R rows:

- `A` closes a running command with exit `None` (D-MISSING-D) and starts a
  pending prompt;
- `B` records the command line;
- `C` starts a command;
- `D` finishes it.

Records whose lines have all left the history are dropped lazily, on the
next mark or query. ED 3 drops them with the lines it clears;
`clear_scrollback` drops every finished record. `aid=`, `cl=`, `redraw=` and
`err=` are ignored (D-AID). OSC 633 (VS Code) and OSC 1337 `SetMark` are not
handled (D-633).

**C API (S7).** `tessera_session_scroll_to_prompt`,
`tessera_session_select_last_output` and `tessera_session_has_command_output`
are in `tessera-ffi/src/prompts.rs`, each under `ffi_guard!`, with a smoke
test in `tests/c/prompts.c`. `has_command_output` reads the records alone,
with no scan, so `validateMenuItem` can call it on every validation.

## Consequences

- **Coalescing drops records (D-COALESCE).** When several commands finish
  inside one read (a pasted `a; b; c`, a script, a fast loop), only the last
  gets a record. Navigation and selection are unaffected, because they read
  the grid, whose marks are complete. The gutter (#45b) reads exit colours
  from the grid's `D` marks and only durations from the ring.
- **Ids not moved on purpose.** IL and DL inside a partial scroll region,
  ED 2 (`CSI 2 J`) and RIS move or clear screen rows without bumping the
  epoch. A record then names the row its marks were on, which may now hold
  other text. Navigation and selection read the grid, so only `command_at`
  metadata can be off. `command_at` answers from the newest record that
  covers a row, so an older record can still answer for a row no newer one
  covers, until the row scrolls out of the history or the ring drops the
  record.
- **A stale menu enable.** After a resize, `has_command_output` is `true` for
  a finished record whose lines are unknown, so the menu item can be enabled
  while Select Last Output finds nothing (it beeps).
- **Shell integration only.** Prompts come from Tessera's integration
  scripts or any shell that sends FinalTerm marks. `[BO]` sources, the
  FinalTerm primary text and the semantics of the `N`, `P`, `L`, `I` letters
  were not verified (the fixture's note).

## Alternatives considered

- **A prompt index fed by `Request::Mark`.** This would make lookups O(log
  n), but every re-wrap invalidates it, and a rebuild is the same scan. The
  chunked scan is bounded and needs no second structure to keep in step.
  Revisit if profiles show the scan is slow.
- **kitty's behaviour for a running command and for empty output** (select
  the output so far, or the last non-empty one). Rejected: the brief asks
  for the last *finished* output, and silently picking an older command
  surprises more than a beep does.
- **Upgrading the chord in the registry** (⌘↑ bound to `prompt_previous`).
  Rejected: a `[keys]` table written for 0.1.0 that rebinds Page Up would
  then lose ⌘↑ paging, and the menu would show ⌘↑ on an item that pages
  only without marks.

## Amendment: #45b (2026-11-08)

### The exit-status gutter

Each prompt row of a finished command gets a mark in the left padding: a
bar in the theme's success or failure colour, or the palette's bright
black when the shell reported no status. The renderer side (the
`FrameInput::gutter` slice, the bar's geometry and colours) is in ADR
0010's amendment; the theme keys in ADR 0019's.

**Which rows (D-GUTTER-ROWS).** `commands::gutter` reads the grid, like
navigation, never the record ring: per D-COALESCE the grid's `D` marks are
complete and the ring is not. A row is marked when it is a prompt stop (a
soft-wrapped prompt is one mark, on its first row) and its command
finished: a `C` follows the prompt, then a `D` (its status) or the next
prompt (status unknown, as D-MISSING-D) comes first. A prompt with no `C`
before its end ran nothing (an empty Enter, Ctrl-C at the prompt) and gets
no mark, and neither does a running command. A row's marks are read in the
order they were set, so zsh's `D` before `A` on the next prompt's row
ends the command above. The alternate screen has none.

**Bounds (S5, S6).** The scan reads the rows in view and, only while a
prompt in view is still open, at most `MAX_GUTTER_SCAN_LINES` (1024)
lines below the view to find its end; a command that ends further down
has no mark (the view is scrolled back over a very long output). It
emits at most one mark per row in view, whatever the lines hold, so a
`cat` of 100 000 marks draws at most a screenful. The snapshot runs it
under the terminal lock it already holds for the capture, and only when
something it depends on moved: the grid's damage generation (a mark
damages its row), `scrollback_pushed`, `scrollback_len`, the offset, the
line epoch and the alternate screen (`GutterKey`). An idle frame does not
scan. The marks are overwritten in place in the snapshot's buffer, and a
gutter generation that changes only when they differ tells the view to
present a frame although no row in view was damaged (a `D` below the
view's last row marks a prompt in view).

**After a reflow (D-REFLOW).** The marks themselves follow the lines
through a re-wrap, so the colours stay. The durations do not: a record of
an older epoch has no lines, so the hover reports the status and an
unknown duration.

### The hover

`App::command_mark_at(id, row)` returns the mark on row `row` of the view
as it is scrolled now: `CommandMark { exit_code, duration }`, or `None`
when the gutter draws nothing there. It runs the same scan, under the
same bounds, and takes the status from it, so the hover always agrees
with the colour. The duration comes from `command_at` on that line (S6:
at most `MAX_COMMAND_RECORDS` records read, under the log's leaf lock),
and only from a finished record whose prompt is that line and whose exit
status is the mark's; otherwise it is `None` (a coalesced command, an
older epoch, a record the ring dropped).

**C API (S7).** `tessera_session_command_at(app, id, row, out)` fills a
`TesseraCommandMark` (`found`, `exit_known`, `exit_code`,
`duration_known`, `duration_ms`). It zeroes `*out` first, so every
failure and every row without a mark reads `found = false`, and runs
under `ffi_guard!`; `tests/c/prompts.c` covers a mark, rows without one,
a scrolled view, `NULL` arguments and an unknown session. It logs nothing
(S8).

**The app.** `TerminalSurfaceView+Gutter.swift` lays out one AppKit
tool-tip rect per visible row over the left padding, again when the
padding, cell height, row count, scale or the view's height change. AppKit
asks for the text when the pointer rests in one; it is built then from
`TesseraSession.commandMark(atRow:)`: "Exit status 1" (or "unknown") and
"Ran for 4.2 s" (or "Duration unknown"), in units that read the same in
every locale. A row without a mark returns an empty string: no tip.
Nothing is logged, and a failed call shows no tip.

### A yield between scan holds (D-SCAN-LOCK, #334)

The prompt scan now yields the thread after each hold of the terminal lock
it continues past. The PTY reader locks the terminal directly and is not
counted as a waiter, and `std::sync::Mutex` is not fair: without the yield
the scanning thread took the lock straight back, and a long scan let the
reader in only once or twice in its whole walk. With it, about 2000 small
feeds land during a walk over 200 000 lines to the prompt (n19 asserts at
least ten; the code before the yield gave one or two and fails it).

### Measurements (#45b)

See ADR 0010's amendment for the render passes. Below, `cargo xtask ab`
against the merge base (ae6a701), 15 interleaved pairs at load 10-40,
head / base, instructions then cycles. A case fills a 60x200 terminal
with one wave-13 corpus line per row and then, per frame, feeds the next
line (the screen scrolls by one). `none`: plain lines. `every row`: each
line is `D;n`, `A`, `$ `, `B`, the text and `C`, so every row has a mark
and every frame moves them all. Bars 1.02 (none) and 1.05 (every row).

**`render_view`** (an offscreen view at 2x: capture under the lock, then
a frame drawn and waited for; 80 frames, box drawing 240). All pass.

| line | none | every row |
|---|---|---|
| ascii | 1.001 / 0.994 | 1.003 / 0.999 |
| latin_diacritics | 1.000 / 0.971 | 1.004 / 1.013 |
| cjk | 1.000 / 1.003 | 1.001 / 0.772 |
| box_drawing | 1.001 / 0.990 | 1.008 / 1.008 |
| hebrew | 0.998 / 0.995 | 1.002 / 0.996 |
| arabic | 1.001 / 1.010 | 1.003 / 1.001 |
| punctuation | 1.001 / 1.003 | 1.005 / 1.009 |
| braille | 1.000 / 0.995 | 1.006 / 1.019 |
| emoji | 0.999 / 1.003 | 1.003 / 1.005 |
| sgr_heavy | 0.998 / 0.993 | 1.006 / 1.011 |
| combining_heavy | 0.997 / 1.003 | 1.006 / 1.010 |

**The capture alone** (`Snapshot::capture`, `rows` and `frame`, the part
that holds the terminal lock; 40 000 frames). Without marks it is within
1.02 but for emoji (1.022). With a mark on every row and every frame the
gutter scan adds about 5 300 instructions a frame (60 rows of four marks
each, about 90 instructions a row), 6-7 %, over the 1.05 bar on the
cheap lines. The figures move by up to 6 % with no scan at all: with the
scan stubbed out, emoji is 1.013 / 1.022 and combining_heavy 0.943, which
is codegen moving in the row copies, not gutter work.

| line | none | every row |
|---|---|---|
| ascii | 1.010 / 1.011 | 1.061 / 1.027 |
| latin_diacritics | 1.010 / 1.009 | 1.061 / 1.046 |
| cjk | 1.010 / 1.016 | 1.061 / 1.033 |
| box_drawing | 1.010 / 0.987 | 1.062 / 1.040 |
| hebrew | 1.011 / 1.015 | 1.066 / 1.039 |
| arabic | 1.011 / 1.011 | 1.064 / 1.048 |
| punctuation | 1.012 / 1.003 | 1.068 / 1.037 |
| braille | 1.012 / 1.016 | 1.069 / 1.079 |
| emoji | 1.022 / 1.024 | 1.020 / 1.029 |
| sgr_heavy | 1.012 / 1.002 | 1.069 / 1.059 |
| combining_heavy | 0.946 / 0.987 | 0.958 / 0.985 |

## Amendment: #377 (2026-11-12)

### The problem

Ctrl-L (zsh, bash and fish all send `CSI H CSI 2 J` and redraw the prompt)
blanks the screen in place: no line scrolls, `scrollback_pushed` does not
move, so the next prompt gets the same line id as an older one that was
on the screen. When the clear, the new prompt and a command arrive in one
chunk, the older record still owned that id, and the hover lent its
duration to the new mark (#377). Neither the line ids nor the epochs
(D-REFLOW) tell the log that the lines were blanked.

### The signal (D-CLEAR)

**A separate count.** `Grid::screen_clears` counts the whole-screen erases
of the primary screen; `Terminal::line_epoch` is left alone by every ED,
ED 0 at home, DECSED, RIS and DECCOLM (asserted in
`tessera-term/tests/screen_clears.rs`), because the ids are still valid:
the lines they name were erased, not renumbered.

**What counts.** Exactly the erases that clear `scrolled_since_clear`
(the grid's `whole_screen` predicate), on the primary screen only:

- ED 2 and DECSED 2, protected cells kept or not;
- ED 0 from the home position, unless the top row continues the newest
  history line (then it erased a continuation, not a screen);
- DECCOLM (when allowed), which erases through the same path;
- RIS, through `Grid::reset`, also while the alternate screen is up (it
  drops the stashed primary rows).

Never counted: ED 0 elsewhere, ED 1, ED 3 and `clear_scrollback`, EL,
resizes, anything on the alternate screen, and the 1047/1049 switches
(their erase is of the alternate screen). **DECALN** (`ESC # 8`) fills
the primary screen with `E` without counting: a known gap, since no shell
sends it at a prompt and its marks are erased with the cells anyway.

**Ctrl-L typed ahead** is a second known gap. When the user presses
Ctrl-L while a command runs, the shell writes the command's `D` and the
clear in one read, so they land in one slice and the clear is applied
before that `D` (the same-slice corner below). The record is then not
cut at the floor: its prompt, in the history, shows no duration on hover
(the gutter has no status for it and `end_erased` is not set), and
`command_at` on lines of the new screen still returns it, ending at its
`D`'s line (probe P1 of the #377 review: `seq 1 60`, then Ctrl-L, in zsh,
bash and fish). Both err away from lending a duration. Cutting a record
when a clear of the same slice has a floor at or above its `D` is left
for a follow-up.

**The floor.** Each counted erase also records the screen-top line id
(`scrollback_pushed` then) if it is the first since the last take:
`Grid::take_first_clear_top` (forwarded by `Terminal`) returns it and
clears it. Ids never decrease within an epoch, so the first is the lowest
line any of them blanked. Recording it is O(1) per erase and allocates
nothing.

### Applying it (D-CLEAR-APPLY)

app-core already advances the terminal in slices of 4096 bytes under the
lock. After each slice it takes the floor, with `line_epoch` read in the
same hold, and pushes `ChunkMark::Cleared { top, epoch }` into the chunk's
requests **before** that slice's marks. Adjacent clears merge: of two with
the same epoch the earlier (lower) floor is kept, of two with different
epochs the newer replaces the older (the older's ids mean nothing in the
new epoch). With the per-kind mark coalescing that bounds the list at
`MAX_CHUNK_MARKS = 2 * MAX_MARK_REQUESTS + 1` entries per chunk (S5).

`CommandLog::screen_cleared(top, epoch)` then, for that epoch:

- drops the pending prompt if it is at or below the floor (`prompt >=
  top`);
- retires a record whose first line (prompt, input or output) is at or
  below the floor: its epoch becomes `REDRAWN`, so `command_at` never
  finds it and its duration is never lent (D-REFLOW's rule for an older
  epoch);
- truncates a finished record that started above the floor and ended at
  or below it: `end = top`, and `end_erased` is set, since its `D` was on
  a blanked line.

A record is therefore stale only if a clear came after its `A` and its
prompt is at or below that clear's floor; the first clear after the `A`
decides, and floors never decrease. A record whose prompt is in the
history above the floor keeps its duration across the clear.

**The same-slice corner.** The floor is per slice, not per byte: a mark
set in the same 4096-byte slice as a clear is applied after it, even when
its bytes came before it. Such a record is not retired by that clear. It
errs safe: its line was blanked, so the grid no longer has its mark there,
the gutter draws nothing on it and the hover, which starts from the
gutter's mark, asks for no duration; when a later prompt is drawn on that
line, its own `A` starts a newer record, and the newest record covering
a line is the one `command_at` returns.

**ED 0 at home** erases from the cursor down, the cursor row's cells
included, but the grid keeps that row's marks (it always did; only
`erase_line` of whole rows drops them). The record is retired by the
floor all the same.

### The relaxed hover filter (#378 M3)

The hover still lends a duration only to a finished record that starts at
that prompt and agrees with the gutter's exit status, with one
relaxation: when the gutter reports no status and `end_erased` is set
(a clear erased the record's `D` below its prompt), they agree. No other
loss of the `D` (a deleted line, #368) relaxes it.

### Measurements (#377)

`cargo xtask ab` against the merge base (bbd2a2c; the table is the run
against 0283266, before main gained the config-only #396, and the
review's run against bbd2a2c, 10 pairs, read 0.995 to 1.000), `tessera-term`, 7
interleaved pairs at load 3-4, gating on instructions (net of process
overhead), bar 1.02. Each corpus case feeds its line plus CRLF to 24 MiB
in 4 KiB slices into a 24x80 terminal; `flood` is B13, 1 MiB of
`CSI H CSI 2 J` then `x`, eight times. Head / base, best of 7; reads above
1.000 were repeated on three independent probe builds.

| case | instructions | cycles |
|---|---|---|
| ascii | 1.000 | 1.000 |
| latin | 1.000 (-0.001 to +0.002 %) | 1.000 |
| cjk | 1.000 | 1.000 |
| box | 1.000 | 1.000 |
| hebrew | 1.000 | 1.000 |
| arabic | 1.000 | 1.000 |
| punct | 1.000 | 1.000 |
| braille | 1.000 | 1.000 |
| emoji | 1.000 | 1.000 |
| sgr | 1.000 | 1.000 |
| combining | 1.000 (+0.002 to +0.008 %) | 1.000 |
| flood | 1.0003 (three builds) | 1.001 |

The flood costs about 5 instructions more per clear (the count and the
floor). A differential case (`clears`, 20 000 seeds of random clears,
erases, alternate-screen switches, DECCOLM, RIS, DECALN, marks and
splits) found the grid, cursor, history, epoch and requests identical to
the base.

## Amendment: capture-only cost under the terminal lock (#368, #392, 2026-11-12)

The "capture alone" measurement above (`Snapshot::capture`, the part that
holds the terminal lock) is over its 1.05 bar when every row carries a mark
and every frame moves them all: 1.061-1.069 in instructions on nine of the
eleven corpus lines (the review of #368 reads 1.061-1.073), the
other two inside the bar. The gutter scan costs about 5 300 instructions
more per frame, held under the terminal lock the PTY reader shares: 60 rows
of four marks each, about 90 instructions a row. The review of #368 asked to
track that cost and to decide whether to reduce it.

**Decision (2026-11-12): accepted for 0.2.0.** The extra work is small next
to a frame: the `render_view` table above, which includes the capture, keeps
its instruction counts within 1.01 on every line; its cycles reach 1.019
(braille, a mark on every row). Reducing the scan's cost under the lock is not
planned for 0.2.0, and the capture-alone figures above stay as measured: nine
of the eleven "every row" lines are over the 1.05 bar, recorded here as an
accepted exception, not as a pass.
