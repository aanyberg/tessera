# ADR 0026: OSC 8 hyperlinks

**Status:** accepted · 2026-11-02

## Context

Issue #30 part 2: programs (`ls --hyperlink`, `gcc`, `git`, `delta`, `rg
--hyperlink-format`) mark text as a link with `OSC 8 ; params ; URI ST`,
and Command-click opens it. The core must store which cells belong to which
link, keep that right through every edit, resize and scroll into history,
and give `App::link_at` (ADR 0025) the link before it tries to detect a URL
in the text. The behaviour is pinned by
`crates/tessera-term/tests/fixtures/hyperlink_spec.txt` (39 rows, from the
spec-researcher, read against the OSC 8 specification, VTE, kitty,
WezTerm and Ghostty).

Constraints:

- **No `Cell` growth.** `Cell` is 20 bytes (ADR 0013) and a screen plus
  10 000 lines of history is millions of them; a link pointer per cell is
  out.
- **`Line` does not grow either.** It is 64 bytes, and a second per-line
  pointer (72 bytes) measured about 1 % more instructions per plain
  character on top of the open-link test: a stride that is not a power of
  two costs an extra instruction in every row lookup, and `rotate_left`
  swaps 72 bytes per row (40 million line feeds cost 6 % more). ascii came
  to 1.023 against the 1.02 bar.
- **Memory is bounded under hostile input.** A program can open a link per
  character forever.
- **The per-character path is hot** (ADR 0013, 0016): no link open must
  cost at most 1.02 times the instructions of the merge base on the term
  performance corpus.

## Decision

### Storage: a flag and the grapheme side table

`CellFlags::LINK` (bit 4) marks a cell that belongs to a link. The link
itself is stored per line in the same side table as grapheme clusters
(ADR 0008), keyed by column. `GraphemeTable` holds two sorted vectors
behind its one `Option<Box<..>>`: the clusters and the links. Its
editing methods (`remove`, `remove_range`, `shift_right`, `delete`,
`clear`, `compact`, equality) apply to both, so every site that already
keeps the clusters in step with the cells (blanking, overwriting, ICH,
DCH, rectangular scrolls, `cut_or_pad`) keeps the links in step without a
new call. `Line::hyperlink(col)` reads one; `Line::graphemes.links()` lists
them. A line without clusters or links still allocates nothing.

A wide character has the flag and a table entry on both halves, so a hit
test on the spacer finds the link. A cluster that grows from one column to
two (VS16) gives its new spacer the base's link; the combining scalars
that join a cell do not change its link.

### Entities and the pool

A `Hyperlink` is a cheap clone of one `Arc`: URI, optional `id`, and a
reference to the pool's shared state (the live and byte counters and the
`id` index). All cells of one OSC 8 open share
it, so "the same link" is pointer identity (`same_link`); `==` compares
URI and `id`.

- A link **without an `id`** is a new entity per open (VTE: an auto id per
  OSC 8 run; iTerm2 would merge adjacent runs of one URI, D-L4).
- A link **with an `id`** is the live entity with the same `id` and URI if
  there is one, so separate runs group for the hover underline (OSC 8:
  "cells with the same URI and the same nonempty id are underlined
  together"); same `id` with another URI, or another `id`, is another
  entity. Ids are compared as exact bytes.
- The grid's `LinkPool` keeps a `HashMap<(id, uri), Weak<..>>` of the
  entities that have an `id` (the strings are shared with the entity, not
  copied), a counter of **live** entities and a counter of their URI and
  `id` bytes. An entity lives while a cell on a screen or in scrollback, or
  the grid's active link, holds it: erasing the cell or evicting its
  history line frees it. When the last holder drops, the entity takes its
  count and bytes off the counters and removes its own index entry (only
  if that entry still points at a dead entity, so a successor opened under
  the same `id` keeps its entry). The index therefore never holds a dead
  entry for longer than the drop takes: 1 000 000 id'd links rewritten
  over one cell leave one live entity and one index entry
  (`hyperlink.rs` unit tests).
- **Caps: `MAX_LIVE_LINKS` = 65 536 live entities and
  `MAX_LINK_TABLE_BYTES` = 16 MiB of their URI and `id` text.** A link
  that would pass either is refused: `Grid::begin_hyperlink` returns
  `LinkRefused::TableFull { first }` for both caps; `tessera-term` closes
  the open link (so text after a refused link is not taken for part of the
  previous one) and logs the first refusal of a run at warn level, later
  ones at trace. A run ends, and the next refusal is reported again, only
  when a link is created while the table is below three quarters of both
  caps (49 152 links and 12 MiB); refilling one freed slot at the cap
  1 000 times gives one warning, not 1 000 (`tests/hyperlink.rs` in
  `tessera-grid`, `tests/hyperlink_warn.rs` in `tessera-term`). An existing entity can still be reopened by `id` at either cap
  and costs no bytes.
- The byte cap is what bounds memory: 16 MiB holds 7 191 maximal links
  (2 083 + 250 bytes), long before the count cap. Measured peak RSS
  (`/usr/bin/time -l`, best of 7, release, a 200x50 terminal with 10 000
  lines of history full of text): 44.3 MB without links; 68.8 MB after
  65 536 attempts at distinct maximal links of one character each (7 191
  created, the rest refused); still 68.8 MB after 300 000 more distinct
  maximal `id`'d links reopened over one cell. Before the byte cap the
  same 65 536 links took 261.9 MB (all created) and the churn 477.7 MB, so
  the link entities and their text now take about 25 MB at most instead
  of the 150 MB this ADR first estimated (which was also low). The
  line entries come on top and are not capped by the table: first 16 bytes
  per linked cell, so one link over all of a 10 000 x 200 history added
  about 32 MB; since #411 16 bytes per run of one link, so the same history
  adds about 160 KB (amendment below).
- Holders keep links alive, so **history can starve new links**: a
  program that leaves 65 536 links or 16 MiB of link text in scrollback
  gets every new link refused (the text still prints, unlinked) until
  those lines are evicted, erased or cleared (`ED 3`, RIS). That is the
  price of a hard bound without evicting links that cells still show.
- **A clone shares the table.** `Grid::clone` (and `LinkPool::clone`)
  shares the entities, both counters and the index with the original: a
  link opened on either counts against the same caps, and dropping the
  clone gives its holders' counts back. Nothing in the product clones a
  live grid; tests and snapshots that do see one table.
- Entries were one per column at first; since #411 they are run-length
  (amendment below).

### The open link, and what ends it

The grid holds the **active link**; `put_char` and `put_cell` give every
cell they write `LINK` and its table entry while one is open. It is a pen
attribute that SGR does not touch (SGR 0 does not close a link), and it is
not tied to a row: it persists across newlines, scrolling and cursor
motion. It ends with an OSC 8 with an empty URI, and with:

- **RIS** (`Grid::reset`), **DECSTR** (`State::soft_reset`) and **every
  switch to or from the alternate screen** (D-L5, kitty; VTE also resets
  its index on a screen switch). A redundant `DECSET 47`, `1047` or `1049`
  while the alternate screen is already active closes the open link too,
  so a set always ends the link whether or not it switches. Cells already
  written keep their links.
- A refused link (empty URI, URI over the cap, a control in the URI,
  table full).

DECALN suspends the link for its `E` fill and restores it. Erased cells
(ECH, EL, ED, ICH's blanks, scroll fills) carry no link whether or not one
is open; the open link is not closed by an erase. Overwriting a cell with
text written while no link is open drops its link.

### Parsing (`tessera-term/src/osc.rs`)

`OSC 8 ; params ; URI`: `params` is a `:`-separated list of `key=value`;
only `id` is read (the last non-empty one wins), anything else, including a
piece without `=`, is skipped and the link still opens (D-L1: VTE and
Ghostty; WezTerm drops the whole OSC). The URI is everything after the
second `;`, further semicolons included, and must be UTF-8. Its length is
summed from the parser's pieces before they are joined, so an over-long
URI is refused without a copy. A URI that contains a C0 control, DEL, a
C1 control (U+0080 to U+009F), a bidirectional formatting character
(U+061C, U+200E, U+200F, U+202A to U+202E, U+2066 to U+2069) or a line
or paragraph separator (U+2028, U+2029) is refused: such a
URI shows the user one target and opens another (the Trojan Source
spoofing of a hover tooltip or a copied link), and no real URI needs them
unencoded. The OSC layer already drops C0 bytes, and NUL handling is
unchanged; the grid checks C0 too, for callers other than the parser.

| Input | Result |
|---|---|
| no second `;` (`OSC 8 ; URI`) | ignored, the open link stays |
| URI not UTF-8 | ignored, the open link stays (if it is also over 2 083 bytes, the length check comes first: the link closes) |
| empty URI, whatever the params | closes (D-L2: VTE, kitty) |
| URI over 2 083 bytes | closes the open link, opens none (D-L3: VTE; a long URI cannot make text look as if it belonged to the previous link) |
| URI with C1, DEL, bidi formatting characters or U+2028/U+2029 | closes the open link, opens none; the text after it prints unlinked |
| `id` over 250 bytes (or empty) | dropped, the link opens without it |
| payload fills the parser's 16 pieces (`MAX_OSC_PARAMS`): a URI with 13 or more `;` (`8`, the params and 14 URI pieces); 12 still opens | closes the open link: a cut URI is another URI |

Non-ASCII UTF-8 in a URI is stored as received (the specification asks
senders to percent-encode; the Swift side encodes when it builds a `URL`).

### Scrollback and reflow

Lines moving into history keep their table, so links survive
(`links_scrollback` rows). A resize joins and re-splits logical lines in
`Rewrapper`: the links of the joined rows are collected next to the
clusters (`take_links`) and set on the output row at the column the cell
lands on, and a `LINK` cell that lost its entry (never produced by the
grid's own edits) loses the flag. Lines with a linked cell take the
general split path instead of `split_plain`, like lines with clusters.
Trailing blanks are trimmed when rows are joined, and a linked space with
the default style counts as blank (`plain_blank` ignores `LINK`): a
trailing linked space does not keep a row alive, so narrowing gives the
same row count with or without it and the oldest history row is not
pushed out by one (4 000 random grids narrowed to two columns, no loss).
The trimmed space loses its link. The
alternate screen's cut keeps links of the columns that remain.

### Hit-testing and hover

`link_at` (ADR 0025) first looks at the hit cell: with `LINK` it returns
`Link { url: <URI>, span }`, where `span` lists, for every row the view
shows, the runs of adjacent columns of the cells that are the **same
entity** (`same_link`), top to bottom; the cells of one `id` that are not
adjacent give several entries for a row. Only if the cell has no link does
the detected-URL path run, so an OSC 8 link whose text is another URL
wins, and the text after it is still found by detection. Hover draws one
`OverlayKind::Underline` per span entry, so the whole entity is underlined,
not the URL the text happens to hold. The result shape (`Link`) and
`tessera_session_link_at` are unchanged. `is_blank` in `selection.rs`
ignores `LINK`, so trailing linked spaces do not count as text.

### No default underline

Linked text is drawn as it was written (D-RENDER): the renderer reads
`CellFlags` only for width and padding, and nothing underlines a link
until it is hovered with Command held.

## Alternatives considered

- **A link id per cell in `Cell`.** Cell growth (ADR 0013 measured 6 to 9 %
  instructions for four bytes).
- **A separate `LinkTable` field in `Line`.** Cleaner, but `Line` becomes 72
  bytes and the corpus cost 1.2 to 4 % more instructions (above).
- **A run-length table (start, end, link).** Smaller for long links, but
  every edit splits and merges ranges; the per-column table mirrors the
  cluster table's proven shift and delete code. Adopted later (#411,
  amendment below).
- **A global intern table with reference counts per link id.** Needs
  explicit retain and release at every edit; the `Arc` with a drop-time
  counter makes the counts follow the cells for free.
- **Keeping the previous link open when a new one is refused** (WezTerm
  for malformed OSCs): text printed next to an attacker's overlong URI
  would look like part of a trusted link; VTE clears.

## Consequences

- The hot path pays one load and branch per character for the open-link
  test, and a flagless cell overwrite stays a single store. Measured against
  the merge base with `cargo xtask ab` (retired instructions, best of 9
  pairs, an 80x24 terminal fed in 4 KiB chunks, each line plus CRLF
  repeated 200 000 to 500 000 times): ascii 1.011, cjk 0.991, sgr_heavy
  1.004, combining_heavy 1.005, latin_diacritics 1.010, box_drawing 1.010,
  emoji 1.003, punctuation 1.005, and 1.012 for the ascii line overwritten
  in place with CR (no scrolling), the worst case: about three instructions
  per character. A linked character costs more (slow path, an `Arc` clone
  and a table insert). Scrolling costs the same as before (`Line` is
  unchanged); 20 000 seeds of mixed input without OSC 8 (escape sequences,
  wide characters, clusters, resizes, DECSTR, margins) give identical
  cells, flags and clusters on both sides.
- A line with links is copied by `copy_span` and takes the copy path of a
  rectangular scroll, never the rotation path (the same rule as clusters).
- `GraphemeTable::is_empty` now also means "no links"; `len` and `iter` are
  clusters only.
- Search and selection text are unaffected: a link is not text.
- #224 (byte cap, eager index removal, re-arm rule,
  control rejection, `plain_blank`) measured with `cargo xtask ab` against
  the merge base, retired instructions, best of 15 pairs: a 200x50
  terminal with 10 000 lines of history of one corpus line, rewrapped
  200 -> 80 -> 200 columns 60 times: ascii 0.977, latin 0.984, cjk 0.991,
  box 0.978, hebrew 0.977, arabic 0.978, punctuation 0.977, braille 0.978,
  emoji 0.999, sgr 0.974, combining 1.007, and 1.002 for a line of three
  OSC 8 links; 1 MiB of short open/close links (half with an `id`) fed 8
  times, 1.025. The entity's index removal runs in an out-of-line
  `extern "C"` function so that a link's drop cannot unwind: with an
  unwinding drop every loop that drops links carries cleanup for it, and
  the CJK reflow measured 1.030 to 1.036.
- Not done: drawing links by default, a tooltip, `file:` host checks (the
  Swift side, 30-app), links across the alternate-screen switch, and
  OSC 8 inside tmux passthrough (tmux strips it unless configured).

## Amendment: the refusal set as built (#269, 2026-11-14)

The list of characters that make a URI refused (Decision, above) now
names the full set the grid refuses since the window-title filter was
shared with it: U+061C (ARABIC LETTER MARK) with the other bidirectional
formatting characters, and the line and paragraph separators U+2028 and
U+2029, which a tooltip or a pasted link would break a line at. The code
already refused them (`tessera-grid`'s `hyperlink.rs`, tested there);
only this record and `tessera-term`'s `osc.rs` documentation lagged
(#430 20.3 m3). No behaviour changes.

## Amendment: run-length link entries (#411, 2026-11-22)

A line's link table now holds runs instead of one entry per column: each
entry is (first column, length, link), 16 bytes, for a stretch of adjacent
cells under one link entity (`same_link`; two equal links opened apart stay
two runs). Runs are sorted, never empty, never overlap, and two adjacent
runs never share an entity; `Grid::check_invariants` checks this. A lookup
stays a binary search, O(log runs). Printing appends to or extends the last
run, the common case; writing inside a run splits it, and the edits that move
cells (ICH, DCH, the erases and protected erases, DECCRA, copies between
lines, reflow) split runs at their edges and merge neighbours they bring
together. A line under one link is one entry at any width, and the worst
case (a new link every cell) is one entry per column, as before.

- Memory: one link over all of a 10 000 x 200 history now costs 16 bytes
  of entries per line, about 160 KB, instead of 16 bytes per cell, about
  32 MB (Decision, above). Measured (`/usr/bin/time -l`, best of 7,
  release, a 200x50 terminal with 10 000 lines of soft-wrapped ASCII in
  history): 47.1 MB without a link on both sides; with every cell under one
  link 89.7 MB before and 48.4 MB after, so the link costs 1.3 MB instead
  of 42.6 MB (the entries, each line's link table allocation and the
  allocator's rounding).
- Speed, `cargo xtask ab` against the merge base, an 80x24 terminal fed in
  4 KiB chunks, each corpus line plus CRLF repeated 60 000 to 120 000 times,
  plain and inside one OSC 8 link; retired instructions head/base and wall
  time head/base, best of 15 interleaved pairs:

  | line | plain instr | plain wall | linked instr | linked wall |
  | --- | --- | --- | --- | --- |
  | ascii | 1.000 | 0.999 | 0.650 | 0.524 |
  | latin | 1.000 | 1.000 | 0.658 | 0.575 |
  | cjk | 0.973 | 0.961 | 0.667 | 0.499 |
  | box | 1.000 | 0.998 | 0.696 | 0.603 |
  | hebrew | 1.000 | 1.007 | 0.664 | 0.599 |
  | arabic | 1.000 | 1.005 | 0.678 | 0.610 |
  | punctuation | 1.000 | 1.000 | 0.650 | 0.521 |
  | braille | 1.000 | 1.001 | 0.675 | 0.579 |
  | emoji | 0.986 | 0.977 | 0.795 | 0.731 |
  | sgr | 1.000 | 1.001 | 0.774 | 0.739 |
  | combining | 1.000 | 1.003 | 0.919 | 0.894 |

  The worst case, a new `id`'d link for every cell (1 MiB of OSC 8): 1.002
  instructions printed across fresh lines, 1.075 when every cell of one line
  is rewritten under a new link. Both tables remove the cell's old link and
  then write the new one (`Line::set` clears the cell first), so the
  operations match; the run table's extra 7.5 % is its searches: about five
  binary searches per cell (finding the run, both ends of the removal's
  overlap, the write's overlap and its insertion point) where the per-column
  table did two, plus the two neighbour checks that keep adjacent runs
  merged.
- 20 000 seeds of random edit sequences with OSC 8 opens and closes (every
  edit listed above, resizes, the alternate screen, RIS, margins) give the
  same link at every cell, the same live-link count and the same grid on
  both sides.
- `copy_span`, DECCRA and reflow now move links a run at a time.

## Amendment: edits merge only at their edges (#619, 2026-11-24)

ICH and DCH (`LinkColumns::shift_right` and `delete`) and the copies between
lines (`copy_span`: SU, SD, IL and DL between DECSLRM margins) used to cut
runs at the edit's edges and then run a merge pass over every run of the
line, O(runs) per edit, so a line with a link per cell paid a pass over its
80 to 2048 runs for every inserted or deleted character. Now each edit
splits at most the runs crossing its edges (two binary searches), moves or
drops the runs between them, and checks for a merge only where two pieces of
one link can meet again: `end` after an insertion, `at` after a deletion,
and the two edges of a copied span. A copy trims the runs crossing the
span's edges in place, as a removal does (a run holding the whole span keeps
its head there and its tail goes in after the copied runs), lets a copied
run that joins a neighbour at an edge extend that neighbour, and puts the
rest of the source line's runs, clipped to the span, in with one
`Vec::splice`, in place of a removal and a write per source run. The table
so never holds more runs than the result, and a full table grows only where
the result needs the room. The cost is O(log runs) plus the runs moved (S6);
the caps (S5) are unchanged, and the tables hold the same runs as before at
every step (a proptest against the old full merge pass, 3000 cases, and
20 000 seeds of `cargo xtask ab diff` over OSC 8, ICH, DCH, ECH, EL, IL, DL,
DECIC, DECDC, DECCRA, DECSTBM and SU/SD under DECSLRM, with
`check_invariants` after every step).

- Speed, `cargo xtask ab` against the merge base, retired instructions
  head/base (the gating metric; wall time was measured at a load of 80-97
  on a shared machine and is reported as cycles), best of 15 interleaved
  pairs (the SU/SD row after the copy stopped cutting runs ahead of the
  splice, at a load of 41):

  | case | instructions | cycles |
  | --- | --- | --- |
  | ICH/DCH, a link per cell, 80 columns | 0.540 | 0.868 |
  | ICH/DCH, a link per cell, 2048 columns | 0.308 | 0.383 |
  | SU/SD between DECSLRM margins, 24x2048, a link per cell | 0.081 | 0.102 |
  | ICH/DCH under one link, 80 columns | 0.988 | 0.888 |
  | ICH/DCH under one link, 2048 columns | 0.998 | 0.998 |

  Each corpus line of the #411 bench (80x24, 4 KiB chunks), printed and then
  edited with DCH and ICH 8 cells in, plain and inside one link: plain
  0.999-1.001 instructions, linked 0.997-1.000.
- Memory, measured against the merge base: a counting-allocator probe over
  every corpus line (plain, under one link, a link per character; then
  ICH/DCH, then SU/SD between margins) gives the same heap on both sides in
  all 99 cases, as do rows of eight 10-cell links (a full table) mixed with
  rows under one link, scrolled once between margins 6 and 75, and a
  24x2048 screen of a link per cell scrolled between margins and then given
  one more link per row (3 543 984 bytes). A copy that outgrows a table
  grows it by doubling, as writing its runs one at a time does: reserving
  just the splice's need left an odd size that the next link doubled, about
  twice the link heap at 24x2048. `tests/reset.rs` bars a linked row's table at twice
  its runs' 16 bytes plus the header, checks that the edits allocate no
  more over linked rows than over plain ones, that a scroll between
  margins over full tables whose result fits allocates nothing, that one
  over plain rows allocates nothing, and that a copy of 30 runs into a
  table with room for 4 leaves room for the 32nd run, as main does.
