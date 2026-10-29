# ADR 0011: Input events across the C ABI

**Status:** accepted · 2026-10-27

## Context

The macOS frontend receives keyboard, mouse, scroll, paste and focus events
from AppKit and must hand them to the core, which owns key bindings, key
encoding (`tessera_term::keyboard`, xterm and kitty protocols) and the
per-session view state. Issue #26 asks for plain C structs that round-trip
every event kind and a header that documents who owns every pointer. The
Swift wrapper for these functions is written later (#27) by someone who
reads only the header, so the header must explain itself.

C can put any integer into any field, so the boundary must never let a
malformed value become undefined behaviour in Rust, and the frontend's UI
thread must not stall behind a child process that stopped reading its
input.

## Decision

**Functions.** Input follows the existing `tessera_session_<verb>` family:

| Function | Event | Outcome |
|---|---|---|
| `tessera_session_send_key(app, id, const TesseraKeyEvent *, TesseraInputOutcome *out)` | key press, repeat, release; composed text | yes |
| `tessera_session_send_mouse(app, id, const TesseraMouseEvent *, TesseraInputOutcome *out)` | button press, release, motion | yes |
| `tessera_session_send_scroll(app, id, const TesseraScrollEvent *, TesseraInputOutcome *out)` | wheel and trackpad scrolling | yes |
| `tessera_session_paste(app, id, const char *text, size_t len)` | pasted UTF-8 text | no |
| `tessera_session_set_focus(app, id, bool focused)` | view gained or lost focus | no |

On the Rust side every event becomes one `tessera_app_core::input::InputEvent`
routed by `App::send_input`, which returns an `InputOutcome`.

**Outcome.** `TesseraInputOutcome` is `IGNORED` (0) or `CONSUMED` (1).
`CONSUMED` means the core acted on the event (wrote to the PTY, ran a
binding, moved the view) and the frontend must not handle it again.
`IGNORED` lets the frontend handle the event itself, which is how menu key
equivalents and bare modifier presses keep working. `*out` is set to
`IGNORED` before anything else, so it reads "ignored" after any failure.

**Routing.** A key event first goes through the binding resolver
(`input::Bindings`, which returns nothing until the configurable key map
lands in #43). Otherwise it is encoded with
`encode_key(event, &KeyboardModes::default())`. `Terminal` does not track
DECCKM, DECKPAM, `modifyOtherKeys` or kitty flags yet (#8), so until then
application cursor keys and the kitty protocol are not honoured. A key
that encodes to nothing is `IGNORED`. Mouse events are validated, their
cell clamped to the grid, recorded as the session's last mouse position
and `IGNORED`; mouse reporting is #12 and selection is #30. Scroll events
move a per-session `scroll_offset` (positive `delta_lines` goes into the
scrollback). Fractional lines accumulate, the offset is clamped to
`0..=scrollback_len`, a fraction pushing past either end is dropped, and
the outcome is `CONSUMED` only when the offset changed. The offset counts
lines up from the bottom, so while output arrives the view drifts towards
newer lines. That is deliberate for now; keeping the view on its content
is left to #31, and every path that shrinks the scrollback (today
`App::clear_scrollback`) resets or clamps the stored offset. Mapping alt-screen
scrolling to arrow keys is #31. Focus is recorded per session (new
sessions are unfocused) and nothing is written; focus reporting
(DECSET 1004) is #8. A consumed key or paste returns the view to the live
screen.

**Paste sanitising.** Before a paste is written, `"\r\n"` and `"\n"`
become `"\r"`, a lone `"\r"` stays, tab is kept, and every other C0
control (including `ESC` and `NUL`), `DEL` and every C1 control
(U+0080 to U+009F) is removed. The rest of the text is unchanged:
`"a\nb\x1b[31mc"` becomes `"a\rb[31mc"`. Escape sequences are not parsed
out. Once `ESC` and the C1 introducers are gone, what remains cannot act as
a sequence, so a parser would add code without adding safety; xterm
filters pastes the same way. Removing `ESC` also means pasted text cannot
end a bracketed paste early. Wrapping in `CSI 200 ~` / `CSI 201 ~` is a
one-line hook in `App::send_input` that #8 switches on.

**Representation.** Every enumeration a C caller writes (`key`, `kind`,
`button`) travels as a fixed-width integer (`uint32_t`). The
named values are `#[repr(C)]` enums with explicit discriminants
(`TesseraKey`, `TesseraKeyEventKind`, `TesseraMouseButton`,
`TesseraMouseEventKind`), listed in `cbindgen.toml`'s `[export] include`
because no signature references them. Rust converts the integers with an
explicit `match` (`from_raw`) and never transmutes. `TesseraModifiers` is
an enum of the kitty bit values; the field is a `uint8_t` bit set, and
every bit is valid. Booleans inside structs are `uint8_t` (0 or not), so a
struct filled by `memset` is never an invalid Rust `bool`.
`TesseraInputOutcome` is written only by Rust and is a real enum type.
Any unknown discriminant, a code point that is not a Unicode scalar value,
text that is not UTF-8, `text == NULL` with a non-zero `text_len` or a
non-finite coordinate or delta makes the event malformed: the call
returns `TESSERA_ERROR_INVALID_ARGUMENT` and the session is not touched.
`NULL` for the event, `out`, `app` or the paste text (with a non-zero
length) is `TESSERA_ERROR_NULL_POINTER`. `error.rs` gains no new code.

`TesseraKey` mirrors `tessera_term::Key` variant for variant, except that
`Key::F(n)` becomes 35 enumerators `TESSERA_KEY_F1` to `TESSERA_KEY_F35`
(the range the encoder supports, and the range AppKit's `NSF1FunctionKey`
to `NSF35FunctionKey` cover). There are 113 keys. `UNIDENTIFIED` is 0 so a
zeroed event sends nothing. Values are stable and new keys are appended.
`Key` is `#[non_exhaustive]`, so the Rust-to-C direction maps any key the
C enum cannot name (including `F(0)` and `F(36..)`) to `UNIDENTIFIED`. A
test checks that every C value maps to a distinct core key and back.

**Ownership.** Every pointer passed in is borrowed for the duration of the
call only. The core copies what it keeps, and the header says so on every
struct, pointer field and function. Text is UTF-8, not NUL-terminated,
with an explicit length.

**Threads and blocking.** All five functions are main-thread-only, like
`tessera_session_resize`; `tessera_session_write` stays the only export
callable from any thread. `tessera_session_close` is main-thread-only
too, so an input call that blocked behind a child that stopped reading
(`sleep 1000`, a stalled `ssh`) would freeze the UI with no way to
recover. Keys and pastes therefore **never block**. Each session has a
pending-input *outbox* of at most `PENDING_INPUT_BYTES` (16 MiB), on top
of the PTY's own 1 MiB write queue, and a per-session writer thread in
`tessera-app-core` drains it into the PTY in arrival order, waiting
whenever the PTY queue is full. A key sent after a paste therefore always
reaches the child after the paste. Input that does not fit in the free
outbox space is refused whole with `TESSERA_ERROR_IO` and nothing of it
is queued, so a single key or paste larger than 16 MiB always fails.
Input to a child that has exited, or to a session that is closing, fails
the same way. Closing a session closes its outbox: pending bytes are
dropped, the writer stops within one write backoff period (20 ms) and is
joined, so a close during a pending paste returns promptly.
`tessera_session_write` still writes directly and blocking, bypassing
the outbox. Both it and the writer thread feed the PTY in 64 KiB chunks,
so its bytes may overtake or interleave with pending keys and pastes (a
pending paste can be split around them). Routing it through the outbox is
left to a later issue.

## Consequences

- The frontend's job reduces to translating `NSEvent` into these structs
  and honouring `IGNORED`. Binding and encoding decisions stay in Rust and
  are covered by Rust tests.
- Integer fields cost Swift a `.rawValue` when assigning
  (`event.key = TESSERA_KEY_CHAR.rawValue`). In return, hostile or stale
  values can never reach Rust as invalid enums.
- Until #8 lands, full-screen programs that switch on application cursor
  keys or the kitty protocol receive legacy encodings, and focus and
  bracketed paste are not reported.
- Every session costs one more thread (the input writer), which idles on a
  condition variable until input arrives.
- Up to 16 MiB of input per session can sit in memory while its child is
  not reading; pastes beyond that are refused rather than buffered without
  bound. The bound counts payload bytes, not memory: each queued input is
  its own allocation plus queue bookkeeping, about 40 bytes, so a flood of
  one-byte keys costs far more than its payload (17 million queued keys
  grew the heap by about 667 MiB in testing). The PTY layer's own 1 MiB
  write queue comes on top of the outbox.
- Closing drops pending input, in the PTY layer as well as in the outbox
  (#75): `Pty::hangup`, `Pty::kill` and dropping the PTY close its write
  queue before they signal the child. Queued chunks are discarded, writes
  fail with `BrokenPipe` from then on, and the writer thread abandons the
  chunk it is writing after the `write(2)` in progress, which carries at
  most 1 KiB; that one slice may still reach the child. Before this,
  dropping ten PTYs whose cooked-mode children each had 1 MiB of input
  pending took 2 to 5 s, and the children were not reaped within 1.5 s. A
  cooked terminal processes input byte by byte under the kernel's terminal
  lock, so ten concurrent 1 MiB `write(2)` calls stay in the kernel for
  seconds and `kill(2)` on the children stalls behind them. With the
  writer slicing its writes, the drops return in under 0.1 ms, and all ten
  children are reaped and both threads of each PTY have finished within
  11 ms (the test allows 100 ms and 1 s).
- One limit remains. The writer's descriptor stays blocking, so for a child
  that ignores `SIGHUP` and never reads, the writer stays asleep in
  `write(2)` until the child dies; a session closed through
  `tessera-app-core` sends it `SIGKILL` once `HANGUP_GRACE` runs out. If
  such a child starts reading first, it receives the rest of that one
  slice, at most 1 KiB. Waking the writer was ruled out:
  - `O_NONBLOCK` belongs to the open file description that the reader's
    and writer's duplicates of the master share, and a master cannot be
    reopened, so the flag cannot be set for the writer alone. Set for
    both, it made reading output cost 35-40% more CPU. Toggling it around
    each write cost 9-13% more instructions whenever input and output flow
    together, and was judged too subtle.
  - Closing the writer's duplicate of the master (the mechanism #75 first
    proposed) is impossible on macOS under any design: `close(2)` of a
    descriptor another thread is blocked writing to hangs the closing
    thread instead of waking the writer.
