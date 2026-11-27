# ADR 0051: Packed scrollback blocks

**Status:** accepted · 2026-11-27

## Context

A scrollback line cost 1 664 B on the heap (a 64-byte `Line` and 80 × 20-byte cells), and `rss_1m` measured 1 895 B
per line (ADR 0049 baselines at 6ad4b0c0): about 18 GB at the 10 000 000-line limit. Memory counts as much as speed
(#612). The lead's bars for #51 were: `heap_lines_plain` at most 256 B per line, `heap_lines_mixed` at most 400 B per
line, and `rss_1m` at least 5x lower; `scroll` and `cat` at most 4 % more instructions, `feed` and `search` at most
2 %, and `rewrap` within 10 % of 0.37 ms per 1 000 mixed lines. Re-wrap reads packed rows in place (no unpack,
re-wrap, repack of the store), with a differential proptest against the cell path.

## Decision

**D1. Hot rows and packed blocks.** `Scrollback` keeps its newest `HOT_ROWS` (256) lines as `Line`s. A resize pulls
these back onto the screen, and shell integration edits the newest of them. Older rows are packed into blocks of 256
rows (`crates/tessera-grid/src/packed.rs`). Each push packs at most one row, evicts at most one, and hands the packed
row's allocation back to the caller to reuse.

**D2. Row format.** A packed row has 24 B of metadata, plus the following in its block's shared vectors:
- the UTF-8 of each cell's `ch`, up to its last cell that is not `Cell::BLANK`;
- 4-byte style runs pointing into a per-block style table, which holds each distinct `Style` once;
- 4 bytes for each cell whose flags are set (wide, spacer, cluster, pad, link);
- cluster columns and their text;
- a record for each rare row that has hyperlink runs, marks or mark anchors.

A row of one style and no flagged cells (most plain output) takes a fast path that writes its text and one run.
Decoding a row rebuilds exactly the `Line` that was packed.

**D3. Readers.** `Grid::scrollback_line`, `logical_line` and `logical_lines` return a `LineRef`. It is a guard that
dereferences to `Line`: a borrowed hot or visible row, or a decoded packed row. Search reads rows one at a time through
`logical_line`, so the rows it reads are decoded through the D4 cache. `Grid::hyperlink_at` returns an owned
`Option<Hyperlink>`. These are Rust API changes only; the C ABI is unchanged.

*Amended 2026-11-28 (#781).* Scans that read only marks (app-core's prompt navigation, Select Last Output's look for
the output, the exit-status gutter and the ranks a resize carries records by) read `Grid::line_marks` and
`Grid::marked_lines` instead of `logical_line`. Both return `LineMarks` (a row's marks and wrap flag) from the packed
block's per-row records, so a packed row is never decoded or cached for them, and `marked_lines` walks a range in
O(blocks + marked rows). Decoding every row made a resize's scans over 1 000 000 packed lines take about 0.4-0.8 s
under the terminal lock (67-69x the unpacked store). The packed format is unchanged.

**D4. Decoded-row cache.** Random access (`Scrollback::get`) caches decoded rows: at most `MAX_UNPACKED_CELLS` (65 536)
cells and at most 1 024 rows, evicted oldest first (docs/security.md bounds). Evicting history drops the cached copies
of the evicted rows at once, so a cached row never keeps an evicted row's links alive; a cap of 0 empties the cache. It
is cleared when the store is cleared, re-wrapped, or loses a packed row other than from the front. Iteration caches
nothing.

**D5. Re-wrap.** Packed source rows are read in place into the re-wrap's joined buffer (`Block::join_into`). No `Line`
is built for them. Output rows are built as `Line`s in a bounded window of the newest `HOT_ROWS + 2 × rows` rows plus
the rows of the logical line being split. They are packed once their logical line is finished. The store is never
unpacked as a whole, but while a re-wrap runs, memory peaks at about 2-3x the packed history: the old blocks live until
`replace_with` swaps the new store in (measured RSS peaks for 1M plain lines: 2.1x at 120→119 columns, 2.8x at 120→60,
2.2x at 120→40; heap returns to the baseline afterwards). `crates/tessera-grid/src/packed_rewrap_tests.rs` checks every line (cells,
clusters, links, marks and anchors, PAD cells, wrap flags), the cursor and the history length against a grid that keeps
every row unpacked, after every resize of a generated session.

**D6. Eviction slack.** An evicted row's links and marks are released at once, and its decoded copy leaves the D4 cache
at once. Its text, runs and flags stay until the rest of its 256-row block is evicted. The worst-case slack is one
block, 255 rows' packed bytes.

**D7. The speed this costs, and why it is accepted (#612).** The memory bars are met by a wide margin; four
instruction bars are not. The user chose to accept the trade for 0.3.0 (#51, option A) and to win the speed back in
0.4.0 (#774). Measured with `cargo xtask ab` (table under Consequences):

- memory: a plain 80-column history line costs 151 B instead of 1 664 (11x), a mixed one 319 B instead of 1 781
  (5.6x), and the resident set of 1 000 000 lines is 11.9x lower; at the 10 000 000-line limit a full history of plain
  lines takes about 1.6 GB instead of about 18 GB (estimated from the per-line cost; mixed lines about 2x that);
- instructions: `scroll` +25.6 %, `cat` +16.7 %, `search` +26.3 %, `rewrap` +71.8 %; `feed` (no scrollback) 0.996.

Under the policy in `docs/testing.md` a regression past the default bar needs this justification. Memory counts as
much as speed (#612): the trade is about 11x less memory for at most 1.26x the instructions on the paths that move
history, and 1.72x on a re-wrap, which runs once per width change (about 0.64 ms per 1 000 mixed lines, from 0.37).

The costs cannot be cut further today without changing more than this issue may:

- **Cell classification.** Packing a row reads every cell to find its last non-blank one, its style runs and its
  flagged cells: about 19 instructions per cell, where the 4 % `cat` budget leaves about 6. Profiling shows that the
  whole overhead of `scroll`, `cat` and `search` is this pass and its inverse (decoding a row for search); there is no
  other hot spot left to tune. Doing it in fewer instructions needs SIMD or `unsafe` code, which `tessera-grid`
  forbids.
- **`Line::cells` is `pub`.** Most rows could skip the pass with a "plain row" hint kept on every cell write, but
  `tessera-term` writes `Line::cells` directly, so no hint stays exact unless the field becomes private and every
  write goes through `Line`: an API change across `tessera-term`, `tessera-grid` and their tests.
- **The re-wrap repack.** A re-wrap reads packed rows in place (D5) but builds its output as `Line`s and packs them
  again: the repack is 33 % of the `rewrap` run. Re-wrapping block to block, from packed rows to packed rows, removes
  it, but needs a second encoder for every row shape (clusters, links, marks, PAD): a redesign, not a tuning step.

**D8. Bars after this change.** The `scroll`, `cat`, `search` and `rewrap` bars are reset to the measured ratios plus
the usual 2 % noise margin, as accepted steps in `scripts/perf-steps.txt` (ADR 0049, amendment of 2026-11-27): the
scheduled job holds its first comparison across this change to those bars, and every later one to the default 1.02
against a baseline that includes it. `feed` keeps its 1.02 bar. The memory bars stay: `crates/tessera-grid/tests/reset.rs`
holds a history line to 256 B (plain) and 400 B (mixed), and the heap cases of the job to 1.02 of the new baseline.

## Consequences

- About 11x less memory per scrollback line, and about 1.6 GB instead of 18 GB at the limit (docs/config.md).
- More instructions to scroll, `cat` and search history and to re-wrap (D7), until #774 (0.4.0: a private
  `Line::cells`, a plain-row hint kept on writes, block-to-block re-wrap) wins them back.
- Readers of history get a `LineRef` instead of `&Line`; a packed row read at random is decoded, and cached within the
  D4 bound.
- An evicted row's packed text can outlive it by up to one block (D6); its links and marks cannot.

`cargo xtask ab`, release build, 9 pairs per case, 1-minute load 7-8, head `fcb233a1` against `d19dea27` (#51 phase 1;
its crates are byte for byte those of `5e8f4f17`, the merge base on origin/main, and the rebase onto it changed no
crate code, so the numbers stand; a re-run after the rebase waited for a load under 10 that did not come):

| case | base | head | head/base | bar | accepted step |
| --- | --- | --- | --- | --- | --- |
| `heap_lines_plain` | 1 664 B/line | 151 B/line | 0.091 | ≤ 256 B | met |
| `heap_lines_mixed` | 1 781 B/line | 319 B/line | 0.179 | ≤ 400 B | met |
| `rss_1m` | 1 895 B/line | 159 B/line | 1/11.9 | ≥ 5x lower | met |
| `feed` | instructions | | 0.996 | ≤ 1.02 | met |
| `scroll` | instructions | | 1.256 | ≤ 1.04 | 1.282 |
| `cat` | instructions | | 1.167 | ≤ 1.04 | 1.191 |
| `search` | instructions | | 1.263 | ≤ 1.02 | 1.289 |
| `rewrap` | instructions | | 1.718 | ≤ 1.10 | 1.753 |
