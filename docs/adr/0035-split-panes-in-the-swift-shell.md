# ADR 0035: Split panes in the Swift shell

**Status:** accepted · 2026-11-06

## Context

Issue #38 splits a tab into panes, each running its own session: ⌘D splits
the focused pane to the right, ⌘⇧D below, ⌘⌥ and an arrow move the focus,
⌘W closes the focused pane. ADR 0015 made every tab an `NSWindow` with one
`TerminalWindowController` and one session, the registry living in the
Swift shell, and left the "windows → tabs → pane tree → sessions" model of
`docs/architecture.md` for later. The core still knows sessions only.

## Decision

### Panes live in the shell (D1)

The pane tree is Swift, like the tab registry; the core is unchanged and
keeps one session per pane. A tab's controller owns a
`PaneContainerView`, the window's content view, which owns a
`PaneTree<Pane>`: a value-type binary tree whose leaves are `Pane`s (a
session and its `TerminalSurfaceView`) and whose inner nodes are splits with
a direction (`.right`: side by side, `.below`: stacked) and the share of the
space their first child takes. The tree's rules (splitting, closing, sizes,
spatial neighbours) are generic over the leaf and tested without AppKit.

- **Layout.** Each split is an `NSSplitView` (`PaneSplitView`, a fixed 1 pt
  divider), nested as the tree is. Each split's ratio is stored in the
  tree as a fraction; only a divider drag (which sets it to the share the
  drag left), a split or a close changes it. After every change the split
  views are built again from the tree, so a dragged divider keeps its
  place. Each pane's view moves to
  its new split view within the same window, so its surface stays attached
  (the view's window-change hooks ignore a move inside one window); the old
  split views are dropped last.
- **Grid.** A pane's grid is the whole cells its frame holds, the rest
  going to the padding band, as for a window (ADR 0032); dividers do not
  snap to cells. Dragging a divider resizes the two neighbouring panes'
  surfaces on every step.
- **Limits.** A tab holds at most 16 panes (`PaneTree.maxLeaves`), and no
  pane is smaller than 10 × 3 cells of its font, padding and a legacy
  scroll bar included. A split that would break either is refused with a
  beep, before a session is spawned: the tree with the new pane must fit
  the tab, the new pane judged by the configured font it opens with: a
  pane at that font, else the larger of a zoomed pane's cells scaled back
  to its actual size and the split pane's own smallest size (the actual
  size is not always the configured font, #100), so a split may be
  refused that would have fitted. Known gap (#315): after a `font_size`
  edit equal to the current zoom (the view keeps its old actual size,
  #100), a reset or zoom-out followed by a split in a tab that fits two
  panes only at the pane's current font can open the new pane at the
  configured font below 10 × 3 cells; the window's minimum size still
  holds.
  The dividers stop at the smallest
  size of the subtrees beside them. A split view that resizes (the window
  did, or an outer divider moved) lays its children out from the stored
  ratio, rounded to the nearest point, but never makes one smaller than
  its subtree's smallest size: the other gives way. The ratio is never
  read back from rounded frames, so repeated resizes do not make a
  divider drift, and a pane held at its smallest size gets its share back
  when there is room again. The window's `contentMinSize` is the tree's
  smallest size, recomputed whenever a pane's smallest size changes: on
  a split or close, and when its cells change (zoom, a configured font,
  a backing-scale change) or its scroll bars' style does; the split views
  are then laid out again with the clamps. So neither a divider, a window
  resize nor a switch from a smaller tab makes a pane smaller. A zoom
  that needs more than the window has squeezes its panes until the window
  grows; the window is not resized for it.
- **Focus.** The focused pane is the one whose view is (or last was) the
  window's first responder: a click, ⌘⌥ and an arrow, a split (the new
  pane) or a close (the pane before the closed one in reading order) move
  it. ⌘⌥ and an arrow pick, among the visible panes wholly on that side
  whose span across the direction overlaps the focused pane's, the
  nearest, then the one overlapping most, then the one whose centre is
  nearest; nothing at the edge. Keyboard input goes to the first
  responder only, so only the focused pane receives it. The window's title is the focused pane's
  title (each pane keeps the last title its session set) and switches
  with the focus.
- **Sessions.** `SessionRouter` routes by session id: the application
  delegate registers every pane's session with its tab's controller as it
  spawns it, before the main actor can run the session's first event, and
  unregisters it when the pane closes. A clean exit closes that pane (the
  last pane closes the tab); any other exit keeps the pane open showing
  the status, as for a tab.
- **⌘W.** Close Tab's `performClose(_:)` goes up the responder chain from
  the focused pane and reaches the `PaneContainerView` before the window:
  with several panes it closes the focused pane, with one it is the tab's
  `performClose(_:)` as before. The tab's close button and AppKit's
  tab-bar closes reach the window directly and close the whole tab.

Window state restore stays one pane per tab: a restored tab opens with a
single pane (ADR 0031).

### Close confirmation (D2)

The close confirmation of ADR 0031 counts panes: closing a tab, a window
(⇧⌘W) or the app asks once for all the busy sessions among every pane of
what closes, naming each busy pane's job, filtered as before
(`docs/security.md` S3). ⌘W on one pane of several asks only while that
pane's session runs a job, with its own wording ("Close this pane?"), and
closes the pane without asking otherwise. While a tab asks, further closes
of it are ignored; a pane that closes under its own question (its shell
exits) ends the sheet as Cancel.

## Consequences

- Panes add no FFI and no core state; the control server cannot see the
  layout yet. Moving the pane tree into `tessera-app-core` later means
  replacing `PaneTree` with the core's model behind the same container.
- A divider drag resizes the surfaces synchronously on every step, as a
  window resize outside AppKit's live resize does; a long scrollback can
  make that step slow (re-wrap under the terminal lock). Coalescing it
  per frame, as the live window resize does, is left to #38b.
- Pane commands beyond these (zoom, swap, equalise, resizing from the
  keyboard, restoring the layout) are #38b.
