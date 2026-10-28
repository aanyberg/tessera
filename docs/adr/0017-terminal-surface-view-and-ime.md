# ADR 0017: The terminal surface view, its display link and input methods

**Status:** accepted · 2026-10-28

## Context

Until #27 a tab showed a placeholder label. The core can now draw a session
into a `CAMetalLayer` (ADR 0014) and take keyboard, mouse, scroll, paste and
focus events (ADR 0011). Issue #27 connects them in the Swift shell:
`TerminalSurfaceView`, an `NSView` that owns the layer, drives frames, and
turns AppKit's events, including dead keys and input methods (IME), into
`TesseraKeyEvent`s. It also wraps both C APIs in TesseraKit
(`TesseraSurface`, `TesseraSession+Input.swift`) and carries two items
deferred to it: coalescing resizes during a live window drag (every resize
re-wraps the scrollback under the terminal lock, 10-100 ms at 100k lines)
and the manual Retina and translucency checks from #20 and #25.

The keyboard behaviour is pinned by a spec fixture (`ime_input_fixture.txt`,
about 30 cases from Ghostty's and winit's AppKit code, the core's encoder
and Apple's `NSTextInputClient` documentation), and the lead's decisions on
it (D1 to D8 below).

## Decision

### TesseraKit wrappers

- `TesseraSession` gains `send(key:)`, `send(mouse:)`, `send(scroll:)`,
  `paste(_:)` and `setFocused(_:)` with Swift mirrors of the C structs
  (`KeyEvent`, `MouseEvent`, `ScrollEvent`, `Modifiers`, `InputOutcome`).
  `Key` mirrors `TesseraKey` case for case with the C values as raw values
  (113 keys, generated from the header; a test checks the count, the range
  and spot values against the C constants). An `InputOutcome` value the
  wrapper does not know reads as `.ignored`, like the core's value after a
  failure.
- `TesseraSurface` (`@MainActor`, final) wraps `tessera_surface_*`. It holds
  a strong reference to the layer from `attach` until the core has let go of
  it: `detach()` returned, or a call reported `NO_SURFACE` or
  `SESSION_NOT_FOUND` (the core detached after a lost swapchain, or the
  session closed, which detaches first). `deinit` detaches a still-attached
  surface on the main thread; it is nonisolated (Swift 6.1 on CI has no
  isolated `deinit`), so off the main thread it hops there keeping the layer
  alive. That deferred detach is keyed by session, so each attach records a
  generation per session and the detach runs only while its generation is
  still the latest: it never detaches a newer surface attached to the same
  session meanwhile. Releasing a surface off the main thread and attaching
  a new one for the same session at once fails (`invalidArgument`) until
  the deferred detach has run on the main thread: the core still holds the
  old attachment. `size` is the size the core last accepted.

### A layer-backed view with `makeBackingLayer`

The view sets `wantsLayer` and returns a `CAMetalLayer` from
`makeBackingLayer()`, so AppKit sizes, positions and scales the layer with
the view and the layer moves with the view in the layer tree (tabs, full
screen). A layer-*hosting* view (assigning `layer` before `wantsLayer`)
would leave all of that to us. `layerContentsRedrawPolicy = .never` and a
top-left `contentsGravity`/`layerContentsPlacement` keep AppKit from
redrawing or stretching the core's frames while a resize is coalesced.
`framebufferOnly` is set, and the core (wgpu) reconfigures device, pixel
format, drawable size and opacity on attach; after attaching, a layer the
core made non-opaque (a translucent theme) makes the window non-opaque with
a clear background, so the translucency reaches the desktop. The view
attaches when it enters a window and detaches when it leaves it or the
window closes (`NSWindow.willCloseNotification`, before the controller
closes the session; either order is safe).

### `CADisplayLink`, not a timer

Frames come from `NSView.displayLink(target:selector:)` (macOS 14+; the
deployment target is 15.0), on the main run loop in `.common` modes, so it
keeps running during a live resize and menu tracking. A display link fires
in step with the display the view is on (ProMotion included) and follows
the window between displays, where a timer would drift, tear or wake the
CPU for nothing; `CVDisplayLink` would call back on its own thread, while
every `tessera_surface_*` call must be on the main thread. The link retains
its target, so the target is a small proxy holding the view weakly.

Each tick applies a coalesced resize, re-attaches a surface the core lost
(at most once a second), and renders once if anything changed since the
last frame: output (`sessionDidOutput` sets a flag, so a flood costs one
frame per refresh), a resize, a focus change, a scroll that moved the view.
The link is paused while the window is occluded or minimised
(`occlusionState`, `didChangeOcclusionState`, `didMiniaturize`,
`didDeminiaturize`), and a tick already queued when the window became
hidden renders nothing; a window that becomes visible again is redrawn.
The link is also paused while there is nothing to do (no frame, resize or
re-attach pending), and any of those wakes it, so an idle terminal costs
no refreshes; only an attached surface has frames to draw, so a view whose
first attach failed (no GPU or font, never retried) idles too. A
`SUBOPTIMAL` frame resends the size, at most three frames in a row and
never in a tick that already passed a coalesced resize on, so a layer that
stays suboptimal does not resize the session on every refresh; a `GPU` error retries next frame; `NO_SURFACE` marks the
surface for re-attach. Re-attaching is tried once a second, at most five
times in a row; after that the view stops (logged once, the link idles)
until the view's size, scale or window changes; a surface lost before the
view moves to another window is retried there. The link and window
observers are set up once per window: moving the view between superviews
of the same window keeps them.

Cursor blink is not implemented: the C API draws a steady block cursor and
has no blink or visibility input (follow-up below).

### Resize coalescing in the view

`setFrameSize`, `layout` and `viewDidChangeBackingProperties` report the
view's backing size and scale. Outside a live resize it goes to the core at
once. Between `viewWillStartLiveResize` and `viewDidEndLiveResize` it is
only recorded, and the next tick passes it on, so at most one resize per
display refresh reaches the core, and the final size is sent when the live
resize ends. An unchanged size is never resent. The core applies every
resize it receives; coalescing lives in Swift because only the frontend
knows when a drag starts and ends.

### Keyboard and input methods

`keyDown` runs `interpretKeyEvents`; `NSTextInputClient` collects what the
input system does during that call (committed text, marked text, editing
selectors), and the view decides afterwards, with
`markedTextBefore = hasMarkedText()` taken first:

- **Composing.** If text was marked before the key or is marked after it,
  the key belongs to the input method: only text it committed is sent, and
  the key itself never is. The Return that confirms a composition, the
  Escape that cancels it, Backspace inside it and arrows that move in it
  send nothing (D3, as Ghostty; winit forwards some of them). Committed
  arrow keys are not replayed (D4).
- **Committed text** is sent with `send(key: .unidentified, text:)`, never
  `paste` (D1): pastes are sanitised and, once DECSET 2004 is honoured,
  bracketed, which would make every IME commit look like a paste to shells
  and editors (Alacritty's behaviour; Ghostty sends commits as key events).
- **Typed text** (no composition) names the key only when the text is the
  key's own character, unshifted or shifted (`a` or `A` for the A key,
  `ö` on the Swedish layout); otherwise (Option-a gives `å`, a dead key and
  Space give `´`) the event is text only, so the core sends the text and not
  the key (D2: the core's encoder ignores composed text for `Char(' ')`).
  Composed text never carries the Alt bit (D8: Option composes; an
  option-as-Alt preference is future work). Anything printable the input
  system committed goes this way, so it can never become the key with Alt:
  the Apple logo U+F8FF (Option-Shift-K on US, Option-a on Swedish,
  Norwegian and Finnish; 88 of 251 layouts type it somewhere) is text, not
  `ESC K` or `ESC a` (which zsh's accept-and-hold would act on).
- **Keys without text** are named from their virtual key code first, then
  from the editing selector AppKit chose (`insertNewline:`,
  `insertBacktab:`, `moveWordLeft:` ...), and otherwise sent as their
  unshifted character with the modifiers held (D6). Key codes are primary
  because selectors lose the real modifiers and can be remapped by
  `DefaultKeyBinding.dict`. Committed text is stripped of controls (C0
  including CR, LF and tab; DEL; C1) and of AppKit's function-key
  characters (U+F700 to U+F8FE; U+F8FF is the Apple logo), since the core
  would ignore the whole text otherwise, and the rest is sent (a multi-line
  dictation commit types its lines joined, never CR or LF). Only when
  nothing is left does the commit stand for the key itself.
- **Control** combinations skip the input system unless it is composing
  (Japanese input uses Ctrl-J/K then) and go to the core as keys with no
  text, carrying the US-layout key at the same position (`base_layout`) for
  non-Latin layouts. What they encode to is the core's decision.
- **Command** combinations that are not menu key equivalents (which AppKit
  handles before `keyDown`) are not sent to the core: they go up the
  responder chain, ending in a beep, as in Terminal.app. The core has no
  Super bindings yet and its encoder turns Super into kitty-style `CSI u`
  sequences (Cmd-V would type `ESC[118;9u` into the shell, and there is no
  Edit menu until #29). Revisited when the core gains Super bindings (#43).
- `unmarkText`, losing first responder and the window losing key status
  discard a composition without sending it
  (D5, as both reference terminals, despite the protocol's "commit"
  wording). Empty `markedRange` and `selectedRange` are `{NSNotFound, 0}`
  (D7); ranges count UTF-16 units. `insertText` outside a key press (a
  candidate clicked, the character viewer, dictation) is sent at once.
  Ghostty's `NSApp.currentEvent` guard is not used.

The **composition overlay** is a `CATextLayer` sublayer of the metal layer,
drawn underlined on the text background over the composition cell, not an
`NSTextField`, and the core knows nothing of it. Its colours are resolved
against the view's appearance and refreshed when it changes (light and
dark mode). At most 2048 characters are drawn and the layer is clipped to
the view's width, so an input method marking a huge string cannot make a
huge layer.
`firstRect(forCharacterRange:actualRange:)` returns the same cell in screen
coordinates, so the candidate window opens just below it.

The C API exports neither the cell metrics (`App::view_cell_metrics`), the
surface size readback (`App::view_size`) nor the cursor position, which the
issue assumed. This issue makes no Rust change (lead decision); #90
exports them. Until then: the composition cell is the grid's first cell (inside the theme's 4 pt
padding) rather than the cursor's, so the overlay and candidate window open
at the top left of the terminal; mouse and scroll events carry exact pixel
positions and cell 0,0 (the core only records the position today); trackpad
scrolling converts points to lines with a 16 pt line; and
`TesseraSurface.size` records what the core accepted. Once the core exports
cell metrics and the cursor cell, `TerminalSurfaceView.cellGeometry` and
`compositionCellRect()` take them and nothing else changes.

### Focus and the pointer

The session is focused while the view is first responder *and* its window
is key: `becomeFirstResponder`, `resignFirstResponder` and the window's
`didBecomeKey`/`didResignKey` drive `setFocused` (the renderer draws an
unfocused cursor hollow). Losing first responder or key status discards a
composition, dropping anything the input context commits while it
discards. The view
is the window's `initialFirstResponder`; `resetCursorRects` sets the I-beam
over the view. A mouse-down makes the view first responder. `mouseDown/Up/
Dragged`, the right and other buttons and `scrollWheel` go to the core; a
scroll the core reports `CONSUMED` moved the view and is redrawn. Selection
and copy are #30.

### Testing

`interpretKeyEvents` with a synthetic `NSEvent` uses the machine's real
input source, so dead keys and Japanese composition cannot be produced
headless. `processKeyDown(_:interpret:)` takes the input system as a
closure: the tests pass the `NSTextInputClient` calls AppKit makes, in its
order, and read back the bytes the PTY received (a raw-mode `cat` writing
to a file), with a sentinel key proving that nothing else was sent. The
resize test drives 200 `setFrameSize` calls inside a simulated live resize
and ticks by hand; the display-link test forces the occlusion state,
because the hosted test app's windows are never on screen and a real link
never fires there. Whether the app's windows become key and visible depends
on the machine (a locked agent session: never; an unlocked CI runner: yes),
so the view under test lives in a test window that is never ordered on
screen and reports the key state the test sets, and the tests drive key
changes through the same entry point AppKit's notifications use. The suite
runs with test windows starting key (`TEST_RUNNER_TESSERA_TEST_WINDOWS_KEY=1`)
and not key, and passes both ways. Surface tests skip with a reason when no Metal device or
renderer is available.

## Consequences

- The lead's manual checks remain (below): real input methods, the
  candidate window's position, tearing during a resize, display moves and
  translucency cannot be observed headless.
- Follow-ups for the core: #90 exports cell metrics, padding, the cursor
  cell and the surface size, so the overlay, candidate window and mouse
  cells follow the cursor; a cursor blink or visibility input;
  `Key::Char(' ')` ignoring composed text (D2, worked around in the view);
  Ctrl-ö sends `;` (0x3b, through `base_layout` under Ctrl) where the
  fixture expected nothing (B4); and the core's Super encoding once
  bindings exist (#43), when Command keys go back to the core.
- Known behaviour, by design or left to the manual checks:
  - A key whose commit mixes text and controls sends the text and drops
    the key: an input method or key binding that commits `ls\r` on Return
    sends `ls`, not the Return.
  - An input method that calls `setMarkedText` from inside
    `discardMarkedText` keeps its composition after the window loses key
    status.
  - Apple's Japanese input method commits its composition when it is
    deactivated, through `insertText` outside a key press, which the view
    sends. So "Cmd-Tab away mid composition: nothing sent" may fail with the
    real input method; the manual check decides whether that commit is
    acceptable.
  - Korean (and any input method that composes syllable by syllable):
    Return pressed mid-syllable commits the syllable and is swallowed (D3),
    so a second Return is needed to send it; checked by hand for usability.
- `TerminalWindowController`'s default content view is now
  `TerminalSurfaceView`; `TerminalPlaceholderView` is gone, and the exit
  status shows in a small label at the bottom of the view.

## Manual checks

To be run by the lead on hardware and recorded in the PR body with
screenshots. Rows from the fixture marked (unverified) are among them.

| Check | Expected | Result |
|---|---|---|
| US/ABC: Option-e e, Option-a, Option-u u, Option-\` a | é, å, ü, à (`cat -v`: `M-CM-)` for é) | pending |
| US/ABC: Option-e then x, then Space, then Escape | `´x`, `´`, nothing | pending |
| Swedish: å ä ö keys, Shift-ö, Option-e dead acute then e | å ä ö Ö é | pending |
| Japanese (Hiragana): にほんご + Return; Space converts with the candidate window; Escape cancels; Shift-Space; Ctrl-J/K while composing | にほんご / 日本語, no stray bytes; candidate window below the first cell (cursor-cell placement is the follow-up) | pending |
| Switch input source mid-composition | no stray bytes | pending |
| Live resize of a window with 100k lines of scrollback | no tearing or stretching, final size correct (`stty size`) | pending |
| Move a window between a Retina and a non-Retina display | crisp text on both, grid size recomputed | pending |
| Translucent theme (`#808080`, `opacity = 0.5`) | desktop shows through; does not look like `#bababa` (#20) | pending |
| Window minimised or fully covered while `yes` runs | CPU drops (link paused); resumes on restore | pending |
| Cmd-V, Cmd-K, Cmd-A (and Cmd-Shift variants) at a zsh prompt | nothing typed into zsh; a beep at most | pending |
| Idle visible window (prompt, no output) | near-zero CPU (link idles) | pending |
| Switch light/dark mode while composing | overlay colours follow | pending |
| Focus loss mid Japanese composition: click another window, Cmd-Tab away, switch tab | composition disappears, nothing sent | pending |
| Output in a background tab (`sleep 2; seq 1 50`), then select that tab | the tab shows the output (relies on AppKit's occlusion notification on tab switch) | pending |
| Cmd-. and Cmd-Backspace at a zsh prompt | nothing typed into zsh | pending |
| Option-Shift-K (US) and Option-a (Swedish) at a zsh prompt with text typed | the Apple logo appears; the line is not run | pending |
| Press-and-hold accents: hold e, then pick 2 (é); hold j in vim's normal mode | é inserted; vim moves down repeatedly, no stray bytes | pending |
| JIS keyboard: Eisu and Kana keys switch input mode | no bytes sent | pending |
| **Cmd-Tab away mid composition and back (Apple Japanese input method)** | composition gone, nothing sent; the input method may commit on deactivation instead (see Consequences): record which | pending |
| Korean 2-Set: type 한 then Return | 한 committed; note whether the Return is swallowed (D3) and a second Return is needed | pending |
