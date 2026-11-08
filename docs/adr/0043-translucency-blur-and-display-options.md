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
material `.underWindowBackground`, state `.active` so it does not change
when another window takes the focus) under its render view, inside the
pane's `TerminalSurfaceView` (`TerminalSurfaceView+Backdrop.swift`). It
fills the pane, sits below everything the pane draws (text, scroll bar,
find bar, the inactive-pane shade) and takes no clicks, cursor or
accessibility. A pane that stops asking for blur loses it; closing the pane
releases it with the view.

**Opacity per tab window (E7).** A window is opaque only while every pane of
its tab is, panes hidden behind a maximized pane included, so zooming does
not flip the window. The window recomputes on every appearance change and
on every rebuild of its panes (split, close, restore). A pane without a
surface (not attached yet, or failed to start) draws nothing and counts as
opaque. Native tabs are windows, so one translucent tab no longer makes
the app's other windows translucent.

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

## Amendment (#52, ADR 0041, 2026-11-08)

`DisplayOptions` is now `{ reduce_transparency, increase_contrast }`, both
`uint8_t` flags read as above, two bytes with no size field (decision F7):
the header and the core ship together, and both sides assert the size (the
Rust `display_options` FFI test and TesseraKit's `MemoryLayout` test). While
`increase_contrast` is set, `theme_for_appearance` draws the cursor and the
selection fully opaque and `App::dim_inactive_panes` is 0. The app reads
Increase Contrast with Reduce Transparency on this ADR's observer and calls
`tessera_app_set_display_options` only when one of them changed, then reads
the pane dimming again.
