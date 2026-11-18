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
for later issues:** the mouse modes (#12), reverse video and the cursor's
blink bit (a host blink clock; the shape is drawn, see "Cursor style"
below), synchronized output (an app-core capture follow-up
holds frames while it is set, with a cap). Stored modes are visible in
`Modes` now so the follow-ups need no new terminal state.

**DECCOLM** is honoured only while DECSET 40 is set, like xterm's default
`c132: false`; kitty and Alacritty ignore DECCOLM entirely. With 40 set,
DECCOLM performs xterm's side effects: it clears the screen, resets the
scroll region and homes the cursor (origin-relative in origin mode), and
records the mode in `Modes::columns_132` (DECRQM reports it, ADR 0024). It never
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
freeze the screen after `reset`. DECSCL (`CSI 61..65 ; Pc " p`, #92) runs
the DECSTR reset before it sets the conformance level; RIS turns 8-bit C1
replies off and keeps the level, as xterm does.

**Unknown modes** (DECANM 2, DECSCLM ?4, KAM 2, SRM 12, 1001, …) are
ignored and logged at trace level; one sequence may set
several modes, and an unknown one among them does not stop the rest. A
mode or kitty keyboard sequence with a colon sub-parameter
(`CSI ? 25:1049 h`, `CSI 4:20 h`, `CSI > 1:2 u`) is ignored as a whole, as
xterm accepts colons only in SGR and XTMODKEYS.

**Cursor style (DECSCUSR).** Added by #113; the rows of
`crates/tessera-term/tests/fixtures/decscusr_spec.txt` pin every point
below. `Terminal::cursor_style() -> CursorStyle { shape: Block | Underline
| Beam, blink }` is the style in effect, each half on its own:
`shape_override.unwrap_or(default.shape)` and
`blink_override.unwrap_or(default.blink)`, where `default` is the
configured one (`[appearance] cursor_shape` and `cursor_blink`, set with
`Terminal::set_default_cursor_style` before a session's first byte and
again by `App::apply_config`) and `shape_override: Option<CursorShape>` and
`blink_override: Option<bool>` are what the program asked for (two fields
since #169; #113 had one `cursor_override: Option<CursorStyle>`). `CSI Ps
SP q` with Ps 1 to 6 sets both (1/2 blinking/steady block, 3/4 underline,
5/6 beam, xterm's "bar"). `Snapshot::capture` maps the shape
to the renderer's and damages the cursor's row when it changes; the blink
bit is recorded but not drawn yet, so a blink-only change damages nothing.
Unfocused views draw every shape hollow and DECTCEM still hides the cursor
whatever its shape.

- *One blink bit.* DECSCUSR and DECSET/DECRST 12 share one
  program-controlled blink bit, as in xterm (`cursor_blink_esc`, written by
  both, last writer wins), Alacritty and kitty. `CSI ? 12 h/l` sets
  `blink_override` only, so `CSI 1 SP q CSI ? 12 l` is steady and `CSI ? 12 h
  CSI 2 SP q` is steady too. `Modes::cursor_blink` stays as the record of
  the last program-requested bit (written by 12 and by DECSCUSR, cleared by
  Ps 0/7, every reset and a blink edit; always
  `blink_override.unwrap_or(false)`), which DECRQM 12 reports. The issue's
  first wording, "blink = DECSCUSR blink || DECSET 12", was dropped: no
  reference terminal combines two bits that way, and it would keep a cursor
  blinking after the program turned blinking off with the other sequence.
- *`?12` never fixes the shape* (#169, user decision D1, 2026-11-01).
  #113 made `CSI ? 12 h/l` materialise a whole override (Alacritty's
  `get_or_insert`), so the shape in effect froze with it. tmux sends cnorm
  (`CSI ? 12 l CSI ? 25 h`) every time it shows the cursor and `vim -u NONE`
  sends `CSI ? 12 h CSI ? 12 l` at start-up, so every tab that ran either
  stopped following `cursor_shape` and `cursor_blink` edits. Now the shape
  stays whatever it was, as xterm's `?12` writes only `cursor_blink_esc`
  and never `cursor_shape`. `CSI ? 12 l` still stops a configured blink
  (Alacritty; xterm's default OR/XOR with a blinking resource would not),
  so a config default cannot defeat a program that asks for a steady
  cursor. Rejected: dropping `?12` when it equals the configured blink (it
  still stops a configured blink in tmux, and the record would depend on
  the configuration).
- *A blink edit takes over from the program* (#169, D1 option C). A
  `set_default_cursor_style` whose blink differs from the default the
  terminal holds drops `blink_override` and clears `Modes::cursor_blink`,
  whichever sequence set them; a default with the same blink (a shape edit,
  or the same style again) changes nothing else. Without it a tab that
  once ran tmux or vim would never follow a `cursor_blink` edit again. The
  comparison is against the terminal's own default, under its lock, so
  concurrent applies cannot make it miss or repeat a change.
- *Consequence: no configured blink inside a running tmux.* tmux sends
  cnorm (`CSI ? 12 l CSI ? 25 h`, in xterm-256color and in tessera.terminfo)
  every time it shows the cursor, and vim sends `CSI ? 12 l` too. Since
  `?12 l` stops a configured blink (above), `cursor_blink = true` is off
  inside a running tmux. A `cursor_blink` edit turns it back on only until
  tmux's next cnorm; after tmux exits the tab follows the configuration
  again only once `cursor_blink` is next edited (or DECSCUSR 0 or a reset
  arrives). Accepted with D1; dropping `?12 l` from tessera.terminfo's
  cnorm would avoid it for `TERM=tessera` and is left to a later issue.
- *Ps 0 and 7 are the configured default* (deliberate deviations): `CSI 0
  SP q`, `CSI SP q` and an empty first parameter (`CSI ; 5 SP q`) drop the
  override, as in Alacritty, kitty (shape), WezTerm and Ghostty; xterm and
  the VT510 manual make 0 a literal blinking block. Ps 7, xterm's "initial
  resources", is the same as 0, our configured default being xterm's
  resources. Ps 1 is always an explicit blinking block.
- *Ps 8 and above* change nothing (xterm, Alacritty, WezTerm), not kitty's
  parity rule. The parser saturates an oversized parameter at `u16::MAX`
  (vte 0.15; `tests/decscusr.rs` checks it), so `CSI 4294967296 SP q` is
  ignored too. Only the first parameter counts (`CSI 5 ; 2 SP q` is a
  blinking beam); a colon sub-parameter anywhere (`CSI 5:1 SP q`) voids
  the whole sequence, as for the mode sequences.
- *Resets.* DECSTR, DECSCL with `Pl` 61 to 65 (through the same soft
  reset) and RIS drop both overrides and clear `Modes::cursor_blink`, so the
  configured default applies; the default itself is configuration and no
  reset touches it. xterm's ctlseqs is silent on what DECSTR and RIS do to
  the cursor style: the facts come from xterm's `charproc.c`, the common
  part of `ReallyReset` (L14379..14384 as fetched 2026-10-29: `InitCursorShape`
  from the resources, `cursor_blink_esc = 0`), which runs for DECSTR, DECSCL
  and RIS alike, so there is no soft/hard difference.
- *Live configuration.* A new default reaches the halves without an
  override at once (the observer's `on_appearance_changed` makes the host
  redraw); a shape override stays until Ps 0/7 or a reset, which then
  lands on the new default (Alacritty), and a blink override stays until
  then or a blink edit (above). A session that starts while a
  configuration is applied reads it again once it is listed.
- *Not saved with the cursor.* DECSC/DECRC, SCOSC/SCORC, the alternate
  screens 47/1047/1048/1049 and a resize neither save, restore nor reset
  the style: one style per terminal, not per buffer (xterm keeps it in
  `TScreen`; `CursorSave2` stores none).
- *Reported (ADR 0024, #10):* DECRQSS `SP q` (`DCS 1 $ r Ps SP q ST`)
  answers from the effective style and DECRQM 12 from
  `Modes::cursor_blink`; the fixture's rows Q01..Q06 run in the table
  test. The split changes none of their replies; Q09 (DECRQSS after `?12 h`
  and a shape edit answers the new shape) and Q10 (a blink edit makes
  DECRQM 12 answer reset) pin the new model.
- *terminfo `Se`* is `CSI 0 SP q` in `tessera.terminfo` (ADR 0005), so a
  program's "default cursor" through tmux lands on the configured style
  rather than a steady block.

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
- DECSCUSR 0 and 7 select the configured cursor style (xterm: 0 is a
  blinking block), `CSI ? 12 l` stops a configured blink, and an edit of
  the configured blink drops the program's (see "Cursor style"). DECSET 12
  sharing DECSCUSR's blink bit is xterm's own behaviour, not a deviation.
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
  DECLRMM and reverse wrap arrived with #97 (ADR 0028). DECRQM (ADR 0024) reads `Modes` and
  answers 0 for a mode Tessera does not know, so each new mode also gets
  its DECRQM answer.

## Amendment: modes a dead program left on (#461, 2026-11-14)

A program that dies without turning its modes off leaves them on for the shell that comes back. The case that
prompted this is `ssh host` running tmux with `mouse on`: when the connection drops, neither the remote tmux server
nor its client can write the sequences that turn the modes off, so mouse motion, clicks and focus changes reach the
local prompt as text such as `ESC [ < 0 ; 6 ; 4 M` and `ESC [ I`. A tmux 3.8 attach, captured on a pty and run in a
Tessera session, leaves the alternate screen, DECCKM and DECKPAM, bracketed paste, focus events (1004),
colour-scheme updates (2031), DECLRMM and button-event mouse tracking in SGR encoding on. Before this amendment only
RIS and DECSTR cleared them.

**Decision.** The shell integration's command end (`OSC 133 ; D`, with or without a status, on either screen) turns
those modes off before the mark is handled. It leaves the alternate screen as DECRST 1049 does (the cursor saved on
entry comes back), shows the cursor, and turns off mouse tracking and its encoding, focus events, 2031, DECLRMM (the
margins go back to the full width), DECCKM, DECKPAM, `modifyOtherKeys` and both kitty keyboard stacks. It also resets
the scroll region (DECSTBM, shared by both screens) to the full screen without moving the cursor, before the carry
below: a dead program's region (tmux sets `1;rows-1` for each scroll burst, irssi and mutt keep one) would make the
carried rows overwrite one line below the region or be discarded inside it, and would keep the shell's output out of
the scrollback. A command end received on the alternate screen was dropped as a mark (ADR 0039; since #489 it is
recorded on the primary screen, see below), and the reset itself queues nothing for the host.

What the alternate screen showed is kept: its rows from the first to the last that shows anything but blanks (at most
the screen's height) are printed on the primary screen from the start of the restored cursor's line (the next line when
the cursor is not at a line's start), each with its trailing blanks trimmed and followed by a new line, so they scroll
into the scrollback like printed text and the next prompt starts below them. That is the dead program's last screen and
what followed it on the alternate screen: ssh's `Connection to host closed by remote host.`, a TUI's panic or traceback,
zsh's `segmentation fault` job status (printed before `precmd` writes `D`). xterm, kitty, WezTerm, Ghostty and iTerm2
keep that text visible (they stay on the alternate screen); so did Tessera 0.1.0. Each cell keeps its character,
grapheme cluster and style, and a full soft-wrapped row stays wrapped; hyperlinks were not carried (since #489 they
are, see below). The pen the leave
restored is set again before each line break, so a row the carry scrolls in (filled with the pen's background) and the
next prompt take the shell's colours, not those of the dead program's last cell (tmux's status bar). A program that
leaves the alternate screen itself before the shell's `D` (vim, htop, a local tmux detaching) is unaffected: the `D`
finds the primary screen.

- **Why the command end and not the prompt start (`A`).** Shells write `D` before their line editor turns its own
  modes back on and `A` after: fish 4 enables bracketed paste, the kitty flags (`CSI = 5 u`), `modifyOtherKeys`,
  `ESC =` and on master 2031 every time it waits for input, after `D` and before it paints the prompt with `A`; zsh
  runs `zle-line-init` (where `echoti smkx` turns DECCKM and DECKPAM on) after the `precmd` that writes `D`; bash
  writes `D` from `PROMPT_COMMAND` and has `A` inside `PS1`, which readline prints after it turned bracketed paste on.
  A reset at `A` would undo all of that at every prompt.
- **Why not bracketed paste.** zsh, bash and fish turn it off before running a command and on again when they read
  the next line, so a leftover one is harmless, and a shell that turns it on once at start-up would lose it.
- **Why not other modes.** Insert, origin, auto-wrap, LNM, reverse video, synchronized output (stored only) and the
  cursor style do not send anything to the shell; RIS (`reset`) still clears them.
- **False positives.** A command end that arrives while a program is still running on the same pty turns its modes
  off. tmux 3.8 does not forward a pane's `OSC 133` to the outer terminal (checked on a pty), except wrapped in its
  `DCS tmux;` passthrough with `allow-passthrough on`, which Tessera's scripts do not write; programs such as vim,
  less and mc leave the alternate screen and their modes before they run a shell. A setup that forwards marks from
  inside tmux will see tmux's mouse mode drop after each command in a pane, until tmux sets it again. Also:
  - A program stopped with Ctrl-Z that keeps its alternate screen or modes over SIGTSTP: the shell's `D` turns them
    off and carries its screen onto the primary one; after `fg` it runs without them until it redraws or sets them
    again (most full-screen programs restore the terminal on SIGTSTP and set it up again on SIGCONT).
  - zsh `precmd` hooks the user installs that turn on modes: Tessera's own `precmd` runs last, so its `D` comes after
    them and turns those modes off. A line editor's modes (`zle-line-init`) are unaffected.
  - fish 4 and later write `D` natively, whatever `TESSERA_SHELL_INTEGRATION` says, so a fish started from another
    program on the same pty (a nested shell inside a running TUI) always resets that program's modes after each of
    its commands.
  - A pane that forwards a `D` through its multiplexer: with tmux's `allow-passthrough on`, or GNU screen, which
    forwards DCS-wrapped OSC 133 by default, a pane printing `ESC P tmux; ESC ESC ] 133 ; D ; 0 BEL ESC \` makes
    Tessera leave the alternate screen and turn tmux's modes off while tmux keeps running, and tmux then draws over
    the primary screen until it redraws in full (reproduced on tmux 3.8).
- **Without shell integration** (or with `no-prompt-mark`) nothing changes: there is no command end to act on. A
  trigger that needs none, such as the pty's foreground process group returning to the shell, was not taken: it is
  polled by the host, so it cannot fire at the right byte of the stream.

**Other terminals** (their source at the time of writing): kitty, WezTerm and Ghostty change no mode on `OSC 133`.
iTerm2 turns mouse reporting off (or offers to), and offers to turn focus reporting and bracketed paste off, when the
shell integration reports a different host (`OSC 1337 ; RemoteHost`), which needs its integration on both sides.

Tests: `crates/tessera-term/tests/stuck_modes.rs` (the tmux attach sequence, each mode, the carried text, the marks that
do not reset, the modes a shell turns on after the command end, a flood), `crates/tessera-app-core/tests/stuck_modes.rs`
(a killed program on a real pty, the real zsh with the integration, and the real tmux with its server and client killed,
ignored).

### How the carry places the rows (#489, 2026-11-17)

The carry above, as the second critic round and the adversarial run of #461 found it, printed each cell again and
cloned the rows first. It now works as follows; where this list and the text above differ, this list holds.

- **Clear before the carry.** Once the leave restored the cursor, the primary screen is cleared from the cursor to the
  end of the screen before any row is carried: the rest of the cursor's line (EL 0, its marks kept; skipped while the
  cursor's wrap is pending, so its last character stays) and every row below it (their marks dropped, as ED 0 drops
  them). Primary text at or below the restored cursor (a program's own output after the cursor saved on entry, a
  stale prompt) no longer shows beside or after the carried rows. It is not counted as a screen clear (ADR 0039,
  D-CLEAR): it is part of the carry, not a program's ED.
- **Cells are moved, not printed again.** Each carried row takes a row of its own, and its cells, clusters and
  hyperlinks are moved into it as they were. Printing them again merged cells that printing merges (an emoji
  skin-tone modifier that the program put next to its base by cursor addressing joined the base, and the row shifted
  left by two), and lost what printing does not keep. Hyperlinks are carried now (the link pool is the grid's, shared
  by both screens).
- **Soft wraps.** A soft-wrapped row stays wrapped, full or padded where a wide character did not fit in the last
  column (its pad is kept), unless it is the last carried row: the next prompt is no continuation.
- **The rows are taken, not cloned.** The alternate screen is discarded by the leave anyway, so its rows are taken out
  of it first: the carry allocates the list of rows and no cells (`tests/rect_alloc.rs`). On a 200 x 500 screen of
  text a drawn screen and a carrying command end cost 0.99 times the same unit with the program leaving the screen
  itself (1.61 times before, 1.34 ms against 0.84 ms; release, best of 15).
- **Trim.** A row's trailing cells are trimmed to the shell's pen only when blank in the shell's background (the
  pen's): a blank in another background is not blank, so a status bar keeps its colour to the end of the row. The
  same rule picks the first and last rows that show anything.
- **The command end mark.** Once the reset took the primary screen back, the `D` is recorded there, on the line below
  the carried text (where the next prompt starts), with its request (ADR 0039, restating N12).
