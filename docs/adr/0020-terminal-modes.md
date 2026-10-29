# ADR 0020: Terminal modes and who owns them

**Status:** accepted · 2026-10-29

## Context

Applications switch terminal behaviour with mode sequences: DECSET/DECRST
(`CSI ? Pm h/l`), SM/RM (`CSI Pm h/l`), DECKPAM/DECKPNM (`ESC =`/`ESC >`),
xterm's `modifyOtherKeys` (`CSI > 4 ; Pv m`) and the kitty keyboard
protocol's flag stack (`CSI > flags u`, `CSI < n u`, `CSI = flags ; mode
u`). Some of these change how the terminal applies output (origin mode,
auto-wrap, insert mode, the alternate screen), others change what the host
sends to the child (cursor keys, bracketed paste, focus reports) or how it
draws (cursor visibility, reverse video). Before issue #8 `tessera-term`
tracked only auto-wrap and insert mode, set through Rust setters, and
`tessera-app-core` encoded every key with default keyboard modes, never
bracketed a paste and never reported focus (`TODO(#8)`).

## Decision

**`Terminal` owns every mode.** The mode sequences arrive in the PTY output
the terminal parses, so it is the only place that sees them in order with
the text they apply to. The host never mirrors a mode: it reads two `Copy`
snapshots, `Terminal::modes() -> Modes` and
`Terminal::keyboard_modes() -> KeyboardModes`, under the same terminal lock
that feeds the parser. `tessera-app-core` reads `keyboard_modes()` once per
key (and releases the lock before queueing the bytes), `modes()` once per
paste and focus event, and `modes().cursor_visible` once per frame in
`Snapshot::capture`. Mirroring modes into app-core through `Request`s was
rejected: two sources of truth, and a key typed between the request and its
delivery would be encoded with stale modes.

Two fields of `Modes` are kept by the grid and copied into the snapshot:
`origin` (DECSC saves and DECRC restores it with the cursor, so it lives
with the saved cursor in `Grid`) and `alt_screen` (`Grid::is_alt_screen`).
The kitty flags in `KeyboardModes` are the top of the active screen's
stack.

**`Modes`** (`#[non_exhaustive]`, `Copy`): origin (DECSET 6), auto_wrap (7),
cursor_visible (25), cursor_blink (12), reverse_video (5), insert (SM 4),
linefeed_newline (SM 20), allow_column_change (40), columns_132 (3),
alt_screen (47/1047/1049), bracketed_paste (2004), focus_events (1004),
mouse (`MouseTracking`: 9, 1000, 1002, 1003, one at a time; resetting any
of them turns tracking off), mouse_encoding (`MouseEncoding`: 1005, 1006,
1015, 1016; the last set wins and a reset clears only the active one, as
xterm's `extend_coords`), synchronized_output (2026). Defaults are xterm's:
auto-wrap and a visible cursor on, everything else off. **`KeyboardModes`**:
DECCKM (1), DECKPAM (`ESC =`, also DECSET 66, the same flag in xterm),
`modifyOtherKeys` level 0-2, kitty flags.

**What the host honours now.** DECTCEM (the renderer draws the cursor only
while it is visible), bracketed paste (app-core wraps the paste in
`ESC [ 200 ~` / `ESC [ 201 ~` and keeps its line breaks, as xterm, kitty,
Alacritty and WezTerm do; ESC is still removed, so the text cannot end the
paste itself), focus events (`ESC [ I` / `ESC [ O` on a change of focus
only; setting the mode reports nothing) and every keyboard mode. **Stored
for later issues:** the mouse modes (#12), reverse video and cursor blink
(render follow-ups), synchronized output (an app-core capture follow-up
holds frames while it is set, with a cap). Stored modes are visible in
`Modes` now so the follow-ups need no new terminal state.

**DECCOLM** is honoured only while DECSET 40 is set, like xterm's default
`c132: false`; kitty and Alacritty ignore DECCOLM entirely. With 40 set,
DECCOLM performs xterm's side effects: it clears the screen, resets the
scroll region and homes the cursor (origin-relative in origin mode), and
records the mode in `Modes::columns_132` (for DECRQM, #10). It never
resizes the grid: the window and the PTY are the single source of truth
for the grid's width. When the mode changes to a width (132 or 80) the
grid does not have, it pushes `Request::Resize { rows, cols }`, so a host
that can resize its window may act on it; a host that does must resize
the view and the PTY together and call `Terminal::resize`, so the three
never disagree. `tessera-app-core` ignores the request, and the macOS host
never sees it. RIS in 132-column mode asks for 80 columns the same way
(xterm asks the window for 80 columns), also without resizing; DECSET 40
itself survives RIS, as xterm's resource does, and DECSTR leaves the mode
alone, as in xterm.

The first version of this decision resized the grid and let the host's
next `Terminal::resize` win. The adversarial run showed why that fails:
vttest 2.7 sends `CSI ? 40 h` in its start-up string and never resets it,
and terminfo's `is2`/`rs2` contain `CSI ? 3 ; 4 l`, so after vttest every
`reset` or `tput reset` in a window that is not 80 columns wide shrank the
grid to 80 while the PTY and the view kept their width, until the user
resized the window. The resizing version also made DECCOLM the most
expensive sequence per byte (each width change re-wrapped the whole
scrollback: about 6 ms per `CSI ? 3 h CSI ? 3 l` pair with 10 000 lines of
history, 307 µs per input byte); without the resize, DECCOLM costs one
screen clear, like ED 2.

**The alternate screen.** 47 enters and leaves; 1047 also clears the
alternate screen before leaving; 1048 is DECSC/DECRC; 1049 saves the cursor,
enters and clears, and on reset leaves and restores the primary screen's
cursor, pen, pending wrap and origin mode as they were on entry (moved with
the text if a resize re-wrapped the primary screen in between, which a
clamped DECSC copy would not be; with origin mode on, the row is clamped to
the bottom margin, as DECRC clamps it). Leaving with 47 or 1047 keeps the cursor
where it is (xterm, esctest). Setting an alternate mode again repeats its
side effects (1049 clears again, xterm). Deliberate divergences from
xterm: a 1049 reset on the primary screen does nothing (kitty, Alacritty;
xterm would jump to the saved cursor), and the alternate screen is blank on
every entry, also for 47, where xterm keeps its contents between visits
(Alacritty does the same; programs clear it after entering). The alternate
screen's saved cursor survives between visits, as in xterm.

**Kitty keyboard stack.** One stack per screen (the protocol requires it),
each at most 8 entries (`KITTY_STACK_DEPTH`, kitty's and Ghostty's depth;
the protocol only asks for a limit); a push onto a full stack evicts the
oldest entry. Flags above bit 4 are masked off. `CSI < 0 u` pops one entry,
not none as in kitty, because the parser reports it exactly like `CSI < u`.

**Resets.** RIS resets every mode to its default (except DECSET 40) and
empties both kitty stacks. DECSTR (xterm's `ReallyReset(full = false)` plus
the kitty rule that a soft reset resets the keyboard mode) resets origin
mode, cursor visibility (on), cursor blink, synchronized output, insert
mode, auto-wrap (on), DECCKM, DECKPAM, `modifyOtherKeys` and both kitty
stacks; it keeps reverse video, LNM, the alternate screen, bracketed paste,
focus events and the mouse modes. Synchronized output is reset by both
although the spec is silent: a program that died inside an update must not
freeze the screen after `reset`.

**Unknown modes** (DECANM 2, DECSCLM ?4, KAM 2, SRM 12, ?45 and ?69 until
#97, 1001, …) are ignored and logged at trace level; one sequence may set
several modes, and an unknown one among them does not stop the rest. A
mode or kitty keyboard sequence with a colon sub-parameter
(`CSI ? 25:1049 h`, `CSI 4:20 h`, `CSI > 1:2 u`) is ignored as a whole, as
xterm accepts colons only in SGR and XTMODKEYS.

**Deviations from the references**, each pinned by a test:

- Parser-limited: the parser reports an absent parameter and an explicit
  `0` alike, so `CSI < 0 u` pops one kitty entry (kitty pops none), `CSI =
  flags ; 0 u` is mode 1 (replace), and `CSI > 4 ; 0 m` and `CSI > 4 m`
  both reset `modifyOtherKeys`.
- A 1049 reset on the primary screen does nothing (kitty, Alacritty;
  xterm restores the saved cursor).
- The alternate screen is blank on every entry, also for 47 (xterm keeps
  its contents between visits).
- A 1049 reset restores the cursor stashed on entry, which a resize moved
  with the text, rather than the DECSC copy, which a resize only clamps.
- DECCOLM never resizes the grid (xterm resizes its window); it asks
  with a `Request::Resize`. DECSET 40 survives RIS (xterm keeps its `c132`
  resource); RIS in 132-column mode asks for 80 columns.
- Synchronized output (2026) is reset by DECSTR and RIS (the spec is
  silent).
- Bracketed pastes keep their line breaks; a paste that sanitises to
  nothing writes nothing, brackets included.

## Consequences

- No FFI change: the frontend already routes keys, pastes and focus
  through `send_input`, and the cursor's visibility reaches the renderer
  through the frame.
- Reading a mode costs one small copy under a lock the caller already takes;
  the per-character print path reads `auto_wrap` and `insert` from the same
  `State` as before, and origin mode costs one branch per positioning
  sequence, none per character.
- `Snapshot::capture` takes the `Terminal` rather than its `Grid`, and a
  DECTCEM change damages the cursor's row so the next frame redraws.
- Later issues add their modes to `Modes` (it is `#[non_exhaustive]`):
  DECLRMM and reverse wrap (#97), DECRQM reports (#10) read `Modes`.
