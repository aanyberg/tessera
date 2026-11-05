# ADR 0027: Mouse reporting and input routing

**Status:** accepted · 2026-11-01

## Context

Issue #12 asks for mouse reporting: programs such as vim (`set mouse=a`),
tmux (`set -g mouse on`) and htop turn on a tracking mode (DECSET 9, 1000,
1002, 1003) and an encoding (DECSET 1005, 1006, 1015, 1016), which #8
already stores in `Modes`, and expect press, release, drag, motion and
wheel events as escape sequences on their input. Since #26 the frontend
sends every mouse event to `App::send_input`, which only recorded it.

The same events also drive selection (#30) and link clicks (#30), so
something must decide, per event, whether the program or the frontend gets
it. The decision needs the tracking mode, which lives in the core under the
terminal lock, and state the frontend does not have (which presses the
program saw). The behaviour is pinned by
`crates/tessera-term/tests/fixtures/mouse_spec.txt` (177 rows), converted
from the spec-researcher's tables (xterm ctlseqs and `button.c`, Alacritty,
kitty, Ghostty, iTerm2, WezTerm) with the milestone lead's decisions.

## Decision

**Encoder.** `tessera_term::encode_mouse(&MouseReport, MouseTracking,
MouseEncoding) -> Option<Vec<u8>>` is pure: one event in, the report's
bytes or `None` out. It follows xterm's `BtnCode`/`EmitButtonCode`: buttons
0-2, wheel 64-67, back/forward 128/129, +4 Shift, +8 Alt (Meta), +16
Control, +32 motion, code 3 for no-button motion and for a release in the
default, UTF-8 and urxvt encodings (SGR keeps the button and ends in `m`).
X10 reports left, middle and right presses only, without modifiers. The
wheel is pressed only, never released. `encode_mouse` always starts a
report with the 7-bit `ESC [`; in 8-bit mode (S8C1T) `tessera-app-core`
rewrites it to the C1 byte `0x9B` with `tessera_term::to_8bit_controls`
before the report is queued (#96, ADR 0024).

Limits: the default encoding cannot carry a one-based column or row above
223, UTF-8 above 2015. A press, motion or wheel event beyond is not
reported (Alacritty, kitty, Ghostty; xterm writes a NUL byte there, which
confuses parsers). A release is never dropped, since that would strand the
program with a button held: its coordinates are clamped to the limit. SGR
and urxvt have no limit.

SGR-pixels (1016) reports the pointer's pixel in the text area, one-based
(`pixel + 1`, as xterm prints it and as every other value of the format is;
kitty and Ghostty print the raw pixel). The frontend sends physical pixels
from the view's top-left corner; the core takes the view's padding off and
clamps to the text area (`cols * cell width` by `rows * cell height`), so a
drag out of the view reports the nearest edge, never 0 or a negative.
`InputEvent::Scroll` carries no pointer pixel, so a wheel report in 1016
uses the last mouse event's pixel when it lies in the wheel's cell, else the
cell's top-left pixel.

**Routing.** The core decides; the frontend routes by the outcome
`App::send_input` returns. `Consumed` means a report was queued for the
program; every event that writes nothing is `Ignored`, and the frontend may
then select or open a link. Per session the core keeps the buttons whose
press it reported and the position of the last report
(`tessera-app-core/src/mouse.rs`). The state is read, the event routed, its
report queued and the new state stored under one hold of the session's
input-state lock, so two threads sending to one session never store over
each other's held buttons (#194). A press or motion refused for want of
room (`WouldBlock`) leaves no trace; a refused release still forgets its
button, which is up whether or not the program heard of it, so later
motion is no drag of it (#194).

| event | tracking off | tracking on, no modifier | Shift held | Cmd held |
|---|---|---|---|---|
| press | Ignored | reported if the mode and encoding can carry it: Consumed | Ignored (selection) | Ignored (link) |
| release | Ignored | Consumed iff its press was reported | same as no modifier, Shift not encoded | same as no modifier, Cmd not encoded |
| motion | Ignored | reported in 1002 with a reported button held, or 1003, when the cell (pixel in 1016) differs from the last report's | Ignored | Ignored |
| wheel | scrollback (primary) or cursor keys (alternate) | one report per whole line on either screen, at most the screen height per event; X10: Ignored, nothing moves | scrollback (primary) or cursor keys (alternate) | Ignored |

- **Shift bypasses** reporting, as in xterm, kitty, WezTerm and Ghostty;
  Option does not (iTerm2's choice) and is reported as Meta, so Shift+Option
  can mean block selection while a program tracks the mouse. Shift is never
  encoded by the routing, although the encoder can encode it.
- **Releases pair with presses.** A release is sent iff its press was
  reported, whatever the modifiers at release time: a Shift-click that went
  to selection never sends a stray release (xterm would), and a reported
  press is always released, even with Shift or Cmd down by then (xterm
  would drop it).
- **A drag reports the lowest held button** the program saw (left, middle,
  right, back, forward; xterm's `FirstBitN`), whichever button the
  frontend's drag event names. A drag whose press went to selection is
  reported as plain motion (1003) or not at all (1002).
- **Motion is deduplicated per cell** (per pixel in 1016) against the last
  report of any kind, press, release and wheel included (xterm's
  `mouse_row`/`mouse_col`).
- **Leaving tracking forgets.** The first mouse event that finds tracking
  off clears the held buttons and the baseline, so a drag interrupted by
  `DECRST 1002` sends no release after `DECSET 1002`. The check is lazy (on
  the next event, not on the mode change): a program that turns tracking
  off and on again between two mouse events keeps its held buttons and
  still gets its release.
- **Activating click.** The click that activates an inactive window is not
  reported, press nor release. The core has no notion of window activation;
  the frontend swallows the event (the Swift half of #12 records its
  routing in a "Frontend routing" section of this ADR).
- **Horizontal wheel** (buttons 66/67) is encoded but not routed:
  `ScrollEvent.delta_x_px` has no defined sign, and the frontend sends it in
  lines for a notched wheel and in pixels for a trackpad. Deferred to 0.2.0.

The click while the view is scrolled back on the primary screen is
reported with the row the frontend computed for the visible (scrolled)
screen. Programs that track the mouse on the primary screen (`fzf
--height`) then see a row of the live screen that is not under the
pointer. Not handled in 0.1.0.

### Frontend routing

`TerminalSurfaceView` (`apps/macos`) sends and routes as follows; it makes
no reporting decision of its own.

- **Every pointer event over the text goes to the core** through
  `TesseraSession.send(mouse:)`: presses, releases and drags of the left,
  right and other buttons, and plain motion with no button held from an
  `NSTrackingArea` (`.mouseMoved`, `.activeInKeyWindow`, `.inVisibleRect`),
  so motion arrives only while the window is key. Positions are the cell
  under the pointer and physical pixels from the view's top-left corner,
  padding included. `otherMouse` button numbers 2, 3 and 4 are middle,
  back and forward; higher buttons have no report and are not sent.
  Amended by #197 and #210: a press or a plain motion is not sent when no
  cell can be named for it, outside the render view (the legacy scroller's
  strip; the edges as AppKit's hit test draws them) or while the cell size
  is unknown (no surface, or between a lost surface and its re-attach).
  The drags and the release of a press that was sent always are, clamped
  to the grid's edge, or at the last position the core took while the cell
  size is unknown.
- **Modifiers** map Shift, Option, Control and Command to Shift, Alt (Meta),
  Control and Super; the core applies the bypasses.
- **The outcome of each press** is kept per button from press to release
  (`pressOutcomes`): `Consumed` means the program has the click, `Ignored`
  that the view may select or open a link (#30). A release is sent for every
  press the view sent, whatever the modifiers by then, and the core decides
  whether it is reported; the record stays until the core accepts the
  release, so a release it refuses (a position that is not a number) does
  not lose the button. A button still held when the view leaves its window,
  the window closes or its tab is switched away from (AppKit posts no
  notification for that; the view observes the tab group's selected window)
  is released to the core at the last position it took, since AppKit sends
  the release, if at all, elsewhere.
- **The activating click** is swallowed by AppKit: `acceptsFirstMouse`
  returns `false`, so the click that activates an inactive window, or the
  app, reaches neither the program nor a selection. A drag or release of a
  button whose press the view did not send is not sent either, so a stray
  release after that click cannot reach the core. AppKit has no first mouse
  for the right and other buttons, so the view drops their presses itself
  while its window is not key (#197): vim's `mousemodel=extend` acts on a
  right-click.
- **Selection and links** (#30, ADR 0025) start only from a left press the
  core answered `.ignored`: with tracking off, or with Shift (selection) or
  Command (link) held. The view then takes Option, or Option with Command, as
  a block selection, Shift with a selection as an adjust, Command alone as a
  link click and the click count as the granularity, all read at the press;
  a press the program took (`.consumed`) starts nothing, so Option-drag is
  the program's while it tracks the mouse and ⇧⌥-drag selects a block. A
  Command-click or Command-drag that is not on a link does nothing and sends
  nothing. A link opens through `LinkPolicy` (`http`, `https`, `mailto`,
  `ftp`, and `file` only for an empty, `localhost` or this Mac's host and an
  existing path); because link text is program output, a local file that
  would launch something (an application or other package, an executable or
  script, `.command`, `.webloc` and similar) is shown in Finder instead.
  A press while text is marked is first offered to the input method
  (`NSTextInputContext.handleEvent`, as `NSTextView` does); if it does not
  use it the composition is committed, the way a commit by mouse is, and
  the click goes on. Pointer events outside the view keep arriving during a drag; one
  above or below the view scrolls it (a timer at iTerm2's pace, stopped by
  the release).
- **The wheel** stays with `scrollWheel`, which sends every gesture as
  before; a `Consumed` scroll redraws. Amended by ADR 0032 (#183): while
  text is marked the event is not sent at all (no scroll, no cursor keys,
  no report), and the scroll bar flashes only when the scroll position's
  offset changed, not on every `Consumed` (a reported wheel moves nothing).

## Consequences

- The frontend's job is to send every pointer event, plain motion included
  (an `NSTrackingArea`), and to start a selection only on `Ignored`.
  Routing is covered by Rust tests on a fake PTY (the routing rows of the
  fixture) and an FFI test with a real child.
- Locks per mouse event, one after the other (#194): the terminal lock once
  to read the modes and grid size, as for a key; in SGR-pixels mode the
  view table once for the padding and cell size; then the session's
  input-state lock once, held while the event is recorded, routed, its
  report queued (the PTY lock for the exit check and the outbox lock, taken
  and released under it) and the new report state stored. A wheel event
  that reports takes, after the terminal lock, in SGR-pixels mode the
  input-state lock once for the last pointer and the view table once for
  its pixel, then the input-state lock once for the whole lines, before
  that last section: two input-state locks, three in SGR-pixels mode, and
  the view table at most once.
- Programs that use the default encoding on grids wider than 223 columns
  never see presses beyond it; their releases arrive clamped. Programs
  that care use SGR (tmux, vim with `ttymouse=sgr`, ncurses through the
  terminfo `XM` string all do).
- A flood of 200,000 motion events through `send_input` takes 0.4 to 0.7 s
  in a debug build (the slowest event under 5 ms, peak footprint 8.5 MB
  against 3.2 MB for an idle test) and queues only the cell changes; each report is queued
  like a key, so a child that stops reading fills the pending input to its
  bound (ADR 0011) and further reports fail with `WouldBlock`.
