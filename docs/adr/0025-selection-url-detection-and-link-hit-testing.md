# ADR 0025: Selection, URL detection and link hit-testing

**Status:** accepted · 2026-10-31 · amended 2026-11-04 (#191: the guarded
rows, CJK around URLs, the lookup's fast path)

## Context

Issue #30 asks for mouse selection (click and drag, double-click words,
triple-click lines, block selection, shift-click), Select All, Copy, and
command-hover and command-click on URLs. The renderer has drawn a
`Selection {start, end, kind}` and `OverlayKind::Underline` since #22; the
core had no selection, no text extraction and no URL detection. This ADR
covers the core half (part 1 of 3): OSC 8 storage is part 2 (ADR 0026) and
the Swift gestures the last. The behaviour is pinned by
`crates/tessera-app-core/tests/fixtures/selection_spec.txt` (123 rows; 141
since #191) and
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

`App::select_all` selects the history and the screen up to the last row
with text, or only the screen on the alternate screen (F04): the blank rows
below it are left out, so the next line of `yes` or `tail -f`, written on
the cursor's blank row, does not change a selected row and clear the
selection (G07f, G07g; output over the last text row still does, G07h); an
empty terminal is selected whole, with empty text (F05). `App::select_clear` drops the selection.

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
  leaves the pasteboard alone then (D11, 30-app). On the alternate screen
  a selection with no text gives `None` (#254): a full-screen program's
  blank screen holds nothing to copy.

**Copy-on-select is off** and has no option (#47): iTerm2's default is on.
Copying stays an explicit Copy.

### When the selection is cleared

On every look at the terminal (each selection call and every frame) the
selection is dropped when:

- a **guarded row** no longer holds what it held when the selection was
  made (D6). The guarded rows are every row whose change can change the
  text the selection copies (#191, lead decision D4, 2026-11-04):
  - the selected rows on the live screen;
  - the newest history line, when it is selected: a whole-screen erase
    (ED 2, RIS) clears its soft-wrap flag, which splits a selected line
    that wraps from the history onto the screen's top row (`abcdefgh`
    would copy as `abcde\nfgh`, G07i);
  - the rest of the last selected row's logical line below the selection:
    whether blanks at the end of the selection are kept or dropped (D2)
    depends on text after them on the same logical line (G07j, G07k).
    When that logical line is still open (its last row is the screen's
    bottom row and soft-wrapped), the row that continues it does not exist
    yet: the selection is cleared as soon as any row is added below the
    bottom row (#254, the #191 adversarial repros, where a later scroll put
    text after the selected blanks). The row added may stay blank, or the
    selection may end in no blanks; its text did not change then, and it is
    cleared anyway (G07k's over-clear). Clearing
    too often is safe, keeping a stale copy is not (lead decision,
    2026-11-05).

  Older history lines are not guarded: they change only by eviction,
  clearing and resizing, handled below. Cells, clusters and the soft-wrap
  flag of each **whole row** are compared, not damage (D6b), so a
  full-screen program repainting the same text keeps it (G05) while one
  rewriting a selected line drops it (G03), even outside the selected
  columns. This is stricter than iTerm2, whose source clears only on SD,
  clear screen and width change: keeping a selection whose text silently
  changed would make Copy return text the user never saw. The guarantee:
  while the selection is kept, Copy returns the text it would have returned
  when it was made, or, after eviction trimmed its start, the end of that
  text;
- the history drops a selected line other than by eviction at its cap: ED 3
  and RIS clear it (G07d; the selection stays when it holds only screen
  rows, G07e). Eviction at the cap is different (LD-1, decided 2026-11-02):
  when it removes only the top of the selection and its end is still there
  (G07, G07c), the start moves to the oldest line left, at column 0 (a block
  keeps its columns), and the selection stays, so Select All and Copy during
  continuous output copy the text that is left (iTerm2 clips a selection
  that moves above the first line rather than dropping it). A selection with
  no line left is cleared (G07b). The core tells the two apart at each look
  by `Grid::history_clear_generation`, a count the grid bumps on every
  history clear (ED 3, RIS, ⌘K's `clear_to_cursor`) and never on the
  scroll path (#225): if it moved since the last look the history was
  cleared, otherwise lines gone were evicted. ED 3 followed by output that
  refills the history to its cap before the next look is a clear (until
  #225 the history's length decided, and that read as an eviction). A
  clear and an eviction between the same two looks count as a clear;
- the grid's size changes (G08; `App::resize` to another size also clears
  it at once), or the screen switches to or from the alternate screen
  (G09);
- `App::clear_scrollback` runs (G10): ⌘K moves the rows the selection
  numbered.

Every guarded row is compared where it is now: on the live screen, or in
the history when it scrolled there since the last look (a row rewritten and
then scrolled off between two looks changed while it was live, and a
guarded newest history line is unwrapped by a whole-screen erase). History
lines change otherwise only by eviction, clearing and resizing, each
handled above; the guarded rows in the history are therefore compared when
the history moved or the whole screen changed. A switch to the
alternate screen and back between two looks restores the same rows and is
not seen; the text is then still the text selected.

**Cost.** The selection keeps a copy of its guarded rows (at most the
screen and one history line) and, at each look, compares only rows that
can have changed:

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
output (RIS, the alternate screen, ED 0 and ED 2, scroll regions and
wrapping lines among it), frames at random offsets and looks, with the
history at a random cap of 0 to 5 lines or 1 000, and holds both paths to
a full comparison of every guarded row; it also checks that a kept
selection copies the text it copied when it was made (or the end of it).

The gestures themselves run under the terminal lock and cost what their
unit costs: a word drag re-reads the word on every extend, about 12 ms on a
single 2.5-million-character word, and Select All and word expansion are
linear in the history or the line. They are user-initiated, one per event,
so this is accepted rather than bounded.

### URL detection (`url.rs`)

`url::url_at(line, at)` finds the URL containing character `at` of a
logical line by rules U1 to U9 of the notes:

- **U1** schemes `http://`, `https://`, `ftp://`, `file://`, `mailto:`,
  ASCII case-insensitive, not preceded by an ASCII alphanumeric, `+`, `-`
  or `.` (J26); ssh, git, tel, javascript and others are not detected
  (J27). A non-ASCII letter before the scheme does not block it (#191,
  lead decision D3, 2026-11-04): CJK prose puts no space before a URL, so
  `詳細はhttps://example.com/を参照` is linked (J38). Until #191 any
  Unicode alphanumeric blocked it;
- **U2** the URL is returned as typed (J25);
- **U3** it runs to the first character that is not an RFC 3986 character
  (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`) or a non-ASCII alphanumeric (IRIs,
  J30); `U+3002` ends it (J29). **After the host** (the text from the
  scheme to the first `/`, `?` or `#`) a CJK character also ends it (D3):
  the blocks of Han (with radicals, strokes and compatibility ideographs),
  Hiragana, Katakana, Bopomofo (U+3100-U+312F, extended U+31A0-U+31BF;
  #254) and Hangul, CJK symbols and punctuation
  (U+3000-U+303F, U+3200-U+33FF) and the fullwidth and halfwidth forms
  (U+FF00-U+FFEF) (`url::is_cjk`; blocks, not script tables, so no
  dependency). CJK prose follows a URL without a space (J39, J40, J52).
  **In the host** a CJK character ends it only right after an ASCII label
  or port: letters, digits and `-` that follow a `.` or `:` of the host
  (#191 critic round 1, lead decision E1, 2026-11-04), so a URL with no
  path stops before the prose too (`https://example.comを参照`,
  `mailto:a@b.jpまで`, `https://a.jp:8080日本`: J45-J48). No top-level
  domain mixes ASCII and CJK, so an IDN host keeps its letters: a CJK
  label starts right after its `.` (`例え.jp`, `www.例え.jp`, `例え.テスト`;
  J30, J43, J49, J50), and the `:` of `user:password@` is followed by
  `password@`, not by the host (J51). The costs, as measured (#254, and
  tests in `url.rs`): a path written in raw CJK (`/wiki/日本`) is cut where
  the CJK starts (J41), while its percent-encoded form, which browsers
  copy, is not (J42); an IDN label that mixes ASCII and CJK is cut at its
  CJK (`https://www.a日本.jp` links `https://www.a`), and so is a password
  (`https://u:p日本@x.jp/` links `https://u:p`) and a mail address
  (`mailto:a.b日本@x.jp` is no link: no `@` is left); a host with no
  ASCII label before its CJK, or a `.` right before it, keeps the prose
  (`https://localhostで`, `https://[::1]日本` and `https://example.com.を`
  are linked whole). A zero-width joiner (U+200D) and the bidi controls
  (U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069;
  `url::is_hidden_url_end`) end a URL anywhere (#254): neither is a URL
  character, and in the cells the terminal drops a bidi control (it joins
  no cluster), while a ZWJ joins the cluster of the character before it,
  so that cell ends the link (`https://go`, U+200D, `od.com/` links `https://g`): a
  joiner cannot splice two runs of text into one host. Other invisible
  scalars that join a cluster (GCB=Extend: ZWNJ U+200C, the variation
  selectors, CGJ U+034F, the tag characters U+E0020-U+E007F) stay in the
  link's text, as the cluster does; none of them makes a host the text
  does not show (UTS 46 maps the variation selectors and CGJ away,
  CONTEXTJ rejects a ZWNJ after Latin, tags are invalid in a host), so S3
  holds. ZWSP, WJ, BOM, SHY and the bidi controls are GCB=Control: the
  terminal drops them when they are printed, and the link is the text the
  cells show. The hover walk reads each cell's base scalar and checks for
  a hidden end only where it can change the answer (see "Hover and
  eviction" below);
- **U4** a trailing `. , ; : ! ? ' "` is stripped, repeatedly (J31); so
  is a trailing `-` of a URL that U3 cut at a CJK character (#254:
  `https://a.jp-日本` links `https://a.jp`, while `https://a.jp/x-` keeps
  its `-`);
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
characters to either side of `at`: a window of n <= 4168 characters. Every
scheme has its `:` at most six characters after its start, so without a
`:` from the start of the run of URL characters around `at` to six
characters past it there is no candidate, and the lookup ends after that
scan, without allocating (#191: the #30 critic measured a hover over
plain text at 41 ns before the lookup built its window for every point and
166 ns after). Otherwise it walks the candidate starts from the run's
first character to `at`, keeping the counts of `)`-minus-`(` and
`]`-minus-`[` before the start as it goes. Matched schemes never overlap
(no scheme's tail starts another), so the scheme ends it asks about only
increase, and so do the host ends after them and the `@` after those: the
next separator, CJK cut and `@` are found by forward-only scans that read
the window about once (a position that is no CJK cut for one candidate is
none for a later one, whose host covers it too). Each distinct candidate
end gets one walk down over trailing punctuation and closers, recording for
each closer the least `)`-minus-`(` (or `]`-minus-`[`) count of the text
before it at that closer and every closer above it. The ends are few, at
most five per lookup: the starts whose scheme ends at or before `at` share
three, and as schemes are at least six characters long and never overlap,
at most two starts lie within a scheme's length of `at`. U5's "more
closers than openers" for a candidate starting at `s` and ending at a
closer `e` is the count at `e` minus the count at `s` being positive, so
the end U4 and U5 trim a candidate to is a binary search in its end's
walk: O(log n) per scheme start before `at`. A CJK cut in the host asks
whether the ASCII label before it follows a `.` or `:`; the answer is
cached for that CJK position, since every later candidate asks about the
same one (#254: 297 `mailto:` starts sharing a host whose last label is
2 000 letters long took 612 921 steps for a lookup in the middle of the
4 081 characters without the cache, about 150 per character, and take
30 630 with it, 7.5 per character; a test holds that line under 20 per
character). The lookup allocates nothing
but the closer lists of a candidate that ends in closers, at most the
window. A first version recounted the brackets for every stripped closer
of every candidate: 2083 characters of 220 `ftp://` and 763 `)` cost
160 ms, under the terminal lock on every hovered frame; #30's prefix-sum
version did that line in about 66 microseconds, and the current one costs
about 410 000 instructions there (see the measurements). A test counts the steps of a lookup
on such lines and holds them under 64 per character; that count is the
code's own, so the same lines are also a `cargo xtask ab` case, whose
instruction counts come from the processor (see the measurements).
Property tests hold the lookup to a straight reading of the rules on lines
of up to 80 and of 2 000 to 4 500 characters, and the App's windowed cell
walk to one over the whole logical line. The cell walk does the same
`:` pre-scan before it collects anything (#254): from the point it walks
back over the run of URL characters only, and when that run holds no `:`
it reads at most six characters on for one; without one there is no
URL, so hovering a long line of plain text costs its run of URL
characters, not a window of 4 168 cells.

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
is the frontend's decision (D-OPEN, 30-app). A cell that carries an OSC 8
hyperlink (ADR 0026) reports that link instead, before detection runs.

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

### URL lookups (#191)

`cargo xtask ab` against the merge base `d0b49a1`, release, instructions
retired, best of 7 interleaved process pairs, Apple M4, load average 14 to
120 (re-run after lead decision E1 at load 60 to 80). `url::url_at` at every position of a 200-character line of the
corpus line (`plain`, bar 0.5), of that line with
` https://example.com/a/b?q=1 ` inserted in the middle (`url`, bar 1.05),
and at the positions inside that URL only (`inside`, bar 1.05); the
`sgr_heavy` line is its visible text. Head against base:

| corpus | plain | url | inside |
|---|---|---|---|
| ascii | 0.192 | 0.372 | 0.894 |
| latin_diacritics | 0.212 | 0.356 | 0.893 |
| cjk | 0.202 | 0.244 | 0.894 |
| box_drawing | 0.867 | 0.885 | 0.894 |
| hebrew | 0.313 | 0.380 | 0.894 |
| arabic | 0.315 | 0.372 | 0.894 |
| punctuation | 0.364 | 0.464 | 0.894 |
| braille | 0.847 | 0.879 | 0.894 |
| emoji | 0.903 | 0.896 | 0.894 |
| sgr_heavy | 0.195 | 0.378 | 0.894 |
| combining_heavy | 0.323 | 0.524 | 0.894 |

The box-drawing, braille and emoji lines hold no URL character, so both
sides return at the first check (is the point a URL character?): the 0.5
bar cannot apply to them (waived, lead decision E2, 2026-11-04), and the head's smaller entry (the body kept out
of line) is what makes it 0.85 to 0.90. Lines of `ftp://` starts and `)`
(the step test's lines, 2 083 characters, lookups at four points) cost
1.14 to 1.19x base (1.16 for 220 and 763 after E1), about 410 000 instructions per lookup on 220 starts
and 763 closers: the forward scans for each candidate's host end replace
the base's per-position tables, which the plain and URL lines no longer
pay for. `cargo xtask ab diff` over 40 000 seeds of random lines (up to
4 500 characters) agrees with base on every line D3 and E1 cannot change
(no non-ASCII alphanumeric right before a scheme, no CJK after a scheme:
2 846 of the first 10 000 seeds, so about 11 400), and over 10 000 seeds
with a copy of the in-tree reference on all.

### Hover and eviction (#254)

`cargo xtask ab` against `5e2511d` (its crates are those of the merge base
`72deca5`), release, instructions retired net of process overhead, best
of 15 interleaved process pairs, Apple M4, load average 4 to 15. `u_<shape>` is `url::url_at` at the
shape's points; `h_<shape>` is `App::link_at` (the hover: cell window and
`url_at`) on the same text fed into a 200-column test session. Shapes:
`plain` (a 200-character line with one https URL, every position), `ftp`
(220 `ftp://` and 763 `)`, four points), `mailto` (297 `mailto:`, a
2 000-letter label and a CJK end, four points), `e1idn` and `e1dota` (the
E1 CJK shapes, every position), `nocolon_run` (2 000 letters, no `:`,
every tenth position) and `nocolon_prose` (the ascii corpus line cycled to
2 000 characters, no `:`, every tenth position). The `x_` shapes are
hovers over cells that hold clusters, fed into ten rows and looked up at
every tenth column: `x_comb` (`https://a.b/` and 1 900 `e` + U+0301),
`x_bigcl` (`https://a.b/` and 1 000 cells of `a` with 31 combining marks)
and `x_comb_nocolon` (2 000 `e` + U+0301, no `:`). Head against base (bar
1.05):

| shape | `url_at` | cycles | hover | cycles |
|---|---|---|---|---|
| plain | 0.569 | 0.549 | 0.368 | 0.340 |
| ftp | 0.479 | 0.438 | 0.732 | 0.702 |
| mailto | 0.726 | 0.658 | 1.001 | 0.943 |
| e1idn | 0.779 | 0.761 | 0.283 | 0.284 |
| e1dota | 0.681 | 0.612 | 0.257 | 0.256 |
| nocolon_run | 0.791 | 0.502 | 0.484 | 0.476 |
| nocolon_prose | 0.578 | 0.492 | 0.010 | 0.011 |
| x_comb | | | 0.925 | 0.967 |
| x_bigcl | | | 0.908 | 1.011 |
| x_comb_nocolon | | | 0.485 | 0.469 |

`fs_<line>` feeds 1 500 lines of a corpus line (cycled to about 190
columns) into a 60x200 session that holds a selection of its first 59
rows, with a look (`has_selection`) after every line (bar 1.03): 1.000 on
all eleven corpus lines (cycles 0.92 to 1.09 under load). The generation
compare replaces a length compare, so the feed path is unchanged.

Three codegen findings shaped the code. `is_url_char` tested ASCII
characters by scanning the class string (`str::contains`); a `matches!`
over the same set slowed the `rposition` scan of `url_at` (1.47x on
`nocolon_run`), and a 128-bit mask (one shift and one test) made it 0.79.
Merging `is_cjk`'s ranges (Bopomofo) changed its inlining; an early
`< 0x1100` exit gives ASCII one compare. A first hover window walked the
run of URL characters twice (1.62x on `h_ftp`) and looked each cell's line
up again; it now collects on the way back and keeps the current row.
With no `:` in the run of URL characters before the point, nor within
`COLON_REACH` characters after it, the hover stops after that walk and
builds no window for `url_at`, hence 0.010 on prose.

Reading each cell's cluster to find a hidden URL end (U3) cost 1.26x to
2.47x on the `x_` shapes, since a cell that holds a cluster has its text
in the line's side table. The walk, the `:` pre-scan and the window
therefore read base scalars, and only a URL that `url_at` finds on them
is checked: the cells outside its span (every cell of the window when it
finds none), since a hidden end there can change which start wins, the
U1 letter before a scheme or the U4 `-` strip at a CJK cut, and the
link's text, scanned once for the lead bytes of a hidden end. A hidden
end in either sends the lookup to the exact path, which reads every cell
of the window with its cluster and runs `url_at` again. A scan per
cluster inside the span was still 1.14x on `x_bigcl`. A differential run
of the base against the head (`url_at` on 220 000 lines, the hover on
45 000 sessions) found no difference.

## Consequences

- The Swift view (30-app) builds the gestures, Copy and command-click on
  the C API without a Rust change.
- OSC 8 hyperlinks (ADR 0026) add explicit links to `link_at` next to
  detection: a cell with an OSC 8 link reports that link (URI, and every
  cell of the same link entity in the view as the span, which hover
  underlines whole) and only a cell without one runs detection.
- Search (#28) can number its matches the same way and reuse the
  decorations path (`OverlayKind::Match`).
- `WORD_CHARS` and copy-on-select become configuration in #47.
- `Terminal::set_max_scrollback` exists (for a test hook today); applying
  the configured `scrollback_lines` to sessions is still to do.
