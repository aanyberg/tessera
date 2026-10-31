# ADR 0022: OSC titles, clipboard writes and shell integration routing

**Status:** accepted · 2026-10-30

## Context

`tessera-vt` hands every operating system command (`OSC Ps ; Pt BEL` or
`ST`) to the terminal as `Action::Osc`, and until #9 the terminal dropped
them all. Five of them matter now: OSC 0/2 set the window title, OSC 52
writes the clipboard (tmux and Neovim use it, also over `ssh`), and the
shell integration scripts of ADR 0021 send OSC 133 prompt marks, OSC 7
working directory reports and a one-time OSC 1337 handshake.

Three constraints shape where each goes:

- **Layering.** `tessera-term` may not depend on
  `tessera-shell-integration` (both sit on `tessera-grid`), yet the OSC 133
  parser lived in the latter and the marks belong on the grid's lines,
  which only the terminal touches.
- **Security.** The clipboard and the title come from whatever runs in the
  terminal, including a remote host or a file `cat`ed by accident. Reading
  the clipboard back, reporting the title, or a title that reorders the
  tab strip's text are classic escape-injection vectors.
- **What the parser does.** The spec researcher ran `tessera-vt` on the
  exact bytes (`crates/tessera-term/tests/fixtures/osc_spec.txt`, header
  "FACTS"): the payload arrives split on `;` and cut after 16 pieces with
  no flag; C0 controls other than BEL, CAN, SUB and ESC are removed from
  it; CAN, SUB and ESC end the OSC and dispatch what was collected (xterm
  discards it); the terminator is not visible; an OSC's length is
  unbounded in the parser.

The fixture's rows (138 run by `tests/osc.rs` under every chunking, 56
pure-function rows in the grid and shell-integration unit tests) are the
contract; its header records the decisions below, named D-xxx, which the
milestone lead accepted on 2026-10-30.

## Decision

### Routing

All OSC logic of the terminal lives in `crates/tessera-term/src/osc.rs`,
called from `apply_sequence`'s `Osc` arm; no per-character path changes.

| OSC | Where it goes |
|---|---|
| 0, 2 | `Request::SetTitle(String)` → `SessionEvent::Title` (unchanged path) |
| 1 | ignored (D-OSC1: the icon name is not modelled) |
| 52 | `Request::SetClipboard(String)` → `SessionEvent::Clipboard` when `[terminal] clipboard_write` allows → FFI `on_session_clipboard` → the pasteboard |
| 7 | `Request::SetCwd(Vec<u8>)`, raw → app-core `parse_osc7` → `App::session_cwd` |
| 133 | a `Mark` on the cursor's line, applied by the terminal |
| 1337 | `Request::ShellIntegrationHandshake(Vec<u8>)`, raw, only for `ShellIntegrationVersion=` → app-core `parse_osc1337` → one warning on a version mismatch |

The OSC 133 parser moves down to the grid as `Mark::from_osc133`, next to
`Mark`; `tessera_shell_integration::parse_osc133` stays as a one-line
forwarder. The terminal can then place marks itself without depending on
the shell-integration crate. OSC 7 and 1337 travel up raw instead, because
their parsers (`parse_osc7`, `parse_osc1337`) are shell-integration's and
their results are session state, not grid state.

Every other OSC, and every malformed one, is ignored without a reply. The
command number must match exactly (D-OSCNUM: `02` and `+2` are not `2`).
An OSC never moves the cursor or resolves a pending wrap.

### Parser facts the terminal lives with

- **Rejoin with `;`** (titles, OSC 7, OSC 1337): `2;a;b` is the title
  `a;b`, as Alacritty and WezTerm do.
- **D-PARAMS:** 16 pieces means "possibly cut". A title is applied as
  received (best effort); OSC 7 and OSC 1337 are dropped, since a cut path
  is another, valid-looking path.
- **D-CAN:** CAN, SUB and ESC inside an OSC apply what was collected (vte's
  behaviour). xterm discards the string; doing that needs the terminator,
  which `tessera-vt` does not report. A truncated OSC 52 is still caught by
  strict base64. A follow-up in `tessera-vt` can change this.
- **The parser's memory is unbounded** for one OSC (its crate docs call it
  a known gap). The caps below bound the requests, not the parser; bounding
  OSC memory in `tessera-vt` is a separate issue.

### Titles

- D-EMPTY: `OSC 2 ;` sets the empty title (the frontend shows its default);
  `OSC 2` without `;` is ignored.
- D-TITLE-CAP: at most 1024 bytes of payload (`MAX_TITLE_BYTES`), measured
  before sanitising; a longer title is ignored whole, not truncated, as
  xterm does (it ignores more than 1000).
- D-BADUTF8: invalid UTF-8 becomes U+FFFD (lossy), unlike the clipboard.
- D-TITLE-SAN (lead decision, option (a)): after decoding, control
  characters (DEL, C1; the parser already removed C0) and the bidirectional
  and line-separator format characters U+061C, U+200E, U+200F,
  U+202A..U+202E, U+2066..U+2069, U+2028 and U+2029 are removed; spaces
  are kept (no trimming). This is our own hardening against tab-strip
  spoofing (the Trojan Source class); no reference emulator strips the
  bidi characters.
- D-TSTACK: the title reports (`CSI 20/21 t`) stay unanswered: reporting
  a title back is an injection vector. *Amended by ADR 0024 (#10):* the
  title stack is implemented. `CSI 22 ; 0|2 t` pushes the title last
  emitted (sanitised; the empty title before any), `CSI 23 ; 0|2 t` pops it
  and re-emits it as `Request::SetTitle`; depth 10, a push onto a full
  stack drops the oldest, a pop on an empty stack does nothing; `22;1` and
  `23;1` (icon) and a third parameter do nothing. Nothing is replied.
- D-FLOOD: the terminal does not coalesce titles (two OSCs give two
  requests in order); the Swift bridge coalesces deliveries.

### Clipboard (OSC 52)

Write only. `?` and every other query are never answered (D-READ); no reply
path is added.

- D-PARAMS52: exactly three pieces (`52`, `Pc`, data); `52;data` and data
  containing `;` are ignored.
- D-SEL: `Pc` may only hold `c p q s 0-7`; any other character rejects the
  sequence. Empty, or naming any of `c`, `p`, `s`, writes the one general
  pasteboard macOS has, once (as iTerm2 does; macOS has no primary
  selection, and Neovim sends `p` for its `*` register). Only `q` and cut
  buffers write nothing. xterm and kitty skip unknown letters instead.
- D-EMPTY52: empty data is ignored. xterm clears the selection; a remote
  program silently wiping the user's clipboard adds nothing they asked for.
- D-B64: strict RFC 4648 standard base64 (`base64` 0.22's
  `general_purpose::STANDARD`): canonical padding, zero trailing bits, no
  whitespace, no URL-safe alphabet. Line breaks never reach it (the parser
  removes them, so wrapped `base64` output still works). xterm skips junk.
- D-CAP52: at most 1 MiB decoded (`MAX_CLIPBOARD_BYTES`, 1 048 576 bytes
  accepted, one more dropped). Data longer than 1 398 104 base64
  characters (the length of 1 MiB) is dropped before decoding; the decoded
  length is checked after. Survey: kitty 512 MB, xterm's string buffer
  20 000 bytes, Alacritty, WezTerm and iTerm2 none found.
- D-CTRL52: the text is passed on unchanged, NUL and ESC included (the FFI
  passes pointer and length for that reason); text that is not UTF-8 is
  dropped. Bracketed paste is the consumer's protection. iTerm2 strips C0.

`[terminal] clipboard_write` (default `true`, as kitty, WezTerm and
Alacritty allow writes by default; xterm and iTerm2 deny) gates delivery in
app-core: the session's reader reads the configuration in effect for each
write, so `App::apply_config` switches it for every session at once. A
non-boolean value warns and keeps the default. The reader delivers only the
last write of each output chunk, since each write replaces the previous:
a flood of OSC 52 costs one observer call per chunk. The frontend writes
the pasteboard on the main actor (`CallbackBridge`, through an injectable
`NSPasteboard`, so tests never touch the user's).

### Shell integration

- **OSC 7** (D-FWD-CAP): forwarded raw when non-empty and at most 4096
  bytes (`MAX_FORWARDED_OSC_BYTES`, `parse_osc7`'s own limit), so a flood
  cannot queue megabytes. The terminal judges nothing else; app-core parses
  with `parse_osc7` and keeps the last valid report per session
  (`App::session_cwd(id) -> Result<Option<Cwd>>`); an invalid one keeps the
  previous value. `parse_osc7` is unchanged (P7-23: a bidi character in a
  path is accepted; it matters only if a UI shows the path).
- **D-HOST** (lead decision, option (a)): every valid report is kept with
  its host, a shell over `ssh` included. `Cwd::is_local(local_hostname)`
  tells a consumer (a new tab in the same directory) whether the report
  names this machine: an empty host, `localhost`, or the host name, ignoring
  ASCII case and a trailing `.`, a bare name also matching itself with a
  domain (`Mac` and `Mac.local`). How Terminal.app, kitty and WezTerm
  compare hosts was not verified.
- **OSC 1337** (D-1337): only payloads starting with
  `ShellIntegrationVersion=` are forwarded (same caps); iTerm2's many other
  keys (`File=` images of megabytes, `SetUserVar`, queries such as
  `ReportCellSize`) and kitty's notifications are ignored and never
  answered. D-HANDSHAKE: app-core logs one `tracing` warning per session
  when the version differs from `scripts::PROTOCOL_VERSION` (1), naming
  both; matching and malformed handshakes log nothing.
- **OSC 133:** `Mark::from_osc133` is strict (D-STRICT133): the letter
  must end the payload or be followed by `;` (`Afoo` and `D0` are not
  marks), and only `A B C D` are known (`L N P I` and lower case are not).
  D-SECOND: `A` with `k=s`, `k=c` or `k=r` (a secondary, continuation or
  right prompt) gives no mark, so prompt navigation never stops at a PS2
  line. `D`'s first option is the exit code (an `i32`, `None` otherwise).
  D-PENDING: the mark goes on the cursor's line when parsed, a pending wrap
  left alone (kitty's `cursor->y`). D-ALT: marks on the alternate screen
  are dropped; its lines vanish on leave. D-DUP: `Line::set_mark` keeps one
  mark of each kind per line, the new one last, so a redrawn prompt does
  not pile marks up. D-FRESH: `133;A` does not move the cursor.

## Consequences

- The C callback table grows by `on_session_clipboard(user_data,
  session_id, text, len)` after `on_appearance_changed`; hosts rebuild
  against the new header (the table is append-only).
- `SessionEvent` gains `Clipboard(String)`; matches over it (the dev
  window's mailbox) gain an arm.
- A program can still replace the clipboard without asking, which is what
  OSC 52 is for; `clipboard_write = false` is the opt-out. Reading is not
  possible at all.
- Divergences from xterm worth knowing: CAN/SUB/ESC apply a partial OSC
  (D-CAN), empty OSC 52 data does not clear (D-EMPTY52), unknown `Pc`
  letters reject the sequence (D-SEL), titles lose bidi characters.
- Deferred: the title reports (D-TSTACK; the stack itself came with ADR
  0024), OSC 4/10/11/12 and
  104/110-112 colours (#10, which owns the reply path), OSC 8 hyperlinks
  (#30), bounding OSC memory in `tessera-vt`, and the xterm treatment of
  CAN/SUB/ESC (needs the terminator from `tessera-vt`).
- The OSC 52 table decodes two 1.4 MB payloads 33 times each; the
  workspace builds `base64` optimised in the dev profile so
  `tests/osc.rs` stays within its 0.5 s budget.
