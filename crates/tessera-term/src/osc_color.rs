//! OSC colours: the palette (`OSC 4`), the dynamic colours (`OSC 10`, `11`
//! and `12`: text foreground, background and cursor) and their resets
//! (`OSC 104`, `110`, `111`, `112`), ADR 0024.
//!
//! | Command | Effect |
//! |---|---|
//! | `OSC 4 ; c ; spec ; c ; spec ...` | sets palette entry `c` (0..=255) to `spec`; a `?` spec queries it |
//! | `OSC 10 ; spec ; spec ...` | sets (or with `?` queries) the foreground, then the background, then the cursor |
//! | `OSC 11 ; ...`, `OSC 12 ; ...` | the same chain from the background, from the cursor |
//! | `OSC 104` / `OSC 104 ; c ; c ...` | resets the whole palette / the entries listed |
//! | `OSC 110`, `OSC 111`, `OSC 112` | resets the foreground, the background, the cursor |
//!
//! The colours a session starts with are the theme the host configures
//! ([`Terminal::set_theme_colors`]); a set records a program override on
//! top of it ([`Terminal::color_overrides`]), a reset drops one, and
//! [`Terminal::colors`] is the two together. The theme is configuration:
//! no reset touches it, and a new theme keeps the overrides (D-THEME-KEEP).
//! RIS drops every override; DECSTR and DECSCL keep them (D-RESET, a
//! deviation from xterm, which resets the palette on both and the dynamic
//! colours on neither). Resizes, the alternate screen and DECSCNM leave
//! them alone; a query reports the colour itself, never the opposite one
//! under DECSCNM (D-REVERSE). [`Terminal::colors_generation`] increases
//! whenever an override or an effective colour changes, and only then
//! (D-GEN): a set equal to the theme's colour moves it, a repeated set of
//! the same colour does not.
//!
//! # Queries
//!
//! A `?` in place of a spec answers
//! `OSC 4 ; c ; rgb:RRRR/GGGG/BBBB` or `OSC Ps ; rgb:RRRR/GGGG/BBBB`
//! (`Ps` = 10, 11 or 12): lowercase hex, each 8-bit channel times 257,
//! ending with BEL when the query did and with ST otherwise (ST, an
//! 8-bit ST, or the CAN, SUB or ESC that aborted it). One reply per `?`,
//! in order, interleaved with the sets around it. Only exactly `?`
//! queries; `?x` or `? ` is a spec that does not parse. A reply holds the
//! command number, the index and the stored colour: never the program's
//! bytes.
//!
//! # Specs
//!
//! [`parse_color_spec`] accepts, case-insensitively:
//!
//! - `rgb:R/G/B` with 1 to 4 hex digits per channel, the same number in
//!   each, scaled as X11 does (`f` is `ff`, `800` is `80`): the high byte
//!   of `v * 65535 / (16^n - 1)`;
//! - `#` and 3, 6, 9 or 12 hex digits in three equal groups,
//!   left-justified as X11 and xterm read them (`#f80` is `#f08000`, not
//!   CSS's `#ff8800`; 9 and 12 digits keep each group's top 8 bits);
//! - an X11 colour name from the X.org `rgb.txt` (782 names, #187),
//!   ASCII case-insensitively over the whole name, spaces significant:
//!   `DarkSlateGray`, `darkslategray` and `dark slate gray` are the same
//!   colour, `dark slategray` and `red ` are not names. X11 values win
//!   where CSS differs (`gray` is `#bebebe`, `green` `#00ff00`); a name
//!   longer than the longest entry (22 bytes) is rejected before the
//!   search.
//!
//! Anything else is ignored whole: `rgbi:`, the CIE forms, CSS `rgb(...)`
//! and keywords (`transparent`), xterm's `XtDefaultForeground`, blanks
//! around the spec, a sign, other digit counts and non-hex bytes. An
//! unparsable spec is traced with its length only; a reply holds the
//! stored colour, never the name.
//!
//! # Malformed input
//!
//! - `OSC 4` takes index and spec pairs; it stops at the first index that
//!   is not 1 to 3 decimal digits below 256, a lone index without a spec,
//!   or a spec that does not parse. The pairs before it stay applied
//!   (D-STOP4). The parser keeps 16 pieces, so at most 7 pairs are seen.
//! - The `OSC 10` chain goes through the colours in order from its start
//!   and ends after the cursor (12): xterm's pointer colours (13, 14) do
//!   not exist here. An empty piece skips one colour, a piece that does
//!   not parse leaves its colour alone and the chain goes on.
//! - `OSC 104` stops at the first malformed index; with no index (or one
//!   empty piece) it resets the whole palette.
//! - `OSC 110`, `111` and `112` with any data but one empty piece are
//!   ignored, as in xterm.
//!
//! Selection colours (`OSC 17`, `19`), the special colours (`OSC 5`,
//! `105`), the pointer and Tektronix colours (`OSC 13` to `16`, `18`) and
//! their resets stay ignored.
//!
//! # Bounds
//!
//! The overrides are a fixed table of [`MAX_COLOR_OVERRIDES`] slots (256
//! palette entries and the three dynamic colours): no sequence makes it
//! grow. Each query queues one [`Request::Reply`], bounded by the host's
//! reply outbox like every other reply (ADR 0024).
//!
//! [`Request::Reply`]: crate::Request::Reply

use std::fmt;

use tracing::trace;

use crate::{State, Terminal, color_names};

/// How many colours a program can override: the 256 palette entries and
/// the text foreground, background and cursor. [`ColorOverrides`] has a
/// slot for each and never grows (docs/security.md, S5).
pub const MAX_COLOR_OVERRIDES: usize = 256 + 3;

/// A colour as sRGB bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Rgb {
    /// The colour `#rrggbb`.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The colour `0xrrggbb`.
    const fn hex(rgb: u32) -> Self {
        let [_, r, g, b] = rgb.to_be_bytes();
        Self { r, g, b }
    }
}

/// The colours a terminal falls back to while a program has overridden
/// none: the configured theme, with every palette entry spelled out (the
/// host computes 16..=255 the way its renderer does).
///
/// [`Default`] is xterm's: `#e5e5e5` on black, a `#b0b0b0` cursor, xterm's
/// 16 colours, its 6×6×6 cube and its 24 greys, the same as
/// `tessera_render::Theme::default()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeColors {
    /// The text foreground (`OSC 10`).
    pub fg: Rgb,
    /// The text background (`OSC 11`). The renderer's opacity is not part
    /// of it.
    pub bg: Rgb,
    /// The text cursor (`OSC 12`).
    pub cursor: Rgb,
    /// The 256-colour palette (`OSC 4`).
    pub palette: [Rgb; 256],
}

/// xterm's 16 colours, as in `tessera_render::Theme::default()`.
const XTERM_ANSI: [u32; 16] = [
    0x00_0000, 0xcd_0000, 0x00_cd00, 0xcd_cd00, 0x00_00ee, 0xcd_00cd, 0x00_cdcd, 0xe5_e5e5,
    0x7f_7f7f, 0xff_0000, 0x00_ff00, 0xff_ff00, 0x5c_5cff, 0xff_00ff, 0x00_ffff, 0xff_ffff,
];

/// Channel levels of xterm's colour cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

impl Default for ThemeColors {
    fn default() -> Self {
        let palette = std::array::from_fn(|i| match u8::try_from(i).unwrap_or(u8::MAX) {
            i @ 0..=15 => Rgb::hex(XTERM_ANSI[usize::from(i)]),
            i @ 16..=231 => {
                let i = usize::from(i - 16);
                Rgb::new(
                    CUBE_LEVELS[i / 36],
                    CUBE_LEVELS[i / 6 % 6],
                    CUBE_LEVELS[i % 6],
                )
            }
            i => {
                let v = 8 + (i - 232) * 10;
                Rgb::new(v, v, v)
            }
        });
        Self {
            fg: Rgb::hex(0xe5_e5e5),
            bg: Rgb::hex(0x00_0000),
            cursor: Rgb::hex(0xb0_b0b0),
            palette,
        }
    }
}

/// The colours a program set with `OSC 4`, `10`, `11` and `12`; `None`
/// where it set none (or reset it). A fixed table of
/// [`MAX_COLOR_OVERRIDES`] slots.
///
/// An override equal to the theme's colour is still an override: a new
/// theme does not move it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorOverrides {
    /// The text foreground (`OSC 10`).
    pub fg: Option<Rgb>,
    /// The text background (`OSC 11`).
    pub bg: Option<Rgb>,
    /// The text cursor (`OSC 12`).
    pub cursor: Option<Rgb>,
    /// The palette entries (`OSC 4`).
    pub palette: [Option<Rgb>; 256],
}

impl ColorOverrides {
    /// No override at all.
    pub const NONE: Self = Self {
        fg: None,
        bg: None,
        cursor: None,
        palette: [None; 256],
    };

    /// Whether no colour is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }

    /// `theme` with these overrides applied: the colours in effect.
    #[must_use]
    pub fn apply(&self, theme: &ThemeColors) -> ThemeColors {
        ThemeColors {
            fg: self.fg.unwrap_or(theme.fg),
            bg: self.bg.unwrap_or(theme.bg),
            cursor: self.cursor.unwrap_or(theme.cursor),
            palette: std::array::from_fn(|i| self.palette[i].unwrap_or(theme.palette[i])),
        }
    }
}

impl Default for ColorOverrides {
    fn default() -> Self {
        Self::NONE
    }
}

/// One colour a program can set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Fg,
    Bg,
    Cursor,
    Palette(u8),
}

impl Slot {
    /// The dynamic colour `OSC 10 + n` is, `None` past the cursor.
    fn dynamic(n: usize) -> Option<Self> {
        match n {
            0 => Some(Self::Fg),
            1 => Some(Self::Bg),
            2 => Some(Self::Cursor),
            _ => None,
        }
    }
}

/// The terminal's colours: the theme, the program's overrides and the
/// generation counter.
#[derive(Debug, Clone)]
pub(crate) struct Colors {
    theme: ThemeColors,
    overrides: ColorOverrides,
    /// Increased by every change of an override or of an effective
    /// colour.
    generation: u64,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            theme: ThemeColors::default(),
            overrides: ColorOverrides::NONE,
            generation: 0,
        }
    }
}

impl Colors {
    fn theme_of(&self, slot: Slot) -> Rgb {
        match slot {
            Slot::Fg => self.theme.fg,
            Slot::Bg => self.theme.bg,
            Slot::Cursor => self.theme.cursor,
            Slot::Palette(i) => self.theme.palette[usize::from(i)],
        }
    }

    fn override_of(&self, slot: Slot) -> Option<Rgb> {
        match slot {
            Slot::Fg => self.overrides.fg,
            Slot::Bg => self.overrides.bg,
            Slot::Cursor => self.overrides.cursor,
            Slot::Palette(i) => self.overrides.palette[usize::from(i)],
        }
    }

    fn override_mut(&mut self, slot: Slot) -> &mut Option<Rgb> {
        match slot {
            Slot::Fg => &mut self.overrides.fg,
            Slot::Bg => &mut self.overrides.bg,
            Slot::Cursor => &mut self.overrides.cursor,
            Slot::Palette(i) => &mut self.overrides.palette[usize::from(i)],
        }
    }

    /// The colour in effect in `slot`.
    fn effective(&self, slot: Slot) -> Rgb {
        self.override_of(slot)
            .unwrap_or_else(|| self.theme_of(slot))
    }

    /// Records the program's colour for `slot`. Any change of the override
    /// moves the generation, even to the theme's colour: a host copies the
    /// overrides when it moves, and a later theme change keeps this one
    /// (#238). The same colour again moves nothing.
    fn set(&mut self, slot: Slot, color: Rgb) {
        let previous = self.override_mut(slot).replace(color);
        if previous != Some(color) {
            self.generation += 1;
        }
    }

    /// Drops the program's colour for `slot`; returns whether there was
    /// one (the caller bumps the generation).
    fn drop_override(&mut self, slot: Slot) -> bool {
        self.override_mut(slot).take().is_some()
    }

    /// Drops the program's colour for `slot`.
    fn reset(&mut self, slot: Slot) {
        if self.drop_override(slot) {
            self.generation += 1;
        }
    }

    /// Drops every palette override (`OSC 104` without an index).
    fn reset_palette(&mut self) {
        let mut changed = false;
        for i in 0..=u8::MAX {
            changed |= self.drop_override(Slot::Palette(i));
        }
        if changed {
            self.generation += 1;
        }
    }

    /// Drops every override (RIS).
    pub(crate) fn reset_all(&mut self) {
        let mut changed = false;
        for slot in [Slot::Fg, Slot::Bg, Slot::Cursor] {
            changed |= self.drop_override(slot);
        }
        for i in 0..=u8::MAX {
            changed |= self.drop_override(Slot::Palette(i));
        }
        if changed {
            self.generation += 1;
        }
    }

    /// Makes `theme` the colours to fall back to.
    fn set_theme(&mut self, theme: ThemeColors) {
        let o = &self.overrides;
        let follows = |ov: Option<Rgb>, old: Rgb, new: Rgb| ov.is_none() && old != new;
        let changed = follows(o.fg, self.theme.fg, theme.fg)
            || follows(o.bg, self.theme.bg, theme.bg)
            || follows(o.cursor, self.theme.cursor, theme.cursor)
            || (0..256).any(|i| follows(o.palette[i], self.theme.palette[i], theme.palette[i]));
        self.theme = theme;
        if changed {
            self.generation += 1;
        }
    }
}

/// `rgb:RRRR/GGGG/BBBB` for a query reply: each byte twice, lowercase.
struct XColor(Rgb);

impl fmt::Display for XColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Rgb { r, g, b } = self.0;
        write!(f, "rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
    }
}

/// Parses an X11-style colour spec into sRGB bytes; `None` for anything
/// it does not accept. See "Specs" in the module documentation.
///
/// The one entry point for colour specs; names go to
/// [`color_names::lookup`].
pub(crate) fn parse_color_spec(spec: &[u8]) -> Option<Rgb> {
    if let Some(channels) = strip_prefix_ignore_case(spec, b"rgb:") {
        parse_rgb_channels(channels)
    } else if let Some(digits) = spec.strip_prefix(b"#") {
        parse_hash(digits)
    } else {
        color_names::lookup(spec)
    }
}

fn strip_prefix_ignore_case<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let (head, tail) = bytes.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(tail)
}

/// The value of `digits`, all hex, 1 to 4 of them; `None` otherwise (a
/// sign included, unlike `from_str_radix`).
fn hex_value(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    digits.iter().try_fold(0, |value, &digit| {
        let nibble = char::from(digit).to_digit(16)?;
        Some(value << 4 | nibble)
    })
}

/// `R/G/B` after `rgb:`: three channels of the same 1 to 4 hex digits,
/// each scaled to 16 bits (`v * 65535 / (16^n - 1)`) and cut to its high
/// byte.
fn parse_rgb_channels(channels: &[u8]) -> Option<Rgb> {
    let mut parts = channels.split(|&b| b == b'/');
    let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || r.len() != g.len() || g.len() != b.len() {
        return None;
    }
    // Before the shift below, which overflows past 7 digits.
    if !(1..=4).contains(&r.len()) {
        return None;
    }
    let max = (1u32 << (4 * r.len())) - 1;
    let scale = |digits: &[u8]| -> Option<u8> {
        let wide = hex_value(digits)? * 0xffff / max;
        u8::try_from(wide >> 8).ok()
    };
    Some(Rgb::new(scale(r)?, scale(g)?, scale(b)?))
}

/// The digits after `#`: 3, 6, 9 or 12 hex digits in three equal groups,
/// each left-justified in 16 bits (`f` is `f000`) and cut to its high
/// byte.
fn parse_hash(digits: &[u8]) -> Option<Rgb> {
    if !matches!(digits.len(), 3 | 6 | 9 | 12) {
        return None;
    }
    let n = digits.len() / 3;
    let group = |i: usize| -> Option<u8> {
        let wide = hex_value(&digits[i * n..(i + 1) * n])? << (16 - 4 * n);
        u8::try_from(wide >> 8).ok()
    };
    Some(Rgb::new(group(0)?, group(1)?, group(2)?))
}

/// An `OSC 4` / `OSC 104` index: 1 to 3 decimal digits, below 256.
fn parse_index(digits: &[u8]) -> Option<u8> {
    if digits.is_empty() || digits.len() > 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = digits
        .iter()
        .fold(0u16, |v, &d| v * 10 + u16::from(d - b'0'));
    u8::try_from(value).ok()
}

/// Whether a reset's data is "none": no piece, or one empty piece.
fn no_data(args: &[Vec<u8>]) -> bool {
    matches!(args, [] | [_]) && args.iter().all(Vec::is_empty)
}

impl State {
    /// `OSC 4 ; c ; spec ...`: see the module documentation.
    pub(crate) fn osc_palette(&mut self, args: &[Vec<u8>], bell: bool) {
        for pair in args.chunks(2) {
            let [index, spec] = pair else {
                trace!("OSC 4 index without a spec: stop");
                return;
            };
            let Some(index) = parse_index(index) else {
                trace!(len = index.len(), "OSC 4 with a malformed index: stop");
                return;
            };
            let slot = Slot::Palette(index);
            if spec == b"?" {
                let color = XColor(self.colors.effective(slot));
                self.reply_osc(format_args!("4;{index};{color}"), bell);
            } else if let Some(color) = parse_color_spec(spec) {
                self.colors.set(slot, color);
            } else {
                trace!(len = spec.len(), "OSC 4 colour spec not understood: stop");
                return;
            }
        }
    }

    /// `OSC 10`, `11` or `12` (`start` 0, 1 or 2) and the chain after it:
    /// see the module documentation.
    pub(crate) fn osc_dynamic_colors(&mut self, start: usize, args: &[Vec<u8>], bell: bool) {
        for (n, spec) in (start..).zip(args) {
            let Some(slot) = Slot::dynamic(n) else {
                return;
            };
            if spec.is_empty() {
                continue;
            }
            if spec == b"?" {
                let color = XColor(self.colors.effective(slot));
                let ps = 10 + n;
                self.reply_osc(format_args!("{ps};{color}"), bell);
            } else if let Some(color) = parse_color_spec(spec) {
                self.colors.set(slot, color);
            } else {
                trace!(
                    len = spec.len(),
                    "OSC {} colour spec not understood",
                    10 + n
                );
            }
        }
    }

    /// `OSC 104` and its indexes: see the module documentation.
    pub(crate) fn osc_reset_palette(&mut self, args: &[Vec<u8>]) {
        if no_data(args) {
            self.colors.reset_palette();
            return;
        }
        for index in args {
            let Some(index) = parse_index(index) else {
                trace!(len = index.len(), "OSC 104 with a malformed index: stop");
                return;
            };
            self.colors.reset(Slot::Palette(index));
        }
    }

    /// `OSC 110`, `111` or `112` (`n` 0, 1 or 2): resets that dynamic
    /// colour unless data follows.
    pub(crate) fn osc_reset_dynamic_color(&mut self, n: usize, args: &[Vec<u8>]) {
        if !no_data(args) {
            trace!("OSC 11x reset with data ignored");
            return;
        }
        if let Some(slot) = Slot::dynamic(n) {
            self.colors.reset(slot);
        }
    }
}

impl Terminal {
    /// Sets the theme's colours: what every colour a program has not
    /// overridden is, and what `OSC 104`, `110` to `112` and RIS return
    /// to. Configuration, not terminal state: no reset touches it, and the
    /// program's overrides stay (D-THEME-KEEP). The host sets it before
    /// the session's first byte and whenever the configured theme changes.
    /// Starts as [`ThemeColors::default`].
    pub fn set_theme_colors(&mut self, theme: ThemeColors) {
        self.state.colors.set_theme(theme);
    }

    /// The colours the program set (`OSC 4`, `10`, `11`, `12`) and has not
    /// reset since. See the `osc_color` rules in ADR 0024.
    #[must_use]
    pub fn color_overrides(&self) -> &ColorOverrides {
        &self.state.colors.overrides
    }

    /// The colours in effect: the theme ([`Terminal::set_theme_colors`])
    /// with the program's overrides ([`Terminal::color_overrides`]). What
    /// a query reports.
    #[must_use]
    pub fn colors(&self) -> ThemeColors {
        self.state.colors.overrides.apply(&self.state.colors.theme)
    }

    /// A counter that increases whenever the program's overrides
    /// ([`Terminal::color_overrides`]) or a colour in effect
    /// ([`Terminal::colors`]) change: a set that records a new override
    /// value (even one equal to the theme's colour), a reset or RIS
    /// dropping an override, or a theme change that moved a colour no
    /// program overrides. Setting an override to the value it already
    /// holds, a reset of a colour not overridden, or a theme that moves
    /// only overridden colours leaves it. A host compares it frame to
    /// frame and copies the overrides and rebuilds its palette only when
    /// it moved.
    #[must_use]
    pub fn colors_generation(&self) -> u64 {
        self.state.colors.generation
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn rgb_specs_scale_each_channel() {
        let ok = |spec: &str, r, g, b| {
            assert_eq!(
                parse_color_spec(spec.as_bytes()),
                Some(Rgb::new(r, g, b)),
                "{spec}"
            );
        };
        ok("rgb:f/8/0", 0xff, 0x88, 0x00);
        ok("rgb:ff/80/00", 0xff, 0x80, 0x00);
        ok("rgb:fff/000/800", 0xff, 0x00, 0x80);
        ok("rgb:123/456/789", 0x12, 0x45, 0x78);
        ok("rgb:1234/5678/9abc", 0x12, 0x56, 0x9a);
        ok("RGB:FF/00/00", 0xff, 0, 0);
        ok("Rgb:aB/Cd/eF", 0xab, 0xcd, 0xef);
    }

    #[test]
    fn hash_specs_are_left_justified() {
        let ok = |spec: &str, r, g, b| {
            assert_eq!(
                parse_color_spec(spec.as_bytes()),
                Some(Rgb::new(r, g, b)),
                "{spec}"
            );
        };
        ok("#f80", 0xf0, 0x80, 0x00);
        ok("#ff8000", 0xff, 0x80, 0x00);
        ok("#123456789", 0x12, 0x45, 0x78);
        ok("#123456789abc", 0x12, 0x56, 0x9a);
        ok("#FfA0b1", 0xff, 0xa0, 0xb1);
    }

    /// Names reach the X11 table; a spec prefix never does.
    #[test]
    fn names_are_x11_colours() {
        let rgb = |spec: &str| parse_color_spec(spec.as_bytes());
        assert_eq!(rgb("DarkSlateGray"), Some(Rgb::new(0x2f, 0x4f, 0x4f)));
        assert_eq!(rgb("dark slate gray"), rgb("rgb:2f/4f/4f"));
        assert_eq!(rgb("gray"), Some(Rgb::new(0xbe, 0xbe, 0xbe)));
        assert_eq!(rgb("rgb:red"), None);
        assert_eq!(rgb("#red"), None);
    }

    #[test]
    fn everything_else_is_rejected() {
        for spec in [
            "",
            "#",
            "#f",
            "#ff",
            "#ff00",
            "#ff000",
            "#ff00000000",
            "#ggg",
            "#+f0000",
            "#-f0000",
            "##ff0000",
            " #ff0000",
            "#ff0000 ",
            "rgb:",
            "rgb:ff",
            "rgb:ff/00",
            "rgb:ff/00/00/00",
            "rgb:/00/00",
            "rgb:ff//00",
            "rgb:fffff/00/00",
            "rgb:gg/00/00",
            "rgb: ff/00/00",
            "rgb:+f/00/00",
            "rgb:00000000/00000000/00000000",
            "rgb:fffffffffffffffff/fffffffffffffffff/fffffffffffffffff",
            "rgb:f/ff/fff",
            "rgbi:1.0/0/0",
            "red ",
            "dark slategray",
            "transparent",
            "rgb(255,0,0)",
            "?",
        ] {
            assert_eq!(parse_color_spec(spec.as_bytes()), None, "{spec:?}");
        }
        assert_eq!(parse_color_spec(b"#ff\xff000"), None);
    }

    #[test]
    fn indexes_are_short_decimals_below_256() {
        assert_eq!(parse_index(b"0"), Some(0));
        assert_eq!(parse_index(b"001"), Some(1));
        assert_eq!(parse_index(b"255"), Some(255));
        for bad in [
            &b""[..],
            b"256",
            b"999",
            b"-1",
            b"+1",
            b"1x",
            b"0x1",
            b" 1",
            b"0001",
        ] {
            assert_eq!(parse_index(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_default_theme_is_xterms() {
        let theme = ThemeColors::default();
        assert_eq!(theme.palette[1], Rgb::new(0xcd, 0, 0));
        assert_eq!(theme.palette[16], Rgb::new(0, 0, 0));
        assert_eq!(theme.palette[196], Rgb::new(0xff, 0, 0));
        assert_eq!(theme.palette[231], Rgb::new(0xff, 0xff, 0xff));
        assert_eq!(theme.palette[232], Rgb::new(8, 8, 8));
        assert_eq!(theme.palette[255], Rgb::new(0xee, 0xee, 0xee));
    }

    proptest! {
        /// Every 6- and 12-digit `#` spec and every `rgb:` spec of two or
        /// four digits a channel gives back the bytes it spells (the high
        /// byte for four digits).
        #[test]
        fn specs_round_trip(r in any::<u16>(), g in any::<u16>(), b in any::<u16>()) {
            let high = |v: u16| v.to_be_bytes()[0];
            let want = Rgb::new(high(r), high(g), high(b));
            let spec = format!("#{r:04x}{g:04x}{b:04x}");
            prop_assert_eq!(parse_color_spec(spec.as_bytes()), Some(want));
            let spec = format!("rgb:{r:04X}/{g:04x}/{b:04x}");
            prop_assert_eq!(parse_color_spec(spec.as_bytes()), Some(want));
            let spec = format!("#{:02x}{:02x}{:02x}", want.r, want.g, want.b);
            prop_assert_eq!(parse_color_spec(spec.as_bytes()), Some(want));
            let spec = format!("rgb:{:02x}/{:02x}/{:02x}", want.r, want.g, want.b);
            prop_assert_eq!(parse_color_spec(spec.as_bytes()), Some(want));
        }

        /// Arbitrary bytes never panic the parser.
        #[test]
        fn arbitrary_specs_do_not_panic(spec in prop::collection::vec(any::<u8>(), 0..20)) {
            let _ = parse_color_spec(&spec);
        }
    }
}
