//! Select Graphic Rendition (`CSI Pm m`): updates the grid's pen.
//!
//! Parameters apply left to right; an empty list or an empty slot is `0`.
//!
//! | Parameter | Effect |
//! |---|---|
//! | `0` | reset: default colours, default underline colour, no attributes |
//! | `1` `2` `3` | bold, dim, italic (bold and dim may both be set) |
//! | `4` | single underline |
//! | `4:0` .. `4:5` | no, single, double, curly, dotted, dashed underline; `4:n` with `n > 5` is single |
//! | `5` `6` | blink (slow and rapid are one flag) |
//! | `7` `8` `9` | inverse, hidden, strikethrough |
//! | `21` | double underline |
//! | `22` | neither bold nor dim |
//! | `23` `24` `25` `27` `28` `29` | italic, every underline style, blink, inverse, hidden, strikethrough off |
//! | `30`-`37` `90`-`97` | foreground: ANSI colour 0-7, 8-15 |
//! | `40`-`47` `100`-`107` | background: ANSI colour 0-7, 8-15 |
//! | `38` `48` `58` | foreground, background, underline colour: `;5;n` / `:5:n` indexed, `;2;r;g;b` / `:2:r:g:b` / `:2:id:r:g:b` direct |
//! | `39` `49` `59` | default foreground, background, underline colour |
//!
//! Everything else (fonts 10-20, 26, 50-57, 60-65, 73-75, unassigned
//! numbers) is ignored and parsing continues with the next parameter.
//!
//! # Extended colours
//!
//! The `;` form of `38`, `48` and `58` reads the next flattened parameters
//! whatever separated them: a kind (`5` or `2`), then one index or three
//! components. A list that ends early sets nothing and ends the sequence; an
//! unknown kind is skipped together with the `38` and parsing continues
//! after it. The `:` form reads only its own sub-parameter group: `5:n`,
//! `2:r:g:b` with exactly four sub-parameters, or `2:id:r:g:b` with five or
//! more (the colour-space id is ignored, as are sub-parameters past the
//! blue component). An index or component above 255 makes that colour a
//! no-op, but its parameters are still consumed.
//!
//! # Sub-parameters
//!
//! A `:` group headed by anything but `4`, `38`, `48` or `58` is ignored as
//! a whole (xterm, Alacritty): `1:2` is neither bold nor dim and `0:1` is
//! not a reset. Sub-parameters a `;` colour left unread are skipped.
//!
//! No palette lives here: colours stay [`Color::Indexed`] or [`Color::Rgb`]
//! and `tessera-render` resolves them (ADR 0010). The expected behaviour of
//! every row above is pinned by `tests/fixtures/sgr_spec.txt`; ADR 0013
//! records the decisions.

use tessera_grid::{Color, Style, UnderlineStyle};
use tracing::trace;

use crate::State;

/// Which colour of the pen an extended-colour parameter sets.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Fg,
    Bg,
    Underline,
}

impl Slot {
    /// The slot of an extended-colour introducer (`38`, `48`, `58`).
    fn of(code: u16) -> Option<Self> {
        match code {
            38 => Some(Self::Fg),
            48 => Some(Self::Bg),
            58 => Some(Self::Underline),
            _ => None,
        }
    }

    /// The pen's colour for this slot.
    fn color_mut(self, pen: &mut Style) -> &mut Color {
        match self {
            Self::Fg => &mut pen.fg,
            Self::Bg => &mut pen.bg,
            Self::Underline => &mut pen.underline,
        }
    }
}

/// True when `params[index]` was introduced by `:`.
fn is_sub(subparams: u32, index: usize) -> bool {
    u32::try_from(index)
        .ok()
        .and_then(|i| subparams.checked_shr(i))
        .is_some_and(|bits| bits & 1 == 1)
}

/// An indexed colour, or `None` when the index is above 255.
fn indexed(index: u16) -> Option<Color> {
    u8::try_from(index).ok().map(Color::Indexed)
}

/// A direct colour, or `None` when a component is above 255.
fn rgb(r: u16, g: u16, b: u16) -> Option<Color> {
    Some(Color::Rgb(
        u8::try_from(r).ok()?,
        u8::try_from(g).ok()?,
        u8::try_from(b).ok()?,
    ))
}

/// Reads the `;` form of an extended colour from `rest`, the parameters
/// after the introducer. Returns the colour, if a valid one was given, and
/// how many parameters of `rest` it consumed.
fn semicolon_color(rest: &[u16]) -> (Option<Color>, usize) {
    match rest {
        [] => (None, 0),
        [5, index, ..] => (indexed(*index), 2),
        [2, r, g, b, ..] => (rgb(*r, *g, *b), 4),
        // Too few values for the kind: consume what exists and stop.
        [5 | 2, ..] => (None, rest.len()),
        // Unknown kind: skip it and go on (xterm, Alacritty).
        [_, ..] => (None, 1),
    }
}

/// Reads the `:` form of an extended colour from `subs`, the
/// sub-parameters after the introducer.
fn colon_color(subs: &[u16]) -> Option<Color> {
    match subs {
        [5, index, ..] => indexed(*index),
        [2, r, g, b] | [2, _, r, g, b, ..] => rgb(*r, *g, *b),
        _ => None,
    }
}

/// The underline style `4:n` selects.
fn underline_style(n: u16) -> UnderlineStyle {
    match n {
        0 => UnderlineStyle::None,
        2 => UnderlineStyle::Double,
        3 => UnderlineStyle::Curly,
        4 => UnderlineStyle::Dotted,
        5 => UnderlineStyle::Dashed,
        // 1, and unknown styles (Alacritty, WezTerm).
        _ => UnderlineStyle::Single,
    }
}

impl State {
    /// SGR: applies `params` (with the sub-parameter bitmask of
    /// [`tessera_vt::Action::Csi`]) to the pen. See the module
    /// documentation.
    pub(crate) fn sgr(&mut self, params: &[u16], subparams: u32) {
        let pen = self.grid.pen_mut();
        let mut i = 0;
        while i < params.len() {
            let code = params[i];
            if is_sub(subparams, i) {
                // Left over after a `;` colour that read fewer values.
                i += 1;
                continue;
            }
            let mut end = i + 1;
            while end < params.len() && is_sub(subparams, end) {
                end += 1;
            }
            if end > i + 1 {
                let subs = &params[i + 1..end];
                match (code, Slot::of(code)) {
                    (4, _) => *pen = pen.with_underline_style(underline_style(subs[0])),
                    (_, Some(slot)) => {
                        if let Some(color) = colon_color(subs) {
                            *slot.color_mut(pen) = color;
                        }
                    }
                    _ => trace!(code, ?subs, "SGR sub-parameter group ignored"),
                }
                i = end;
                continue;
            }
            i += 1;
            if let Some(slot) = Slot::of(code) {
                let (color, used) = semicolon_color(&params[i..]);
                i += used;
                if let Some(color) = color {
                    *slot.color_mut(pen) = color;
                }
                continue;
            }
            match code {
                0 => *pen = Style::DEFAULT,
                1 => *pen = pen.with(Style::BOLD),
                2 => *pen = pen.with(Style::DIM),
                3 => *pen = pen.with(Style::ITALIC),
                4 => *pen = pen.with_underline_style(UnderlineStyle::Single),
                5 | 6 => *pen = pen.with(Style::BLINK),
                7 => *pen = pen.with(Style::INVERSE),
                8 => *pen = pen.with(Style::HIDDEN),
                9 => *pen = pen.with(Style::STRIKETHROUGH),
                21 => *pen = pen.with_underline_style(UnderlineStyle::Double),
                22 => *pen = pen.without(Style::BOLD | Style::DIM),
                23 => *pen = pen.without(Style::ITALIC),
                24 => *pen = pen.with_underline_style(UnderlineStyle::None),
                25 => *pen = pen.without(Style::BLINK),
                27 => *pen = pen.without(Style::INVERSE),
                28 => *pen = pen.without(Style::HIDDEN),
                29 => *pen = pen.without(Style::STRIKETHROUGH),
                // The ranges guarantee the subtraction fits in a `u8`.
                30..=37 => pen.fg = Color::Indexed(ansi(code - 30)),
                40..=47 => pen.bg = Color::Indexed(ansi(code - 40)),
                90..=97 => pen.fg = Color::Indexed(ansi(code - 90 + 8)),
                100..=107 => pen.bg = Color::Indexed(ansi(code - 100 + 8)),
                39 => pen.fg = Color::Default,
                49 => pen.bg = Color::Default,
                59 => pen.underline = Color::Default,
                _ => trace!(code, "SGR parameter ignored"),
            }
        }
    }
}

/// An ANSI colour number (0-15) as an index; callers pass only those.
fn ansi(n: u16) -> u8 {
    u8::try_from(n).unwrap_or(0)
}
