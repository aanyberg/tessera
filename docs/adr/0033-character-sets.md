# ADR 0033: Character sets: SCS, shifts, DECNRCM

**Status:** accepted · 2026-11-02

## Context

Issue #112 asks for the terminal's character sets: SCS (`ESC ( ) * +` and
`ESC - . /`), the locking shifts SO, SI, `ESC n`, `ESC o`, `ESC ~`,
`ESC }` and `ESC |`, the single shifts `ESC N` and `ESC O`, DECNRCM
(`CSI ? 42 h/l`), and DECSC/DECRC saving the sets. The one that matters in
practice is DEC Special Graphics: terminfo's `smacs` is `ESC ( 0` and
`rmacs` `ESC ( B`, so every ncurses program, `tmux`, `mc` and `dialog` draws
its boxes with it; `tput smacs; printf '\x6c\x71\x6b'; tput rmacs` must
draw `┌─┐`. Without it the boxes are letters.

The behaviour is pinned by `crates/tessera-term/tests/fixtures/charset_spec.txt`
(123 rows, each with the xterm source it follows; `charset_spec.notes.md`
lists where the sources disagree) and by the vttest menu 2 screen 15 row
(`V2S15_save_restore_cursor_charsets`, SGR with DECSC/DECRC and SCS).
Expected values were derived from xterm's `charsets.c`, `charsets.h`,
`charproc.c`, `cursor.c`, `VTPrsTbl.c` and `fontutils.c` (`dec2ucs`), not
from running xterm.

## Decision

**State** (`crates/tessera-term/src/charset.rs`): G0 to G3, GL, GR and a
pending single shift, in `State`. Defaults, as xterm's `resetCharsets` in
UTF-8 mode: every set ASCII, GL = G0, GR = G2 (so designating G2 changes
what GR does at once). `Charsets::active` is true when GL, GR or the single
shift is not ASCII; `State::print` tests it once per character and
translates only then. `Modes::nrcs` is DECNRCM.

**Designation** is xterm's `xtermDecodeSCS`: the final is looked up with the
conformance level and DECNRCM in a table in xterm's order. A final that no
row accepts at this level or without DECNRCM selects ASCII (a national set
designated without DECNRCM, `ESC ( 1` at level 4, the level-5 `% 2`); a
final xterm's parse tables never route to a designation (`ESC ( D`, `ESC (
" >` below level 5, `ESC - B` below level 5, a non-VT100 final at level
1) changes nothing. Level gates are xterm's: LS2, LS3, LS1R to LS3R, SS2,
SS3 and DECNRCM need level 2, `ESC - . /` and the VT300 finals level 3;
SO, SI and the VT100 finals work at every level.

**Sets implemented:** US ASCII, UK (`A`), DEC Special Graphics (`0`), DEC
Supplemental (`< ` at level 2, `% 5` as DEC Supplemental Graphic), DEC
Technical (`>`), ISO Latin-1 (`ESC - A`, and the user-preferred set `<` at
levels 3 and 4, which is ISO Latin-1 in xterm's UTF-8 mode), and the
national sets xterm offers up to level 4: Dutch, Finnish, French, French
Canadian, German, Italian, Norwegian/Danish, Portuguese, Spanish, Swedish
and Swiss. Not implemented (an xterm below level 5 does not have them
either, and Tessera answers as it does): the VT500 sets (Greek, Hebrew,
Turkish, Russian, Cyrillic, SCS NRCS, ISO Latin-2, -5, Greek, Hebrew,
Cyrillic) and the level-3 JIS sets. Their finals select ASCII or are ignored
exactly as in xterm at level 4; DECSCL 65 does not turn them on.

**Translation** is xterm's `xtermCharSetOut` in UTF-8 mode, and follows it
literally (rows GR01 to GR13): a printed code point up to `U+00FF` goes
through GL (below `0x80`, or the single-shifted set for the first
character after SS2/SS3) or GR (`0x80` and up); the set maps the
seven-bit value; a DEL result is dropped for a seven-bit set; a result in
`0x21..=0x7F` for a GR character gets bit 7 back. Code points above
`U+00FF` are never translated. Consequences that look odd but are xterm's:
with a seven-bit GR a no-break space prints as a space and `U+00FF`
disappears; GL translates ASCII only and GR only Latin-1, so
`ESC ( 0` does not touch `é`; designating G2 as DEC Special Graphics
changes `ì` to `┌` because G2 is GR. REP repeats the character as printed
(translated), through the same cell check as before. DEC Special Graphics
`_` (0x5F) prints a space (xterm stores code 0).

**UTF-8 and NRCS.** xterm's `modified_DECNRCM` turns its UTF-8 decoder off
while DECNRCM is on, so GR then sees raw bytes and the NRCS tables'
Unicode-only entries (Dutch ij and florin) only apply in that mode.
Tessera keeps decoding UTF-8 always and applies the NRCS tables to the
decoded code points, so those entries always map and a UTF-8 program that
turns DECNRCM on still has its multibyte text read as UTF-8. Nothing in a
byte-stream fixture can tell the two apart, so the fixture does not cover
it.

**DECNRCM** (`CSI ? 42`) is level 2 and up. It only gates which sets can be
designated and whether the UK set maps `#`; it does not change sets that are
already designated: DECRST 42 leaves a designated national set translating
(xterm; DEC says it needs national mode). The UK set maps `#` to `£` only
with DECNRCM on or at level 1, which is what xterm's `xtermCharSetOut`
does at its default level 4 and what vttest assumes (it lists British as
NRCS-only); this is the one place a user may notice the difference from
other terminals (row NR9C). DECRQM `?42` reports the mode from level 2.

**DECSC/DECRC** save and restore G0 to G3, GL and GR (not the single shift)
with the cursor, in the same slot as the grid's saved cursor: one per
screen, so DECSC on the alternate screen does not replace the primary
screen's. A DECRC without a prior DECSC resets the sets (and the single
shift), as xterm. `CSI s`/`CSI u` (SCOSC/SCORC), DECSET 1048 and the
entry and exit of 1049 are the same operations. The saved sets live in
`State` (`saved_charsets`), beside the grid's slots, because the grid knows
nothing of character sets.

**Resets.** RIS: sets, GL, GR, the single shift, DECNRCM and the primary
screen's saved copy to defaults. DECSTR and DECSCL: the sets, GL, GR, the
single shift and the active screen's saved copy to defaults; DECNRCM is
kept, as xterm (DEC's table 5-9 resets it). A pending single shift
survives control characters and other escape sequences until the next
printed character, as in xterm (`WriteNow`).

**Reports.** DA1 lists feature 9 (NRCS) from level 2: `CSI ? 6L ; 9 ; 22 c`
at levels 2 and 3, `CSI ? 6L ; 9 ; 21 ; 22 c` at 4 and 5 (ADR 0024's
"never 9" is amended). The reports_spec rows and the app-core test that
encode DA1 changed with it.

## Consequences

- `ESC ( 0` works: ncurses boxes, tmux borders and `mc` draw as boxes.
- Printing is one branch heavier (`Charsets::active`): `State::print` now
  returns the character it printed, and the old body is `State::put`,
  inlined. Measured with
  `cargo xtask ab` against the merge base (instructions, best of 15, 16 MiB
  corpora, whole reads and 4 KiB reads): ascii 0.997 and 0.996, punctuation
  0.998, cjk 0.999 and 0.998, sgr_heavy 1.002 and 1.002, combining_heavy
  0.999 and 0.999, Latin-1 text 0.998. A stream of DEC Special Graphics
  text, which does the translation, costs 1.20 times the base's
  instructions for the same bytes (the base does no work for them); a
  stream of DECSC/DECRC/SCOSC/SCORC pairs 1.04.
- 300,000 differential seeds of streams that designate only sets that map as
  ASCII (ASCII, the UK set at level 4, ISO Latin-1, the NRCS finals without
  DECNRCM) with shifts, saves, 1049, DECSTR, REP and Latin-1 text print
  exactly what the base prints.
- A program that designates G2 as DEC Special Graphics (rare: `ESC * 0`)
  sees Latin-1 characters turn into line-drawing characters, as in xterm.
- The VT500 sets are a follow-up if anyone needs them.

## Alternatives considered

- *Map the UK `#` always* (kitty, WezTerm, VTE): friendlier, and what most
  programs expect of `ESC ( A`, but not what the oracle does and not what
  vttest tests. One condition in `Set::map` (`charset.rs`) turns it on if
  the lead decides so; row NR9C and the unit test change with it.
- *GL and GR as Unicode-only views* (translate GL for ASCII and ignore GR):
  simpler, but the brief says to follow xterm, and programs that print
  Latin-1 text with G2 designated would then differ from xterm.
- *Reset DECNRCM on DECSTR* (DEC table 5-9): safer for `reset(1)` after a
  program that left NRC mode on, but not xterm's behaviour. RIS clears it.
