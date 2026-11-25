# ADR 0043: Translucency, background blur and display options (amends 0037)

**Status:** accepted · 2026-11-07

## Context

`[appearance] opacity` and `blur`, and the same keys in a profile's
`[profiles.appearance]`, have been parsed since 0.1.0 and resolved per
session since ADR 0037, but only opacity reached the screen: the core drew
a session's background at its opacity, and the host made every window
translucent while any live view was (`on_appearance_changed(opaque)`, ADR
0037's "one opacity switch"). Blur did nothing, a translucent session made
every window of the app translucent, and the system's Reduce Transparency
setting was ignored. Issue #42 finishes it.

## Decision

**Blur per pane (E7).** A pane whose session is translucent and asks for
blur gets an `NSVisualEffectView` (`PaneBackdropView`: `.behindWindow`,
material `.fullScreenUI`, state `.active` so it does not change when
another window takes the focus) under its render view, inside the pane's
`TerminalSurfaceView` (`TerminalSurfaceView+Backdrop.swift`). It fills the
pane, sits below everything the pane draws (text, scroll bar, find bar, the
inactive-pane shade) and takes no clicks, cursor or accessibility. A pane
that stops asking for blur loses it; closing the pane releases it with the
view. The material was `.underWindowBackground` until #363: in a manual
check its grey system tint washed the theme's colours out below 0.9
opacity. Compared side by side with the other materials, `.fullScreenUI` was
picked; `.popover` came second and is the alternative, a one-word swap, if
the colours still look grey somewhere.

**Opacity per tab window (E7).** A window is opaque only while every pane of
its tab is, panes hidden behind a maximized pane included, so zooming does
not flip the window. The window recomputes on every appearance change and
on every rebuild of its panes (split, close, restore). A pane without a
surface (not attached yet, or failed to start) draws nothing and counts as
opaque. Native tabs are windows, so one translucent tab no longer makes
the app's other windows translucent. A translucent window has a
transparent title bar over full-size content (#363), so the title bar and
tab bar draw no material band of their own; the panes stay below them
(`contentLayoutRect`), a blur (the same `PaneBackdropView`, so the same
material) always fills the strip above them, whatever the panes' own blur
setting, so the title and tab titles stay legible over a busy desktop, and
the switch keeps the window's frame. The theme's
background tint under the strip is added by the amendment below.

**Blur only when translucent.** A session's look carries `blur =
appearance.blur` only while its background is translucent after display
options: an opacity below 1.0, or a theme whose background has alpha.

**The appearance is the session's.** `tessera_session_appearance(app, id,
out)` reports `{ opaque, blur }` for one session, resolved by the core from
the session's profile and the display options; the app reads it when a
surface attaches and on every `on_appearance_changed`, and never parses the
configuration. `on_appearance_changed` keeps its signature (ABI) and now
also fires when only blur changes; its `opaque` argument (all views opaque)
is no longer what the app uses.

**Display options (E8).** `tessera_app_set_display_options(app, options)`
takes `DisplayOptions { reduce_transparency }`, a `#[repr(C)]` struct of
`uint8_t` flags (0 off, any other value on, as ADR 0011 requires of
booleans a frontend writes), so a zeroed field is off and no byte pattern
is an invalid Rust value. It has no size field: later fields (#52's
`increase_contrast`) are only appended, with the header and the core
shipped together. While `reduce_transparency` is set the core's
`theme_for_appearance` draws every session at opacity 1.0 and without
blur, so no session can opt out. The options are part of the applied
state, so a configuration reload keeps them, and setting them bumps the
look generation, so a restyle computed before the change cannot put a
translucent look back. A repeat is silent; a change calls
`on_appearance_changed` on the calling thread before the call returns.
The app sets them at core start from
`NSWorkspace.accessibilityDisplayShouldReduceTransparency` and again on
every `accessibilityDisplayOptionsDidChangeNotification` from
`NSWorkspace.shared.notificationCenter`.

**No opacity slider here.** Changing the keys from the app is the
Preferences window's (#40b).

## Consequences

- A translucent pane next to an opaque one in a split shows the desktop
  through the translucent one only; the opaque pane still draws its own
  background.
- `on_appearance_changed`'s `opaque` argument is redundant for this host;
  it stays for ABI stability and for hosts with one window.
- Increase Contrast (#52) adds a field to `DisplayOptions` and one more
  check in `theme_for_appearance`; no new export.

## Amendment: Increase Contrast, from ADR 0041 (#52, 2026-11-08)

`DisplayOptions` is now `{ reduce_transparency, increase_contrast }`, both
`uint8_t` flags read as above, two bytes with no size field (decision F7):
the header and the core ship together, and both sides assert the size (the
Rust `display_options` FFI test and TesseraKit's `MemoryLayout` test). While
`increase_contrast` is set, `theme_for_appearance` draws the cursor and the
selection fully opaque and `App::dim_inactive_panes` is 0. The app reads
Increase Contrast with Reduce Transparency on this ADR's observer and calls
`tessera_app_set_display_options` only when one of them changed, then reads
the pane dimming again.

## Amendment: strip tint, from ADR 0045 (#388, 2026-11-11)

**Strip tint (E7).** The strip is no longer left without the theme's colour.
The core reports the session's resolved theme background in
`SessionAppearance` as four `f32` (red, green, blue, alpha, each 0..=1,
straight not premultiplied alpha), after the profile's theme, its opacity and
the display options (Reduce Transparency: alpha 1) are applied. The host
draws that colour at its alpha over the strip's `PaneBackdropView`, below the
title and tab titles, as a pane draws its background over its own blur; it
never reads the theme itself. Each pane that touches the top edge of the content tints its own
segment of the strip with its own colour, so a split with two themes shows
both; if the eye check shows that as noisy, the top-left pane's colour is used
for the tab-bar area only, each top-edge pane still tinting its own segment.
Only the configured theme counts (profile theme, opacity, display options): an
OSC 11 background set by the program is not followed, a documented gap. The strip keeps its material and has no blur key
of its own (ADR 0045).

**Hidden panes (E7).** The window's opacity counts every pane of the tab,
including one hidden behind a maximized pane, as stated above. So a hidden
translucent pane keeps the window translucent, and a hidden pane that asks
for blur is not drawn but still counts toward the window's blur: this is
intended, it keeps zooming from changing the window, and the wording above
and the code comments say "every pane of the tab", not "every visible pane".

## Amendment: the app is always dark (#364, 2026-11-13)

**Decision.** The app sets `NSApp.appearance` to `.darkAqua` in
`applicationWillFinishLaunching`, before any window exists, so every window,
sheet and panel it shows is dark whatever the system's Light or Dark setting:
terminal windows, their title and tab bars, the find bar, Settings, alerts,
sheets and the About panel. No window or view sets an appearance of its own;
they inherit the app's. Context menus are dark too; the menu bar and its
menus are drawn by the system and follow the system's setting. This is the maintainer's decision: terminal themes are
dark, and light window chrome around a dark theme looked out of place.

**The backdrop no longer follows the system mode.** The blur backdrop's
material (`.fullScreenUI`, `.underWindowBackground` before #363) draws its
system tint from the effective appearance. With the app always dark, the tint
is the dark one in Light mode too, and switching the system between Light and
Dark while the app runs changes nothing in it.

**The choice comes later.** There is no setting: a dark, light and system
choice is #365, which replaces the one call that sets the appearance
(`AppDelegate.applyAppAppearance(_:)`).

## Amendment: app appearance setting (#365, 2026-11-13)

**Decision.** `[appearance] app_appearance` chooses the app's own appearance:
`"dark"` (the default, so the #364 behaviour stays what a configuration without
the key gets), `"light"` or `"system"` (follow System Settings > Appearance,
also when it changes while the app runs). It is read exactly as written; any
other value, another case or another type gives `"dark"` with one warning that
names the key and the escaped value (docs/security.md S8), as for the other
string settings.

**App-wide only.** The app has one appearance, so the key is not a profile
key: in a profile's `appearance` it is ignored with the same window-level
warning as `dim_inactive_panes` and `blur_radius`, and every resolved profile
carries `[appearance]`'s value.

**Only the chrome.** The setting decides the windows, title and tab bars,
find bar, Settings, sheets, alerts and panels, and the backdrop material's
tint, as the #364 amendment lists them. The terminal's colours stay the
theme's: a light app appearance around a dark theme draws the theme unchanged.

**How the host reads it.** The core reports it with
`tessera_app_app_appearance` (`TESSERA_APP_APPEARANCE_DARK` = 0, `_LIGHT` = 1,
`_SYSTEM` = 2). No new callback: the host reads it at start and again on every
`on_config_applied`, which the watch calls for each configuration applied
whether or not anything changed. Settings writes it through
`tessera_config_set("appearance.app_appearance", "\"light\"")` like any other
key. The host side (`AppDelegate.applyAppAppearance(_:)`) is #365's app half.

## Amendment: native full screen (#494, 2026-11-18)

**Decision (D7).** In native full screen the title bar and tab bar of a
translucent window draw opaque: `titlebarAppearsTransparent` is false and the
content has no strip under them (the strip's blur and tint are zero high),
whatever the window's opacity and blur. The terminal content keeps its
opacity setting: the window stays transparent and each pane draws its
background at its opacity, with its own blur. Leaving full screen brings the
windowed look back (transparent title bar over the blurred, tinted strip). An
opaque window looks the same in and out of full screen. In both, while the tab
bar shows, the panes lose 8 pt of height in full screen to the tab-row spacer:
a title bar accessory over the tab bar that gives the tabs as much space above
them as below (#621).

**Why.** In full screen AppKit moves the title bar and tab bar into its
separate full-screen toolbar window, which slides in over the content. There
is no desktop behind them there, so a transparent bar showed the content or
the space's backdrop through it, over a strip sized for the windowed title
bar (#494, found in the 0.2.0 manual pass).

**How.** One pure function, `TerminalWindowContentView.titlebarLayout(fullScreen:translucent:measuredStrip:tabRow:)`,
returns `(titlebarTransparent, stripHeight, tabRowSpacerHeight)`: a
transparent bar over the measured strip only for a translucent window out of
full screen, and in full screen a spacer as high as the tab row's space below
it minus its space above (8 pt on macOS 27, zero without a tab bar). The content
view applies it on every layout pass, reading
`window.styleMask.contains(.fullScreen)` (the transition resizes the window
and moves its `contentLayoutRect`, and a tab selected in full screen lays out
again), and `TerminalWindowController`'s `windowDidEnterFullScreen` and
`windowDidExitFullScreen` bring every tab of the window up to date at the end
of the transition. `.fullSizeContentView` follows opacity as before, so the
panes stay in `contentLayoutRect` and the window's frame is AppKit's.

**Tests.** The decision is unit tested for all four cases; the layout is
tested on windows that report `.fullScreen` (AppKit raises if anyone else sets
it), through the delegate and through a layout pass alone; and a hosted test
enters and leaves full screen with `toggleFullScreen(_:)` on a window and tab
opened as ⌘N and ⌘T open them, posting nothing, one toggle in and one out;
in full screen it selects the tab and reads its layout there, since a tab not
shown keeps its windowed frame and style mask until it is selected (macOS 15).
That test makes its toggles in a child copy of the test host, because a
transition that does not complete (a locked or sleeping console, a session off
the console) ends in an AppKit assertion (`NSWindowStackController`) a few
seconds later that kills the process, even after one toggle. The parent skips,
printing why, when the child reports that a toggle did not finish within 5 s,
or when the child crashes or stalls; it skips before launching one on a locked
console (`CGSSessionScreenIsLocked`, `IOConsoleLocked`) or a session off the
console (`kCGSessionOnConsoleKey`). Whether the bar looks like the windowed one
is an eye check (E2).
