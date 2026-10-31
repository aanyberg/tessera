# ADR 0024: Device reports and the reply path

**Status:** accepted · 2026-10-31

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
  \`), also after DECSCL selected 8-bit replies. `c1_8bit_replies` is
  recorded and reported (DECRQSS of DECSCL) but not honoured until #96.

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
  `ESC Z` (DECID) answers like DA1.
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
- Still to come: the OSC colour queries and their echo (#10's second
  part, which reuses `reply_osc` and the reply path), 8-bit replies
  (#96), XTGETTCAP (not planned).
