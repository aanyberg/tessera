# ADR 0023: Scrollback viewport and scroll anchoring

**Status:** accepted · 2026-10-30

## Context

Since #26 a session keeps a scroll offset (lines above the live screen) that
the wheel moves, but `View`/`Snapshot` ignored it: nothing scrolled back was
ever drawn (#31). Drawing the history raises four questions the offset alone
does not answer:

- **Output while scrolled back.** An offset counted from the bottom drifts:
  every line of output moves the view one line towards newer text, so a
  user reading a build log watches it crawl away. Terminal.app and iTerm2,
  among others, keep the viewed text still.
- **History that goes away.** RIS (`ESC c`) and ED 3 (`CSI 3 J`, sent by
  `clear` on most systems) empty the scrollback; ⌘K
  (`App::clear_scrollback`) does too. The #107 adversarial run showed that
  a stored offset survives a child's RIS (offset 60 against 77 lines, and
  after RIS and 50 new lines the view sat at the top of the new history).
  At the scrollback cap the oldest lines are evicted under the view.
- **The alternate screen** has no scrollback, but full-screen programs
  (`less`, `man`, `vim` without mouse mode) expect the wheel to move them.
- **Cost.** The capture runs under the terminal lock on every frame
  (ADR 0014, 0016); a view deep in a 100 000-line history must cost what a
  live one does, and output that does not change the lines in view must not
  redraw anything.

## Decision

### The grid counts lines entering history

`Grid::scrollback_pushed() -> u64` is a monotonic count of the lines that
scrolled off the top of the primary screen into history: every line a line
feed, an index or SU moves off it while the scroll region is the full
screen, including the ones that evict the oldest line at the cap (and the
ones a zero cap discards). Lines leaving a partial region or the alternate
screen never enter history and do not count. Rows a resize moves between the
screen and history do not count either: a resize re-wraps the whole history,
so its readers re-read it rather than follow lines through it. Nothing
resets the count (`clear_scrollback`, `clear_to_cursor`, `reset`, a resize,
a cap change). The increment is one saturating add per `scroll_up` call, on
the path every line feed at the bottom takes.

Together with `scrollback_len` and `max_scrollback` the count tells output
from a clear: between two looks, an operation that only scrolls leaves
`min(cap, len_before + pushed_since)` lines. A grid property test holds
every grid operation other than a clear, a resize and a cap change to that
identity.

### The session anchors the view to its lines

The session stores the offset together with the count and the history
length it was taken against (`ScrollAnchor` in `session.rs`), and resolves
it again against the terminal on every read (each frame, each
`App::scroll_offset`, each scroll):

- Offset 0 stays 0: the live screen follows the output.
- A non-zero offset grows by the lines pushed since, so the view keeps
  showing the same lines while output continues.
- Once those lines are evicted the view pins to the oldest line (the
  offset is clamped to the history length).
- When the history holds fewer lines than the pushes since account for,
  something removed them: RIS, ED 3 or ⌘K. The lines the view showed are
  gone and it returns to the live screen (offset 0), so a child's RIS no
  longer leaves the view at the top of the new history. `App::clear_scrollback`
  also resets the offset directly. A clear followed by at least a full
  history's worth of output before the next read leaves exactly the
  history pure output would have (the newest `cap` lines), so it is treated
  as output: the view pins to the oldest line.
- A resize re-wraps the history: `Session::resize` resolves the anchor
  against the history before the resize and stores it again against the
  one after, clamped, so the view keeps its distance from the live screen.
- On the alternate screen the offset is 0 and the view shows the
  alternate screen; the stored anchor is reset, so leaving the alternate
  screen returns to the live primary screen.
- Typing and pasting return the view to the live screen, as before.

The resolved offset is stored back after every read with a
compare-and-store: if a scroll stored a new position between the read of
the anchor and the store, the scroll wins. The terminal lock and the input
lock are still never held together; the session's frontend calls (render,
scroll, resize) all come from the main thread, so the only interleaving
left is the reader thread adding lines, which the anchor accounts for.

### The snapshot copies the lines at the offset

`Snapshot::capture(term, offset)` takes the resolved offset. At 0 (and
always on the alternate screen) it is the live path of ADR 0016 unchanged:
damage-driven comparison of the visible rows, plus the cursor's row when
the cursor's visibility or shape changed (#113). Above 0 the rows are the
logical lines `scrollback_len - offset ..`: history lines, with the top of
the live screen below them while the offset is smaller than the screen. The
cursor is not drawn while scrolled back (`ViewMetrics::cursor_on_screen`
was already false), and the view's "what was shown" record no longer counts
the cursor's cell then, so a cursor moving under a scrolled view draws
nothing.

The snapshot remembers which lines it holds (`ScrolledAt`: the top line's
number `pushed - offset`, the count and the length):

- **The view moved** (another top line, a first capture, another size, the
  live screen before): every row is copied, `DamageRows::All`. An offset
  change therefore redraws the whole view.
- **The view stayed and the history moved** (lines pushed, evicted or
  cleared): every row in view is compared and only rows whose cells or
  clusters differ are copied and damaged. A line that was still being
  written when the view was taken shows its new text when it is compared.
- **The view stayed and the history did not move:** only the live rows in
  view can have changed; the grid's `damage_since` names them, `offset`
  rows lower, and only those are compared. A view deeper than the screen
  compares nothing.

Output that leaves the lines in view alone thus damages nothing, and the
frame is `Skipped` without GPU work. Every path costs O(rows × cols) at
most, never O(history): `logical_line` is an O(1) ring lookup.

### The wheel on the alternate screen sends cursor keys

With no scrollback to move, the wheel sends cursor keys (xterm's
"alternate scroll", which xterm leaves off by default and Terminal.app,
iTerm2 and WezTerm do by default; Tessera turns it on and does not yet
implement DECSET 1007 to switch it): one key per whole line, Up for older output
and Down for newer, `CSI A`/`CSI B` or `SS3 A`/`SS3 B` under DECCKM,
always the plain legacy keys whatever keyboard protocol is active. A
single event sends at most the screen's height of keys and drops the rest;
fractions accumulate as on the primary screen. The view never moves on the
alternate screen. While the program asked for mouse reports (DECSET 9,
1000, 1002, 1003) the wheel belongs to the mouse protocol: until mouse
reporting is encoded (#12) it is `Ignored` there and sends nothing. On the
primary screen the wheel scrolls the view whatever the mouse mode, as it
did before.

### API

- `App::scroll_to(id, ScrollTarget) -> Result<bool>` with
  `ScrollTarget { Top, Bottom, PageUp, PageDown, Offset(usize) }`: a page is
  the screen height less one line (at least one line); every target is
  clamped to the history; returns whether the offset changed and drops a
  banked wheel fraction. On the alternate screen it changes nothing.
- `App::scroll_position(id) -> Result<ScrollPosition>` with
  `ScrollPosition { offset, history, rows }`, read in one look at the
  terminal (history is 0 on the alternate screen): enough for a scroll bar.
- FFI: `tessera_session_scroll_to(app, session_id, target, offset)` with
  `target` a `TesseraScrollTarget` value carried as `uint32_t` (Top = 0,
  Bottom = 1, PageUp = 2, PageDown = 3, Offset = 4; another value is
  `TESSERA_ERROR_INVALID_ARGUMENT`, like every enum the C API takes) and
  `offset` used by `TESSERA_SCROLL_TARGET_OFFSET` only, and
  `tessera_session_scroll_position(app, session_id, out)` filling a
  `TesseraScrollPosition { uint64_t offset, history; uint16_t rows; }`
  (zeroed on failure). The C test is `crates/tessera-ffi/tests/c/scroll.c`.

## Measurements

Release build, Apple M4 at load 3.4 to 6.1, `cargo xtask ab` against
`9c41758` with the wave 7 perf corpus (50x200 grid, 10 000 lines of
scrollback; instructions retired, best of 7 interleaved base/head process
pairs). `feed_<corpus>` feeds a 64 KiB corpus 400 times (the line-feed path
with the counter increment); `capture_frame_<corpus>` is 90 x (feed once,
then 1 000 cursor moves each followed by `Snapshot::capture` at offset 0,
rows and frame).

| corpus | feed (bar 1.01) | capture_frame (bar 1.02) |
|---|---|---|
| ascii | 1.000 | 1.002 |
| latin_diacritics | 1.000 | 1.004 |
| cjk | 1.000 | 1.002 |
| box_drawing | 1.000 | 1.002 |
| hebrew | 1.000 | 1.002 |
| arabic | 1.000 | 1.006 |
| punctuation | 1.000 | 1.003 |
| braille | 1.000 | 1.002 |
| emoji | 1.000 | 1.004 |
| sgr_heavy | 1.000 | 1.004 |
| combining_heavy | 1.000 | 1.005 |

Two release-only tests in `view.rs` bound the scrolled paths:
`scrolling_through_a_long_history_is_smooth` (a 50x200 grid with 100 000
lines of history stepped 3 lines per frame over the whole history: about
3 µs per capture and frame, p99 about 4 µs against a bar of 1 ms; a full
capture at offset 50 000 costs 0.98x one at offset 0, and one at offset
5 000 1.00 to 1.02x between 10 000 and 100 000 lines of history, bars 1.10
and 1.05) and `a_flood_under_a_scrolled_view_keeps_captures_short` (10 000
reads of 4 KiB under a view anchored 1 000 lines back: p99 about 7 µs, bar
1 ms, and the snapshot never grows).

## Consequences

- The frontend (task 8 of wave 7) builds ⌘↑/⌘↓/⌘Home/⌘End, an overlay
  scroller and momentum handling on `scroll_to` and `scroll_position`
  without a Rust change.
- Mouse reporting (#12) takes the wheel on the alternate screen when the
  program asks for it; DECSET 1007 can later make alternate scroll
  switchable.
- Selection (#30) and search (#28) can number lines the same way
  (`scrollback_pushed - offset`) to keep a highlight on its text while
  output continues.
- A resize keeps the distance from the live screen, not the lines in view;
  following lines through a re-wrap would need the reflow to report where
  each line went.
