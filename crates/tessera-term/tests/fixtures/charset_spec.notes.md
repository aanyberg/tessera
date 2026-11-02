# #112 spec notes (charset_spec.txt): SCS, SO/SI, shifts, DECNRCM

Sources read this run: xterm ctlseqs (invisible-island.net), xterm-snapshots master `charsets.c` (rev 1.131), `charsets.h`,
`charproc.c`, `cursor.c`, `VTPrsTbl.c`, `fontutils.c` (dec2ucs), `misc.c` (DECRQM), VT510 RM (SCS, DECNRCM), vttest 20251205
`charsets.c` and `main.c` (`tst_screen`), esctest2 (`decstr.py`, `save_restore_cursor.py`, file list). kitty, WezTerm, Alacritty
and iTerm2 behaviour is NOT cited: nothing in the fixture depends on them.

`charset_spec.txt` has 123 rows; every expected value was derived from the xterm source above, not from running xterm
(no xterm on the build machine) and not from Tessera.

## Where the sources disagree, and the recommendation (xterm wins unless noted)

| # | Question | xterm (charsets.c / charproc.c) | Other source | Recommendation |
|---|---|---|---|---|
| 1 | Does `ESC ( A` (UK) map `#` to the pound sign? | Only with DECNRCM on or at level 1; at the default level 4 without DECNRCM it prints `#` (row NR9C) | vttest lists British as an NRCS (tested only in NRC mode); kitty, WezTerm, VTE map always (from memory) | xterm. ADR 0033 documents it; the one-line change is in the translation function if the lead prefers "always". |
| 2 | Pending single shift across a control character | Kept until a printable arrives (SS06) | DEC: "next character" | xterm |
| 3 | DECRST 42 after designating a national set | The set stays designated and keeps translating (NR93) | DEC: national sets need national mode | xterm |
| 4 | DECSTR and DECNRCM | Not reset (NR95) | VT510 table 5-9: reset to multinational | xterm. Note `reset(1)` sends rs2 = DECSTR without `?42l`, so a program that left NRCS on stays that way until RIS. |
| 5 | DECSTR/RIS default G2/G3 | ASCII in UTF-8 mode, DEC Supplemental in 8-bit mode (`resetCharsets`) | DEC: Supplemental | ASCII (Tessera is UTF-8 only); the issue text says ASCII. |
| 6 | GR in UTF-8 mode | `xtermCharSetOut` maps U+0080..U+00FF through GR with the seven-bit arithmetic, including NBSP -> space and U+00FF -> dropped (GR11, GR12); `> 0xFF` is never touched | A strict UTF-8 terminal would ignore GR | xterm: implement the same arithmetic; it is the "GR translation under UTF-8" the brief asks to follow. |
| 7 | NRCS under UTF-8 | `modified_DECNRCM` turns the UTF-8 decoder off while DECNRCM is on (utf8_nrc_mode), so the multibyte text of a UTF-8 program is read as Latin-1 | none | Not copied: Tessera keeps decoding UTF-8 and applies the NRCS tables to decoded code points. Dutch ij and florin (xterm `UNI()` entries) therefore always map. Documented in ADR 0033. |
| 8 | DEC Supplemental `ESC ( <` | Level 2 only; level 3+ it is the user-preferred set, = ISO Latin-1 in UTF-8 mode (identity without DECNRCM) (SU04) | VT510: user-preferred supplemental | xterm |
| 9 | 96-set `ESC - A` with DECNRCM | `British Latin-1` when NATIONAL, else `ISO Latin-1 Supplemental` (SU05, SU06) | | xterm |
| 10 | DEC Special Graphics 0x5F | xterm stores code 0 (blank); other terminals NBSP or space | | space (SG01) |

## Things the issue text gets wrong or leaves out

- No esctest `decscs`, `so` or `si` pending cases exist: esctest2 has no SCS/SO/SI tests and `esctest_pending.txt` is empty.
  The vttest menu 2 screen 15 row is the runnable external reference (`#> V2S15` in csi_spec.txt and esctest_spec.txt).
- "`ESC - . /` for G1-G3 if xterm supports them at level 4": only final `A` (ISO Latin-1 Supplemental). B F H L M are level 5.
- xterm's NRCS sets need DECNRCM; without it the designation selects ASCII (SG11, NR90), so the brief's "at least B, 0, A" for
  an application that never sends `?42h` means B and 0 only, and A prints `#` at level 4.
- DECSC/DECRC without a prior DECSC resets the character sets (SC05), it does not leave them alone.
- xterm saves the character sets per screen buffer (`sc[whichBuf]`): the alternate screen has its own slot (SC09, SC10).
- Level-5 sets and JIS (level 3) are not implemented: open question for the lead whether a follow-up issue is wanted.

## Open points the fixture cannot settle

- Whether the 8-bit C1 mode (S8C1T) changes anything for GR: not modelled here (in 8-bit mode the parser hands C1 bytes to the
  controls before they reach `print`).
- DECRQM `?42` and DA1 feature 9 are in `reports_spec.txt` (implementer's step), not in this fixture.
