# ADR 0024: Device reports and the reply path

**Status:** accepted · 2026-10-31 · amended 2026-11-02 (#96: 8-bit
controls, below)

## Context

Programs ask the terminal what it is and what state it is in, and many
wait for the answer: neovim sends a DECRQM burst and DA1 and waits for the
DA1 reply as its "terminal has processed everything" sentinel; tmux sends
DA1, DA2 and XTVERSION and matches each reply; vim reads DA2 to pick its
mouse protocol; fzf and fish ask for the cursor position (CPR). Until #10
`tessera-term` answered nothing, so these programs waited out their
timeouts or guessed. Two pieces were missing:

- **A reply path.** The terminal produces `Request`s for the host
  (`tessera-app-core`), which until now only went to the observer (bell,
  title, clipboard). Replies must reach the child's PTY, in query order,
  without the reader thread ever waiting for a child that stopped reading.
- **The reports themselves**, and the identity behind them: whether
  Tessera claims to be xterm, which DA1 features it advertises, what it
  says when it cannot know an answer (the cell size before the first
  frame), and which reports it refuses.

The spec researcher read xterm's source (patch 412), ctlseqs, DEC's
manuals and the probing code of vim, neovim, tmux and fish and wrote the
contract: `crates/tessera-term/tests/fixtures/reports_spec.txt` (196
rows, run by `tests/reports.rs` whole, byte by byte and at every two-way
split) and `reports_spec.notes.md`. The decisions below are named as in
the fixture; the milestone lead accepted them on 2026-10-31.

## Decision

### The reply path

- `Request::Reply(Vec<u8>)`: one request per answered query, queued the
  moment the query completes, in query order (D-ORDER). The terminal never
  drops a reply; bounding them is the host's job.
- `tessera-app-core`'s reader thread (`Pump::run`) collects the replies of
  each slice of output (4 KiB, see ADR 0014) under the slice's terminal
  lock, joins them into one buffer and, after releasing the lock, queues
  it in the session's input outbox (`Outbox::try_queue_reply`), behind the
  input already queued: appended to the last entry still queued, or as a
  new entry when the queue is empty (the writer pops an entry before
  writing it, so order is kept). The writer thread writes it like any
  input. However the child's output is read (one query per read included),
  a flood of queries costs bytes, not entries.
- Bounds: the outbox's `PENDING_INPUT_BYTES` (16 MiB) applies to replies
  too. A reply that does not fit, or that arrives while the session is
  closing or after its writer stopped, is dropped: counted, and logged at
  debug level the first time and at every power of two. The reader takes
  the outbox lock only for the push (the writer releases it before it
  writes), so it never waits for the child. Measured: 1,000,000 `CSI 6 n`
  from a child that never reads queue 6,000,000 bytes in one entry (20,000
  single-query reads too);
  2,000,000 more fill the outbox to 16 MiB and drop the rest. A slice of
  4 KiB of `CSI 6 n` holds the terminal lock about 60 µs (1,024 replies);
  50 MiB of them in release measured a 99th-percentile frame-lock wait of
  0.1-0.4 ms (the #83 bar is 2 ms) but, at load 70, single waits of 6-11
  ms, so the case is not part of the gated `reader_lock_hold_is_bounded`.
- A reply to a query sent while a paste is still queued reaches the child
  after the paste. Accepted: the program is reading its input in order
  anyway.
- **7-bit rule.** Every reply is built by `reply_csi`, `reply_dcs` or
  `reply_osc` and uses 7-bit controls (`ESC [`, `ESC P`, `ESC ]`, `ESC
  \`) unless 8-bit mode is on; see "Amendment: 8-bit controls" (#96).

### Identity

- **D-ID**: Tessera is not xterm-compatible by identity. XTVERSION
  (`CSI > q`) answers `DCS > | Tessera <version> ST`, with a space and no
  parenthesis, so none of tmux's name prefixes (`XTerm(`, `kitty(`, …)
  matches and tmux falls back to terminfo and DA1. DA2 (`CSI > c`)
  answers `CSI > 1 ; Pv ; 0 c`: Pp 1 (VT220 class, as kitty, foot and
  Ghostty answer), never 77/84/85 (tmux: mintty, tmux, urxvt) and never Pp
  1 with Pv 4000..4009 (vim: kitty).
- **DA2_PV** = `10000 + minor * 100 + patch` of the crate version,
  computed at compile time (0.1.0 → 10100). vim reads Pv ≥ 2500 as "not an
  xterm": it skips xterm-only workarounds and still uses SGR mouse.
- **D-DA1**: DA1 follows `conformance_level()` (DECSCL), as on the VT510
  (xterm keeps its `decTerminalID` and ignores DECSCL here). Level 1
  answers `CSI ? 1 ; 2 c`; levels 2..5 answer `CSI ? 6L ; 22 c`. Only what
  Tessera implements is advertised: 22 (ANSI colour). Never 3, 4 (no
  ReGIS/Sixel), 9 (NRCS), 21, 28 or 52 (tmux would then try to read the
  clipboard); a feature number is added by the issue that implements it.
  `ESC Z` (DECID) answers like DA1. *Amended by #112 (2026-11-02):* levels 2
  and up list 9 (NRCS) too, `CSI ? 6L ; 9 ; 22 c`, and from level 4 `9 ; 21 ;
  22` (21: ADR 0028), because SCS and DECNRCM exist (ADR 0033).
- **D-L5**: level 5 answers as a VT525: DA1 `CSI ? 65 ; 22 c`, DECRQSS
  `" p` → `65;Pc" p`. DA2 does not follow the level.
- DA3 (`CSI = c`) answers `DCS ! | 00000000 ST` from level 4 (xterm).

### The reports

- DSR 5 → `CSI 0 n`; CPR (DSR 6) → `CSI row ; col R`, origin-relative
  under DECOM, the last column while a wrap is pending. DECXCPR (`CSI ? 6
  n`) adds the page `;1` from level 3. DEC DSRs `?15` (→ `?13n`, no
  printer), `?25`, `?26`, `?55`, `?56`, `?75`, `?85` with xterm's level
  gates.
- **D-RQM0**: DECRQM (`CSI Ps $ p`, `CSI ? Ps $ p`) answers every query,
  with 0 for a mode Tessera does not know: neovim matches its burst of
  replies by order. Modes Tessera cannot change answer 3 (SRM 12, DECANM
  `?2`) or 4 (KAM 2, CRM 3, DECSCLM `?4`, DECARM `?8` and the ANSI modes
  DEC lists as permanently reset). `?2026` answers the real state:
  Tessera implements synchronized output, and the Contour specification
  (which tmux and neovim follow) asks for 1/2 there.
- The kitty keyboard query `CSI ? u` answers the active screen's flags.
- **D-RQSS**: DECRQSS (`DCS $ q Pt ST`) answers `m` (SGR, in xterm's
  order, colours in xterm's colon forms, underline styles as `4:n` because
  neovim's probe needs `4:3`), `r` (DECSTBM), ` q` (DECSCUSR, the
  effective style) and `" p` (DECSCL); any other `Pt`, and one longer than
  64 bytes, answers `DCS 0 $ r ST`.
- XTWINOPS: `18 t` and `19 t` (**D-19T**: the screen is the grid) answer
  the grid in characters; `14 t` and `16 t` answer in pixels once
  `Terminal::set_cell_pixels` has both dimensions above zero (**D-CELLPX**),
  saturated at 65535. `tessera-app-core` passes each view's cell size on
  attach, resize, font size change and configured font size change.
- **D-TSTACK** (amends ADR 0022): `22 ; 0|2 t` pushes the title last
  emitted, `23 ; 0|2 t` pops it and re-emits it as `Request::SetTitle`;
  depth 10, a push onto a full stack drops the oldest, a pop on an empty
  stack does nothing. DECSTR and RIS keep the stack, as xterm keeps its
  saved titles across RIS (`ReallyReset` does not free them; read from the
  source, not verified against a running xterm). A program that pushed,
  then a `reset` in a shell, then a pop still gets its title back.

### Deliberate silences

A query whose answer would be a lie or a security hole gets none, and a
program then waits its own timeout:

- DA1, DA2, DA3 and XTVERSION with a non-zero parameter; DSR other than 5
  and 6; DEC DSRs other than the ones above (`?53`, `?62`, `?63`, and
  tmux's colour scheme query `?996`); DA3 below level 4; DECRQSS `" p` at
  level 1.
- **D-WIN2 / D-SECURITY**: `CSI 11 t`, `13 t` (vim's `getwinpos()` waits
  its timeout), `14 ; 2 t`, `15 t`, `20 t` and `21 t`: a title is never
  echoed back. `14 t` and `16 t` while the cell size is unknown (a zero
  cell size divides by zero in image tools).
- XTGETTCAP (`DCS + q`), every other DCS and a malformed DECRQSS (`DCS $
  m`; xterm sends CAN).
- `CSI ? N u` with a non-zero parameter (the reply form), a DECRQM whose
  parameter saturated at 65535, and every query carrying a colon
  sub-parameter (ADR 0020's rule).

**Accepted risk: `CSI ? 0 u`.** The reply to the kitty flags query with no
flags set is `CSI ? 0 u`, and `vte` 0.15 reports that exactly like the
query `CSI ? u` (it pushes the pending parameter, 0, on every dispatch, and
exposes no parser state), so Tessera answers it too. A peer that echoes
its input back raw would ping-pong with the terminal. Telling the two apart
needs a scan of every CSI byte in `tessera-vt` or a fork of `vte`, both
rejected for the hot path; the fixture rows KT07 and RP10 were edited to
leave `CSI ? 0 u` out (lead decision, 2026-10-31). A tty in cooked mode
echoes `ESC` as `^[`, which breaks the loop; only raw-mode echo is
affected.

### Deviations from xterm

From the notes' table (d), each pinned by a fixture row:

| Query | xterm 412 | Tessera |
|---|---|---|
| DSR `?15` | `?13n` (ctlseqs says `?10n`/`?11n`) | `?13n` |
| DECXCPR page | from level 4, or 3 with id ≥ 330 | from level 3 |
| DA1, DA2 vs DECSCL | follow `decTerminalID` | DA1 follows the level, DA2 does not |
| DECRQSS `" p` at level 1 | silent | silent |
| DECRQSS malformed | sends CAN | silent |
| DECRQSS `m` underline | `;4` only | `4:n` for styled underlines |
| DECRQM `?2026` | 0 (unknown) | 1 / 2 |
| DECRQM SRM, DECANM, `?4`, KAM | changeable (1/2) | 3 / 3 / 4 / 4 |
| XTVERSION | `XTerm(412)` | `Tessera <version>` |
| `23 t` past depth 10 | ring buffer: the 11th pop returns a stale title | 11th pop does nothing |
| `19 t` | root window / cell size | the grid |
| `14 ; 2 t`, `15 t` | answered | silent |
| CAN/SUB-aborted DECRQSS | discarded | answered (the parser ends both alike) |

## Consequences

- `tessera-term` gains `reports.rs` (the replies) and `dcs.rs` (DECRQSS
  collection), DCS arms in `apply_sequence` and a title, a title stack and
  the cell size in `State`. The per-character print path is untouched;
  the DCS arms cost a match arm per DCS action (the `dcs_flood` bench).
- `tessera-app-core`'s outbox now carries replies as well as input;
  `Session::set_cell_pixels` is called from every view size path.
- Fixture rows that pinned "nothing is answered" (csi_spec `UNK*`,
  `SCO07`, decscl `NEAR01`, osc_spec `T35`/`T36`, decscusr `Q01..Q06`)
  now see replies or titles; their harnesses check replies separately
  from the other requests (`reply:` lines, `split_replies`).
- The live check `crates/tessera-app-core/tests/probe_replies.rs`
  (ignored) runs vim, nvim, tmux, fzf, less, htop and fish and asserts
  every query is answered except the silences above, and that each
  program draws its first screen within 1 s.
- Still to come: XTGETTCAP (not planned). The OSC colour queries and
  their echo (#10's second part) are the "OSC colours" amendment below.

## Amendment: 8-bit controls (#96, 2026-11-02)

The contract is `crates/tessera-term/tests/fixtures/c1_spec.txt` (from the
spec researcher's tables, with the lead's decisions of 2026-11-01), run by
`tests/c1.rs` whole, byte by byte and at every two-way split, plus the
esctest `*_8bit` rows in `esctest_spec.txt` and the routing rows G05-G07 in
`mouse_spec.txt`.

### The mode

- **8-bit mode** is `Terminal::c1_8bit_replies()`. S8C1T (`ESC SP G`)
  turns it on and S7C1T (`ESC SP F`) off, both only at conformance level
  2 and up (xterm `vtXX_level >= 2`; DEC ignores them in VT Level 1).
  DECSCL's `Pc` keeps setting it (0, 2 or absent: on; 1: off; level 1:
  off). RIS clears it and keeps the level; DECSTR keeps it.
- At level 1 `ESC SP G` is consumed and ignored. xterm also leaves its
  parser in the `ESC SP` table there and swallows the next byte; Tessera
  does not copy that.

### Replies and the host's input

- In 8-bit mode every reply's introducer is the raw C1 byte (`0x9B` CSI,
  `0x90` DCS, `0x9D` OSC) and a string reply ends with `0x9C` (ST); a
  BEL-terminated OSC reply keeps its BEL. Raw single bytes, never their
  UTF-8 encoding: xterm's `writePtyData` writes the byte in UTF-8 mode
  too, and esctest and vttest read raw bytes. A UTF-8-decoding receiver
  sees invalid UTF-8; that is what S8C1T asks for.
- The mode is read when the reply is queued, so `CSI c ESC SP F CSI c`
  after S8C1T answers once in 8-bit and once in 7-bit form.
- **One helper**, `tessera_term::to_8bit_controls`, does the rewrite: a
  leading `ESC [`, `ESC O`, `ESC P` or `ESC ]` becomes `0x9B`, `0x8F`,
  `0x90` or `0x9D`, and after `ESC P` or `ESC ]` a trailing `ESC \`
  becomes `0x9C`. `State::reply` applies it to every reply;
  `tessera-app-core` applies it at each send point to every key encoding,
  mouse and wheel report, focus report, bracketed-paste marker and
  alternate-scroll key, one sequence at a time before any repetition.
  `KeyboardModes` and `encode_mouse` do not change. A two-byte sequence
  is an Alt-prefixed key (`ESC [` for Alt+`[`) and stays as it is, and so
  do the bytes between introducer and terminator (an X10 mouse report's
  payload), C0 controls and pasted text, whose C1 controls the sanitiser
  already removes.
- **Deviation:** an Alt-prefixed legacy key whose key part is itself a
  CSI or SS3 sequence, such as Alt+Shift+Tab (`ESC ESC [ Z`), stays 7-bit
  as a whole. xterm sends the Alt `ESC` followed by an 8-bit CSI
  (`ESC 0x9B Z`). The helper rewrites only a leading introducer; programs
  that run in 8-bit mode read both forms.

### C1 controls in the output

- Honoured **only in 8-bit mode**. A default 7-bit session treats a C1
  byte as before: an ignored control in the ground state, dropped
  elsewhere. That is xterm's behaviour in UTF-8 mode, which ignores
  U+0080..U+009F altogether (charproc.c), and it keeps cp1252 or Latin-1
  text that happens to contain `0x85` or `0x9B` from moving the cursor or
  opening a CSI. Kitty and Ghostty never honour C1; WezTerm always does.
- In 8-bit mode a C1 code is processed exactly as its 7-bit form `ESC
  (code - 0x40)`, in every parser state: `0x9B` starts a CSI (and
  restarts one in progress), `0x9C` ends any string, `0x84`, `0x85`,
  `0x88`, `0x8D` are IND, NEL, HTS and RI, `0x9A` is DECID. A C1 code is a
  raw byte in `0x80..=0x9F` that is not part of a UTF-8 sequence, or the
  UTF-8 encoding `C2 80..=C2 9F`; a continuation byte of another
  character (`C3 9C` is Ü) never is. This is `tessera_vt::Parser::advance_c1`,
  which drives a `tessera_vt::Sink`: the terminal's state applies each
  action and answers `c1_controls()`, which the parser asks before the
  first byte and after every ESC and CSI sequence (the only ones that can
  switch the mode). So `ESC SP G` followed by a C1 control in the same
  read takes effect at once, the parser does not need to know which
  sequences switch, and nothing is added per printed character. Every
  C1 code reaches `esc()` as its `ESC Fe` form: SS2 and SS3 act since
  #112 (ADR 0033), SPA and EPA since #95 (ADR 0034), and the unassigned
  ones are ignored escapes that print nothing.
- **DCS fix.** `vte` ends a DCS at any `0x9C` byte, including the second
  byte of a UTF-8 character such as U+025C (`C9 9C`), which used to end a
  tmux passthrough early. Now a `0x9C` that belongs to a UTF-8 character
  is payload (dropped with the character, as every non-ASCII payload byte
  already was), in both modes. A lone `0x9C` still ends a DCS in 7-bit
  mode, as it always has; in 7-bit mode the UTF-8 encoded `C2 9C` is
  payload too.

### Cost

Only input that holds a byte in `0x80..=0x9F` takes the new path; for the
rest `advance_c1` is `vte`'s `advance` after a scan for such a byte
(branch-free per 32-byte block). The new path splits the input only at
character boundaries and uses `vte`'s `advance_until_terminated` to stop at
a DCS start or end and at a mode switch. Where to step in next (the next
C1 code, the next character with a `0x9C` byte) comes from two cursors
that only move forward, so a read is scanned at most once per cursor
however many DCS strings and mode switches it holds: time is linear in
the read. The first version rescanned the rest of the read at each of
them, which made a 1 MiB flood of `ESC P q x ESC \` with one CJK
character per 64 KiB read 320 times slower than the base (630 times in
8-bit mode); `tests/c1.rs` `c1_aware_parsing_is_linear_in_the_read` pins
linearity (sixteen times the read takes under 64 times as long, the
geometric midpoint between linear and quadratic; DCS, S7C1T and DECSCL
floods in 7-bit mode and a DCS flood in 8-bit mode). The same 1 MiB
floods now cost 1.03 to 1.19 times the base in 64 KiB or 4 KiB reads.

Measured with `cargo xtask ab` against the merge base (instructions, best
of 7): the 11 perf-corpus lines at 0.993-1.009. The highest are
`sgr_heavy` (1.009 whole, 1.008 in 4 KiB reads), which pays the mode
question after every CSI, and ASCII and punctuation (1.003); the
non-ASCII lines, which take the new path, are at 0.993-1.001. The default
7-bit path is identical to the base on 200,000 differential seeds of C1
bytes, UTF-8, queries and DCS strings fed in random reads, and mode
switches mid-read give the same result as one-byte reads on 200,000
more. A stream that switches the mode around every C1 control (`ESC SP
G 84 ESC SP F 84` repeated) costs about 840 more instructions per switch
than the base spends ignoring it (2.3 times its instructions, 3.4 times
its cycles; measured on the final design), and an 8-bit DECID flood
costs 1.2 to 1.3 times a 7-bit one per reply.

## Amendment: OSC colours (#10, 2026-11-03)

The contract is `crates/tessera-term/tests/fixtures/osc_color_spec.txt`
(105 rows from the spec researcher, with the lead's decisions of
2026-10-31 and 2026-11-03), run by `tests/osc_color.rs` whole, byte by
byte and at every two-way split. The rules live in
`crates/tessera-term/src/osc_color.rs`.

### What is answered and set

- **D-REPLY.** `OSC 4 ; c ; ?` answers `OSC 4 ; c ; rgb:RRRR/GGGG/BBBB`,
  and `OSC 10`, `11`, `12 ; ?` answer `OSC Ps ; rgb:RRRR/GGGG/BBBB`:
  lowercase hex, each 8-bit channel times 257. The reply ends with BEL
  when the query ended with BEL and with ST otherwise (`ESC \`, the 8-bit
  ST, and the CAN, SUB or ESC that aborted it): `tessera-vt`'s
  `Action::Osc` now carries `bell_terminated` (ADR 0022 amended). Replies
  go through `State::reply_osc`, so in 8-bit mode they start with `0x9D`
  and end with `0x9C` or BEL (#96 D3). They come out in query order,
  interleaved with the other replies, and hold only the command number,
  the index and the stored colour.
- **D-QUERY, D-SELECT.** Only 4 (index 0..=255), 10, 11 and 12 answer.
  The `OSC 10 ; a ; b ; c` chain goes through the foreground, the
  background and the cursor from its start and ends at 12; an empty piece
  skips one colour, an unparsable one leaves its colour alone. OSC 5, 13
  to 19, 105 and 113 to 119 stay ignored (selection and special colours
  are out of scope).
- **D-STOP4, D-PIECES.** OSC 4 and 104 stop at the first malformed pair or
  index (1 to 3 decimal digits below 256; xterm's `atoi` takes `1x` and
  the empty string, Tessera does not). The parser keeps 16 pieces, so an
  OSC 4 sees at most 7 pairs; xterm answers all of them.
- **D-SPEC, D-HASH, D-NAMES.** `rgb:` with 1 to 4 hex digits per channel
  (the same count in each) scaled as X11 does, and `#` with 3, 6, 9 or 12
  digits in X11's left-justified groups (`#f80` is `#f08000`, not CSS's
  `#ff8800`). `rgbi:`, the CIE forms, CSS `rgb(...)`, blanks and signs
  are ignored whole. X11 colour names were ignored here too; #187 accepts
  them through the one entry point `parse_color_spec` (the "X11 colour
  names" amendment below). An unparsable spec is traced with its length
  only.
- **D-REVERSE.** Under DECSCNM a query reports the colour itself; xterm
  reports the opposite one.
- **D-996.** tmux's `CSI ? 996 n` (colour scheme) stays silent in 0.1.0.

### Theme, overrides and resets

- The theme is configuration: `Terminal::set_theme_colors` (pushed by
  `tessera-app-core` before a session's first byte, at `App::attach` and
  on every `Live::apply` that changes the colours, read under the
  terminal lock like the cursor default, #168). A set records an
  override (`Terminal::color_overrides`, a fixed table of
  `MAX_COLOR_OVERRIDES` = 259 slots, docs/security.md S5), even one equal
  to the theme's colour. `Terminal::colors_generation` increases exactly
  when a colour in effect changes.
- **D-THEME-KEEP.** A new theme changes the defaults and keeps the
  overrides; a reset then lands on the new theme.
- **D-RESET.** RIS drops every override; DECSTR and DECSCL keep them;
  resizes and the alternate screen never touch them. xterm resets the
  palette on DECSTR and RIS and keeps 10 to 12 on both; kitty, Alacritty,
  WezTerm and Ghostty drop the colours on RIS only, which Tessera follows.
  `OSC 110` to `112` with data (other than one empty piece) are ignored,
  as in xterm.

### Drawing

`tessera_render::Theme` gains `indexed_overrides` (256 `Option<Rgba>`,
no heap), applied by `Theme::resolve` and so by `Palette::new`; the
per-cell lookup is unchanged. `Snapshot::capture` compares the
terminal's colour generation (one load per frame) and copies the
overrides only when it moved and they differ, or after a re-theme:
a program colour equal to the old theme's moves no generation, so
`View::set_appearance` makes the next capture compare the table again; `View::render` then
rebuilds the renderer's theme from the configured one and the overrides,
at most once per frame and outside the terminal lock, so a flood of sets
costs one rebuild per drawn frame. **D-BG-ALPHA:** an OSC 11 background
keeps the theme's alpha times `appearance.opacity`, so OSC 11 never
switches a surface between opaque and translucent.

### Cost

Measured with `cargo xtask ab` against the merge base (instructions
retired, best of 7 interleaved process pairs, load average 5 to 9), on
the 11 perf-corpus lines: `Snapshot::capture` and `Snapshot::frame` per
frame with no override (60x200, 90 x 1000 captures) at 0.9999 to 1.0001;
`Terminal::advance` (12 MB per line in 64 KiB reads) at 0.9996 to
1.0002, and 1.003 on an `osc_heavy` line of titles, OSC 8 and OSC 133
(the extra match arms); full frames rendered with 256 indexed overrides
against none (head only, 60x200 at 2x, damage everywhere) at 0.999 to
1.003. The default path is identical to the base on 10,000 differential
seeds of text, SGR, titles, OSC 8 and 133 and every colour sequence form
fed in random reads (screens and non-reply requests compared).

### Deviations from xterm

| Behaviour | xterm 412 | Tessera |
|---|---|---|
| DECSTR | resets the palette | keeps every override |
| RIS | resets the palette, keeps 10 to 12 | drops every override |
| `#rgb` | X11 left-justified | the same (not CSS doubling) |
| Colour names | X11 `rgb.txt` via the X server | the same 782 names (#187) |
| OSC 4 index `1x`, empty | `atoi`: 1, 0 | malformed: stop |
| OSC 4 / 104 index of 4+ digits (`0001`) | `atoi`: 1 | malformed: stop |
| `rgb:` with mixed digit counts (`rgb:f/ff/fff`) | accepted (X11) | ignored whole |
| OSC 104 index 256 | skipped, the loop goes on | malformed: stop |
| More than 7 OSC 4 pairs | all answered | the first 7 (parser cap) |
| Chain past 12 | pointer colours 13, 14 | ends at 12 |
| Query under DECSCNM | the opposite colour | the colour itself |
| CAN/SUB/ESC-aborted query | discarded | answered with ST |

## Amendment: X11 colour names (#187, 2026-11-03)

The contract is `crates/tessera-term/tests/fixtures/osc_color_names_spec.txt`
(70 rows from the spec researcher, decisions D-N1 to D-N10, confirmed by
the lead on 2026-11-03), run by `tests/osc_color.rs` like the OSC colours
fixture; `osc_color_spec.txt` row OI-NAME flips to the names' colours.
The table and the lookup live in `crates/tessera-term/src/color_names.rs`.

- **D-NAMES (revised).** Wherever OSC 4, 10, 11 and 12 take an `rgb:` or
  `#` spec they take an X11 colour name: the X.org `rgb.txt` of xorg-rgb
  1.1.1 (blob `7096643`, 2023-07-10), all 782 entries, which is also the
  X server's built-in table (`os/oscolor.c`) that xterm's `XParseColor`
  reaches through the `LookupColor` request. The file carries the
  xorg-rgb MIT/X11 notice. X11 wins over CSS: `gray` and `grey` are
  `#bebebe`, `green` `#00ff00`, `maroon` `#b03060` and `purple` `#a020f0`;
  the CSS values stay reachable as `web gray`, `web green`, `web maroon`
  and `web purple`. CSS adds no other name (all 148 are in `rgb.txt`);
  CSS keywords (`transparent`), xterm's `XtDefaultForeground` and
  `XtDefaultBackground`, and the Xcms `cms red` are not names.
- **D-N1, D-N2, D-N3 (matching).** As the X server's `OsLookupColor`:
  ASCII case-insensitive over the whole name, spaces significant, nothing
  trimmed, only A to Z folded. `DarkSlateGray`, `darkslategray` and
  `dark slate gray` match because `rgb.txt` lists both spellings; `dark
  slategray`, ` red` and `red ` do not. libX11's Xcms step strips spaces,
  but its database holds only the three `cms` names, so it changes no
  `rgb.txt` lookup. A byte of 0x80 or above never matches (no Unicode
  folding: U+212A KELVIN SIGN is not `k`).
- **D-N6 (cap).** `MAX_NAME_LEN` = 22 bytes, the longest entry (`light
  goldenrod yellow`); a longer piece is rejected before the search, which
  folds the name into a 22-byte stack buffer. xterm has no such cap (it
  drops specs over 65,535 bytes), and none is lost.
- **D-N7 (unknown names).** As any unparsable spec: in an OSC 10 chain
  only that colour is skipped; OSC 4 stops at the pair (D-STOP4). No
  reply, no override, no generation bump; the trace carries the length
  only (S8), and a reply is always the stored `rgb:` value, never the
  name (S1).
- **D-N4, D-N10 (parser facts).** tessera-vt drops C0 bytes inside an OSC,
  so `re<TAB>d` is `red`, and CAN and SUB dispatch what was collected
  (ADR 0022); xterm would ignore the first (unverified for NUL) and
  abort the second. Rows NJ04, ND05 and NK04 record it.

### Cost

The lookup is a binary search, among the names with the input's first
letter, over a `static` array of 8-byte big-endian name prefixes computed
at compile time, then a whole compare of the few names sharing the
input's prefix: no allocation, no lazy initialisation, nothing on the
per-byte path. Measured with `cargo xtask ab` (instructions retired, the
gating metric, best of 7 interleaved process pairs; the critic repeated
it at load average 3): 10,000 `OSC 11` sets by name (16 names, 3 to 22
bytes) against the same 10,000 sets as `rgb:` specs, both on the new
code, at 0.95 in instructions (the names are shorter) but 1.17 in cycles
and wall time, and 0.94 and 1.30 for a shuffled mix of all 782 names.
The cycles gap is real, not load: the binary search's dependent loads
and data-dependent branches cost about 80 to 100 cycles (25 to 30 ns)
per name, once per colour set, never per byte. The `rgb:` flood and plain
text against the base are at 1.004 and 1.000; a flood of 10,000 unknown
names, which the base rejected without a lookup, at 1.08. A plain binary search over the names
(a `memcmp` per step) cost 1.27 times the instructions of the `rgb:`
flood, and the prefix array without the letter index 1.33 times its
cycles: the search is bound by the latency of its dependent loads.
A million 64 KiB bogus names (64 GiB, 105 s in a release build) leave the
resident set at 4.0 MiB; the test suite runs a million 23-byte names and
two thousand 64 KiB ones.
