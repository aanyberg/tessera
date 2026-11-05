//! Character sets: SCS designation, the locking and single shifts, and the
//! translation of printed characters (ADR 0033).
//!
//! xterm's model, which Tessera follows: four G-sets (G0 to G3), GL (the set
//! `0x21..=0x7E` print through, G0 until SO, LS2 or LS3 change it), GR (the
//! set for `U+0080..=U+00FF`, G2 until LS1R, LS2R or LS3R change it) and a
//! pending single shift (SS2 or SS3) that applies to the next printed
//! character only. The output is Unicode, so a set maps a printed code point
//! to another one:
//!
//! - GL translates the ASCII range, GR the Latin-1 range (xterm keeps
//!   GR as a view on `U+0080..=U+00FF` in UTF-8 mode); code points above
//!   `U+00FF` are never translated.
//! - The arithmetic is xterm's `xtermCharSetOut`: the set maps the seven-bit
//!   value `code & 0x7F`; a result that is DEL is dropped for a seven-bit
//!   set; a result in `0x21..=0x7F` for a GR character gets bit 7 back.
//!   (So with a seven-bit GR a no-break space prints as a space and `ÿ`
//!   disappears, as in xterm.)
//! - Printing when GL, GR and the single shift are all ASCII costs one
//!   branch on [`Charsets::active`].
//!
//! Sets: US ASCII, the UK set, DEC Special Graphics, DEC Supplemental (also
//! the user-preferred set at levels 3 and 4, where it is ISO Latin-1),
//! DEC Technical, ISO Latin-1 (96 characters) and the national replacement
//! sets that xterm offers up to level 4: Dutch, Finnish, French, French
//! Canadian, German, Italian, Norwegian/Danish, Portuguese, Spanish,
//! Swedish and Swiss. The VT500 sets (Greek, Hebrew, Turkish, Russian,
//! Cyrillic, SCS, the ISO Latin-2, -5, Greek, Hebrew and Cyrillic 96-sets)
//! and the VT382 JIS sets are not implemented: their finals behave as
//! xterm's behave without them (ASCII is selected).

/// One character set. The NRCS variants only ever map the twelve ASCII
/// positions national sets may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Set {
    /// US ASCII (`B`).
    Ascii,
    /// The UK set (`A`) and ISO Latin-1 with DECNRCM (xterm's British
    /// Latin-1): `#` becomes `£` with DECNRCM on, or at level 1.
    British,
    /// ISO Latin-1 (`ESC - A` without DECNRCM) and the user-preferred set at
    /// levels 3 and 4: the same mapping as [`Set::British`], but not a
    /// seven-bit set (it keeps `U+00FF`).
    Latin1,
    /// DEC Special Graphics (`0`): the line-drawing set.
    DecSpecial,
    /// DEC Supplemental (`< ` at level 2) and DEC Supplemental Graphic
    /// (`% 5`).
    DecSupplemental,
    /// DEC Technical (`>`).
    DecTechnical,
    /// Dutch (`4`).
    Dutch,
    /// Finnish (`5`, `C`).
    Finnish,
    /// French (`R`, `f`).
    French,
    /// French Canadian (`Q`, `9`).
    FrenchCanadian,
    /// German (`K`).
    German,
    /// Italian (`Y`).
    Italian,
    /// Norwegian/Danish (`E`, `6`, `` ` ``).
    NorwegianDanish,
    /// Portuguese (`% 6`).
    Portuguese,
    /// Spanish (`Z`).
    Spanish,
    /// Swedish (`7`, `H`).
    Swedish,
    /// Swiss (`=`).
    Swiss,
}

/// The conformance level at which DECNRCM is not a mode and the UK set maps
/// `#` always (xterm: `vtXX_level <= 1`).
const VT100: u8 = 1;

impl Set {
    /// Whether xterm counts the set as seven-bit, so that a DEL it produces
    /// is dropped (`isSevenBit`).
    fn seven_bit(self) -> bool {
        self != Self::Latin1
    }

    /// The code the set gives `code` (a code point up to `U+00FF`), before
    /// the DEL and bit-7 rules of [`Charsets::translate`]. `national` is
    /// DECNRCM; `level` the conformance level.
    fn map(self, code: u32, national: bool, level: u8) -> u32 {
        let seven = code & 0x7F;
        let in_table = |table: &[(u8, char)]| {
            let found = table.iter().find(|(from, _)| u32::from(*from) == seven);
            found.map_or(seven, |&(_, to)| u32::from(to))
        };
        match self {
            Self::Ascii => code,
            Self::British | Self::Latin1 => {
                if national || level <= VT100 {
                    let chr = if national { seven } else { code };
                    if chr == 0x23 { 0xA3 } else { chr }
                } else {
                    code
                }
            }
            Self::DecSpecial => match seven {
                0x60..=0x7E => u32::from(DEC_SPECIAL[(seven - 0x60) as usize]),
                // xterm tests the unmasked code here: only a GL underscore
                // becomes the blank (code 0); a GR one stays an underscore.
                _ if code == 0x5F => 0x20,
                _ => seven,
            },
            Self::DecSupplemental => DEC_SUPPLEMENTAL
                .iter()
                .find(|(from, _)| u32::from(*from) == seven)
                .map_or(code, |&(_, to)| u32::from(to)),
            Self::DecTechnical => match seven {
                0x21..=0x7E => u32::from(DEC_TECHNICAL[(seven - 0x21) as usize]),
                _ => seven,
            },
            Self::Dutch => in_table(DUTCH),
            Self::Finnish => in_table(FINNISH),
            Self::French => in_table(FRENCH),
            Self::FrenchCanadian => in_table(FRENCH_CANADIAN),
            Self::German => in_table(GERMAN),
            Self::Italian => in_table(ITALIAN),
            Self::NorwegianDanish => in_table(NORWEGIAN_DANISH),
            Self::Portuguese => in_table(PORTUGUESE),
            Self::Spanish => in_table(SPANISH),
            Self::Swedish => in_table(SWEDISH),
            Self::Swiss => in_table(SWISS),
        }
    }
}

/// DEC Special Graphics for `0x60..=0x7E` (xterm's `dec2ucs`).
const DEC_SPECIAL: [char; 31] = [
    '\u{25c6}', '\u{2592}', '\u{2409}', '\u{240c}', '\u{240d}', '\u{240a}', '\u{b0}', '\u{b1}',
    '\u{2424}', '\u{240b}', '\u{2518}', '\u{2510}', '\u{250c}', '\u{2514}', '\u{253c}', '\u{23ba}',
    '\u{23bb}', '\u{2500}', '\u{23bc}', '\u{23bd}', '\u{251c}', '\u{2524}', '\u{2534}', '\u{252c}',
    '\u{2502}', '\u{2264}', '\u{2265}', '\u{3c0}', '\u{2260}', '\u{a3}', '\u{b7}',
];

/// Dutch NRCS (xterm `map_NRCS_Dutch`).
const DUTCH: &[(u8, char)] = &[
    (b'#', '£'),
    (b'@', '¾'),
    (b'[', 'ĳ'),
    (b'\\', '½'),
    (b']', '|'),
    (b'{', '¨'),
    (b'|', 'ƒ'),
    (b'}', '¼'),
    (b'~', '´'),
];

/// Finnish NRCS (xterm `map_NRCS_Finnish`).
const FINNISH: &[(u8, char)] = &[
    (b'[', 'Ä'),
    (b'\\', 'Ö'),
    (b']', 'Å'),
    (b'^', 'Ü'),
    (b'`', 'é'),
    (b'{', 'ä'),
    (b'|', 'ö'),
    (b'}', 'å'),
    (b'~', 'ü'),
];

/// French NRCS (xterm `map_NRCS_French`).
const FRENCH: &[(u8, char)] = &[
    (b'#', '£'),
    (b'@', 'à'),
    (b'[', '°'),
    (b'\\', 'ç'),
    (b']', '§'),
    (b'{', 'é'),
    (b'|', 'ù'),
    (b'}', 'è'),
    (b'~', '¨'),
];

/// French Canadian NRCS (xterm `map_NRCS_French_Canadian`).
const FRENCH_CANADIAN: &[(u8, char)] = &[
    (b'@', 'à'),
    (b'[', 'â'),
    (b'\\', 'ç'),
    (b']', 'ê'),
    (b'^', 'î'),
    (b'`', 'ô'),
    (b'{', 'é'),
    (b'|', 'ù'),
    (b'}', 'è'),
    (b'~', 'û'),
];

/// German NRCS (xterm `map_NRCS_German`).
const GERMAN: &[(u8, char)] = &[
    (b'@', '§'),
    (b'[', 'Ä'),
    (b'\\', 'Ö'),
    (b']', 'Ü'),
    (b'{', 'ä'),
    (b'|', 'ö'),
    (b'}', 'ü'),
    (b'~', 'ß'),
];

/// Italian NRCS (xterm `map_NRCS_Italian`).
const ITALIAN: &[(u8, char)] = &[
    (b'#', '£'),
    (b'@', '§'),
    (b'[', '°'),
    (b'\\', 'ç'),
    (b']', 'é'),
    (b'`', 'ù'),
    (b'{', 'à'),
    (b'|', 'ò'),
    (b'}', 'è'),
    (b'~', 'ì'),
];

/// Norwegian/Danish NRCS (xterm `map_NRCS_Norwegian_Danish`).
const NORWEGIAN_DANISH: &[(u8, char)] = &[
    (b'@', 'Ä'),
    (b'[', 'Æ'),
    (b'\\', 'Ø'),
    (b']', 'Å'),
    (b'^', 'Ü'),
    (b'`', 'ä'),
    (b'{', 'æ'),
    (b'|', 'ø'),
    (b'}', 'å'),
    (b'~', 'ü'),
];

/// Portuguese NRCS (xterm `map_NRCS_Portuguese`).
const PORTUGUESE: &[(u8, char)] = &[
    (b'[', 'Ã'),
    (b'\\', 'Ç'),
    (b']', 'Õ'),
    (b'{', 'ã'),
    (b'|', 'ç'),
    (b'}', 'õ'),
];

/// Spanish NRCS (xterm `map_NRCS_Spanish`).
const SPANISH: &[(u8, char)] = &[
    (b'#', '£'),
    (b'@', '§'),
    (b'[', '¡'),
    (b'\\', 'Ñ'),
    (b']', '¿'),
    (b'{', '°'),
    (b'|', 'ñ'),
    (b'}', 'ç'),
];

/// Swedish NRCS (xterm `map_NRCS_Swedish`).
const SWEDISH: &[(u8, char)] = &[
    (b'@', 'É'),
    (b'[', 'Ä'),
    (b'\\', 'Ö'),
    (b']', 'Å'),
    (b'^', 'Ü'),
    (b'`', 'é'),
    (b'{', 'ä'),
    (b'|', 'ö'),
    (b'}', 'å'),
    (b'~', 'ü'),
];

/// Swiss NRCS (xterm `map_NRCS_Swiss`).
const SWISS: &[(u8, char)] = &[
    (b'#', 'ù'),
    (b'@', 'à'),
    (b'[', 'é'),
    (b'\\', 'ç'),
    (b']', 'ê'),
    (b'^', 'î'),
    (b'_', 'è'),
    (b'`', 'ô'),
    (b'{', 'ä'),
    (b'|', 'ö'),
    (b'}', 'ü'),
    (b'~', 'û'),
];

/// DEC Supplemental (xterm `map_DEC_Supp_Graphic`), 0x5F included.
const DEC_SUPPLEMENTAL: &[(u8, char)] = &[
    (0x24, '\u{2426}'),
    (0x26, '\u{2426}'),
    (0x28, '¤'),
    (0x2c, '\u{2426}'),
    (0x2d, '\u{2426}'),
    (0x2e, '\u{2426}'),
    (0x2f, '\u{2426}'),
    (0x34, '\u{2426}'),
    (0x38, '\u{2426}'),
    (0x3e, '\u{2426}'),
    (0x50, '\u{2426}'),
    (0x57, 'Œ'),
    (0x5d, 'Ÿ'),
    (0x5e, '\u{2426}'),
    (0x5f, '_'),
    (0x70, '\u{2426}'),
    (0x77, 'œ'),
    (0x7d, 'ÿ'),
    (0x7e, '\u{2426}'),
];

/// DEC Technical (xterm `map_DEC_Technical`), 0x21 to 0x7E.
const DEC_TECHNICAL: [char; 94] = [
    '⎷', '\u{250c}', '\u{2500}', '⌠', '⌡', '\u{2502}', '⎡', '⎣', '⎤', '⎦', '⎧', '⎩', '⎫', '⎭', '⎨',
    '⎬', '\u{eeee}', '\u{eeef}', '\u{eef0}', '\u{eef1}', '\u{eef2}', '\u{eef3}', '\u{eef4}',
    '\u{2426}', '\u{2426}', '\u{2426}', '\u{2426}', '≤', '≠', '≥', '∫', '∴', '∝', '∞', '÷', 'Δ',
    '∇', 'Φ', 'Γ', '∼', '≃', 'Θ', '×', 'Λ', '⇔', '⇒', '≡', 'Π', 'Ψ', '\u{2426}', 'Σ', '\u{2426}',
    '\u{2426}', '√', 'Ω', 'Ξ', 'Υ', '⊂', '⊃', '∩', '∪', '∧', '∨', '¬', 'α', 'β', 'χ', 'δ', 'ε',
    'φ', 'γ', 'η', 'ι', 'θ', 'κ', 'λ', '\u{2426}', 'ν', '∂', 'π', 'ψ', 'ρ', 'σ', 'τ', '\u{2426}',
    'ƒ', 'ω', 'ξ', 'υ', 'ζ', '←', '↑', '→', '↓',
];

/// One row of xterm's `scs_table`: a designation the terminal accepts.
struct Row {
    set: Set,
    /// The intermediate between `ESC (` and the final (`%`, `"`, `&`), or 0.
    prefix: u8,
    final_byte: u8,
    /// The lowest conformance level that offers the set, and the highest.
    min_level: u8,
    max_level: u8,
    /// The set needs DECNRCM to be selected.
    needs_nrcs: bool,
}

const fn row(set: Set, prefix: u8, final_byte: u8, levels: (u8, u8), needs_nrcs: bool) -> Row {
    Row {
        set,
        prefix,
        final_byte,
        min_level: levels.0,
        max_level: levels.1,
        needs_nrcs,
    }
}

/// xterm's `scs_table`, in its order (the first match wins), without the
/// sets Tessera does not implement.
const ROWS: &[Row] = &[
    row(Set::Ascii, 0, b'B', (1, 9), false),
    row(Set::British, 0, b'A', (1, 9), false),
    row(Set::DecSpecial, 0, b'0', (1, 9), false),
    row(Set::DecSupplemental, 0, b'<', (2, 2), false),
    row(Set::Dutch, 0, b'4', (2, 9), true),
    row(Set::Finnish, 0, b'5', (2, 9), true),
    row(Set::Finnish, 0, b'C', (2, 9), true),
    row(Set::French, 0, b'R', (2, 9), true),
    row(Set::French, 0, b'f', (2, 9), true),
    row(Set::FrenchCanadian, 0, b'Q', (2, 9), true),
    row(Set::German, 0, b'K', (2, 9), true),
    row(Set::Italian, 0, b'Y', (2, 9), true),
    row(Set::NorwegianDanish, 0, b'E', (2, 9), true),
    row(Set::NorwegianDanish, 0, b'6', (2, 9), true),
    row(Set::Spanish, 0, b'Z', (2, 9), true),
    row(Set::Swedish, 0, b'7', (2, 9), true),
    row(Set::Swedish, 0, b'H', (2, 9), true),
    row(Set::Swiss, 0, b'=', (2, 9), true),
    // `<` from level 3 is the user-preferred set, ISO Latin-1 in UTF-8 mode
    // up to level 4 (xterm's HandleUPSS) and ASCII from level 5, which
    // the fallback below gives.
    row(Set::Latin1, 0, b'<', (3, 4), false),
    // `ESC - A` and the 94-character `A` of a 96-set designation: British
    // Latin-1 with DECNRCM, else ISO Latin-1.
    row(Set::British, 0, b'A', (3, 9), true),
    row(Set::DecSupplemental, b'%', b'5', (3, 9), false),
    row(Set::DecTechnical, 0, b'>', (3, 9), false),
    row(Set::FrenchCanadian, 0, b'9', (3, 9), true),
    row(Set::NorwegianDanish, 0, b'`', (3, 9), true),
    row(Set::Portuguese, b'%', b'6', (3, 9), true),
    row(Set::Latin1, 0, b'A', (3, 9), false),
];

/// xterm's `xtermDecodeSCS`: the set for a designation of group `sgroup`
/// (the lowest level a final of its kind implies), or ASCII when no row
/// fits: a final the terminal knows but cannot offer at this level or
/// without DECNRCM selects ASCII.
fn lookup(level: u8, national: bool, sgroup: u8, prefix: u8, final_byte: u8) -> Set {
    ROWS.iter()
        .find(|row| {
            row.prefix == prefix
                && row.final_byte == final_byte
                && sgroup <= row.min_level
                && (row.min_level..=row.max_level).contains(&level)
                && (!row.needs_nrcs || national)
        })
        .map_or(Set::Ascii, |row| row.set)
}

/// What `ESC I [prefix] F` designates, if the terminal acts on it: the G-set
/// index and the set. `intermediates` are the bytes after ESC (`(`, `)`,
/// `*`, `+`, `-`, `.` or `/`, then an optional `%`, `"` or `&`).
///
/// A final the xterm parse tables do not route to a designation, or route
/// only at a higher level (VT220 and up for anything but `0`, `1`, `2`, `A`
/// and `B`; VT320 for `9`, `>`, the backquote and `ESC - A`; VT500 for the
/// `"` and `&` prefixes and the other 96-sets), does nothing and returns
/// `None`.
pub(crate) fn designation(
    level: u8,
    national: bool,
    intermediates: &[u8],
    final_byte: u8,
) -> Option<(usize, Set)> {
    let (&first, rest) = intermediates.split_first()?;
    let (g, is_96) = match first {
        b'(' => (0, false),
        b')' => (1, false),
        b'*' => (2, false),
        b'+' => (3, false),
        b'-' => (1, true),
        b'.' => (2, true),
        b'/' => (3, true),
        _ => return None,
    };
    let (sgroup, prefix) = match (is_96, rest, final_byte) {
        (false, [], b'0' | b'1' | b'2' | b'A' | b'B') => (1, 0),
        (
            false,
            [],
            b'4'..=b'7'
            | b'<'
            | b'='
            | b'C'
            | b'E'
            | b'H'..=b'K'
            | b'Q'
            | b'R'
            | b'Y'
            | b'Z'
            | b'f',
        ) if level >= 2 => (2, 0),
        (false, [], b'9' | b'>' | b'`') | (true, [], b'A') if level >= 3 => (3, 0),
        (true, [], b'B' | b'F' | b'H' | b'L' | b'M') if level >= 5 => (5, 0),
        (false, [p @ b'"'], b'4' | b'>' | b'?') if level >= 5 => (5, *p),
        (false, [p @ b'&'], b'4' | b'5') if level >= 5 => (5, *p),
        (false, [p @ b'%'], b'0' | b'2' | b'=') if level >= 3 => (5, *p),
        (false, [p @ b'%'], b'3' | b'5' | b'6') if level >= 3 => (3, *p),
        _ => return None,
    };
    Some((g, lookup(level, national, sgroup, prefix, final_byte)))
}

/// The state a DECSC saves: the sets and their invocation, not the single
/// shift (xterm's `SavedCursor` has no `curss`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Saved {
    sets: [Set; 4],
    gl: u8,
    gr: u8,
}

/// G0 to G3, GL, GR and the single shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Charsets {
    sets: [Set; 4],
    gl: u8,
    gr: u8,
    single_shift: Option<u8>,
    /// Whether a printed character needs [`Charsets::translate`]: GL, GR or
    /// the pending single shift is not ASCII. The only thing the print path
    /// reads.
    pub(crate) active: bool,
}

impl Default for Charsets {
    /// xterm's `resetCharsets` in UTF-8 mode: everything ASCII, GL = G0,
    /// GR = G2.
    fn default() -> Self {
        Self::from(Saved {
            sets: [Set::Ascii; 4],
            gl: 0,
            gr: 2,
        })
    }
}

impl From<Saved> for Charsets {
    fn from(saved: Saved) -> Self {
        let mut charsets = Self {
            sets: saved.sets,
            gl: saved.gl,
            gr: saved.gr,
            single_shift: None,
            active: false,
        };
        charsets.update();
        charsets
    }
}

impl Charsets {
    fn update(&mut self) {
        self.active = self.single_shift.is_some()
            || self.sets[usize::from(self.gl)] != Set::Ascii
            || self.sets[usize::from(self.gr)] != Set::Ascii;
    }

    /// Designates `set` as G`g`.
    pub(crate) fn designate(&mut self, g: usize, set: Set) {
        self.sets[g] = set;
        self.update();
    }

    /// Invokes G`g` as GL (SI, SO, LS2, LS3).
    pub(crate) fn shift_left(&mut self, g: u8) {
        self.gl = g;
        self.update();
    }

    /// Invokes G`g` as GR (LS1R, LS2R, LS3R).
    pub(crate) fn shift_right(&mut self, g: u8) {
        self.gr = g;
        self.update();
    }

    /// SS2 or SS3: G`g` for the next printed character.
    pub(crate) fn single_shift(&mut self, g: u8) {
        self.single_shift = Some(g);
        self.update();
    }

    /// The saved part, for DECSC.
    pub(crate) fn saved(&self) -> Saved {
        Saved {
            sets: self.sets,
            gl: self.gl,
            gr: self.gr,
        }
    }

    /// DECRC of a save: the sets and GL and GR as saved; a pending single
    /// shift is not touched.
    pub(crate) fn restore(&mut self, saved: Saved) {
        let single_shift = self.single_shift;
        *self = Self::from(saved);
        self.single_shift = single_shift;
        self.update();
    }

    /// The default state: RIS, DECSTR, DECSCL and a DECRC without a DECSC.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The code point to print for `ch`, or `None` when xterm drops it.
    /// Consumes the pending single shift. `national` is DECNRCM, `level`
    /// the conformance level.
    pub(crate) fn translate(&mut self, ch: char, national: bool, level: u8) -> Option<char> {
        let left = self.single_shift.take().unwrap_or(self.gl);
        self.update();
        let code = u32::from(ch);
        if code > 0xFF {
            return Some(ch);
        }
        let set = self.sets[usize::from(if code >= 0x80 { self.gr } else { left })];
        let mut mapped = set.map(code, national, level);
        if mapped == 0x7F && set.seven_bit() {
            return None;
        }
        if code >= 0x80 && mapped > 0x20 && mapped < 0x80 {
            mapped |= 0x80;
        }
        char::from_u32(mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translated(charsets: &mut Charsets, text: &str) -> String {
        text.chars()
            .filter_map(|ch| charsets.translate(ch, false, 4))
            .collect()
    }

    #[test]
    fn a_default_state_needs_no_translation() {
        let charsets = Charsets::default();
        assert!(!charsets.active);
        assert_eq!((charsets.gl, charsets.gr), (0, 2));
    }

    /// The fast path skips the translation while every set is ASCII; the
    /// slow path must give the same for the sets that map as ASCII too.
    #[test]
    fn the_sets_that_map_as_ascii_translate_to_themselves() {
        for set in [Set::Ascii, Set::British, Set::Latin1] {
            let mut charsets = Charsets::default();
            for g in 0..4 {
                charsets.designate(g, set);
            }
            for code in (0x20..=0x2FF).filter(|code| !matches!(code, 0x7F..=0x9F)) {
                let ch = char::from_u32(code).expect("a scalar value");
                assert_eq!(charsets.translate(ch, false, 4), Some(ch), "{set:?} {ch:?}");
            }
        }
    }

    #[test]
    fn line_drawing_in_gl_and_gr() {
        let mut charsets = Charsets::default();
        charsets.designate(0, Set::DecSpecial);
        assert!(charsets.active);
        assert_eq!(translated(&mut charsets, "lqk_x"), "┌─┐ │");
        charsets.designate(0, Set::Ascii);
        assert!(!charsets.active);
        charsets.designate(2, Set::DecSpecial);
        assert!(charsets.active, "G2 is GR by default");
        assert_eq!(translated(&mut charsets, "l\u{ec}\u{df}"), "l┌ß");
    }

    #[test]
    fn a_single_shift_is_used_once_and_clears_the_active_flag() {
        let mut charsets = Charsets::default();
        charsets.designate(3, Set::DecSpecial);
        charsets.single_shift(3);
        assert!(charsets.active);
        assert_eq!(translated(&mut charsets, "ll"), "┌l");
        assert!(!charsets.active);
    }

    #[test]
    fn seven_bit_gr_drops_u00ff_and_folds_the_no_break_space() {
        let mut charsets = Charsets::default();
        charsets.designate(2, Set::DecSpecial);
        assert_eq!(translated(&mut charsets, "a\u{a0}\u{ff}b"), "a b");
        charsets.designate(2, Set::Latin1);
        assert_eq!(translated(&mut charsets, "\u{ff}"), "\u{ff}");
    }

    /// With DECNRCM a Latin-1 GR folds `U+00FF` onto DEL like any other
    /// set, but only the seven-bit ones drop it (xterm `isSevenBit`).
    #[test]
    fn only_a_seven_bit_set_drops_the_del_decnrcm_makes_of_u00ff() {
        let mut charsets = Charsets::default();
        charsets.designate(2, Set::Latin1);
        assert_eq!(charsets.translate('\u{ff}', true, 4), Some('\u{ff}'));
        charsets.designate(2, Set::British);
        assert_eq!(charsets.translate('\u{ff}', true, 4), None);
    }

    #[test]
    fn nothing_above_u00ff_is_translated_but_it_still_takes_a_single_shift() {
        let mut charsets = Charsets::default();
        charsets.designate(2, Set::DecSpecial);
        charsets.single_shift(2);
        assert_eq!(translated(&mut charsets, "日q"), "日q");
        assert_eq!(charsets.single_shift, None);
    }

    #[test]
    fn the_uk_set_maps_the_pound_sign_in_national_mode_or_at_level_1_only() {
        let mut charsets = Charsets::default();
        charsets.designate(0, Set::British);
        assert_eq!(charsets.translate('#', false, 4), Some('#'));
        assert_eq!(charsets.translate('#', true, 4), Some('£'));
        assert_eq!(charsets.translate('#', false, 1), Some('£'));
    }

    #[test]
    fn designations_follow_xterm_s_table() {
        let set = |level, national, inter: &[u8], byte| designation(level, national, inter, byte);
        assert_eq!(set(4, false, b"(", b'0'), Some((0, Set::DecSpecial)));
        assert_eq!(set(4, false, b")", b'B'), Some((1, Set::Ascii)));
        assert_eq!(set(4, false, b"*", b'A'), Some((2, Set::British)));
        assert_eq!(set(4, false, b"+", b'>'), Some((3, Set::DecTechnical)));
        assert_eq!(set(4, false, b"(%", b'5'), Some((0, Set::DecSupplemental)));
        // A national set without DECNRCM, and a set from a higher level,
        // select ASCII.
        assert_eq!(set(4, false, b"(", b'K'), Some((0, Set::Ascii)));
        assert_eq!(set(4, true, b"(", b'K'), Some((0, Set::German)));
        assert_eq!(set(4, true, b"(", b'J'), Some((0, Set::Ascii)));
        // A final that does not dispatch, or only at a higher level, is
        // ignored.
        assert_eq!(set(4, false, b"(", b'D'), None);
        assert_eq!(set(1, false, b"(", b'K'), None);
        assert_eq!(set(4, false, b"(\"", b'>'), None);
        assert_eq!(set(4, false, b"-", b'B'), None);
        assert_eq!(set(4, false, b"(%", b'Z'), None);
        assert_eq!(set(4, false, b"!", b'B'), None);
        // `<` is DEC Supplemental at level 2, the user-preferred set (ISO
        // Latin-1) after.
        assert_eq!(set(2, false, b"(", b'<'), Some((0, Set::DecSupplemental)));
        assert_eq!(set(3, false, b"(", b'<'), Some((0, Set::Latin1)));
        assert_eq!(set(4, false, b"(", b'<'), Some((0, Set::Latin1)));
        // From level 5 the user-preferred set is ASCII in UTF-8 mode
        // (xterm's HandleUPSS keeps `gsets_upss`), with DECNRCM or not.
        assert_eq!(set(5, false, b"(", b'<'), Some((0, Set::Ascii)));
        assert_eq!(set(5, true, b"(", b'<'), Some((0, Set::Ascii)));
        // 96-sets.
        assert_eq!(set(4, false, b"-", b'A'), Some((1, Set::Latin1)));
        assert_eq!(set(4, true, b"/", b'A'), Some((3, Set::British)));
    }

    #[test]
    fn dec_supplemental_defaults_to_the_character_itself() {
        let mut charsets = Charsets::default();
        charsets.designate(0, Set::DecSupplemental);
        assert_eq!(translated(&mut charsets, "A(W$_"), "A¤Œ\u{2426}_");
    }
}
