# ADR 0046: Rectangular area operations (amends 0034)

**Status:** accepted · 2026-11-13

## Context

The VT420 rectangle operations DECERA (`CSI Pt;Pl;Pb;Pr $ z`), DECFRA
(`CSI Pch;Pt;Pl;Pb;Pr $ x`), DECCRA (`CSI Pts;Pls;Pbs;Prs;Pps;Ptd;Pld;Ppd $ v`)
and DECSERA (`CSI Pt;Pl;Pb;Pr $ {`) are used by full-screen programs and by
the esctest2 suite; ADR 0034 descoped DECSERA to #251 because Tessera had no
rectangle infrastructure. The sources are the VT510 reference pages (DECERA,
DECFRA, DECCRA, DECSERA, DECSACE), xterm's ctlseqs and its source
(`screen.c` `xtermParseRect`, `validRect`, `ScrnFillRectangle`,
`ScrnCopyRectangle`, `ScrnWipeRectangle`; `charproc.c` `CASE_DEC*`) and
esctest2. ctlseqs gives no defaults; they come from the VT510 pages and are
what xterm implements. The rows of
`crates/tessera-term/tests/fixtures/rect_spec.txt` (132 cases) pin every
decision below; where stock xterm differs the row records it as
`xterm_screen:` and the row's own `screen:` wins.

## Decision

**Parameters.** Positions are 1-based; absent and `0` are the default: the
first row or column for a top or left edge and for DECCRA's destination, the
last one for a bottom or right edge. A position past the screen is clamped
to the last row or column, so a start beyond the screen still covers the
last row (RE10, not invalid). Parameters saturate at 65535 and extra ones
are ignored. A rectangle whose top is below its bottom or whose left is
right of its right, after the defaults and the clamp, does nothing and
damages nothing (D11). DECCRA's page parameters are accepted and ignored,
present, zero or absent (D5): Tessera has one page. A colon sub-parameter
voids any of the four (D10, `voided_by_subparams`); a private marker
(`CSI ? ... $ z`) is another sequence and is ignored. All four are gated on
conformance level 4 and up, like DECIC/DECDC: DECSCL 61-63 make them no-ops
that keep the screen, 64 and 65 bring them back (Q5). They act on the active
screen only, primary or alternate, never the scrollback (RH02). `CSI * x`
(DECSACE) is not implemented and does not change them (RH01).

**Origin mode (D4).** With DECOM set, rows count from the top margin and
columns from the left margin, and every edge is clamped to the origin region
(the scroll region and the left and right margins). An omitted or zero
parameter's default is that region's edge, counted once. Without DECOM the
margins do not limit the operations. DECCRA's destination is clipped to the
same bounds, so in origin mode to the origin region (Q2, as xterm does;
RO10, RO11).

**Effects.**

- DECERA (D1) erases every cell, protected or not, to a blank that takes the
  pen's background only. No other pen colour or SGR flag is stamped and the
  blank never carries `Style::PROTECTED`.
- DECFRA (D2, Q1) fills with `Pch` in the full pen style, the PROTECTED bit
  included: a filled cell is a written character, as printing writes, so a
  protecting pen (DECSCA 1) protects it and DECSED keeps it. `Pch` (D6, Q4):
  0 or absent is a space; 32-126 and 160-255 (Latin-1) are mapped through the
  character set invoked into GL (or GR) as printing maps them, without
  consuming a pending SS2 or SS3 (with DEC Special Graphics in GL, `Pch` 113
  fills with `─`, RF25); a value from 256 up must be a one-column scalar that
  is not a noncharacter. 1-31 and 127-159 (X6: no `c1_printable` option), a
  wide, zero-width or combining character, a surrogate and a noncharacter
  (65535, a saturated parameter, RF17) make DECFRA do nothing. DECFRA never
  writes a C0, C1 or DEL cell.
- DECCRA copies text, style (PROTECTED included, RC18), cluster text and the
  OSC 8 hyperlink of each cell (Q3: the copy shares the source's link, so the
  live-link count does not grow, and the link lives while any cell holds
  it). Overlapping rectangles copy as if through a buffer.
- DECSERA (D3) erases to spaces that keep the cell's fg, bg, SGR flags and
  PROTECTED bit. It honours DECSCA protection only: in DEC mode it keeps the
  protected cells; ISO protection (SPA/EPA) does not protect against it, and
  with protection off it erases every cell (X7, RS07). DECSED and DECSEL keep
  honouring both modes (ADR 0034).
- DECERA, DECSERA and DECFRA drop the cluster and the link of every cell
  they change (Q3).

**Wide characters (D7).** A wide character cut by an edge of a rectangle is
blanked whole: erased or filled halves take the other with them, a DECSERA
keeps both halves of a protected one, and a DECCRA source half whose partner
lies outside the source arrives as a blank of its own background, never as
an orphan spacer (RW01-RW10).

**What stays (D8).** None of the four moves the cursor, clears a pending
wrap, changes the pen, the DECSCA mode, the saved cursor, the tab stops, the
margins or a mode, or touches the scrollback, a line's soft-wrap flag or its
OSC 133 marks. None answers, and none emits a per-cell or per-operation
event (S1, S2, S6). DA1 does not gain attribute 28 (rectangular editing,
D9): DECCARA, DECRARA and DECRQCRA are out of scope, and a test pins the DA1
string `CSI ? 64;6;9;21;22 c`.

**Implementation (D1-1 to D1-4).** `tessera-grid` has `Rect` (inclusive,
0-based) and `Grid::erase_rect`, `fill_rect`, `copy_rect` and
`selective_erase_rect` in `src/rect.rs`; each clips the rectangle to the
screen again, so no call writes outside it. Parsing, origin mode, the `Pch`
mapping and the level gate are in `tessera-term/src/rect.rs`, called from
the `(b"$", 'z' | 'x' | 'v' | '{')` arms of `csi_dispatch`. DECCRA copies row
by row, bottom-up when the destination is below the source, each row
through scratch of at most one row (D1-1): its cells and, when it carries
clusters or links, their entries; the destination row's own entries are
taken out and set back with the copied ones in column order, so a row costs
linear work however many entries it holds (inserting each copied entry on
its own moved the row's tail each time: quadratic, #251 round 1). There is
no whole-rectangle buffer and no buffer sized from a parameter. `State::print`,
the grid's write functions and `Line` storage are unchanged (D1-3). A
changed row is damaged through `line_mut` like an EL'd row, so selection and
search treat it as they treat EL on that row (D1-4); a test compares the
two damages.

**xterm differences (X1-X7)** and the side Tessera takes:

| | xterm | Tessera |
|---|---|---|
| X1 | DECERA keeps the cell's old colours and stamps the pen's bold, underline and PROTECTED (RE17-RE19; RF21 for DECFRA with a default pen) | the DEC text: an erase blanks with the pen's background only; DECFRA writes the full pen |
| X2 | in DECOM an omitted parameter's default gets the margin added twice (RO03, RO09) | counted once |
| X3 | a wide `Pch` is written into single cells without spacers (RF14) | ignored |
| X4 | a DECCRA source cut at a wide character carries an orphan spacer (RW07) | arrives as a blank |
| X5 | DECCRA's destination in DECOM is clipped to the origin region; the DEC text says "off-page" | xterm's side (Q2) |
| X6 | `Pch` 128-159 accepted with `c1_printable` (RF09) | ignored, no such option |
| X7 | DECSERA ignores ISO protection (RS07) | xterm's side |

## Consequences

- ADR 0034's "DECSERA descoped" paragraph is replaced by its amendment
  below; its "erase blanks never carry the bit" holds for DECERA and
  DECSERA, and DECFRA is the stated exception (a write, not a blank).
- Work per sequence is a constant per cell of the clipped rectangle, at
  most the screen; 1 MiB of full-screen operations costs about what 1 MiB
  of ED 2 costs (within 4 times, release tests `b02_*`; copies over a
  screen of clusters or links within 10 times). The only new buffers are
  DECCRA's scratch of at most one row (2048 cells); plain text costs one
  allocation per operation (`tests/rect_alloc.rs`; docs/security.md
  "Current bounds").
- The protect_spec.txt `#>` block (D01-D10, K02, K03b) and the esctest2
  `saveRestoreCursor_protection` rows are live tests now.
- DECCARA, DECRARA, DECRQCRA, DECSACE and DA1 attribute 28 remain open.
