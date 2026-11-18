# ADR 0041: VoiceOver, Increase Contrast and Reduce Motion

**Status:** accepted · 2026-11-08 · amended 2026-11-18 (#357: D10, the
manual checks, the known limit)

## Context

Issue #52: a terminal view was one opaque `NSView` to assistive clients.
VoiceOver could not read the screen, follow the cursor or hear new output,
and the system's Increase Contrast and Reduce Motion settings were ignored
(Reduce Transparency is ADR 0043's). Terminal.app and iTerm2 both expose the
view as one `AXTextArea` whose value is the visible text and announce new
output; this follows them.

Exposing terminal text to assistive clients is a new surface (security rule
S9): the text is the child's, untrusted, and may hold secrets.

## Decision

**One text area per pane.** `TerminalSurfaceView` is an accessibility
element with role `AXTextArea`
(`TerminalSurfaceView+Accessibility.swift`). Its value is the text its view
shows; line `n` is row `n` of the view (D-3); the insertion point is the
cursor and the selected range is the selection, in UTF-16 units. The label
is the pane's title, which the core already sanitised (S3, ADR 0022), and
`terminal` while it has none. Nothing set through the element takes effect
(D-6): a terminal is not edited through its text. The selected range is
never settable (setting it would move neither the cursor nor the terminal
selection). The value is settable, through a setter that does nothing, only
while VoiceOver is off, as in iTerm2: Full Keyboard Access sends Space to
the view as a key press only when its value is settable, and while VoiceOver
is on a settable value wedges its QuickNav. The
parameterised attributes (line for index, range for line, string for range,
range for index, range for position, bounds for range) are answered from
one snapshot; only cell-to-screen rectangles are computed in Swift.

**The core builds the text.** `App::accessible_text` (FFI
`tessera_session_accessible_text`, an opaque snapshot freed with
`tessera_accessible_text_free`, read through `tessera_accessible_text_*`)
copies the rows the view shows under the terminal lock, the same copy a
frame's capture makes, with the selection clipped to the view unchecked, as
a frame draws it (a selection output overwrote is cleared by the next
frame's check and told at the next settle), and builds the text after the
lock is released (`crates/tessera-app-core/src/accessibility.rs`;
`Capture::of_view` is all that runs under the lock, and a release test
holds it to `Snapshot::capture`'s time for the same grid):

- Blanks after a row's last text are dropped, except on the cursor's row,
  which keeps them up to the cursor so the insertion point is inside the
  text (D-1). Blank rows below the cursor's row are dropped.
- A wide character is one character over two cells (D-2); a grapheme
  cluster gives its whole text.
- A snapshot owns its data, outlives its session and may be read from any
  thread.

**The text is sanitised and capped (S3, S5).** A NUL becomes a space, a C0,
DEL or C1 control becomes U+FFFD, the bidirectional formatting controls
(U+061C, U+200E, U+200F, U+202A to U+202E, U+2066 to U+2069) are removed
and U+2028 and U+2029 become spaces, so what VoiceOver reads cannot be
reordered or split differently from what the screen shows. Cell text past
`MAX_TEXT_UNITS` (4 Mi UTF-16 units, `TESSERA_MAX_ACCESSIBLE_TEXT_UNITS`) is
dropped; a full grid of the largest size is 2 Mi units of plain text, so
only clusters of many scalars reach it. The text is never logged (S8).

**Announcements.** `announcement(previous, current)`
(`tessera_accessible_text_announcement`) gives the lines new output added
between two snapshots of a view: from the line the old cursor was on to the
line the new one is on, by line number so output that scrolled is
followed, without blank lines and without the old cursor line while its
text is unchanged (the typed command is not read again). Nothing is
announced while the cursor stays on its line (typing, a progress bar) or
either snapshot is scrolled back. Nothing is announced either when the
terminal's line numbering changed between the snapshots: a resize to
another size (a split, a zoom, a window or live resize, even one back to
the same size) or a clear to the cursor bumps `Terminal::line_epoch`, and
entering or leaving the alternate screen (a full-screen program starting
or quitting) flips the screen flag; both are kept in the snapshot and
compared, the value still changes (`valueChanged` only) and output after
that is announced again. When the cursor moved up on the same screen and
size (a clear) the lines from the top to the cursor are. At most
`MAX_ANNOUNCEMENT_CHARS` (1024, `TESSERA_MAX_ANNOUNCEMENT_CHARS`)
characters, the newest kept, cut between characters (D-5).

Known limit (M-C2-1): output arriving in the same settle (0.15 s after the
last change, at most 1 s) as a resize, screen switch or Cmd-K is not
announced, because the suppression compares only the two snapshots of one
settle: the `git commit` summary printed right after vim quits, or up to
1 s of build output during a split. The direction for the fix, left to
0.3.0: on leaving the alternate screen compare with the last primary-screen
snapshot told (output on the alternate screen pushes no lines), and keep a
baseline per line epoch.

**Notifications are bounded (S6).** `TerminalAccessibility` (one per view)
tells VoiceOver about output:

- Only while VoiceOver is on (`NSWorkspace.isVoiceOverEnabled`, the only
  client a public API reports, D-7). Without it an output callback only
  drops the cached snapshot: no snapshot, no timer, no post.
- Output settles first: 0.15 s after the last change (cancelled and
  re-armed by each one), or 1 s after the first while output keeps coming,
  so a flood is told once a second and once after it stops.
- A settle takes one snapshot and compares it with the last one told: new
  text is announced (`announcementRequested` to `NSApp`, priority medium
  so it queues after what VoiceOver is saying, D-4; iTerm2 uses high)
  before `valueChanged`; a moved cursor or selection posts
  `selectedTextChanged`. `layoutChanged` is never posted.
- Only the pane with the keyboard focus announces (D10): the view that
  is the first responder of the key window, its `isAccessibilityFocused`.
  An announcement goes to `NSApp` and names no source, so output in a
  background window or tab or in an unfocused pane is not announced: its
  settle posts `valueChanged` and `selectedTextChanged` only, and records
  the snapshot, so focusing the pane later does not read what it missed.
  That holds with a settle still pending when the focus comes (24.16 M1):
  the change the focus gain makes (first responder or key window) takes a
  snapshot and records it without a post, so the settle reads only output
  after the focus (VoiceOver reads the element the focus moved to
  itself); a gain only the settle sees (no change told the view)
  announces nothing. Accepted: output printed just before the focus
  leaves, within the same settle, is never announced, since only the pane
  focused at the settle speaks.
  Nothing is announced while another application is active or the pane's
  find field has the keyboard. macOS reports where the VoiceOver cursor
  is through no public API; when VoiceOver moves the keyboard focus with
  its cursor (it sets the element's AXFocused, which AppKit makes the
  first responder) the pane it lands on announces, and a pane the
  VoiceOver cursor reads without the keyboard focus stays silent. iTerm2
  announces every session's output; this is quieter, and says only what
  the user is looking at.
- At most 10 posts in any second per view (a settle is delayed until the
  last second's posts leave room for its three), at most one timer
  pending, two snapshots held.
- VoiceOver turning on posts one `valueChanged`: the first settle after
  it, or after a view is created, has no snapshot told yet, so it posts
  `valueChanged` alone and records the snapshot later output is compared
  with; output printed while VoiceOver was off is never announced. A view
  out of its window posts nothing; the timer holds the
  controller weakly, so closing a tab is not held up by a pending settle.

**Increase Contrast.** `DisplayOptions` gains `increase_contrast` (ADR 0043,
amended): the core draws the cursor and the selection fully opaque in their
theme colours, and `App::dim_inactive_panes` is 0 while it is on, so an
inactive pane is never darkened. The app reads
`NSWorkspace.accessibilityDisplayShouldIncreaseContrast` with Reduce
Transparency on the same observer (#42's) and calls the core only when one
of the two changed.

**Reduce Motion stays in Swift.** The only motion the app has is the
overlay scroll bar's fade. Under
`NSWorkspace.accessibilityDisplayShouldReduceMotion` a flash shows at full
opacity at once and hides after the hold with no animation; turning the
setting on or off during a flash settles it. Every scroll bar follows a
change, and new ones start with the setting.

## Consequences

- Switch Control, Voice Control and Zoom can query the view but are told
  about no output: no public API reports them as attached.
- A snapshot copies the view's rows once per settle and per query burst
  (the cached one is kept until the content changes). It holds the text,
  a few kB for a normal view and at most `MAX_TEXT_UNITS` (8 MiB), plus a
  4-byte offset per cell whatever the text (8 MiB at 1024x2048): about
  16 MiB in the core. The wrapper also copies the text into a Swift
  `String` (UTF-8: up to three bytes per UTF-16 unit, so at most 12 MiB,
  2 MiB for a full 1024x2048 grid of plain text), so about 28 MiB at most
  per snapshot. The app holds two per view (the cached one and the last
  told).
- Terminal content now reaches assistive clients, as on screen. It reaches
  only the processes macOS lets query the app's accessibility tree (those
  the user granted Accessibility access), the same trust as reading the
  screen; it is never logged or written anywhere by Tessera.
- Lines beyond the view (scrollback) are not part of the value; VoiceOver
  reads them after the user scrolls the view.

## Manual checks

These need a person at the keyboard; tests cover the selector, the setter
and the focus gate, not what the system does with them.

- Full Keyboard Access on (System Settings, Accessibility, Keyboard),
  VoiceOver off: click into a pane first, so it has the keyboard focus
  (Full Keyboard Access's focus ring may start elsewhere in the window),
  then type `echo a b` and Return; the shell prints `a b` (Space reaches
  the shell). With Full Keyboard Access and VoiceOver both on, Space may
  be swallowed, as in iTerm2: that combination is not part of the check.
  If Space does not reach the shell with VoiceOver off, the fallback is a
  no-op `setAccessibilityContents(_:)` beside the `setAccessibilityValue`
  one; it is not added unless this check fails.
- VoiceOver on: QuickNav (Left and Right arrows together) toggles and the
  arrows move through the text; VoiceOver reads new output and follows the
  cursor.
- VoiceOver on, two windows, the first split: make them with New Window
  (⌘N) twice and, in the first, Split Right (⌘D, in the
  Shell menu). Background window: type `sleep 3; echo far` there, then
  click into the other window within 3 s: `far` is not read, then or once
  you click back. Unfocused pane: in the left pane type `sleep 3; echo
  near`, then click the right pane within 3 s: `near` is not read. Move
  the VoiceOver cursor into the left pane (VO-Right Arrow, or VO-Command
  with the arrows) without clicking: output there (`sleep 3; echo
  cursor`) is read only if VoiceOver moved the keyboard focus with it
  (the pane becomes the first responder); note which in #428. Output in
  the focused pane (`echo here`) is read.
