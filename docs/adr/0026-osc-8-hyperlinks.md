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
reference to the pool's live counter. All cells of one OSC 8 open share
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
  copied) and a counter of **live** entities, decremented when the last
  holder drops. An entity lives while a cell on a screen or in
  scrollback, or the grid's active link, holds it: erasing the cell or
  evicting its history line frees it.
- **Cap: `MAX_LIVE_LINKS` = 65 536 live entities.** At the cap
  `Grid::begin_hyperlink` returns `LinkRefused::TableFull { first }`;
  `tessera-term` closes the open link (so text after a refused link is not
  taken for part of the previous one) and logs the first refusal of each
  run at warn level, later ones at trace. An existing entity can still be
  reopened by `id` at the cap. Dead index entries are swept when they are
  at least as many as the live ones (floor 1 024), so the index never
  holds more than twice the live links plus the floor: a flood of
  1 000 000 links rewritten over one cell stays at two live entities and a
  1 024-entry index (`tests/hyperlink.rs`, `hyperlink.rs` unit tests).
  Worst case at the cap is 65 536 links of up to 2 083 + 250 bytes, about
  150 MB, only if a program keeps that many distinct maximal links on the
  screen and in history at once.

### The open link, and what ends it

The grid holds the **active link**; `put_char` and `put_cell` give every
cell they write `LINK` and its table entry while one is open. It is a pen
attribute that SGR does not touch (SGR 0 does not close a link), and it is
not tied to a row: it persists across newlines, scrolling and cursor
motion. It ends with an OSC 8 with an empty URI, and with:

- **RIS** (`Grid::reset`), **DECSTR** (`State::soft_reset`) and **every
  switch to or from the alternate screen** (D-L5, kitty; VTE also resets
  its index on a screen switch). Cells already written keep their links.
- A refused link (empty URI, URI over the cap, table full).

DECALN suspends the link for its `E` fill and restores it. Erased cells
(ECH, EL, ED, ICH's blanks, scroll fills) carry no link whether or not one
is open; the open link is not closed by an erase. Overwriting a cell with
text written while no link is open drops its link.

### Parsing (`tessera-term/src/osc.rs`)

`OSC 8 ; params ; URI`: `params` is a `:`-separated list of `key=value`;
only `id` is read (the last non-empty one wins), anything else, including a
piece without `=`, is skipped and the link still opens (D-L1: VTE and
Ghostty; WezTerm drops the whole OSC). The URI is everything after the
second `;`, further semicolons included, and must be UTF-8 (C0 bytes are
already dropped by the OSC layer, so there is nothing to reject for them).

| Input | Result |
|---|---|
| no second `;` (`OSC 8 ; URI`) | ignored, the open link stays |
| URI not UTF-8 | ignored, the open link stays |
| empty URI, whatever the params | closes (D-L2: VTE, kitty) |
| URI over 2 083 bytes | closes the open link, opens none (D-L3: VTE; a long URI cannot make text look as if it belonged to the previous link) |
| `id` over 250 bytes (or empty) | dropped, the link opens without it |
| payload fills the parser's 16 pieces (a URI with 14 or more `;` may be cut) | closes the open link: a cut URI is another URI |

Non-ASCII UTF-8 in a URI is stored as received (the specification asks
senders to percent-encode; the Swift side encodes when it builds a `URL`).

### Scrollback and reflow

Lines moving into history keep their table, so links survive
(`links_scrollback` rows). A resize joins and re-splits logical lines in
`Rewrapper`: the links of the joined rows are collected next to the
clusters (`take_links`) and set on the output row at the column the cell
lands on, and a `LINK` cell that lost its entry (never produced by the
grid's own edits) loses the flag. Lines with a linked cell take the
general split path instead of `split_plain`, like lines with clusters. The
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
  cluster table's proven shift and delete code.
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
- Not done: drawing links by default, a tooltip, `file:` host checks (the
  Swift side, 30-app), links across the alternate-screen switch, and
  OSC 8 inside tmux passthrough (tmux strips it unless configured).
