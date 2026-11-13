# ADR 0045: Window blur radius (amends 0043)

**Status:** accepted · 2026-11-11

## Context

ADR 0043 blurs a translucent pane with an `NSVisualEffectView` material
(`PaneBackdropView`, `.fullScreenUI`). A material has a fixed strength and a
grey system tint, and nothing in `config.toml` can change either. Issue #390
asks for a configurable blur radius, as iTerm2 and Ghostty have. The only way
to get a plain gaussian blur of an adjustable radius behind a window is
`CGSSetWindowBackgroundBlurRadius`, a private, undocumented WindowServer call
that blurs the whole window.

The reference emulators (sources read for #390): iTerm2 looks the symbol up
with `CFBundleGetFunctionPointerForName` on ApplicationServices, once, and
logs and skips the blur if it is missing; its radius is a float profile key
(default 2, slider maximum 64), the window is blurred while any tiled session
asks for it, at the mean of their radii. Ghostty declares the symbols
directly, skips the call unless the window is translucent and visible, takes
`background-blur` as an integer 0..255 (`true` is 20, default off) and sets
no blur in native full screen. Both ship it.

Empirical findings, on macOS 27.2 only (macOS 15 and 26 were not available;
for them the reference emulators' use is the only evidence):

- The symbols resolve through `dlsym`. The call takes a connection id, a
  window number and an integer radius, and returns 0 for any window number,
  including bogus ones, so the return code does not signal a failure.
- Radius 1 looks like 0; the blur width grows linearly with the radius and
  stops growing at about 64; any larger value, and a negative one, gives the
  64 result, so the caller has to clamp. The blur is untinted.
- A window that is not on screen has window number -1; a call then has no
  effect. Once the window is visible the blur persists across order out and
  in, resize, `isOpaque` and style mask changes.
- An `NSVisualEffectView` over a CGS-blurred window shows its own material
  blur and tint, independent of the radius; the two do not stack.
- iTerm2's note that going from a radius above 0 back to 0 breaks the blur
  for good did not reproduce.

## Decision

**The key.** `[appearance] blur_radius` is an integer, 0..=64, default 0. 0
keeps today's material blur (ADR 0043); 1..=64 asks for the CGS blur at that
radius. A value above 64 is clamped to 64 with one warning; a negative or
non-integer value (-1, 20.0) is a parse error, as for
`terminal.scrollback_lines`. It is
a window-level key: inside `[profiles.appearance]` it is ignored with a
warning, as `dim_inactive_panes` is, because the call blurs the window and
not a pane. The core reports it with `App::blur_radius()` and
`tessera_app_blur_radius`, as an unsigned integer (`uint32_t`), not an `f32`,
because the call takes an integer and the host should not round.

**Window or pane (option ii).** A tab's window uses the CGS blur only when
every translucent pane of the tab asks for blur (`blur = true`) and
`blur_radius` is above 0. Otherwise each pane keeps its own material blur as
in ADR 0043. A window therefore never blurs a pane that asked for none, and
the material stays the fallback in every case. While the CGS blur is in use
the panes' `PaneBackdropView`s are hidden, since a material over a CGS-blurred
window would replace the radius in its rectangle. iTerm2 takes the other
choice (any blurred pane blurs the window, at the mean radius); it was
rejected for blurring panes that asked for none.

**The strip (D4).** The strip over the panes (title bar and tab bar, ADR 0043
E7) keeps its `PaneBackdropView` material whatever `blur_radius` is, and gets
no second key. Over a CGS-blurred window the material shows its own blur and
tint, so the strip keeps the look it has today. Where it meets panes
blurred by CGS there is a visible seam (the material is stronger and
greyer); whether that is acceptable is a look item for the author's eye
check, not a fault to engineer around here.

**Access to the private API.** The host looks the functions up with `dlopen`
and `dlsym` once, lazily: `CGSSetWindowBackgroundBlurRadius` and
`CGSMainConnectionID` from ApplicationServices, never through
`@_silgen_name` or a link-time declaration, so a macOS that drops the symbol
cannot make the app fail to launch. If either lookup returns NULL the host
logs once, without user content, and every window uses the material path. The
call's return code cannot be trusted (see above); failure detection is the
missing symbol, plus a guard that applies the radius only once the window is
visible (`windowNumber` above 0), never with -1. The call is made again only
when the radius, the window's blur condition or the window number changes.
The host clamps the radius to 0..=64 again before the call and never passes a
negative one. It sets the radius to 0 when the window is opaque and while
Reduce Transparency is on (ADR 0043 E8 already makes every session opaque and
unblurred then).

**Not special-cased: native full screen.** Ghostty sets the radius to 0 in
native full screen. Tessera does not; it is a check for the author's eyes,
and the decision is revisited if the look is wrong there.

**Distribution.** Tessera is distributed as an unsandboxed Developer ID app
(`docs/releasing.md`); notarisation checks signing, the hardened runtime and
malware, not private API use, and a `dlsym` of a system framework symbol
needs no entitlement. The path is not eligible for the Mac App Store while it
exists. The remaining risk is a macOS update that changes or removes the call,
which the symbol lookup and the material fallback absorb.

## Consequences

- The default changes nothing: a configuration without `blur_radius` looks as
  before.
- Behaviour on macOS 15 and 26 is unverified here. The feature degrades to
  the material if the symbol is missing; a changed behaviour of the call
  itself would not be detected.
- Reduce Transparency and native full screen are untested for this call and
  go on the manual check list.
- Enabling the blur from 0 is not instantaneous (a flat grey was seen for up
  to about a second); manual checks must not judge pixels inside that time.
- NSGlassEffectView (macOS 26 glass, as Ghostty's `macos-glass-*`) is out of
  scope; a later ADR can add it.

## Amendment: tab switches, off-screen windows, full screen (#405, #409, 2026-11-13)

**The title bar under the content (#409).** Found headlessly, not yet confirmed
by eye: a translucent window had AppKit's `NSTitlebarContainerView` (the title,
the tab bar, the buttons) below the window's content view, where the strip's
blur and tint cover it. A pane attaches as it moves into the window, which
made the window transparent while AppKit was still inside
`window.contentView =`; a window whose background changes then keeps its title
bar's views under the new content view for good (reproduced in a plain AppKit
program as well). The content view is now installed before the panes go into
it (`TerminalWindowContentView.install(in:)`), and the tests check that the
window's close button is in a view above the content after every tab route.
Why the user saw it only on a tab switch is not known; a stacking that the
compositor ignored until the tab bar moved between windows fits.

**One path to the screen.** A tab window makes its opacity, title bar, strip
and radius what its panes ask for, as on a pane's attach, whenever it may come
on screen: ordered in (`order(_:relativeTo:)`, which every tab route and an app
unhide reach on macOS 27), its tab group's `selectedWindow` changing (KVO, for
a route that does not order in), a window of its tab group deminiaturized, the
app unhidden, and `windowDidBecomeMain` as the backstop (no test reaches it:
the test host never becomes main). What is already right is left alone, so
each is cheap.

**Off screen.** While the window has no number (a tab not yet shown, minimized,
the app hidden) an update keeps the blur state it has, since the radius set on
the window stays: a theme reload or a new pane meanwhile does not show the
panes' material, which would flash as the window comes back. The radius wanted
meanwhile is set when the window is back on screen. On macOS 27 an unselected
tab that has been shown keeps `isVisible` and its window number, so this guard
covers minimized windows and a hidden app, not unselected tabs: those are
brought up to date when they become visible again, as above.

**Recorded, not changed.**

- Native full screen is not special-cased: the radius applies to a full-screen
  window as to any other, over the desktop space behind it. Eye check (#363).
- A failed radius-0 call leaves whatever blur WindowServer had on the window,
  under the panes' material (shown again), and is not retried until the radius
  or the window number changes.
- `WindowBlurRadius.systemBackend()` stays internal so a test can call it.
- The strip over an inactive pane is not dimmed by `dim_inactive_panes`
  while the pane is: the strip is window chrome. Accepted for 0.2.0.
- The smallest content size of a translucent window adds the height of the
  title bar and tab bar as last measured over panes with height (#403 m2), so
  a programmatic frame at the smallest size leaves each pane its smallest
  size in a tabbed window too.
