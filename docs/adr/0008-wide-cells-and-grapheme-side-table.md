# ADR 0008: Wide cells as spacer pairs, grapheme clusters in a per-line side table

**Status:** accepted · 2026-10-27

## Context

Until issue #4 every `tessera_grid::Cell` was one column and held one
`char`. Real output needs more:

- East Asian Wide and Fullwidth characters and most emoji take two
  columns (UAX #11).
- A user-perceived character is often several scalars: `e` + U+0301,
  `👍` + `🏽`, the ZWJ family `👨‍👩‍👧`, a flag made of two regional
  indicators, `☺` + VS16 (UAX #29 extended grapheme clusters, UTS #51
  emoji sequences).
- Zero-width scalars (combining marks, ZWJ, variation selectors, format
  characters) must not take a cell of their own.
- The emulator's edits (overwrite, ICH, DCH, ECH, EL, ED, scrolling) must
  never leave half a wide character behind.

`Cell` was 14 bytes padded to 16 (`char` + `Style`); a full screen and a
10 000-line scrollback should not grow because a few cells hold clusters.
`tessera-render` carried its own condensed width table, which disagreed
with `EastAsianWidth.txt` for 245 code points (deferred from #19).

The expected behaviour is pinned by
`crates/tessera-grid/tests/fixtures/wide_grapheme_spec.txt` (180 rows, each
citing UAX #11, UAX #29, UTS #51, xterm, kitty, Alacritty or WezTerm), whose
recommended semantics and decisions D1-D9 the milestone lead adopted. This
ADR records the model; the fixture holds the case-by-case evidence.

## Decision

### Widths come from `unicode-width` 0.2, clusters from `unicode-segmentation` 1

Both are MIT OR Apache-2.0 and already in the lockfile through cosmic-text
(0.2.2 and 1.13.3, Unicode 17), so no new crate enters the graph. The grid
uses `UnicodeWidthChar::width` (the non-CJK variant: ambiguous width is
narrow) for a lone scalar, exposed as `tessera_grid::char_width`, and
`UnicodeWidthStr::width` of the whole cluster text for a cluster: a
cluster's width is not the sum of its scalars (`👍🏽` is 2, not 4). A cell
holds at most two columns, so both answers are clamped to 1..2 (D4):
U+17D8 KHMER SIGN BEYYAL, the only scalar `unicode-width` 0.2.2 calls three
columns wide, is written as an ordinary wide pair (fixture rows A33, D37,
H18, H19).
`tessera-render` drops its table and asks `unicode-width` too, so the grid
and the renderer can never disagree about which cells are wide. The width
rows of the fixture pin the library answers; a dependency bump that
changes them fails a test first.

### A wide character is a WIDE cell plus a real SPACER cell

`Cell` gains `flags: CellFlags` (a `u8` bit set: `WIDE`, `SPACER`,
`CLUSTER`), which fits in the padding, so `Cell` stays 16 bytes (a test
asserts it). A wide character writes `Cell { ch, flags: WIDE }` and, in the
next column, `Cell { ch: ' ', flags: SPACER }` with the same `Style`, and
advances two columns; reaching the line end that way arms pending wrap on
the spacer. The spacer is a real cell so every column still has exactly
one cell, background passes need no special case, and the renderer can
simply skip it. The pair never straddles the end of a line.

- **One column left, DECAWM on (D1):** the stranded last column is blanked
  with the pen background, the row is marked wrapped and the character is
  written on the next row. This is Alacritty's behaviour; xterm and kitty
  leave the stranded cell untouched, but then it would be copied out as
  part of the wrapped line.
- **One column left (or pending), DECAWM off (D2):** the character is
  dropped and nothing changes (xterm, Alacritty). The grid has no DECAWM
  state; `tessera-term`'s `print` decides this.
- **A one-column grid** drops wide characters.
- **Splitting** (writing into either half, or an erase, insert or delete
  whose boundary falls between the halves) blanks the orphaned half with
  the pen background (xterm, Alacritty).

### Cluster text lives in a per-line side table

`Line::graphemes: GraphemeTable` maps a column to the full cluster text,
base included, for exactly the cells that carry `CLUSTER`; `Cell::ch` stays
the base scalar, so every reader that only wants one `char` per cell keeps
working. An entry always has 2 to 32 scalars (D6: the cap stops "Zalgo"
text from growing a line without bound; the longest RGI sequence has 10).
The table is `Option<Box<Vec<_>>>`: a table that never held a cluster
costs one pointer and no allocation, so `Line` grows from 56 to 64 bytes
and a scrollback of lines without clusters allocates nothing more. A line
on screen keeps its emptied vector for reuse, so rewriting clusters on the
same row does not allocate it each time; when a line scrolls into
history its table is compacted (freed if empty, shrunk to fit otherwise),
so a scrollback line pays only for the clusters it holds. A fat cell (an
inline small-string or a pointer per cell) would have doubled the cell or
the cost of every line for a feature most cells never use.

Each cluster costs its text (a `Box<str>` of at most 32 scalars) plus a
24-byte entry, on top of its 16-byte cell. The worst case is a line made
of nothing but full clusters: 32 four-byte scalars give 128 bytes of text
plus the entry per cell, about 13.5 KB for an 80-column line, ten times a
plain line's 1 344 bytes; "Zalgo" text of 31 two-byte combining marks per
letter (64-byte texts) is about 8.4 KB, six times. Ordinary text with a
few marks or emoji sequences per line costs a few hundred bytes more.

Every primitive that moves or clears cells keeps the table consistent:
overwrite and erase drop entries, ICH and DCH shift them with their cells,
scrolling and the alternate screen move whole lines, a recycled scrollback
line is cleared, and `Line::text` emits cluster text and skips spacers.

### Attachment happens in `Grid::put_char`

`put_char(ch)` first asks whether `ch` joins an existing cluster:

1. **Target (D7):** the cell left of the cursor, or the cell under it while
   a wrap is pending, resolved through a spacer to its wide base; never a
   cell on another row (a mark after a soft wrap and a CR is dropped), and
   never a cell showing `' '`. The grid keeps no "last written cell", so a
   mark after cursor motion joins whatever is left of the cursor (kitty and
   WezTerm would drop it).
2. **Gate (D4):** the cluster plus `ch` must be one UAX #29 grapheme *and*
   at most 2 columns wide, unless the cluster already contains a ZWJ (an
   RGI ZWJ sequence can be 3 wide while incomplete, `👩‍❤` before the VS16).
   UAX #29 alone would put 3 or 4 columns of content into one cell pair
   (`a` + `🏽`, two leading Hangul jamo); those scalars become cells of
   their own instead, as UTS #51 describes a free-standing modifier.
3. **Room (D3):** a cluster that grows from 1 to 2 columns (a flag's second
   regional indicator, VS16, a spacing mark) writes its spacer over the
   next column, without shifting the rest of the line, even in insert mode.
   With the base in the last column there is no room: the scalar does not
   attach (a zero-width one is dropped, a width-1 one becomes an ordinary
   character). No base is ever relocated, so attachment needs no DECAWM
   knowledge and never scrolls. A cluster that shrinks from 2 to 1 (VS15
   on a wide emoji) turns its spacer into a blank and moves the cursor
   back onto it (kitty, xterm with `OPT_EMOJI_WIDTH`).

A zero-width scalar that does not attach is dropped (D5): nothing to its
left, a blank to its left, a format character that is its own grapheme
(ZWSP, WJ, BOM, soft hyphen), or a refused growth. Only the
width-1 Prepend characters (U+0600..U+0604 such as ARABIC NUMBER SIGN,
U+06DD, U+110BD, U+110CD) take a cell, and under GB9b the next scalar,
even a space, joins them into a two-column cluster (fixture row D36); the
row's column count still matches the library width of the text. The
zero-width Prepend characters (U+0605, U+070F, U+0890..U+0891, U+08E2,
U+0D4E and others) are dropped like any other zero-width scalar that does
not attach. VS15/VS16 on a base without emoji variation attaches without
changing the width (D9, as UAX #29 and kitty; xterm drops it).

`Grid::would_attach(ch)` is the same decision as a pure query (D8). The
emulator needs it because insert mode, the pre-wrap of insert mode and the
DECAWM-off "overwrite the last column" move must all be skipped for a
scalar that joins the previous character: inserting `width(🏽)` cells for a
modifier would open a two-cell gap, and moving the cursor first would
redirect a mark to the wrong cell. `State::print` therefore asks
`would_attach` first, then applies DECAWM and IRM using the scalar's width
(`insert_chars(2)` for a wide character), and stays a thin caller of the
grid.

### Plain text stays on a fast path

Segmenting every scalar would make printing several times slower than a
one-cell-per-`char` grid, so `put_char` proves the common cases cheaply
and only segments what is left. Each shortcut is checked against the full
rule by a unit test:

- Printable ASCII, Latin-1 and Latin Extended, IPA, Greek, Cyrillic,
  Armenian, Hebrew, Arabic and Georgian letters, Latin Extended
  Additional (Vietnamese), dashes and quotes (U+2010..U+2027), box
  drawing and block elements (U+2500..U+259F) and braille
  (U+2800..U+28FF, btop's graphs), all grapheme break
  property `Other`, one column and not `Extended_Pictographic` apart from
  U+00A9 and U+00AE, never join each other, so one of them written after
  another is a plain cell write with no width lookup or segmentation. The
  test checks every pair.
- A simple character after a lone wide character never joins it (no
  two-column scalar is `Prepend` or a conjunct linker, and no simple
  character extends anything). The test checks every two-column scalar of
  planes 0 and 1 against a sample of simple characters and every simple
  character against a sample of two-column bases; the fast path also
  looks through the spacer.
- A two-column scalar after a lone scalar of nonzero width would make a
  cluster at least three columns wide, which the gate refuses, unless it
  is an emoji modifier. The test checks every two-column scalar of planes
  0 and 1 (and a sample of planes 2 and 3) after a sample of 16 bases,
  not every base; a property test tries arbitrary bases.
- A simple character after a cluster ending in a simple character or a
  combining diacritic always starts a new grapheme (checked for every
  tail and every follower against samples of the other), and a combining
  diacritic after a simple character always joins it at one column
  (checked for every pair).
- In the default modes (DECAWM on, IRM off) `State::print` calls
  `put_char` directly instead of asking `would_attach` first.

Measured with the `print_throughput` harness (`cargo test --release -p
tessera-term --test wide -- --ignored --nocapture print_throughput`): one
line of 40 to 70 characters printed 200 000 times on a 50x200 terminal, in
ns per character on an Apple-silicon Mac, best of five runs, main and this
branch measured back to back:

| workload | main before #4 | #4 first round | #4 now | now / main |
|---|---|---|---|---|
| ASCII | 5.1 | 14.7 | 6.9 | 1.4x |
| CJK | 6.9 | 82 | 17.6 | 2.6x |
| CJK mixed with ASCII | 6.6 | | 10.8 | 1.6x |
| emoji between ASCII words | 6.2 | | 9.2 | 1.5x |
| accented Latin, Greek, Cyrillic | 6.8 | 45 | 9.0 | 1.3x |
| Vietnamese | 6.7 | | 8.8 | 1.3x |
| Hebrew | 6.8 | | 9.0 | 1.3x |
| Arabic | 7.0 | | 9.2 | 1.3x |
| box drawing | 7.0 | | 9.3 | 1.3x |
| braille (btop graphs) | 6.7 | | 9.2 | 1.4x |
| Armenian | 6.8 | | 9.3 | 1.4x |
| Georgian | 7.3 | | 9.8 | 1.3x |
| smart punctuation | 6.3 | | 8.5 | 1.3x |
| `e` + U+0301 pairs | 6.3 | | 30 | 4.8x |

Before the rule that a simple character never joins a lone wide
character, the emoji and mixed CJK lines measured about 4.3x and 2x.

Text that actually builds clusters pays for segmentation and for the
cluster text's allocation; main never built clusters at all. These are
known costs, measured by the verification of #4 with the same method and
not optimised yet (main ns/char, then the ratio): Thai 6.7 (9.0x), Hindi
7.4 (10.1x), Bengali 7.5 (10.1x), Tamil 7.1 (10.4x), Hebrew with niqqud
6.7 (9.9x), Arabic with harakat 6.6 (11.0x), flags 6.5 (9.7x), ZWJ
families 6.9 (7.6x), skin-tone modifiers 6.4 (4.2x), Zalgo with 30 marks
per letter 7.0 (30x). Text outside the fast-path ranges that builds no
clusters pays for segmentation alone: geometric shapes 4.4x (left out on
purpose: U+25B6, U+25C0 and U+25FB..U+25FE are `Extended_Pictographic`),
mathematical symbols 3.4x, kaomoji 3.3x, Persian 3.2x, polytonic Greek
3.0x, Urdu 2.7x.

### Rendering

`Shaper::shape_line` keeps its `&[Cell]` signature, skips spacer cells and
treats a cell as wide when the grid marks it `WIDE`, or when it is not a
cluster and its character is two columns on its own; the wide glyph keeps
`cell_span` 1 and is centred over both columns, as ADR 0007 planned.
`prefers_emoji` is rebuilt on the new width test, so text-presentation
pictographs (U+263A, U+1F321) no longer go to the colour emoji font.

## Consequences

- `中文` occupies four cells and renders as two glyphs; emoji with skin
  tones, ZWJ sequences, flags, keycaps and combining marks occupy the cell
  count `unicode-width` gives the whole cluster.
- Supersedes these "Known limitations" of ADR 0007: "The grid does not
  model wide cells yet" and "Wide-character detection and emoji
  presentation use a condensed table". "Fallback is per character, not per
  grapheme cluster" still holds for shaping, see below.
- Only the base scalar of a cluster is shaped this wave. Compared with
  main, where every scalar had a cell and was drawn, the scalars that now
  join a cluster are **not drawn at all** until `shape_line` reads
  `Line::cluster`: combining marks (`e` + U+0301 shows a bare `e`), Indic
  vowel signs and conjunct tails, skin-tone modifiers (`👍🏽` shows a
  default-tone `👍`), the second regional indicator of a flag (`🇸🇪` shows a
  lone `🇸`) and everything after a ZWJ (`👨‍👩‍👧` shows `👨`). The text is
  intact in the grid, in `Line::text` and in copy and search. Shaping the
  full cluster text (which also centres `e` + U+0301 over one cell, the
  second #19 deferral) is deferred to the render work tracked with #21.
- `Shaper::shape_line` draws a cell wide when the grid marks it `WIDE`, or
  when it is not a cluster and its character is wide on its own; a
  cluster narrowed by VS15 (`⌚` + U+FE0E) is drawn in one column.
- Known limitations kept as documented behaviour in the fixture: a flag
  growing in insert mode overwrites the next cell instead of inserting
  (H08); spacing-mark and Indic-conjunct clusters follow the library's
  widths (D34, D35); a width-1 Prepend character swallows the scalar
  after it (D36); an Arabic lam-alef pair, which `unicode-width` counts
  as one column for the pair, takes two cells because the grid works per
  cluster (A30).
- A space followed by a combining mark loses the mark (a blank cell never
  takes one); NBSP + mark, the usual standalone form, works.
- Code that edits `Line::cells` directly must keep the WIDE/SPACER pairing
  and the table in step; nothing outside `tessera-grid` builds `Cell` or
  `Line` with struct literals.
