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
`PaneContainerView` (inside the window's content view, a
`TerminalWindowContentView` that keeps it below a transparent title bar,
#363), which owns a
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
  snap to cells. Dragging a divider resizes the neighbouring panes'
  surfaces at most once per frame (below).
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

Window state restore keeps each tab's pane tree since #38b (ADR 0031).

### Maximize, dim, equalise and restore (#38b, amended 2026-11-07 and for #323, 2026-11-10)

- **Maximize.** `zoom_pane` (⌘⇧↩, Shell ▸ Maximize Pane, checked while
  on) shows the focused pane alone over the whole tab and toggles back.
  The tree's split views stay in the container as they are, hidden, not
  built again and no longer resized with it (an empty autoresizing mask,
  and no relayout for a changed smallest size), with a stand-in at the
  maximized pane's frame in its slot, so the hidden panes keep their
  sessions, take output and keep their frames and grids, a clamped
  divider included; showing every pane again puts the pane back in its
  slot, every pane at the frame it had unless the tab was resized
  meanwhile (then laid out as for a resize). A split, a close, a focus move (⌘⌥ and an arrow,
  ⌘[ and ⌘], or focusing another pane) and a new tab added to the window
  show every pane first. A tab with one pane cannot be maximized
  (a beep). The state is not saved. The pane's view moves out of the
  split views before they are hidden, as hiding a view takes the
  keyboard from a first responder inside it: the pane keeps the keyboard
  and a marked-text composition, a DECSET 1004 child gets no focus-out
  and focus-in reports, and a find field with the keyboard keeps it.
  Only while the window itself holds the keyboard does the focused pane
  get it (#323).
- **Dim.** `[appearance] dim_inactive_panes` (0.0 to 0.8, default 0.0
  for off; the core clamps a value out of range or not a number, with a
  load warning, and ignores it in a profile) is drawn as a black
  `PaneDimView` of that opacity, the topmost subview of each pane
  without the focus, only while the tab shows more than one pane. It
  takes no clicks and is not in the accessibility tree. The app reads
  it from the core (`tessera_app_dim_inactive_panes`) at start and after
  every applied configuration, so a live reload applies it.
- **Equalise.** Double-clicking a divider gives that split's two sides
  equal shares, `equalize_panes` (Shell ▸ Equalize Panes, no default
  key) every split's. A share is weighted by the panes side by side
  along the split's direction, so a row of three panes made by two
  splits gets thirds and L | (R1 / R2) gets halves. Ratios stay the
  stored fractions (D19), so equalising does not make a divider creep.
- **Divider drags.** `NSSplitView`'s own drag tracking runs as a live
  resize of every view under the split view (`viewWillStartLiveResize`
  until the button is up), so each surface already passed at most one
  size to the core per display refresh, and its last size at the end, as
  in a live window resize (the first version of this ADR said a drag
  resized on every step; it did not). A test drags through AppKit's
  tracking and pins it (#315). The drag saves the window state once,
  when the button is up and a divider moved, not per step (#323).
- **⌘W's title** reads Close Pane while the focused tab has more than one
  pane (validated from the `PaneContainerView`), Close Tab otherwise, and
  goes back to Close Tab when the window stops being key or closes. While
  the keyboard is outside the panes (the window itself is first
  responder), the `TerminalWindow` validates it as Close Tab, which is
  what ⌘W then does (#323).
- **Commands.** The pane items (split_right, split_down,
  focus_pane_left/right/up/down, focus_pane_previous/next, zoom_pane,
  equalize_panes) are registry commands with their item's identifier, so
  `[keys]` rebinds them (ADR 0036).

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
- A divider drag resizes each surface at most once per frame, through
  AppKit's live resize (see Divider drags above).
- Swapping panes and resizing them from the keyboard are not done yet; a
  maximized pane and the focus are not restored.
