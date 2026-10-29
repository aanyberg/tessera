# ADR 0009: Resize with soft-wrap reflow

**Status:** accepted · 2026-10-28

## Context

Until issue #3 a `tessera_grid::Grid` kept the size it was created with:
`App::resize` told the PTY about a new window size, but the grid went on
drawing the old one (`TODO(#3)` in `tessera-app-core`). A terminal that
follows its window must decide, on every resize, what happens to the
scrollback, the visible rows, the alternate screen, the cursor, the saved
cursor, marks, tab stops and the scroll region, and whether text that was
soft-wrapped at the old width is joined and split again at the new one
(*reflow*).

Emulators disagree on most of these. The expected behaviour is pinned by
`crates/tessera-grid/tests/fixtures/reflow_spec.txt` (43 rows, each
citing the resize code of Alacritty, WezTerm, kitty, xterm or iTerm2),
whose recommended algorithm, disagreements R1-R12 and corrections C1-C7
the milestone lead adopted (brief #3, "Lead decisions for #3 after spec
pass", 2026-10-28). Review round 1 changed three rules (brief, "Lead
decisions for #3 after round 1"): an empty continuation row may be
dropped, only default blanks are trimmed (fixture row C01 was rewritten),
and a cursor past a full row always waits with a wrap pending.
Verification cycle 1 replaced the guess that identified the column a
wrapped wide character leaves behind with an explicit marker
(`CellFlags::PAD`, fixture rows B01-B08 rewritten). This ADR records the
model; the fixture holds the case-by-case evidence.

## Decision

### One entry point: `Grid::resize(rows, cols)`

`Grid::resize` is the only resize primitive; `Terminal::resize` delegates
to it, and `Session::resize` (behind `App::resize`) resizes the terminal
first and the PTY second, so output the child writes in answer to
`SIGWINCH` lands on a grid that already has the new size. The two locks
are taken one after the other, never together. A size with zero rows or
columns leaves the terminal alone; the PTY refuses it. A size above
`MAX_VIEW_ROWS` × `MAX_VIEW_COLS` (1024 × 2048, the largest grid a view
derives) is refused with `PtyError::InvalidSize` before anything changes,
also through `tessera_session_resize`: `Grid::resize` itself is total and
would allocate every row and history line at the requested width (tens of
gigabytes at 65535 × 65535).

`resize` is total. A zero dimension is raised to one, as `Grid::new` does.
The current size is an O(1) no-op that leaves everything, including a
custom scroll region and tab stops, untouched (fixture J01). The
algorithm lives in `tessera-grid/src/reflow.rs`. `Cell` stays 20 bytes
and `Style` and `Line` are unchanged; `CellFlags` gains one bit, `PAD`
(below).

### The primary screen and its scrollback are one buffer

Rows change first, at the old width, then columns (Alacritty's order).

**Rows** move whole lines only, in O(rows):

- Growing pulls the newest scrollback lines back onto the top of the
  screen, as many as the growth (Alacritty, xterm's default
  `resizeGravity`; WezTerm and iTerm2 refill the whole window, kitty adds
  blank rows by default). The cursor moves down with its text. Rows that
  history cannot fill are blank default rows at the bottom.

  *Amended 2026-10-29 (#86):* a screen that has not scrolled since it
  was last cleared pulls history only when its cursor anchors it at the
  bottom (the cursor is on the last row, or its logical line ends there);
  any other cleared screen, such as a prompt after Ctrl-L, grows by blank
  rows at the bottom, so shrinking by one row and growing back never
  pulls a line that was not on the screen (below).
- Shrinking first drops blank rows below the cursor (every cell
  `Cell::BLANK` or a `PAD` cell of any style, no marks, not wrapped), then
  pushes rows from the top into scrollback, evicting the oldest lines past
  the cap. A dropped row may be the empty continuation of a wrapped row,
  as readline leaves one after erasing the end of a long command line; the
  row above then loses its wrap flag, so the prompt the cursor is on stays
  visible (script `x01`). A row filled with a background colour is not
  blank. A cursor whose own row leaves the screen stays on row zero:
  content wins over the cursor row (WezTerm, iTerm2; xterm and Alacritty
  keep the cursor row and drop the rows below it instead, fixture D06).
- The cursor column, pending wrap and every row's cells are untouched,
  and so is every wrap flag but that one.

**Columns** re-wrap the whole buffer, scrollback included, in O(total
cells). A logical line is a maximal run of rows in which every row but the
last has `Line::wrapped`; a hard line break (`wrapped == false`) is never
joined.

- *Join.* The rows' cells are concatenated. A wrapped row's last cell is
  skipped when it is padding: the column `put_wide` left behind when a
  wide character did not fit and wrapped (ADR 0008, D1). That cell is a
  blank with the pen's background, marked `CellFlags::PAD`, as Alacritty
  marks it `LEADING_WIDE_CHAR_SPACER` and iTerm2 `DWC_SKIP` (R7). Any
  write or erase of the cell clears the marker (put, ECH, EL, ED, DECALN,
  RIS), and an ICH or DCH that moves it away from the last column turns
  it into an ordinary blank, so padding is only ever the last column of
  a row. Every other cell is text: a space typed in the last column right
  before a wide character survives every re-wrap (fixture B03). The first
  version guessed instead (drop a blank before a wide character, as
  iTerm2 does without its marker); every width then exposed different
  genuine spaces, and a window drag erased most spaces of Korean text.
- *Trim.* Trailing default blanks (exactly `Cell::BLANK`, or padding of
  any style) of the whole logical line are dropped and rows are re-padded
  with default blanks; blanks inside it, including at the end of a
  non-final row, are text and move with it (WezTerm, R6). A space with any
  colour or attribute (an inverse status bar, an underlined input field, a
  background fill) is text too, as in Alacritty, which keeps coloured
  blanks (C4); kitty and WezTerm would trim them. This deviates from the
  spec file's recommendation, which trimmed every space; fixture row C01
  now expects the background-coloured spaces to move with the text. A
  resize is not an erase: every added row and cell uses the default style,
  not the pen (fixture L01), except the padding a split leaves (below).
- *Split.* The cells are cut greedily at the new width. A wide character
  and its spacer are never separated: one that does not fit starts the
  next row and leaves a padding cell with the wide character's background
  behind (C5, amended below), which the next join drops again. On a
  one-column grid a wide character is dropped, as `put_wide` drops it and
  kitty skips it (R8). A cluster is one cell, so its `GraphemeTable` entry
  moves with it and is re-keyed to its new column. Every row but the last
  of a logical line is marked wrapped.

  *Amended 2026-10-28 (#88):* C5 gave that padding cell the default
  style, so a coloured band (`ESC[44m…ESC[K`) that a wide character
  wrapped through showed a one-cell gap after a widen and a narrow back.
  The padding now takes the background of the wide character it
  precedes and nothing else of its style (`Cell::pad_with`), the cell a
  live write leaves: the padding `put_wide` writes takes the pen's
  background, like an erase. Issue #88 proposed the whole `Style`; only
  the background is taken, so a re-wrap never draws an underline or
  inverse video where a live write leaves the column plain. Padding of any
  colour is still not text: the join drops it, the trim drops it and a
  row made of it is blank for the rows a height shrink may drop. A DCH
  that moves re-wrapped padding off the last column leaves a coloured
  blank without the marker, exactly as it does for the padding of a live
  write, so a later reflow or height shrink keeps that cell as text where
  the default-style padding before #88 became a default blank and was
  dropped; this is the live-write behaviour, not a regression of #88.
- *Marks* of all rows of a logical line collect on its first row, in row
  order (kitty keeps line attributes on the first row, R10).
- *Window.* The screen is the last `rows` rows of the re-wrapped buffer,
  so the window stays anchored at the bottom: a narrower grid pushes lines
  into scrollback and a wider one pulls them back (Alacritty, WezTerm).
  Before that, blank rows below the cursor absorb as many of the rows the
  narrower width adds as they can, wherever those rows come from (fixture
  step 2e). When most of the growth is in history, as after a clear with
  the prompt at the top of an otherwise empty screen, the blank rows are
  dropped and the window moves up with the buffer: the prompt goes to the
  bottom and older output reappears above it, as in WezTerm. Keeping such
  a prompt in place is a possible follow-up, not this design.
  *For a screen cleared since it last scrolled, superseded 2026-10-29 by
  the amendment below (#86).* History is
  cut to the cap, oldest first; a cut may start history in the middle of a
  logical line. While re-wrapping, the output never holds more than twice
  the cap plus four times the screen (at least 1024 rows): once it reaches
  that, the oldest rows are dropped down to the cap plus twice the screen,
  also in the middle of one long logical line, so narrowing a dense
  history to one column never holds every row it produces at once. The
  margin of a second screen is what the prune may still remove afterwards;
  a row dropped this way takes its marks with it, as the history cap
  would.

### Amendment 2026-10-29: a cleared screen keeps its rows (#86)

Issue #86: after `ESC[H ESC[2J` the prompt sits on row 0 of an empty
screen; narrowing 80 -> 40 columns doubled the rows of 100 history lines,
step 2e counted that growth, dropped the 23 blank rows and moved the
window up, so old output filled the screen and the prompt went to row
23. Widening did not undo it, and alternating the height by one row
pulled one more history line per cycle (row grow pulled
`min(growth, history)` wherever the cursor was). The milestone lead
adopted the spec researcher's fixture `reflow_after_clear_spec.txt`
(cases R86-01..R86-10, derived from the resize code of kitty, ghostty,
Alacritty, WezTerm and iTerm2; nothing was run in an emulator) as lead
decisions D1-D4 of brief #86, and after verify cycle 1 the memory bit
below (lead decisions, 2026-10-29: the bit after verify cycle 1, E1, E2
and m1 of review round 2).

**One bit of memory: `Grid::scrolled_since_clear`.** The rule cannot be
read from the cells. zsh with a right prompt (powerlevel10k has one by
default), scrolled to the bottom, redraws its prompt on `SIGWINCH` with
`CR ED 0`: after a narrow re-wrapped the prompt to two rows, the redraw
puts it back on one and leaves a blank row below the cursor. A screen
cleared with Ctrl-L (`ESC[H ESC[2J`) has its prompt on row 0 and blank
rows below it too. Only the amount of blank space tells them apart, and
a rule built on that breaks one or the other (the first design, without
memory, moved that zsh prompt from row 22 to row 11 on the widen, with
eleven rows of output pushed into history). So the primary screen
remembers whether rows of it have gone into history since it was last
cleared:

- *Set* when a line feed or SU with the full screen as scroll region
  moves a row of the primary screen off its top (also with a history cap
  of zero, which keeps nothing: harmless, and equal to the code before
  #86 since there is nothing to pull), and when a resize pushes rows that
  were on the primary screen into history: a height shrink that pushes,
  or a re-wrap whose screen block is still longer than the screen after
  the blank rows below the cursor absorbed what they could. Either way
  the screen's content has reached its bottom. The stashed primary
  screen behind the alternate screen counts too (lead decision E2).
- *Cleared* when the primary screen is erased whole: ED 2, ED 0 with the
  cursor at the home position (the `clear` of vt100, screen, tmux and
  linux terminfo, and Node's `console.clear`; lead decision m1) unless
  the top row continues the newest history line (below), and RIS. Each
  also ends the newest history line (clears its wrap flag, in O(1)): the
  top row that may have continued it is blank now, and the prompt
  written there next is a line of its own, not glued to the old output
  by a later re-wrap. (`Grid::reset` keeps scrollback, so only a
  direct `Grid` user sees that for RIS; `tessera-term` clears history
  separately.)
- *Unchanged* by everything else: ED 3 and `clear_scrollback` (they empty
  history but leave a screen full of scrolled output; clearing the bit
  there brought the right-prompt jump back, lead decision E1; `clear` is
  ED 2 plus ED 3 and is cleared by its ED 2), ED 0 anywhere but home (a
  shell's prompt redraw: zsh's `CR ED 0` starts on the prompt's first
  row, which on a scrolled screen is below the top), ED 0 from home when
  the top row continues the newest history line (lead decision after
  review round 2: a narrow made the prompt line taller than the window
  and pushed its first row into history, so zsh's redraw starts at home;
  that is a redraw of a line that started above the screen, not a
  clear), ED 1, EL, cursor motion, DECSTR, a resize that
  pushes nothing (a narrow absorbed by blank rows, any grow), a partial
  scroll region (it keeps no history), and anything on the alternate
  screen (it has no history; ED 2 there leaves the primary screen's bit
  alone).

It is placement state only: it is in `Debug` and `Clone`, changes no
cell, damages no row, and a grid's other observable state is the same
whichever way it is set until the next resize.

**A screen that has scrolled** is continuous with its history and is
resized exactly as before #86 (above): one buffer anchored at the
bottom, blank rows below the cursor absorbing growth wherever it comes
from, a height grow pulling history. `cargo xtask ab diff` checks it
against the code before #86 on random scripts (cursor anywhere,
including right prompts redrawn after every resize): no difference,
except after an earlier ED 2 cut a wrapped history line, which now ends
there (by design, above).

The narrowed ED 0 rule has two costs. A vt100 `clear` (`ESC[H ESC[J`)
issued while the top row continues a wrapped history line looks like
that redraw and does not count as a clear (`ESC[2J` does; test
`a_vt100_clear_inside_a_wrapped_line_is_not_a_clear`). And when a narrow
makes the prompt line exactly as tall as the window, its first row is
row 0 and starts a line of its own, so zsh's redraw from home is
indistinguishable from a clear and counts as one: the widen then keeps
the prompt at the top instead of pulling history back. Only tiny windows
get there: the A/B diff's random right-prompt sessions meet it in 1,658
of 20,000 sessions of 2-30 rows (10,167 of 20,000 with 40-80 drastic
resizes each) and in none of 20,000 of 10-30 rows (none either with
40-80 drastic resizes each); before the narrowing it was 2,256 of
20,000 of 2-30 rows.

Setting the bit on a resize that pushed (E2) has a cost the adversarial
recordings show: after Ctrl-L and a screenful of output (the prompt on
the last row), a narrow pushes the top rows into history, and from then
on the screen is resized as before #86, so a drag back pulls the output
from before the clear (in `zsh_clear_output.rec`, lines L089..L101) back
onto the screen, as the code before #86 did. Without E2 such a screen,
with a right prompt redrawn after each resize, moved its prompt from row
22 to row 11 on the widen (test
`a_cleared_screen_filled_to_the_bottom_stays_at_the_bottom`); a prompt
that stays at the bottom wins. A cleared screen whose content never
reached the bottom (the #86 repros: a narrow absorbed by the blank rows
below the prompt) pushes nothing and keeps its rows.

**A cleared screen** (no rows of it have gone into history since the
clear; its history can still change, as a narrow re-wraps it or a grow
of a screen anchored by its cursor pulls rows back):

- **Columns (D1, step 2e).** The *screen block* is the re-wrapped rows
  from the one holding the old top screen row's first cell to the end.
  Only its growth counts: blank rows below the cursor absorb it first
  (D3, unchanged; kitty and WezTerm do the same, Alacritty, ghostty and
  iTerm2 keep the blank rows), then the block's top rows go into history.
  Lines wholly in history only lengthen or shorten history. A block
  shorter than the screen (a widen, or a trim) takes rows back from
  history only when the cursor anchors the screen at the bottom (below);
  otherwise it keeps its place and blank rows fill the bottom. kitty
  (screen rows re-wrapped into a screen that starts at row 0,
  `kitty/resize.c` `rewrap`), ghostty (`PageList.resizeCols` keeps the
  rows below the cursor; its comment names this very case), Alacritty
  (`shrink_columns` moves the cursor only for rows at or below it) and
  iTerm2 keep the prompt on row 0 when narrowing after a clear. WezTerm
  does not: it prunes the blank rows below the cursor and then shows the
  last `rows` rows of the buffer (`term/src/screen.rs` `resize`), which
  is what Tessera did before.
- **Rows (D2, step 1).** A height grow pulls history only when the
  cursor anchors the screen at the bottom (a shrink that pushed rows has
  set the bit, so the grow back pulls them): ghostty's rule
  (`PageList.resizeWithoutReflow` pulls only when the cursor is on the
  last row); kitty never pulls by default
  (`scrollback_fill_enlarged_window`). Alacritty's `grow_lines` and
  WezTerm pull whatever the cursor row, which is the drift #86 reports.
- **Anchored by the cursor.** Tessera extends ghostty's "cursor on the
  last row" to a cursor whose logical line ends on the last row (its row
  and every row below it but the last are wrapped; milestone lead,
  2026-10-29), one predicate for rows and columns. The reason is the
  cursor moved to the start of a long wrapped command line (Ctrl-A in
  readline): a narrow pushes the top rows into history and splits the
  command so the cursor is no longer on the last row, and a shrink does
  the same by rows. Before #86 the widen or grow pulled those rows back;
  under ghostty's literal rule it would not. With the wider predicate
  such a screen stays anchored through a narrow or a shrink, so both
  round trips restore it (test
  `a_cursor_inside_a_wrapped_last_line_stays_anchored`, on a screen that
  has scrolled and so pinned by the bit as well).
- **A line straddling history and the screen (D4).** No emulator
  documents it. The block starts at the output row holding the old top
  row's first cell, so the cells before it on that row come onto the
  screen with it, as a cursor there would (on a wide character dropped at
  one column: the row the next kept cell starts). A top row that
  continues a history line but holds only its trailing blanks (EL 2 on
  the top row erases the continuation but not the wrap flag of the
  history row above; ED 2 now ends that line) has no text on the screen:
  it stays a blank row of its own, with the cursor if it was there, and
  the line stays in history.

What a round trip restores, pinned by property tests (`tests/reflow.rs`,
`a_column_round_trip_restores_the_screen`,
`a_row_round_trip_restores_the_screen`,
`history_never_refills_a_screen_not_anchored`): a column or row change
and back gives the same screen when the screen has scrolled and no
blank row below the cursor absorbed growth on the way; when it is
cleared and the cursor anchors it before and after; and when it is
cleared, not anchored, starts with a line of its own and the first
change still leaves a blank row below the cursor (nothing was pushed).
The rest is ambiguous even with the bit: on a cleared screen a narrow
whose growth both used up the blank rows and pushed rows into history
leaves the same state as one that only pushed; a shrink that drops blank
rows until the cursor reaches the last row anchors the screen, so the
grow pulls history (ghostty does the same); and a straddling line's
shared row moves to the screen at a narrow and takes the rest of its
row from history at the widen. In the first two cases a second round
trip changes nothing more; a straddling line's top row can move back by
one row of that line per round trip, never past the line's start
(fixture R86-10's test pins both).

What changed against the code before #86, as `cargo xtask ab diff` on
random scripts shows: nothing on a screen that has scrolled since it
was last cleared (except after an ED 2 that cut a wrapped history line,
by design), nothing without history, and nothing on a cleared
screen while the cursor anchors it (widths of two columns or more).
Elsewhere, on a cleared screen, the text is the same (ED 2 now ends the
history line it cut, so a prompt is no longer glued to old output) and
only where the screen sits and which blank rows exist differ. One class
of cleared screen no longer round-trips where it did: content below the
cursor's line (a full-screen program drawing on the primary screen after
clearing it, `fzf --height` or a multi-line zsh buffer after Ctrl-L with
the cursor on an upper line). After a narrow pushed rows of such a
screen into history, its state cannot be told apart from "blank rows
absorbed, then rows pushed", which must not pull history back; ghostty
behaves the same (it keeps the rows below the cursor and pulls history
only for a cursor on the last row), and such programs redraw on
`SIGWINCH` anyway. The critic's round-1 example of this class (eight
7-character lines on a 4x8 screen, cursor moved to row 0, narrow to 4
columns and widen back) has scrolled, so it round-trips as before. At
one column a line whose wide characters are dropped can end in an empty
row below a cursor anchored by its wrapped line; on a cleared screen
that row absorbs only the growth of the screen's own rows.

vim, less, htop, tmux and other full-screen programs that switch to the
alternate screen with DECSET 1049 drew on the primary screen until #8
implemented the alternate-screen modes (ADR 0020): there a resize
re-wrapped their screen like shell output, and after a clear (which they
issue) the repaint landed below rows a narrow kept, so a window drag
could leave copies of their screen in history. Since #8 they draw on the
alternate screen, where nothing is re-wrapped.

Existing fixture rows whose outcome changed: none (`D06` changed with the
first version of this amendment and is back: its shrink pushes `aa` into
history, which sets the bit, so the grow pulls it back).
`reflow_spec.txt` steps 1 and 2e/f carry the new rule.

### The cursor follows its character

Its offset in its logical line is the cells before it, plus one while a
wrap is pending (the place after the last cell of a full row).

- On a glyph (a spacer counts as its wide character): the cell holding
  that glyph after the split, pending wrap cleared (C3).
- Past the text (on blanks after it, or after the last glyph): it keeps
  its distance from the text while that fits on the last row of its line,
  so a prompt `$ ` with the cursor after the space does not jump left.
  When the last row of its line is exactly full it sits on that row's
  last column with a wrap pending, however far past the text it was, so
  the next character wraps instead of overwriting the last one
  (Alacritty, iTerm2 and WezTerm's right margin; kitty loses the flag,
  R5). Otherwise it stops on the last column, which is then a blank, never
  a spacer (kitty; a cursor never creates a row, R4). A clamped offset is
  not restored by growing again (fixture D02).

### The alternate screen is not re-wrapped

Full-screen programs redraw on `SIGWINCH`, and their rows are not prose.
xterm, kitty, WezTerm and Alacritty cut and pad the alternate screen;
iTerm2 alone re-wraps it (R1, C1). Its rows follow the primary row rule,
except that rows leaving it are discarded and growing adds blank rows (it
has no history). Each row is cut or padded to the new width; a wide
character whose spacer falls off the edge is blanked and its cluster entry
dropped, and wrap flags are kept as they were. The cursor is clamped and
pending wrap is cleared when the width changes.

While the alternate screen is active, the stashed primary screen is
resized as above with its own cursor, pending wrap and saved cursor,
sharing the one scrollback, so `leave_alt_screen` restores a primary
screen of the current size (its `debug_assert`s check it).

### Everything else

- **Saved cursor (DECSC):** each screen's slot is clamped to the new size
  and is not moved with the text; its pending flag survives only when the
  width does not change (Alacritty, R9). Content-tracking emulators would
  drag the initial slot (home, default pen) along with the text.
- **Scroll region:** every real resize resets it to the full screen
  (unanimous, R12).
- **Tab stops:** stops below the old width keep their state, set or
  cleared by the user; columns beyond it get the default stop every eight
  columns (kitty, Alacritty, WezTerm, R11).
- **Pen, modes and the cursor's visibility** are unchanged.

## Consequences

- Shell output and long command lines read correctly after a window
  resize, history included, and widening the window shows more history.
- Resizing a full 10 000-line history by columns re-wraps every line.
  Measured on an Apple-silicon Mac, release build, best of five: the
  resizes 80 -> 40 and 40 -> 80 columns of a 24-row grid with 10 000
  history lines of each line of the shared wave-4 corpus take 0.6 to
  3.5 ms, except text made of combining-mark clusters (up to 38 ms under
  load, 10 ms on a quiet machine), where every cluster's text moves to a
  new table; the budget is 100 ms (brief #3). With one line per row
  group, source rows are freed as they are re-wrapped and the peak extra
  heap of one call stays below about half the grid's own footprint. The
  worst case is one logical line spanning the whole history (output with
  no line break): its rows are joined into one buffer before they are
  split again, so the peak extra heap is 1.2 to 1.3 times the footprint
  (2.2 to 2.3 times in total; each source row is released as soon as it
  is copied) and the calls take 0.7 to 9 ms:

  | 10 000-row logical line of | 80 -> 40 ms | 40 -> 80 ms | peak extra / footprint |
  |---|---|---|---|
  | ascii | 2.09 | 0.95 | 1.19 |
  | cjk | 3.88 | 1.86 | 1.19 |
  | emoji | 7.45 | 3.25 | 1.22 |
  | combining_heavy | 8.79 | 4.00 | 1.28 |
  | every other corpus line | 1.6 - 2.0 | 0.7 - 0.8 | 1.19 |

  Narrowing the same 10 000-line history from 80 columns to one takes 1
  to 25 ms and its peak extra heap is at most 0.17 times the footprint,
  because rows beyond what history and the screen keep are dropped while
  the re-wrap runs. For one logical line spanning the whole history it
  takes 2 to 25 ms and peaks at 1.06 times the footprint (16.9 MiB extra
  on 15.9 MiB): the joined cells of that one line, not its output rows.

  Rows of plain text (no wide or cluster cells) are copied in whole
  slices; other rows are placed cell by cell. A row-only change does no
  re-wrapping.
- Every scrollback line is as wide as the grid, so a resize to a very
  wide grid with a long history allocates that width for every retained
  line (10 000 lines at `u16::MAX` columns would be about ten gigabytes).
  Window sizes do not come close; the property tests keep the history
  short when they try extreme widths.
- The edges of the model are documented in the fixture: a cursor on
  blanks past the text is clamped rather than remembered (D02), and a
  wide character resized through a one-column grid is gone (B06).
- The property tests (`tests/reflow.rs`) check P1-P7 of the fixture, and
  since #86 the screen round trips above:
  resize never panics, every invariant holds on both screens after any
  sequence of operations and resizes, a repeated or same-size resize
  changes nothing, a column change keeps the text of every logical line,
  shrinking and growing back (and growing and shrinking back) restore the
  text and the cursor's place in it, and a row-only change only moves
  rows.
- Selections, a scrolled-back viewport offset and images are not grid
  state; whoever adds them must decide how they follow a reflow.
