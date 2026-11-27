# ADR 0045: Window blur radius (amends 0043)

**Status:** accepted · 2026-11-11 · amended 2026-11-15 (#490: turned off for
0.2.0. The app does not read `blur_radius` and has no Settings slider for
it, so every blurred window keeps the material blur of ADR 0043; the key
still parses, is clamped and is reported by `tessera_app_blur_radius`, and
has no effect. It returns with the rework in 0.3.0, #491) · amended
2026-11-26 (#491: the private call is gone; one masked material view per
window, see the last amendment)

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

## Amendment: one masked material per window, no private call (#491, 2026-11-26)

Supersedes the CGS path above (the key, its range and its clamping stay).

**What the user saw (#491).** With a radius above 0, `CGSSetWindowBackgroundBlurRadius`
blurs the window's whole square rectangle, so a sharp-cornered outline with a dark edge
showed round the rounded window, also on a theme change at opacity 0.5. Radius 0 fell back
to the panes' material, which looks like no slider position, so "Off" was not off.

**Path taken: the fallback the lead decision names.** The CGS call takes a window number
and a radius and nothing else; no public call clips it, its result cannot be measured by a
test (the hosted tests have no screen capture and the call reports success for any window),
and a second private call to reshape the window would add a risk for a look no test can
check. So the app no longer calls it, nor looks it up: `WindowBlurRadius` has no `dlopen`,
`dlsym` or `TESSERA_WINDOW_BLUR_RADIUS` any more, and the Distribution concern above is gone.

- **One view per window.** A translucent window has one `WindowBackdropView`
  (`NSVisualEffectView`, `.behindWindow`, `.fullScreenUI`, always active) under everything
  in its content view, filling it. Panes have no effect view of their own any more.
- **The shape.** The view's layer has a `CAShapeLayer` mask whose path is the window's
  outline (a rectangle with the window's corner radius: AppKit's private `_cornerRadius`,
  read by key-value coding where the window answers it, read-only, else 10 pt before macOS
  26 and 16 pt from it; 0 in native full screen) intersected with the strip under the title
  bar and tab bar and each shown translucent pane that asks for `blur`; a pane hidden behind
  a maximized one is left out. AppKit's own `maskImage` is a layer mask over the material's
  `CABackdropLayer` (with `capInsets` as its `contentsCenter`), so a shape-layer mask is the
  same mechanism. A `maskImage` the window's size was a 32-bit bitmap, about 20 MB per window
  and tab at 2x, drawn again on every resize step (#491 B3); the path has no bitmap in the
  app (0 bytes of mask backing, bar 256 KB), and an update per resize or divider step takes
  a median 21 µs under TSan (bar 1 ms; 8 µs for the path alone, optimised), measured at a
  load of about 76. The path changes, without animation, only when the regions, the corner
  radius or the size change. Tests render the mask layer and read its pixels at the corners
  and along the arc.
- **Off is off.** Radius 0, an opaque window and Reduce Transparency (the app sets radius
  0) leave no effect view in the window: no blur under the panes or under the title bar and
  tab bar, whose tint stays.
- **The radius, approximated without a private call.** AppKit has no behind-window blur of a
  chosen radius, so `blur_radius` is the strength of the material: the view's `alphaValue`
  is `radius / 64`, the material over the unblurred desktop. 64 is today's material blur,
  the strongest there is.
- **Default 48** (was 0), three quarters of the material: close to the old look with room
  above it; the eye check on #589 confirms it or moves it toward 64. Range 0..=64 kept.
- **Option ii is dropped.** A tab of mixed panes no longer has to agree: each pane that
  asks for blur is in the mask, one that does not is not, and the strip always is while the
  radius is above 0.
- **Migration.** The meaning of a stored 0 changes: it was the material behind each pane
  that asked for `blur`, and is now no blur. The 0.2.0 and 0.2.1 templates wrote
  `blur_radius = 0` under the comment "No effect in 0.2.0 ... It returns in 0.3.0.", so
  the loader reads that line, untouched (in `[appearance]`, right under those two comment
  lines, exactly `blur_radius = 0`), as the default 48: in memory only, the file is never
  rewritten. A 0 the user wrote (no such comment, an edited comment, another spelling) is
  no blur. Setting the key in Settings takes the stale comment off it (the editor lists the
  two lines as removed), so a 0 chosen there stays Off.
- **The off-screen rules of the #405 amendment** were about the window number the call
  needed; a view needs none, so the occlusion observer is gone and an update applies at
  once whatever the window's state.

