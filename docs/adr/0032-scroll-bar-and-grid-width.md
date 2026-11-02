# ADR 0032: Scroll bar and grid width

**Status:** accepted · 2026-11-02

## Context

The #31 frontend (ADR 0023, "Consequences") put an overlay `NSScroller`
over the terminal's right edge and kept the grid at the view's width. The
#31 review and its adversarial run (issue #183) found that:

- With macOS's "Show scroll bars: Always" (AppKit's preferred scroller
  style `.legacy`, which "Automatically" also gives with a mouse attached)
  the scroller stayed over the last column, where Terminal.app and iTerm2
  put it beside the text.
- A device whose scroll events carry no gesture phase (a smooth-scrolling
  utility, some mice) could not scroll by momentum after a key: the flag
  that drops a gesture's momentum after a key was cleared only by a
  gesture phase that begins.
- The wheel, the View menu's scroll items and the scroller moved the view
  during an input method composition, away from the cursor the candidate
  window is anchored to.
- The pointer was an I-beam over the scroller.
- Since #12 the core reports a wheel event it sends to a program that
  tracks the mouse as `Consumed` (ADR 0027), so the scroller flashed on the
  primary screen although nothing scrolled.
- (a) A faded-out scroller kept taking clicks: it asked whether its flash
  animation was still on its layer, and Core Animation removes a finished
  animation only at its next commit, which an idle terminal never makes.
- (b) Releasing a knob held still during output jumped the view: AppKit
  sends one more action on release with the knob's frozen value, which
  then applied to a larger history.
- A scroller hidden while the pointer was over it (the history went away)
  came back revealed, as if still under the pointer.

## Decision

### The grid narrows beside a legacy scroll bar

`TerminalSurfaceView` stays the window's content view, the first
responder, the `NSTextInputClient`, and the owner of the session, the
`TesseraSurface` and the scroller. It is now a plain layer-backed view with
no background of its own. The `CAMetalLayer` is the backing layer of a new
subview, `TerminalRenderView`, with the settings ADR 0017 gave the view
(`framebufferOnly`, opaque until the core says otherwise, top-left gravity
and placement, `layerContentsRedrawPolicy = .never`). Its `hitTest(_:)`
returns `nil`, so every pointer event still reaches `TerminalSurfaceView`.
The scroller is its sibling, above it.

The core is never given a pixel size smaller than the layer it draws into:
a narrower surface under a full-width layer would leave the drawables
mismatched (`.suboptimal` resends, a stretched or unpainted strip). Nor is
a container view made the window's content view, which would break the
`contentView as? TerminalSurfaceView` contract the app and its tests use.

Layout, from the scroller's effective style (the test override, else
`NSScroller.preferredScrollerStyle`):

- The render view's frame is `(0, 0, width - reserved, height)`, where
  `reserved` is `NSScroller.scrollerWidth(for: .regular, scrollerStyle:
  .legacy)` in legacy style and 0 in overlay style. The scroller's frame is
  the right-edge strip as wide as a regular scroller in that style.
- `metalLayer` is the render view's layer, and the surface's pixel size is
  the render view's size in backing pixels. The composition overlay stays a
  sublayer of the metal layer. The render view shares the outer view's
  origin and height, so cell coordinates do not change.
- The width depends on the style only, never on the history or the screen
  shown: entering or leaving the alternate screen never reflows the grid or
  sends SIGWINCH. A legacy scroller is therefore always shown, with no
  history and on the alternate screen too: it is disabled, draws its track
  with no knob, and takes no action (`target(for:)` is `nil`). Nothing else
  paints the strip, as in Terminal.app. An overlay scroller is hidden with
  no history, as before.
- `NSScroller.preferredScrollerStyleDidChangeNotification` and a change of
  the test override go through one path: the scroller restyles itself and
  calls `onStyleChange`, and the view lays out again and passes the render
  view's new size to the core through `surfaceSizeChanged()` (at once, or
  coalesced during a live resize): one resize per change.
- Window frames are unchanged: new windows and frames restored by ADR 0031
  keep their size in points, and in legacy style the grid has a column or
  two fewer.

### Momentum after a key

A key, a paste or a keyboard scroll sets `ignoresMomentum`; while it is
set, events with a momentum phase are dropped. It is cleared by an event
that starts a new gesture:

- an event whose gesture phase contains `.began` (as before);
- an event with no gesture phase, no momentum phase and precise deltas (a
  device or utility that sends no phases);
- an event whose momentum phase contains `.began` and whose gesture phase
  is empty, provided no event with a gesture phase arrived since the flag
  was set (a device whose events all carry a momentum phase). A trackpad
  flick interrupted by a key while the fingers are down still has its
  momentum dropped: its `.changed` and `.ended` events after the key mark
  the momentum that follows as its own, and a further key before the
  momentum begins does not unmark it (only setting the flag when it was
  clear forgets the phased events).

A notched wheel (no phases, line deltas) does not clear the flag, and is
never dropped, since its events have no momentum phase.

### Nothing scrolls during a composition

While text is marked:

- A wheel event is not sent to the core at all, so it neither scrolls the
  view, nor sends cursor keys on the alternate screen, nor writes a wheel
  report. The core cannot be asked to report a wheel without scrolling, and
  the input method owns input during a composition, as it does keys. The
  momentum bookkeeping above still runs on the dropped event, so a gesture
  that started during the composition behaves as any other after it.
- The scroller's knob drags and track clicks move nothing; the knob returns
  to the view's position when the button goes up.
- The View menu's four scroll items are disabled (`validateMenuItem`), so
  ⌘↑/⌘↓/⌘Home/⌘End do not scroll.

### The scroll bar shows when the view moved

`scrollWheel` reads `scrollPosition` before and after `send(scroll:)` and
flashes the scroller only if the outcome is `Consumed` and the offset
changed; every `Consumed` scroll still redraws. Two O(1) reads per wheel
event (at most about one per frame). Output that pushes lines while the
view is scrolled back between the two reads can flash it once for a wheel
reported to a program: accepted. `scroll(to:)` (the menu, the scroller)
flashes unconditionally, as feedback for an explicit command.

### Scroller details

- **Cursor.** `cursorRects()` returns an I-beam over the text and an arrow
  over the scroller's frame while the scroller is not hidden (an overlay
  scroller shows when the pointer reaches the strip), and an I-beam over
  the whole view while it is hidden; `resetCursorRects` applies them. The
  rects do not overlap, since AppKit leaves overlapping cursor rects
  undefined: over an overlay scroller the I-beam stops at its left edge.
  The window's cursor rects are invalidated when the scroller's hidden
  state, frame or style changes.
- **Hidden under the pointer.** When the scroller becomes hidden it forgets
  the pointer and any flash; shown again it is not assumed to be under the
  pointer, and waits for a scroll or `mouseEntered`.
- **(a) Fade.** `flash()` records when the fade ends (now + hold + fade, on
  an injectable clock); `isRevealed`, and so `hitTest`, compares against
  it instead of asking whether the animation is still on the layer. Being
  shown for good clears it.
- **(b) Knob release.** Within one tracking session (`mouseDown` to its
  end) a `.knob` or `.knobSlot` action whose value equals the last one sent
  is not sent again. A press that lands on the knob
  (`testPart(event.locationInWindow) == .knob`) records the knob's value
  as already sent, so a knob clicked and held still sends nothing on
  release (AppKit sends one action then, with the value it was pressed
  at, which would apply to a history grown meanwhile); a press elsewhere
  in the track (a `.knobSlot` jump) still sends its first action. Track
  clicks (`.decrementPage`, `.incrementPage`) are not deduplicated.
- **Line parts.** `.decrementLine` and `.incrementLine` stay in
  `target(for:)`: macOS draws no scroller arrows, but VoiceOver's increment
  and decrement buttons reach them.

## Consequences

- With "Show scroll bars: Always" the text never sits under the scroll
  bar, at the cost of a column or two at the same window size.
- A live change of the setting resizes every terminal once; full-screen
  programs redraw at the new width.
- Phase-less precise devices and momentum-only devices scroll again after
  a key; a trackpad flick interrupted by a key is still dropped.
- During a composition the wheel does nothing, including for a program that
  tracks the mouse.
- AppKit reports the key equivalent of a disabled menu item as handled
  (`NSMenu.performKeyEquivalent` returns `true` without sending the
  action), so during a composition ⌘↑/⌘↓/⌘Home/⌘End are swallowed rather
  than handed to the input method. Handing them on would need the menu's
  key equivalents to stand aside, a change to the menu, not to this view.
- Not covered: the horizontal wheel and DECSET 1007 (ADR 0023, 0027).

## Manual checks

- "Show scroll bars: Always": the grid ends left of the scroller, which
  shows its track with or without history and on the alternate screen
  (vim, less, htop); entering or leaving the alternate screen does not
  redraw the program at a new width.
- A live change of the setting (Always and When scrolling, Automatically
  with and without a mouse) while a shell and while vim run: one resize,
  the program redraws at the new width, no stray strip.
- A legacy scroller with a translucent theme, in light and dark
  appearance: the track is drawn and the knob is visible.
- Trackpad momentum through a long history, then typing during it: the
  view stays on the live screen; mouse-wheel notches still scroll after
  typing; a phase-less device scrolls again after typing.
- A Japanese composition, then the wheel, ⌘↑/⌘↓/⌘Home/⌘End (items
  greyed) and a scroller drag: nothing scrolls, the candidate window stays
  at the cursor.
- The arrow cursor over the scroller strip, the I-beam over the grid.
- After a scroll, let the overlay scroller fade, then click at the right
  edge: the click reaches the terminal.
- Drag the knob and hold it still during fast output, then release: the
  view does not jump.
- In vim, htop or `fzf --height` with mouse tracking, the wheel on the
  primary screen with history: the program scrolls, the scroller does not
  flash.
