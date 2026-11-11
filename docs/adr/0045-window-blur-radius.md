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
