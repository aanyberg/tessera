# ADR 0034: Protected areas and selective erase

**Status:** accepted · 2026-11-04

## Context

Issue #95 asks for character protection: ISO protected areas (SPA `ESC V`
and EPA `ESC W`, the 8-bit `0x96` and `0x97`), DEC protection (DECSCA,
`CSI Ps " q`) and the selective erases that honour it (DECSED `CSI ? Ps J`,
DECSEL `CSI ? Ps K`). Form-filling applications use them to keep labels on
screen while erasing the fields between them; esctest2 has tests for each,
and DA1 attribute 6 ("selective erase") tells a program the terminal has
them. Until now Tessera ignored all of them: `CSI ? 2 J` was a no-op
instead of a clear, and DA1 did not list 6.

The behaviour is pinned by `crates/tessera-term/tests/fixtures/protect_spec.txt`
(70 rows, hand-derived by the spec-researcher from xterm's `charproc.c`,
`util.c`, `screen.c`, `cursor.c` and `misc.c` and from esctest2's `ed`,
`el`, `ech`, `decsed`, `decsel`, `decstr`, `s8c1t` and
`save_restore_cursor` tests), run by `tests/protect.rs`, and by four
esctest2 tests lifted into `esctest_spec.txt`
(`E_{ED,EL,ECH}_respectsISOProtection`, `E_S8C1T_SPA_EPA`). The lead
accepted the researcher's decisions D-P1 to D-P4 (follow xterm) and the
planner's D-P5 and D-P6.

## Decision

**State.** One terminal-wide mode, `off | dec | iso` (xterm's
`protected_mode`), and one cell bit, `Style::PROTECTED` (bit 15 of
`Style::flags`), on the pen and in every cell the pen prints (D-P1). Both
halves of a wide character carry it. SPA sets the mode to `iso` and the pen
bit; EPA clears the pen bit and leaves the mode. DECSCA sets the mode to
`dec` for any `Ps`; its first parameter 1 sets the pen bit, absent, 0 and 2
clear it, anything else leaves it; later parameters are ignored, and a
colon sub-parameter voids it, as for every mode sequence (ADR 0020). The
cell bit does not remember which of the two wrote it, so `DECSCA 0` after an
SPA makes the SPA text erasable by ED again, and an SPA after DECSCA makes
the DECSCA text survive ED (rows M01, M02): xterm's one-mode model.

**Which erase honours the bit** (D-P2, xterm exactly):

| Operation | off | dec | iso |
|---|---|---|---|
| ED, EL, ECH | erases all | erases all | keeps protected |
| DECSED, DECSEL | erases all | keeps protected | keeps protected |
| ED 3, DECSED 3 | scrollback only | scrollback only | scrollback only |
| ICH, DCH, IL, DL, SU, SD, scrolling, printing | never look at the bit | | |

DECSED and DECSEL honouring ISO cells is an xterm quirk ("for backward
compatibility, per email from Thomas"; esctest2 marks it a `knownBug`);
DEC and the spec say only DECSCA cells are kept. DECSED takes 0 (below),
1 (above), 2 (all) and 3 (the scrollback, as xterm's DECSED shares
`do_erase_display` with ED); DECSEL 0, 1 and 2; any other `Ps` does
nothing. Both ignore the left and right margins, as ED and EL do. The
moving operations carry the bit with the cell; printing over a protected
cell replaces its protection with the pen's (ISO's "protected against
manual alteration" is about the keyboard, and xterm does not block host
writes). Erase, insertion and scroll blanks never carry the bit; they keep
taking the pen's background (BCE). A selectively erased line keeps its
protected cells with their cluster text and hyperlink; its marks and wrap
flag change exactly as for the plain erase. A protected wide character is
atomic: a range that covers only one half erases nothing of it, while an
unprotected one cut by the range is still blanked whole (xterm
`ClearInLine2`, row M16). The pending wrap is disarmed even when every cell
of an ECH or EL range is protected (xterm leaves it armed there; a
deviation listed in the fixture, not replicated).

**Saving and resets.** DECSC, SCOSC and DECSET 1048/1049 save and restore
the pen bit with the pen, not the mode (xterm's `DECSC_FLAGS`; rows K01,
K03, K05). DECSTR, DECSCL and RIS clear the pen bit and set the mode to
`off`; they do not scan the screen, so cells keep their bit and a later
DECSCA or SPA honours them again (D-P3, row R05: free, and what xterm does).
xterm's ED 2 / DECSED 2 also turn the mode off when they find no protected
cell left; Tessera does not (D-P5): the mode is the program's choice, and a
later protected write must not become erasable by DECSED.

**Reports.** DECRQSS `" q` answers `DCS 1 $ r Ps " q ST` with `Ps` 1 only
when the mode is `dec` and the pen bit is set, else 0, so 0 after SPA
(D-P4); framing follows S8C1T, and the reply is a fixed string from
terminal state (S1). DA1 and DECID list 6 from level 2:
`CSI ? 62 ; 6 ; 9 ; 22 c`, `CSI ? 64 ; 6 ; 9 ; 21 ; 22 c`; level 1 stays
`CSI ? 1 ; 2 c`. DECSCA, DECSED and DECSEL are not level-gated, as in
xterm (D-P6). DECRQSS `m` never reports the bit and SGR never changes it,
SGR 0 included.

**A style bit, not side storage.** The bit lives in `Style::flags`, the one
free bit, so `Cell` stays 20 bytes and printing copies it for free with the
pen; a parallel per-cell array would cost a second write per printed cell.
What that commits the rest of the code to:

- `Cell::is_blank` ignores the bit, so a protected space trims, reflows and
  copies like any blank (it loses its protection where a re-wrap trims it,
  accepted).
- The renderer never reads the bit: shaping runs compare the font style
  (bold, italic) only, the decoration and cursor passes mask with their
  decoration bits and the glyph pass with the script bits. The read-back
  test `protection_draws_like_the_same_cells_unprotected`
  (`tessera-render/tests/golden.rs`) renders a screen of styles, wide
  characters, ligature candidates, a selection and a cursor with every,
  every second and every third character protected and requires the bytes
  to equal the unprotected frame, in Menlo and in each installed ligature
  font.
- Code that compares whole styles sees protected and unprotected cells as
  different: `tessera-app-core`'s selection guard (`same_cells`, which
  compares `style.flags`) clears a selection on a live row whose
  protection changed (accepted: that row was rewritten); the devtool's
  ANSI dump starts a new SGR run where only the protection changes (an
  extra `CSI m`, harmless); the grid's invariant check requires a wide
  cell and its spacer to share the style, which printing keeps.

**Fast path.** Selective erase scans a range for protected cells and, when
there is none, is the plain fill. To keep ED, EL and ECH at one comparison
while protection is in use but nothing on screen is protected (a program
that sent `ESC V x ESC W` once), the terminal keeps one flag per screen,
"known to hold no protected cell": cleared for both screens whenever the
pen protects (SPA, DECSCA 1, a DECRC, SCORC or DECSET 1048/1049 that
restores a protecting pen) and by a resize (a re-wrap can pull protected
cells back from the scrollback); set again by an erase of the whole screen
(ED 2, DECSED 2, or ED 2 with protection off) that left no protected cell
with the pen not protecting, and by RIS. ED, EL and ECH honour protection
only when the mode is `iso` and the active screen's flag is clear; DECSED
and DECSEL only when the mode is not `off` and the flag is clear. With
protection off an erase pays one comparison of the mode.

**DECSERA descoped.** DECSERA (`CSI Pt ; Pl ; Pb ; Pr $ {`, selective erase
of a rectangle) needs rectangle infrastructure Tessera does not have
(DECERA, DECFRA, DECCRA, `$` rectangle parameters with defaults, clamping,
DECOM and validation, the level-4 gate). It moves to #251 with those
operations, together with the fixture rows D01-D10, K02 and the DECSERA
form of K03 (kept as `#>` blocks in `protect_spec.txt`) and the decision
that DECSERA keeps the erased cells' colours (D-P7). The two esctest2
`saveRestoreCursor_protection` tests erase with DECSERA and stay `#>`
pointing to #251; rows K01 (DECRC) and K03 (SCORC) prove the same restore
with DECSEL.

## Consequences

- `CSI ? 2 J` and `CSI ? 2 K` are now real erases (`csi_spec.txt` ED14 and
  EL13 were renamed `*_is_decsed` / `*_is_decsel`), and every DA1 reply
  from level 2 gained `6`.
- `tessera-grid` has `Style::PROTECTED`, `Cell::is_protected` and the
  `Grid::erase_{chars,in_line,in_display}_unprotected` forms, which return
  whether the range held a protected cell; the plain forms are unchanged.
  `Terminal::protection_mode` (doc-hidden) exposes the mode to the tests.
- A selective erase is one pass over the cells and at most one over the
  line's cluster and link table per row, whatever the number of runs
  (S6). The worst shape, every other cell of a 50x200 screen a protected
  cluster in a hyperlink, costs about 1.5 times ED 2 per DECSED 2 or
  ISO-mode ED 2; `selective_erase_over_interleaved_clusters_is_like_ed_2`
  bounds it at 6 times (the first version edited the table once per run:
  38 times ED 2), and `decsed_flood_over_protected_cells_is_linear` covers
  a screen protected throughout. No new buffer (S5): the mode and the
  flags are scalars.
- Performance, `cargo xtask ab` against the merge base, instructions, best
  of 7: see the PR body for the tables (corpus lines and `erase_heavy` at
  most 1.02, `erase_heavy_after_epa` at most 1.05).
- `cargo xtask ab diff` against the base agrees on input without SPA, EPA,
  DECSCA, DECSED, DECSEL and DA1/DECRQSS queries.
