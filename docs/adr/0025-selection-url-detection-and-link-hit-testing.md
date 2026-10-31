# ADR 0025: Selection, URL detection and link hit-testing

**Status:** accepted · 2026-10-31

## Context

Issue #30 asks for mouse selection (click and drag, double-click words,
triple-click lines, block selection, shift-click), Select All, Copy, and
command-hover and command-click on URLs. The renderer has drawn a
`Selection {start, end, kind}` and `OverlayKind::Underline` since #22; the
core had no selection, no text extraction and no URL detection. This ADR
covers the core half (part 1 of 3): OSC 8 storage is the next part and the
Swift gestures the last. The behaviour is pinned by
`crates/tessera-app-core/tests/fixtures/selection_spec.txt` (116 rows) and
the decisions in its `selection_spec.notes.md`, both from the
spec-researcher, read against iTerm2, kitty, WezTerm, Ghostty, VTE and
Alacritty sources.

Three constraints shape it:

- **Output keeps moving.** A selection must stay on its text while output
  scrolls the screen and while the view is scrolled back (ADR 0023), and
  must not keep text that is no longer on the screen.
- **Frames are hot.** The capture runs under the terminal lock on every
  frame (ADR 0014, 0016); a terminal without a selection must cost what it
  did, and one with a selection barely more.
- **No new dependency.** URL detection has to be hand-written.

## Decision

### Coordinates

Every call takes view-relative cells (`Point { row, col }`: row 0 is the
top row the view shows at its scroll offset); FFI calls take `col, row`.
The core stores the selection in **absolute logical-line numbers**: line
`n` is the line that was the screen's top row when `n` lines had entered
the history, so `n = scrollback_pushed - scrollback_len + index` for
logical line `index`, and the screen's top row is always line
`scrollback_pushed` (ADR 0023's count). A line keeps its number while it
scrolls up the screen and into the history; the view at offset `o` shows
lines `pushed - o ..`. Selection does not go through
`InputEvent::Mouse` (#12 routes mouse reporting, wave 9).

### Granularity and gestures

`App::select_begin(id, at, granularity, block)`:

- `Char` (one click): selects nothing (fixture A03) until
  `App::select_extend`, which selects whole cells from the clicked one to
  the pointer's, both included (D1: no half-cell snapping; the frontend
  extends only when the pointer reaches another cell, and extending onto
  the clicked cell selects it, A04).
- `Word` (double-click): the run of word characters, Unicode alphanumerics
  and `WORD_CHARS = "/-+\~_."` (iTerm2's default), or the run of
  whitespace under the click; any other character, `: = @ ? & % #`
  included, is selected alone (iTerm2's "other" class, C04, C05). Past the
  end of the logical line's text nothing is selected (D4, C09). Words
  continue across soft wraps and stop at hard line ends; CJK runs are not
  segmented further (B08). No configuration key (#47).
- `Line` (triple-click and more, D9: no smart selection in 0.1.0): the
  whole logical line, every soft-wrapped row at full width.

A drag extends by the same unit and keeps the unit first clicked whole
(C14). `block` selects the rectangle between the two points; its points
are reported as clicked, and a spacer or wide half is widened when the
text is cut (the renderer widens it when drawing, as before).

`App::select_adjust` (shift-click, D5) moves the endpoint **nearer** the
click (distance in cells along the lines; a tie moves the start, E04) to
the click's unit, keeps the granularity and block-ness, and is a no-op
without a selection (E05). iTerm2 does the same; the draft's "keep the
anchor" would have moved the wrong end after a forward drag (E02). The end
that stays becomes the anchor a following drag continues from.

`App::select_all` selects the history and the screen, or only the screen
on the alternate screen (F04); an empty terminal is selected too, with
empty text (F05). `App::select_clear` drops the selection.

### Text extraction

`App::selection_text` returns `None` without a selection, otherwise:

- soft-wrapped rows are joined with no newline; every hard line end gives
  `\n`, and so does every row of a block selection (D03: a block does not
  join wraps);
- **every blank after the last text of a logical line is dropped** (D2),
  but not blanks inside a line or at a soft wrap with text after it (H05).
  The grid cannot tell a space the program wrote from a cell it never
  touched (`Cell::BLANK` is `' '`), so written trailing spaces go too:
  a deviation from iTerm2 and kitty (`strip_trailing_spaces never`), which
  keep them;
- **the newlines of all trailing blank rows are dropped** (D8, H03, F03):
  Select All of a mostly empty screen copies only the text. Alacritty
  drops one, iTerm2 the last;
- a wide character is taken whole when either half is selected; spacer and
  padding (`CellFlags::PAD`) cells give nothing (B07, H09); a cluster gives
  its whole text (B05, B06);
- **a tab gives the spaces it moved over** (D7, H01): tabs are cursor
  motion in the grid, so the core cannot know one was there. iTerm2 and
  Alacritty copy a real tab;
- a selection of blanks only gives `Some("")` (D05, F05). The frontend
  leaves the pasteboard alone then (D11, 30-app).

**Copy-on-select is off** and has no option (#47): iTerm2's default is on.
Copying stays an explicit Copy.

### When the selection is cleared

On every look at the terminal (each selection call and every frame) the
selection is dropped when:

- a selected row on the live screen no longer holds the text it held when
  the selection was made (D6). Cells, clusters and the soft-wrap flag of
  the **whole row** are compared, not damage (D6b), so a full-screen
  program repainting the same text keeps it (G05) while one rewriting a
  selected line drops it (G03), even outside the selected columns. This is
  stricter than iTerm2, whose source clears only on SD, clear screen and
  width change: keeping a selection whose text silently changed would make
  Copy return text the user never saw;
- the history evicts or clears a selected line (G07; RIS and ED 3 too);
- the grid's size changes (G08; `App::resize` to another size also clears
  it at once), or the screen switches to or from the alternate screen
  (G09);
- `App::clear_scrollback` runs (G10): ⌘K moves the rows the selection
  numbered.

Every guarded row is compared where it is now: on the live screen, or in
the history when it scrolled there since the last look (a row rewritten and
then scrolled off between two looks changed while it was live). History
lines change otherwise only by eviction, clearing and resizing, each
handled above, and by a whole-screen erase (ED 2, RIS) clearing the newest
history line's soft-wrap flag; the history copies are therefore compared
when the history moved or the whole screen changed. A switch to the
alternate screen and back between two looks restores the same rows and is
not seen; the text is then still the text selected.

**Cost.** The selection keeps a copy of its rows that were on the live
screen when it was made (at most the screen) and, at each look, compares
only rows that can have changed:

- outside a frame (`has_selection`, `selection_text`, the gesture calls):
  the rows the grid reports damaged since the last look
  (`Grid::damage_since`), every row, and the guarded lines in the history,
  when the history moved or the whole screen changed;
- in a frame, after the capture: when the previous frame checked the
  selection with the view on the same lines, the guarded rows in view that
  the capture found unchanged still hold the text (the capture leaves the
  snapshot an exact copy of the rows in view), so only the rows the capture
  found changed are compared. A cursor moving over a selected screen
  compares nothing. Rows outside the view use the damage.

Without a selection nothing is compared. A property test drives random
output, frames at random offsets and looks, and holds both paths to a full
comparison of every guarded row.

### URL detection (`url.rs`)

`url::url_at(line, at)` finds the URL containing character `at` of a
logical line by rules U1 to U9 of the notes:

- **U1** schemes `http://`, `https://`, `ftp://`, `file://`, `mailto:`,
  ASCII case-insensitive, not preceded by an alphanumeric, `+`, `-` or
  `.` (J26); ssh, git, tel, javascript and others are not detected (J27);
- **U2** the URL is returned as typed (J25);
- **U3** it runs to the first character that is not an RFC 3986 character
  (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`) or a non-ASCII alphanumeric (IRIs,
  J30); `U+3002` ends it (J29);
- **U4** a trailing `. , ; : ! ? ' "` is stripped, repeatedly (J31);
- **U5** a trailing `)` or `]` is stripped while the URL holds more of it
  than of its opener (Wikipedia's `Foo_(bar)` keeps its parenthesis, J05;
  `(url)` loses the outer one, J06). kitty instead ends at the first closer
  after a matching opener before the URL, which cuts J06;
- **U6** `http`, `https` and `ftp` need a host character after `://`;
  `file` may have an empty host (`file:///etc/hosts`); every URL needs
  something after its scheme;
- **U7** `mailto:` needs text, `@`, text; bare e-mail addresses are not
  links (D14);
- **U8** soft wraps continue a URL, hard line ends stop it (the caller
  passes the logical line);
- **U9** a candidate (scheme to the end of U3) longer than 2083
  characters is not a link (D15, the OSC 8 cap of VTE and iTerm2).

When several candidates contain `at` (a URL quoted in another's query),
the earliest that passes every rule wins.

**Cost of one lookup.** Because of U9 a lookup reads at most 2083
characters to either side of `at`: a window of n <= 4168 characters. It
does O(n) work on it once (the run of URL characters, prefix sums of
`)`-minus-`(` and `]`-minus-`[`, the next `@` from each position, and one
walk down from the run's end over trailing punctuation and closers,
keeping the least prefix sum seen per closer) and then O(log n) per scheme
start before `at`: U5's "more closers than openers" for a candidate
starting at `s` and ending at `e` is `sum[e] - sum[s] > 0`, so the end U4
and U5 trim it to is a binary search in that walk. That is about 9 n
operations: on 2083-character lines of `ftp://` starts and `)`, the worst
measured was 18 748 steps and 66 microseconds (release, load about 50). A first version
recounted the brackets for every stripped closer of every candidate:
2083 characters of 220 `ftp://` and 763 `)` cost 160 ms, under the
terminal lock on every hovered frame. A test counts the steps of a lookup
on such lines and holds them under 64 per character; property tests hold
the lookup to a straight reading of the rules, and the App's windowed
cell walk to one over the whole logical line.

**Why no regex dependency.** The rules are a scheme match, a character
class scan and two trimming loops; a regex would express U1 to U3 but not
U5's balance count or U9's bound on the scan, would add a dependency and a
compile-time cost to the core, and would make the per-hover cost depend on
the engine. The hand-written scan is about 150 lines with a reference
implementation in its tests.

### Link hit-testing and hover

`App::link_at(id, at) -> Option<Link { url, span }>` maps the cells of the
logical line around `at` to characters (one per character cell: a
cluster's base scalar for its class, the whole cluster for the text;
spacers and padding skipped), runs `url_at`, and reports the URL and its
cells per view row (`(row, cols)`, end exclusive; a wide character's two
columns included; rows outside the view omitted). The trimmed punctuation
is outside the span (J34). Opening it, and which schemes may be opened,
is the frontend's decision (D-OPEN, 30-app).

`App::set_hover(id, Option<Point>)` stores the cell the pointer hovers
with the link modifier held. Every frame looks the link up again (the
output may have moved it) and draws one `OverlayKind::Underline` per row
of its span; nothing without a hover or a link.

### Frames

`App::render_view` locks the selection, captures the view as before, then
asks the selection for the frame's decorations: the selection clipped to
the rows in view (a linear selection starting above the view starts at
(0,0), one ending below ends at the last cell; a block keeps its columns;
one wholly outside is `None` while `has_selection` stays true) and the
hover's underlines. `Snapshot::frame` passes them to the renderer. The view
compares them with the last presented frame's, so an unchanged selection
and hover keep skipping frames (`FrameStatus::Skipped`), and a change draws
one without damage (the renderer compares coverage per row).
`Snapshot::capture` is unchanged.

### Locks

The selection and hover live in the session under their own mutex, taken
**after the view table and before the input and terminal locks**, never
the other way round: views → selection → input → terminal (ADR 0014).
Every selection call takes selection, then resolves the viewport (input,
terminal, input). Nothing takes the view table or the selection lock while
holding the terminal.

### FFI

In `crates/tessera-ffi/src/selection.rs`, with the conventions of
`session.rs` (`ffi_guard`, a `uint32_t` enum checked to
`TESSERA_ERROR_INVALID_ARGUMENT`, out-parameters reset on failure):
`tessera_session_select_begin(app, id, col, row, granularity, block)` with
`TesseraGranularity` (`CHAR = 0`, `WORD = 1`, `LINE = 2`),
`tessera_session_select_extend`, `tessera_session_select_adjust`,
`tessera_session_select_all`, `tessera_session_select_clear`,
`tessera_session_has_selection(app, id, *out)`,
`tessera_session_selection_text(app, id, buf, cap, *len)` and
`tessera_session_link_at(app, id, col, row, buf, cap, *len)` (UTF-8, no
NUL; `*len` is the size needed; when `cap` is short nothing is written and
the call returns `TESSERA_ERROR_INVALID_ARGUMENT`; no selection and no
link give `*len = 0`), and `tessera_session_set_hover(app, id, col, row,
active)`. The C test is `crates/tessera-ffi/tests/c/selection.c`.

## Measurements

`cargo xtask ab` against the merge base `75adbed`, release, instructions
retired, best of 7 interleaved base/head process pairs, Apple M4 under
wave load. Each case is 90 x (fill a 60x200 screen with the corpus line,
then 1 000 x (a random cursor move, `Snapshot::capture` at offset 0, the
frame's decorations, `Snapshot::frame`)). `<line>`: base against head
with no selection (bar 1.02). `sel_<line>`: head with no selection against
head with a full-screen linear selection (bar 1.05). The corpus lines are
the eleven of the wave 5 corpus, one representative line each (defined in
the probe; listed in the #30 report).

| corpus | no selection, head/base (bar 1.02) | cycles | full-screen selection / none (bar 1.05) | cycles |
|---|---|---|---|---|
| ascii | 1.000 | 0.986 | 1.019 | 1.014 |
| latin_diacritics | 1.000 | 1.019 | 1.020 | 1.039 |
| cjk | 1.000 | 0.976 | 1.020 | 1.033 |
| box_drawing | 1.000 | 0.987 | 1.020 | 1.002 |
| hebrew | 1.000 | 0.989 | 1.020 | 1.051 |
| arabic | 1.000 | 1.001 | 1.020 | 1.051 |
| punctuation | 1.000 | 0.998 | 1.019 | 1.039 |
| braille | 1.000 | 0.964 | 1.020 | 1.016 |
| emoji | 1.000 | 1.001 | 1.019 | 1.017 |
| sgr_heavy | 1.000 | 1.004 | 1.018 | 1.022 |
| combining_heavy | 1.000 | 0.989 | 1.017 | 1.012 |

Load average 60 to 105 during the run, so cycles are noisy; instructions
are the gating metric. With a selection, the cost is the per-row soft-wrap
flag comparison and the clip, about 2 %.

The first measurement showed head at 0.976 instructions but 1.17 cycles
with no selection, and the same with all selection work removed from the
measured loop: comparing guarded rows with `Cell`'s derived `==` gave
`Style::eq` more callers, and the compiler stopped inlining it into
`Snapshot::sync_row`, the capture's per-cell comparison. The guard now
compares cells field by field (`same_cells`), and the capture's machine
code matches the base again. A first version of the frame check compared
every row the grid reported damaged, which for a cursor move is the range
between its old and new row: 1.95x with a selection, hence the
capture-driven frame check above.

## Consequences

- The Swift view (30-app) builds the gestures, Copy and command-click on
  the C API without a Rust change.
- OSC 8 hyperlinks (30-links) can add explicit links to `link_at` next to
  detection; how the two combine is that issue's decision.
- Search (#28) can number its matches the same way and reuse the
  decorations path (`OverlayKind::Match`).
- `WORD_CHARS` and copy-on-select become configuration in #47.
- `Terminal::set_max_scrollback` exists (for a test hook today); applying
  the configured `scrollback_lines` to sessions is still to do.
