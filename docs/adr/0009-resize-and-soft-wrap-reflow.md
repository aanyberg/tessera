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
- Shrinking first drops blank rows below the cursor (every cell
  `Cell::BLANK`, no marks, not wrapped), then pushes rows from the top
  into scrollback, evicting the oldest lines past the cap. A dropped row
  may be the empty continuation of a wrapped row, as readline leaves one
  after erasing the end of a long command line; the row above then loses
  its wrap flag, so the prompt the cursor is on stays visible (script
  `x01`). A row filled with a background colour is not blank. A cursor
  whose own row leaves the screen stays on row zero: content wins over
  the cursor row (WezTerm, iTerm2; xterm and Alacritty keep the cursor
  row and drop the rows below it instead, fixture D06).
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
- *Trim.* Trailing default blanks (exactly `Cell::BLANK`, or padding) of
  the whole logical line are dropped and rows are re-padded with default
  blanks; blanks inside it, including at the end of a non-final row, are
  text and move with it (WezTerm, R6). A space with any colour or
  attribute (an inverse status bar, an underlined input field, a
  background fill) is text too, as in Alacritty, which keeps coloured
  blanks (C4); kitty and WezTerm would trim them. This deviates from the
  spec file's recommendation, which trimmed every space; fixture row C01
  now expects the background-coloured spaces to move with the text. A
  resize is not an erase: every added row and cell uses the default style,
  not the pen (fixture L01).
- *Split.* The cells are cut greedily at the new width. A wide character
  and its spacer are never separated: one that does not fit starts the
  next row and leaves a padding cell with the default style behind (C5),
  which the next join drops again. On a one-column grid a wide character
  is dropped, as `put_wide` drops it and kitty skips it (R8). A cluster is
  one cell, so its `GraphemeTable` entry moves with it and is re-keyed to
  its new column. Every row but the last of a logical line is marked
  wrapped.
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
  a prompt in place is a possible follow-up, not this design. History is
  cut to the cap, oldest first; a cut may start history in the middle of a
  logical line. While re-wrapping, the output never holds more than twice
  the cap plus four times the screen (at least 1024 rows): once it reaches
  that, the oldest rows are dropped down to the cap plus twice the screen,
  also in the middle of one long logical line, so narrowing a dense
  history to one column never holds every row it produces at once. The
  margin of a second screen is what the prune may still remove afterwards;
  a row dropped this way takes its marks with it, as the history cap
  would.

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
- The property tests (`tests/reflow.rs`) check P1-P7 of the fixture:
  resize never panics, every invariant holds on both screens after any
  sequence of operations and resizes, a repeated or same-size resize
  changes nothing, a column change keeps the text of every logical line,
  shrinking and growing back (and growing and shrinking back) restore the
  text and the cursor's place in it, and a row-only change only moves
  rows.
- Selections, a scrolled-back viewport offset and images are not grid
  state; whoever adds them must decide how they follow a reflow.
