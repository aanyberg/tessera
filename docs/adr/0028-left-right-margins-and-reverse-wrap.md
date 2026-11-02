# ADR 0028: Left and right margins and reverse wrap

**Status:** accepted · 2026-11-01

## Context

Issue #97 asks for the VT420's left and right margins: DECLRMM (`CSI ? 69
h`) turns on left/right margin mode, in which `CSI Pl ; Pr s` is DECSLRM
(set the margins) instead of SCOSC (save the cursor). tmux uses them to
scroll one of two side-by-side panes without redrawing the other, but only
when DA1 lists feature 21, after which it sends the bare `CSI s` to reset
them; so advertising 21 commits the terminal to treating `CSI s` as
DECSLRM. The issue also asks for xterm's reverse wraparound (DECSET 45),
and the VT420 column operations DECIC, DECDC, DECBI and DECFI come with the
margins.

The behaviour is pinned by `crates/tessera-term/tests/fixtures/margins_spec.txt`
(144 rows) from the spec-researcher's reading of xterm (`charproc.c`,
`cursor.c`, `util.c`, `tabs.c`, `screen.c`, `misc.c`) and esctest2, and by
57 esctest2 tests lifted into `esctest_spec.txt`, transcribed by running
each test function against Tessera through a shim of esctest's `esccmd` and
`escio` with every one of its assertions checked. The milestone lead
accepted the researcher's decisions D1-D9 (below).

## Decision

**State.** `tessera-grid` keeps the margins as `Margins { left, right }`
next to the scroll region: the full width by default, shared by the primary
and the alternate screen (D7), not saved by DECSC. The terminal keeps the
modes (`Modes::left_right_margins`, `reverse_wrap`,
`reverse_wrap_extended`). The margins are only ever narrower than the screen
while DECLRMM is on: turning it off resets them, so the grid needs no mode
flag and every primitive reads the margins alone; with full-width margins
each one behaves as before, except HPR and VPR from a cursor above the
region in origin mode, which now follow xterm (see below; `cargo xtask ab
diff` against the base with that fix applied to both sides agrees).

**Level gate (D1).** DECLRMM, DECSLRM, DECIC, DECDC, DECBI and DECFI work
only at conformance level 4 and up, as in xterm (`vtXX_level >= 4`); below,
the sequences do nothing, DECRQM ?69 answers 0 and DECRQSS `s` is invalid.
DA1 lists 21 at levels 4 and 5 (`CSI ? 64 ; 21 ; 22 c`), never below, so
tmux never sends margins the gate would ignore.

**DECSLRM.** `CSI Pl ; Pr s` while DECLRMM is on: `Pl` 0 or absent is 1,
`Pr` 0, absent or past the screen is the last column; a pair whose right
margin is not right of its left one is ignored entirely; a valid pair homes
the cursor (to the margins' corner in origin mode). The bare `CSI s` is
DECSLRM with both defaults, which resets the margins (tmux's `Clmg`). While
DECLRMM is off `CSI s` stays SCOSC. `CSI ? 69 s` (XTSAVE) is neither.

**Printing.** The line ends at the right margin, or at the last column for
a cursor right of the margin (xterm lets such text run to the edge). A wrap
marks the row soft-wrapped even when the margin is not the screen edge (D3,
as xterm's `WrapLine`), indexes as LF does and returns to the left margin.
A wide character that does not fit before the right margin leaves the
margin cell blank and wraps early (D8); the blank is only flagged `PAD`
when it is the last column. Insert mode inserts up to the right margin, and
overwrites without inserting outside the margins.

**Moving and editing.** CR goes to the left margin (column 0 for a cursor
left of it outside origin mode); BS and CUB stop at the left margin, CUF at
the right one, each at the screen edge when the cursor starts outside;
HT stops at the right margin, and like xterm moves back to it from right of
it (D6); CBT ignores the margins except in origin mode, where it stops at
the left margin (xterm `TabToPrevStop`). CUP, HVP, CHA, HPA, VPA, HPR, VPR
and CPR count columns from the left margin in origin mode, and DECRC clamps
the column to the right margin. As in xterm's `CursorSet`, the coordinate
CHA, HPA, VPA, HPR and VPR keep (or move relatively) stays where it is in
origin mode, clamped only to the bottom and right margins, even for a
cursor above or left of the margins (reachable through DECRC); before #97
HPR and VPR moved such a cursor onto the top margin (#93), now they follow
xterm. IND, LF, RI and NEL scroll only while the cursor is between the
margins (NEL indexes before its carriage return, as xterm does); SU, SD,
IL, DL and the wrap scroll the rectangle of both margin pairs, moving
cells, cluster text and nothing else (wrap flags and marks stay with their
rows); IL and DL, ICH and DCH do nothing outside the margins. ED, EL and
ECH ignore the margins. DECIC and DECDC insert and delete columns from the
cursor to the right margin in every row of the scroll region; DECBI and
DECFI scroll that rectangle one column at the left and right margin and
otherwise move the cursor. A wide character cut by a margin edge is blanked
whole before cells move. These keep a pending wrap, as xterm's
`xtermColScroll` does.

**Scrollback.** Only a scroll of the full width (the margins at the screen
edges, whether or not DECLRMM is on) and of the full screen on the primary
screen feeds the history; a margin scroll discards what leaves the
rectangle.

**Reverse wrap (D2, D5).** With DECAWM on, DECSET 45 lets BS and CUB (and
DECBI off the margin) continue at the right margin of the line above when
that line is soft-wrapped; DECSET 1045 (xterm's extended form) into any
line and from the top margin to the bottom one, and wins when both are set.
A pending wrap makes the first step a no-move. xterm's `CursorBack` is
followed step by step, quirks included (a count of 2 from a pending wrap
does not move; a failed 45 wrap below the bottom margin leaves the cursor
on the line above). One deviation: the cursor never wraps from row 0 into
the scrollback (xterm reads the newest history line there); it stops at row
0's left margin.

**Resets.** DECSTR, DECSCL and RIS turn DECLRMM, 45 and 1045 off and reset
the margins (xterm `ReallyReset`). DECALN and DECCOLM (with DECSET 40)
reset both margin pairs and keep DECLRMM. A resize resets the margins and
keeps DECLRMM (D4), as xterm's `ScreenResize` does. The alternate screen
switches keep them (D7).

**Reports.** DECRQM answers ?45, ?1045 and (level 4 and up) ?69;
DECRQSS `s` answers `1 $ r Pl ; Pr s` from level 4, whether or not DECLRMM
is on (xterm).

**Out of scope (D9).** SL and SR (`CSI Ps SP @`, `CSI Ps SP A`), which
xterm also maps to the margin rectangle.

## Consequences

- A flood of margin scrolls costs O(rows x width) per line feed: at 300 x
  1000 with margins 1;500, an LF at the bottom margin costs about 150 us
  per byte (1 MB in about 150 s; DECFI at the right margin the same), about
  twice ED 2's per-byte cost, the worst before #97, and in the same
  O(rows x cols) class (rust-critic, #97 round 1).
- tmux (DA1 21) scrolls side-by-side panes with margins; the
  `tessera-app-core` test `tmux_margins` checks after `seq 1 500` in the
  left pane that both panes in Tessera's grid equal `tmux capture-pane` and
  that tmux turned DECLRMM on.
- A margin wrap marks the row soft-wrapped (D3), so selection, copy and a
  resize treat such a row as joined to the next one. Margins only exist
  while a full-screen program draws, and a resize resets them, so this is
  rare and matches xterm.
- The vertical scrollback rule is unchanged: xterm also keeps lines that
  leave a region with its top at row 0 but a bottom above the last row;
  Tessera keeps only full-screen scrolls, as before #97.
- A scroll inside narrower margins touches every row of the region: a
  one-row scroll rotates the lines and moves the narrower outside parts
  back (a property test checks it against the plain copy of the inside),
  any other copies the inside row by row. Either is O(rows), where a
  full-width scroll is a line rotation: about 5,000 instructions against
  1,200 for 24 x 80 with margins 10;70. ASCII text between those margins
  costs 1.46x the instructions of the same text without margins on the
  #97 corpus (lines of about 67 characters, which wrap twice as often
  between the margins) and 1.25x on the critic's (150-character lines):
  the ratio depends on how many line feeds the text has per character
  (`cargo xtask ab`). With DECLRMM off the hot paths
  do one extra comparison per character (the right margin) and per line
  feed at the bottom margin (the cursor column): at most 1.6 % more
  instructions on the eleven corpus lines and an LF flood.
- BS and CUB take a row's columns at once and skip whole laps of the
  region with DECSET 1045, so `CSI 65535 D` costs O(rows), not 65535 steps
  as in xterm (checked against the step-by-step walk by a property test).
- The grid's invariant check no longer requires a pending wrap to sit on
  the last column: it is armed on the right margin, and DECRST 69 or a
  resize can move the margins while it stays armed, as in xterm, where the
  next character wraps from wherever the cursor is.
