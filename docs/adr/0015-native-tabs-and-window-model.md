# ADR 0015: Native tabs and the window model in the Swift shell

**Status:** accepted · 2026-10-28

## Context

Until now the app opened one window with one session at launch and quit
when it closed. Issue #28 adds windows and tabs: ⌘N for a window, ⌘T for a
tab, ⌘W to close one, ⇧⌘[ and ⇧⌘] to cycle, ⌘1 to ⌘9 to jump, and dragging
tabs to reorder them or out into a new window, behaving like Safari's or
iTerm2's native tabs.

`docs/architecture.md` puts a "windows → tabs → pane tree → sessions" model
in `tessera-app-core`, so that the control server (and a future non-macOS
front end) can see and drive the layout. None of that model exists yet: the
core knows sessions only. AppKit, meanwhile, already implements tabs, as
windows grouped into a tab bar, with the ordering, dragging, merging and
"Show Tab Bar" behaviour users expect, none of which we want to rebuild.

## Decision

### Tabs are windows

Every tab is an `NSWindow` with its own `NSWindowController`
(`TerminalWindowController`) and exactly one `TesseraSession`. Every window
has `tabbingMode = .preferred` and the shared `tabbingIdentifier`
`com.aanyberg.tessera.terminal`; AppKit groups them into tab bars and owns
the order of the tabs in each bar (`NSWindow.tabGroup.windows`).

- ⌘T (`AppDelegate.newTab(in:)`) spawns a session and adds its window to
  the key window's tab bar with `addTabbedWindow(_:ordered: .above)`, right
  after the selected tab. The tab bar's "+" button does the same through
  `newWindowForTab(_:)`.
- ⌘N (`AppDelegate.newWindow()`) opens a standalone window, cascaded from
  the previous one. `.preferred` would put a window that is ordered front
  into the key window's tab bar, so the window is shown with tabbing
  disallowed and set back to `.preferred` afterwards. The same applies to
  any later ordering in: `orderOut(_:)` followed by `makeKeyAndOrderFront(_:)`
  on a standalone terminal window puts it into the key window's tab bar. Code
  that hides and shows windows (#27, #29) must avoid that pair, or disallow
  tabbing around it as `newWindow()` does.
- ⌘W is `performClose(_:)`: it closes the key window, which is the selected
  tab; AppKit removes the tab bar's window with its last tab. ⇧⌘W (Close
  Window) sends `performClose(_:)` to every tab of the key window's tab bar.
- The Window menu uses AppKit's tab actions: Show Previous Tab (⇧⌘[), Show
  Next Tab (⇧⌘]), Move Tab to New Window and Merge All Windows. Show Tab
  Bar moved to the View menu in #29, beside AppKit's Show All Tabs, as in
  Terminal.app and Safari.
  Dragging tabs to reorder them or out into a new window is AppKit's.
- ⌘1 to ⌘9 select `tabGroup.windows[0...8]`, and are disabled past the
  last tab.

AppKit's own `selectNextTab(_:)` and `selectPreviousTab(_:)` did nothing in
the test host, where the application is never active and no window is key.
`TerminalWindow`, the tab's `NSWindow` subclass, overrides both to select by
index in its tab bar, wrapping at either end as AppKit does; ⌘1 to ⌘9 use
the same method. The menu items still send AppKit's selectors, so anything
else that sends them reaches the same code.

The menu bar is built in `MainMenu`; #29 added its Edit, View and Help
menus there.

### The model stays in Swift for 0.1.0

The registry of open tabs is `AppDelegate.windowControllers`, and AppKit's
tab groups are the only record of which tabs share a window and in what
order. `SessionRouter`, the core's `TesseraAppDelegate`, maps each session to
its controller and forwards output, title, bell and exit. Nothing here could
live in Rust today: it is AppKit window management over sessions the core
already owns.

`SessionRouter` is the seam for moving the model into `tessera-app-core`
later. What moves then: window and tab identity and ordering (so the
control server can list and reorder them), the tab title (session title,
falling back to a configured default), and the create/close commands. The
router then turns the core's model events into AppKit calls, and AppKit's
tab-bar drags into commands to the core, instead of the controllers being
the source of truth.

### Lifecycle

- Closing a tab (⌘W, the close button, closing the window) closes its
  session in `windowWillClose`; the controller leaves the registry and the
  router.
- A session that exits with code 0 closes its tab. A non-zero exit or a
  signal keeps the tab open with the status in its content, like
  Terminal.app's default, so the user can see what happened; the session is
  closed with the tab. Keeping the exited session until then also keeps its
  screen for #27's surface.
- The app keeps running with no windows open
  (`applicationShouldTerminateAfterLastWindowClosed` returns `false`), as
  macOS apps do; ⌘N, or clicking the Dock icon with no window open
  (`applicationShouldHandleReopen`), opens one. This replaces the earlier
  quit-on-last-window.
- `window.title` is the session's title when the core reports one, else
  "Terminal"; `window.subtitle` is unused. The core does not apply OSC titles
  yet (#9), so every tab shows "Terminal" for now.
- The signal handling (`SIGTERM`, `SIGINT`, `SIGHUP` to `NSApp.terminate`,
  ADR 0012) and the 100 ms bell throttle are unchanged; the throttle moved
  into `SessionRouter`.

### Content

The controller holds its content as `any TerminalContentView`, a
main-actor protocol for `NSView` subclasses with two requirements,
`sessionDidOutput()` and `sessionDidExit(code:signal:)`, installs it as the
window's content view and reports the session's events through them. Until
the terminal surface lands (#27) the content is `TerminalPlaceholderView`:
the core version, the output-event count and the exit status, the label the
app showed before. #27 adds `TerminalSurfaceView` as another conforming view
and passes it to the controller's initializer; the controller does not
change.

### Tests

A new app-hosted XCTest target, `TesseraTests`, runs inside `Tessera.app`
(which opens no window at launch when it hosts tests) and drives
`AppDelegate` directly with sessions running `cat` or `sh -c`: windows and
tab groups, the tab bar's "+" button, ⌘1-style selection, wrapping next and
previous, closing tabs, windows and sessions, the last window, reopen, clean and failed exits, titles and the
menu's key equivalents. `just app-test` runs it after `TesseraKitTests`,
both under the Thread Sanitizer. The test bundle links nothing itself: the
app already contains `TesseraKit`, and a second copy would duplicate its
types.

## Consequences

- Tab behaviour (drag to reorder, drag out, merge, the tab bar's overflow
  and "+" button, full-screen tabs) is AppKit's and stays native without our
  code.
- Split panes, when they come, live inside one tab's content view; they do
  not change this model.
- The control server cannot see or reorder windows and tabs until the model
  moves into `tessera-app-core`; until then `SessionRouter` is the only place
  that knows which session runs in which tab.
- Closing a tab blocks the main thread while its session closes (up to about
  two seconds for a child that ignores `SIGHUP`, ADR 0012). The cost is per
  tab: Close Window (⇧⌘W) on N tabs closes their sessions one after another
  and blocks for about N times that (20 tabs whose children ignore `SIGHUP`
  measured 20.5 s), and a `SIGTERM`, `SIGINT` or `SIGHUP` arriving meanwhile
  is handled only afterwards. `TesseraApp.shutdown()` does not have this
  problem: it caps the whole set at about two seconds. A non-blocking session
  close in the core is the follow-up that removes it.
- A window or tab that fails to open reports the error with
  `NSAlert.runModal()`, which blocks the main queue until it is dismissed, so
  the signal sources' handlers cannot run meanwhile. Today it runs only from
  menu actions and the Dock, where that is the usual modal behaviour. A
  future caller that opens tabs from a main-queue job (a control-server
  `Task`, say) must present the error asynchronously or as a sheet instead.
