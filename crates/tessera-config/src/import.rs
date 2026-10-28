//! Importers for other terminals' colour schemes: iTerm2 `.itermcolors`
//! property lists and base16 YAML schemes (ADR 0019).
//!
//! Both return a [`Theme`]; [`Theme::to_toml`] turns it into a theme file.

use std::cell::Cell;
use std::collections::HashMap;

use crate::load::truncated;
use crate::theme::{Color, DERIVED_SELECTION_ALPHA, Theme, clip_theme_name};

/// Largest input either importer accepts, in bytes. A real scheme is a few
/// kilobytes.
pub const MAX_IMPORT_LEN: usize = 1024 * 1024;

/// Longest piece of input quoted in an [`ImportError`], in bytes.
const MAX_QUOTED_LEN: usize = 64;

/// Why a colour scheme could not be imported.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The input is larger than [`MAX_IMPORT_LEN`].
    #[error("input is {0} bytes, larger than the {MAX_IMPORT_LEN}-byte limit for a colour scheme")]
    TooLarge(usize),
    /// The input is not an XML or binary property list.
    #[error("not a property list: {0}")]
    Plist(#[source] plist::Error),
    /// The property list's root is not a dictionary.
    #[error("the property list's root is not a dictionary")]
    NotADictionary,
    /// The property list holds more values or text than any colour scheme
    /// (a binary plist can repeat one object many times by reference).
    #[error("the property list is too large or too repetitive for a colour scheme")]
    TooComplex,
    /// A required key is absent: a colour slot (`Ansi 7 Color`,
    /// `base0C`) or a colour's component (`Ansi 7 Color / Green Component`).
    #[error("missing key `{0}`")]
    MissingKey(String),
    /// A colour or colour component that is not a dictionary or not a
    /// finite number; the key names the slot and component.
    #[error("`{key}`: not a colour component (expected a finite number)")]
    BadComponent {
        /// The slot, or `slot / component`.
        key: String,
    },
    /// A base16 colour that is not six hex digits.
    #[error("`{key}`: invalid colour {value:?}; expected six hex digits")]
    BadHex {
        /// The base16 key, such as `base00`.
        key: String,
        /// The rejected value, truncated.
        value: String,
    },
    /// YAML this importer does not read (nesting other than `palette:`,
    /// lists, anchors, duplicate keys, tabs) or a scheme system other than
    /// base16.
    #[error("line {line}: {reason}")]
    Unsupported {
        /// 1-based line number.
        line: usize,
        /// What is not supported there.
        reason: String,
    },
}

// --- iTerm2 ---------------------------------------------------------------

/// Generic RGB (what iTerm2's "Calibrated" colours resolve to) gamma:
/// 461/256, as in the macOS profile.
const GENERIC_RGB_GAMMA: f64 = 1.800_781_25;

/// Linear Generic RGB to linear sRGB (D50-adapted, `ColorSync`'s
/// relative-colorimetric path); from the spec fixture, rule R3.
const GENERIC_RGB_TO_SRGB: [[f64; 3]; 3] = [
    [1.025_263_3, -0.026_296_4, 0.001_318_5],
    [0.019_383_0, 0.947_956_5, 0.032_584_1],
    [-0.001_783_5, -0.001_466_4, 1.003_106_1],
];

/// Linear Display P3 to linear sRGB; from the spec fixture, rule R4.
const P3_TO_SRGB: [[f64; 3]; 3] = [
    [1.224_745_0, -0.224_904_3, 0.0],
    [-0.042_057_8, 1.042_081_0, 0.0],
    [-0.019_642_3, -0.078_654_9, 1.098_537_2],
];

/// Reads a colour scheme file for [`import_itermcolors`] or
/// [`import_base16`]: only a regular file of at most [`MAX_IMPORT_LEN`]
/// bytes, opened without blocking on a FIFO and never read past the limit
/// (see [`crate::load_file`]).
pub fn read_scheme_file(path: &std::path::Path) -> Result<Vec<u8>, crate::Error> {
    crate::load::read_capped(
        path,
        u64::try_from(MAX_IMPORT_LEN).unwrap_or(u64::MAX),
        "colour scheme",
    )
}

/// Imports an iTerm2 `.itermcolors` file (XML or binary property list) as
/// a theme called `name` (the file carries no name; callers use its stem),
/// cut to [`crate::MAX_THEME_NAME_LEN`] bytes.
///
/// Reads `Ansi 0 Color` to `Ansi 15 Color`, `Foreground Color` and
/// `Background Color` (all required), and `Cursor Color` and `Selection
/// Color` (a missing cursor is the foreground; a missing selection is the
/// foreground at alpha `0x40`). Each colour has `Red`/`Green`/`Blue
/// Component` numbers in 0..1 (out-of-range values are clamped) and an
/// optional `Alpha Component` (default 1). Its `Color Space` is decoded the
/// way iTerm2 does: `sRGB` is taken as is, `P3` is converted from Display
/// P3, and anything else, including no `Color Space` at all, is Apple
/// Generic RGB (gamma 1.8) and converted. Other keys, including iTerm2's
/// `(Light)`/`(Dark)` variants, are ignored.
pub fn import_itermcolors(bytes: &[u8], name: &str) -> Result<Theme, ImportError> {
    if bytes.len() > MAX_IMPORT_LEN {
        return Err(ImportError::TooLarge(bytes.len()));
    }
    let PlistRoot(dict) = read_plist(bytes)?;
    let slot = |key: &str| {
        dict.get(key)
            .map(|value| decode_colour(key, value))
            .transpose()
    };
    let required = |key: &str| slot(key)?.ok_or_else(|| ImportError::MissingKey(key.to_owned()));
    let mut ansi = [Color::BLACK; 16];
    for (i, colour) in ansi.iter_mut().enumerate() {
        *colour = required(&format!("Ansi {i} Color"))?;
    }
    let foreground = required("Foreground Color")?;
    let background = required("Background Color")?;
    let cursor = slot("Cursor Color")?.unwrap_or(foreground);
    let selection =
        slot("Selection Color")?.unwrap_or_else(|| foreground.with_alpha(DERIVED_SELECTION_ALPHA));
    Ok(Theme {
        name: clip_theme_name(name).to_owned(),
        foreground,
        background,
        cursor,
        selection,
        ansi,
    })
}

/// One `.itermcolors` colour dictionary as an sRGB colour.
fn decode_colour(key: &str, value: &PlistSlot) -> Result<Color, ImportError> {
    let PlistSlot::Colour(colour) = value else {
        return Err(ImportError::BadComponent {
            key: key.to_owned(),
        });
    };
    let component = |name: &str| -> Result<Option<f64>, ImportError> {
        colour
            .numbers
            .get(name)
            .map(|value| {
                value
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| ImportError::BadComponent {
                        key: format!("{key} / {name}"),
                    })
            })
            .transpose()
    };
    let required = |name: &str| {
        component(name)?.ok_or_else(|| ImportError::MissingKey(format!("{key} / {name}")))
    };
    let rgb = [
        required("Red Component")?,
        required("Green Component")?,
        required("Blue Component")?,
    ]
    .map(unit);
    let alpha = component("Alpha Component")?.unwrap_or(1.0);
    // iTerm2 compares the string exactly (`isEqualToString:`), so `srgb`
    // or a non-string is Generic RGB like a missing key.
    let [r, g, b] = match colour.color_space.as_deref() {
        Some("sRGB") => rgb,
        Some("P3") => to_srgb(P3_TO_SRGB, rgb.map(srgb_eotf)),
        _ => to_srgb(GENERIC_RGB_TO_SRGB, rgb.map(|v| v.powf(GENERIC_RGB_GAMMA))),
    };
    Ok(Color {
        r: byte(r),
        g: byte(g),
        b: byte(b),
        a: byte(alpha),
    })
}

/// Most values (scalars, collections and dictionary entries) one
/// `.itermcolors` import visits. A real file has about 25 colour
/// dictionaries of up to six entries.
const MAX_PLIST_VALUES: usize = 4096;

/// Most bytes of strings and data one `.itermcolors` import visits.
const MAX_PLIST_TEXT_BYTES: usize = 256 * 1024;

/// Longest `Color Space` value kept; a longer one is no colour space
/// iTerm2 knows and reads as Generic RGB.
const MAX_COLOR_SPACE_LEN: usize = 32;

/// Why the budgeted reader stopped a property list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The root is not a dictionary.
    NotADictionary,
    /// The work budget ran out.
    TooComplex,
}

/// The work left for the import running on this thread.
#[derive(Debug, Clone, Copy)]
struct Budget {
    values: usize,
    text_bytes: usize,
    refusal: Option<Refusal>,
}

thread_local! {
    /// The budget of the import running on this thread; `None` outside
    /// one. `plist::from_bytes` takes no seed, so the budget the
    /// `Deserialize` impls below share is kept here.
    static BUDGET: Cell<Option<Budget>> = const { Cell::new(None) };
}

/// Parses `bytes` with a fresh budget, mapping a refusal to its error.
fn read_plist(bytes: &[u8]) -> Result<PlistRoot, ImportError> {
    /// Clears the budget even if the parser panics.
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BUDGET.set(None);
        }
    }
    BUDGET.set(Some(Budget {
        values: MAX_PLIST_VALUES,
        text_bytes: MAX_PLIST_TEXT_BYTES,
        refusal: None,
    }));
    let _reset = Reset;
    let result = plist::from_bytes::<PlistRoot>(bytes);
    let refusal = BUDGET.get().and_then(|b| b.refusal);
    match (result, refusal) {
        (_, Some(Refusal::NotADictionary)) => Err(ImportError::NotADictionary),
        (_, Some(Refusal::TooComplex)) => Err(ImportError::TooComplex),
        (Ok(root), None) => Ok(root),
        (Err(error), None) => Err(ImportError::Plist(error)),
    }
}

/// Spends one value and `text` bytes of the budget, or records why the
/// parse must stop and returns the error that stops it.
fn charge<E: serde::de::Error>(text: usize) -> Result<(), E> {
    let Some(mut budget) = BUDGET.get() else {
        return Err(E::custom("property list read outside an import"));
    };
    if budget.values == 0 || budget.text_bytes < text {
        return Err(refuse(Refusal::TooComplex));
    }
    budget.values -= 1;
    budget.text_bytes -= text;
    BUDGET.set(Some(budget));
    Ok(())
}

/// Records `refusal` and returns the error that stops the parse.
fn refuse<E: serde::de::Error>(refusal: Refusal) -> E {
    if let Some(mut budget) = BUDGET.get() {
        budget.refusal.get_or_insert(refusal);
        BUDGET.set(Some(budget));
    }
    E::custom(match refusal {
        Refusal::NotADictionary => "the root is not a dictionary",
        Refusal::TooComplex => "too large or too repetitive for a colour scheme",
    })
}

/// The root of a property list as an `.itermcolors` file has it: a
/// dictionary of slots.
///
/// The model is as deep as such a file and no deeper (root dictionary,
/// colour dictionary, scalar), every value and dictionary entry is
/// charged to a per-import budget ([`MAX_PLIST_VALUES`],
/// [`MAX_PLIST_TEXT_BYTES`]), and collections a colour scheme never
/// contains are errors rather than drained. A hostile file can therefore
/// neither recurse deeply (`plist::Value` overflows the stack on tens of
/// thousands of nested arrays) nor amplify: a binary plist may reference
/// one object from many places, so a few kilobytes can otherwise describe
/// a cube of shared arrays or a big string repeated thousands of times.
#[derive(Debug)]
struct PlistRoot(HashMap<String, PlistSlot>);

/// A value of the root dictionary: a colour dictionary, or a scalar.
#[derive(Debug)]
enum PlistSlot {
    Colour(PlistColour),
    Other,
}

/// A colour dictionary's components and colour space.
#[derive(Debug, Default)]
struct PlistColour {
    /// Entries other than `Color Space` by key (`Red Component`, ...):
    /// the number, or `None` when the value is not one.
    numbers: HashMap<String, Option<f64>>,
    /// `Color Space`, when it is a string of at most
    /// [`MAX_COLOR_SPACE_LEN`] bytes.
    color_space: Option<String>,
}

/// A dictionary key, kept when short enough to name a slot or component.
enum PlistKey {
    Short(String),
    Long,
}

/// A scalar inside a colour dictionary: a number or something else.
struct PlistNumber(Option<f64>);

/// A `Color Space` value: a short string or something else.
struct PlistSpace(Option<String>);

/// Implements the scalar `visit_*` methods of a visitor, charging each to
/// the budget: `$number` maps a float to the value, `$string` a `&str`, and
/// everything else is `$other`.
macro_rules! budgeted_scalars {
    ($value:ty, $other:expr, $number:expr, $string:expr) => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<$value, E> {
            charge(0).map(|()| $other)
        }
        #[expect(clippy::cast_precision_loss, reason = "components are clamped to 0..1")]
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<$value, E> {
            charge(0).map(|()| $number(v as f64))
        }
        #[expect(clippy::cast_precision_loss, reason = "components are clamped to 0..1")]
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<$value, E> {
            charge(0).map(|()| $number(v as f64))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<$value, E> {
            charge(0).map(|()| $number(v))
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<$value, E> {
            charge(v.len()).map(|()| $string(v))
        }
        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<$value, E> {
            charge(v.len()).map(|()| $other)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<$value, E> {
            charge(0).map(|()| $other)
        }
    };
}

/// Implements the scalar `visit_*` methods of the root's visitor: a
/// scalar root is not a dictionary.
macro_rules! refusing_scalars {
    ($value:ty) => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
        fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<$value, E> {
            Err(refuse(Refusal::NotADictionary))
        }
    };
}

/// Implements `Deserialize` for a plist model type with a visitor whose
/// body is `$($visitor)*` (and whose `expecting` says `$expecting`).
macro_rules! plist_model {
    ($ty:ty, $expecting:literal, $($visitor:tt)*) => {
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str($expecting)
                    }
                    $($visitor)*
                }
                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

plist_model!(
    PlistRoot,
    "a dictionary of colours",
    refusing_scalars!(PlistRoot);
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<PlistRoot, A::Error> {
        Err(refuse(Refusal::NotADictionary))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<PlistRoot, A::Error> {
        charge(0)?;
        let mut slots = HashMap::new();
        while let Some(key) = map.next_key::<PlistKey>()? {
            let value = map.next_value::<PlistSlot>()?;
            if let PlistKey::Short(key) = key {
                // A repeated key keeps the last value, as
                // `plist::Dictionary` does.
                slots.insert(key, value);
            }
        }
        Ok(PlistRoot(slots))
    }
);

plist_model!(
    PlistSlot,
    "a colour dictionary",
    budgeted_scalars!(PlistSlot, PlistSlot::Other, |_| PlistSlot::Other, |_| {
        PlistSlot::Other
    });
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<PlistSlot, A::Error> {
        Err(serde::de::Error::custom("a colour scheme holds no arrays"))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<PlistSlot, A::Error> {
        charge(0)?;
        let mut colour = PlistColour::default();
        while let Some(key) = map.next_key::<PlistKey>()? {
            match key {
                PlistKey::Short(key) if key == "Color Space" => {
                    colour.color_space = map.next_value::<PlistSpace>()?.0;
                }
                PlistKey::Short(key) => {
                    let value = map.next_value::<PlistNumber>()?.0;
                    colour.numbers.insert(key, value);
                }
                PlistKey::Long => {
                    map.next_value::<PlistNumber>()?;
                }
            }
        }
        Ok(PlistSlot::Colour(colour))
    }
);

plist_model!(
    PlistKey,
    "a dictionary key",
    budgeted_scalars!(PlistKey, PlistKey::Long, |_| PlistKey::Long, |v: &str| {
        if v.len() <= 64 {
            PlistKey::Short(v.to_owned())
        } else {
            PlistKey::Long
        }
    });
);

plist_model!(
    PlistNumber,
    "a colour component",
    budgeted_scalars!(PlistNumber, PlistNumber(None), |v| PlistNumber(Some(v)), |_| {
        PlistNumber(None)
    });
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<PlistNumber, A::Error> {
        Err(serde::de::Error::custom("a colour component is not a collection"))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<PlistNumber, A::Error> {
        Err(serde::de::Error::custom("a colour component is not a collection"))
    }
);

plist_model!(
    PlistSpace,
    "a colour space name",
    budgeted_scalars!(PlistSpace, PlistSpace(None), |_| PlistSpace(None), |v: &str| {
        PlistSpace((v.len() <= MAX_COLOR_SPACE_LEN).then(|| v.to_owned()))
    });
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<PlistSpace, A::Error> {
        Err(serde::de::Error::custom("a colour space is not a collection"))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, _: A) -> Result<PlistSpace, A::Error> {
        Err(serde::de::Error::custom("a colour space is not a collection"))
    }
);

/// `v` clamped to 0..=1 (`NaN` to 0).
fn unit(v: f64) -> f64 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// An encoded 0..=1 value as a byte, rounding half up:
/// `floor(v * 255 + 0.5)` after clamping.
fn byte(v: f64) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255.5 before the floor"
    )]
    let b = (unit(v) * 255.0 + 0.5).floor() as u8;
    b
}

/// The sRGB transfer function: encoded to linear.
fn srgb_eotf(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The inverse sRGB transfer function: linear to encoded.
fn srgb_oetf(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Linear `rgb` through `matrix` into linear sRGB, clamped (out-of-gamut
/// colours saturate) and encoded.
fn to_srgb(matrix: [[f64; 3]; 3], rgb: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| srgb_oetf(unit(row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])))
}

// --- base16 ---------------------------------------------------------------

/// The base16 keys, in order.
const BASE16_KEYS: [&str; 16] = [
    "base00", "base01", "base02", "base03", "base04", "base05", "base06", "base07", "base08",
    "base09", "base0A", "base0B", "base0C", "base0D", "base0E", "base0F",
];

/// The base16 colour (index into [`BASE16_KEYS`]) of each ANSI colour, as
/// every base16 terminal template maps them (tinted-shell).
const BASE16_ANSI: [usize; 16] = [
    0x0, 0x8, 0xB, 0xA, 0xD, 0xE, 0xC, 0x5, 0x3, 0x8, 0xB, 0xA, 0xD, 0xE, 0xC, 0x7,
];

/// Imports a base16 scheme as a theme.
///
/// Both published formats are read: the legacy flat one (`scheme:`,
/// `author:`, then `base00:` .. `base0F:` with six hex digits, usually
/// quoted, no `#`) and the current one (`system: "base16"`, `name:`, then
/// the sixteen colours indented under `palette:`, with `#`). Values may be
/// bare, single- or double-quoted, with an optional `#`; `# comments`
/// (a `#` outside quotes at the start of a line or after whitespace) are
/// ignored. A block scalar (`|` or `>`, as in a multi-line `description:`)
/// is skipped when its key is one the importer ignores. Anything else YAML
/// can express (other nesting, lists, anchors, flow collections, duplicate
/// keys, tab indentation, a block scalar for a key the importer reads) is
/// [`ImportError::Unsupported`], as is a `system:` other than `base16`.
///
/// The theme is named after `name:`, else `scheme:` (either cut to
/// [`crate::MAX_THEME_NAME_LEN`] bytes), else `default_name`.
/// Colours map as the base16 terminal templates do: ANSI 0-7 are base00,
/// 08, 0B, 0A, 0D, 0E, 0C, 05; ANSI 8-15 are base03, 08, 0B, 0A, 0D, 0E,
/// 0C, 07; foreground and cursor base05, background base00, selection
/// base02.
pub fn import_base16(text: &str, default_name: &str) -> Result<Theme, ImportError> {
    if text.len() > MAX_IMPORT_LEN {
        return Err(ImportError::TooLarge(text.len()));
    }
    let scheme = parse_base16(text)?;
    let mut base = [Color::BLACK; 16];
    for (colour, key) in base.iter_mut().zip(BASE16_KEYS) {
        let value = scheme
            .colours
            .get(key)
            .ok_or_else(|| ImportError::MissingKey(key.to_owned()))?;
        *colour = parse_base16_hex(key, value)?;
    }
    Ok(Theme {
        name: scheme
            .name
            .or(scheme.scheme)
            .filter(|n| !n.trim().is_empty())
            .map_or_else(
                || default_name.to_owned(),
                |name| clip_theme_name(&name).to_owned(),
            ),
        foreground: base[0x5],
        background: base[0x0],
        cursor: base[0x5],
        selection: base[0x2],
        ansi: BASE16_ANSI.map(|i| base[i]),
    })
}

/// What [`parse_base16`] keeps of a scheme.
#[derive(Debug, Default)]
struct Base16Scheme {
    name: Option<String>,
    scheme: Option<String>,
    /// `baseXX` values (top level or under `palette:`), unparsed.
    colours: HashMap<&'static str, String>,
}

/// Where the line being read sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// At the top level.
    Top,
    /// Inside `palette:`, whose children are indented by `Some(n)` spaces
    /// (`None` until the first child).
    Palette(Option<usize>),
    /// Inside another top-level key with no value: any child is
    /// unsupported.
    Other,
}

/// Reads the scalar keys of a base16 file; see [`import_base16`].
fn parse_base16(text: &str) -> Result<Base16Scheme, ImportError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut scheme = Base16Scheme::default();
    let mut scope = Scope::Top;
    let mut top_keys: HashMap<String, usize> = HashMap::new();
    let mut palette_keys: HashMap<String, usize> = HashMap::new();
    // The indentation of the key whose block scalar (`|`, `>`) is being
    // skipped: its content is every following line that is blank or
    // indented further.
    let mut block: Option<usize> = None;
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        let unsupported = |reason: String| ImportError::Unsupported { line, reason };
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(owner) = block {
            if continues_block(raw, owner) {
                continue;
            }
            block = None;
        }
        let content = strip_comment(raw).trim_end();
        if content.trim_start().is_empty() {
            continue;
        }
        let body = content.trim_start_matches(' ');
        let indent = content.len() - body.len();
        if body.starts_with('\t') {
            return Err(unsupported("tab indentation".into()));
        }
        if indent == 0 && matches!(body, "---" | "...") {
            continue;
        }
        if indent == 0 {
            scope = Scope::Top;
        } else {
            match scope {
                Scope::Top | Scope::Other => {
                    return Err(unsupported(format!(
                        "nested value {} is not supported (only `palette:` may nest)",
                        quote(body)
                    )));
                }
                Scope::Palette(None) => scope = Scope::Palette(Some(indent)),
                Scope::Palette(Some(expected)) if expected != indent => {
                    return Err(unsupported(format!(
                        "{} is indented differently from the other `palette:` keys \
                         (nesting deeper than `palette:` is not supported)",
                        quote(body)
                    )));
                }
                Scope::Palette(Some(_)) => {}
            }
        }
        let (key, value) = split_key_value(body)
            .ok_or_else(|| unsupported(format!("expected `key: value`, found {}", quote(body))))?;
        let keys = if indent == 0 {
            &mut top_keys
        } else {
            &mut palette_keys
        };
        if let Some(first) = keys.insert(key.to_owned(), line) {
            return Err(unsupported(format!(
                "duplicate key `{}` (first on line {first})",
                truncated(key, MAX_QUOTED_LEN)
            )));
        }
        let colour_key = BASE16_KEYS.into_iter().find(|k| *k == key);
        if is_block_scalar_header(value) {
            // Only keys this importer ignores (`description:`, ...) may
            // hold a block scalar; its lines are skipped unread.
            if colour_key.is_some() || matches!(key, "name" | "scheme" | "system" | "palette") {
                return Err(unsupported(format!(
                    "a block scalar for `{key}` is not supported (write the value on one line)"
                )));
            }
            block = Some(indent);
            continue;
        }
        let value = scalar(value).map_err(unsupported)?;
        if indent == 0 && value.is_none() && colour_key.is_none() {
            scope = if key == "palette" {
                Scope::Palette(None)
            } else {
                Scope::Other
            };
            continue;
        }
        let value = value.unwrap_or_default();
        if indent == 0 {
            read_top_level(&mut scheme, key, &value).map_err(unsupported)?;
        }
        if indent == 0 && value.is_empty() {
            // `base00:` with nothing (or only a comment, as in
            // `base00: #282a36`) after it: an empty colour, and nothing
            // may nest under it.
            scope = Scope::Other;
        }
        if let Some(key) = colour_key
            && scheme.colours.insert(key, value).is_some()
        {
            return Err(unsupported(format!(
                "`{key}` is given both at the top level and under `palette:`"
            )));
        }
    }
    Ok(scheme)
}

/// Whether `raw` belongs to a block scalar whose key is indented by
/// `owner` spaces: it is blank or indented further.
fn continues_block(raw: &str, owner: usize) -> bool {
    let lead = raw.len() - raw.trim_start_matches([' ', '\t']).len();
    raw.trim().is_empty() || lead > owner
}

/// Applies the top-level scalar `key: value` to `scheme`: `name`,
/// `scheme` and a `system` that must be `base16`; other keys are ignored.
fn read_top_level(scheme: &mut Base16Scheme, key: &str, value: &str) -> Result<(), String> {
    match key {
        "name" => scheme.name = Some(value.to_owned()),
        "scheme" => scheme.scheme = Some(value.to_owned()),
        "system" if value != "base16" => {
            return Err(format!(
                "scheme system {} is not supported (only base16)",
                quote(value)
            ));
        }
        _ => {}
    }
    Ok(())
}

/// `line` without its `# comment`: a `#` outside quotes that starts the
/// line or follows whitespace, as YAML defines it. A quote opens a quoted
/// scalar only where one can start (at the start or after whitespace), so
/// the apostrophe of a bare `Bob's` does not hide a comment.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut previous: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            None if (c == '"' || c == '\'') && previous.is_none_or(char::is_whitespace) => {
                quote = Some(c);
            }
            None if c == '#' && previous.is_none_or(char::is_whitespace) => {
                return &line[..i];
            }
            Some(_) | None => {}
        }
        previous = Some(c);
    }
    line
}

/// Splits `key: value` (or `key:` with nothing after it) at the first
/// colon followed by a space or the end. Keys are plain identifiers.
fn split_key_value(body: &str) -> Option<(&str, &str)> {
    let colon = body
        .char_indices()
        .find(|&(i, c)| c == ':' && body[i + 1..].chars().next().is_none_or(|n| n == ' '))?
        .0;
    let key = &body[..colon];
    let plain = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    plain.then(|| (key, body[colon + 1..].trim()))
}

/// Whether `value` starts a YAML block scalar: `|` or `>`, optionally with
/// a chomping indicator (`+`, `-`) and an indentation digit in either
/// order.
fn is_block_scalar_header(value: &str) -> bool {
    let mut chars = value.chars();
    if !matches!(chars.next(), Some('|' | '>')) {
        return false;
    }
    let rest = chars.as_str();
    rest.chars().all(|c| matches!(c, '+' | '-' | '1'..='9'))
        && rest.chars().filter(|c| matches!(c, '+' | '-')).count() <= 1
        && rest.chars().filter(char::is_ascii_digit).count() <= 1
}

/// A YAML scalar's text: unquoted, `'single'` (with `''` for a quote) or
/// `"double"` (with `\"` and `\\`). `None` for an empty value. Flow
/// collections, anchors, aliases and tags are errors (block scalars are
/// handled, and skipped, by [`parse_base16`]).
fn scalar(value: &str) -> Result<Option<String>, String> {
    let Some(first) = value.chars().next() else {
        return Ok(None);
    };
    match first {
        '"' => {
            let mut out = String::new();
            let mut chars = value[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => return rest_is_empty(chars.as_str()).map(|()| Some(out)),
                    '\\' => match chars.next() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('t') => out.push('\t'),
                        Some(other) => {
                            return Err(format!("escape `\\{other}` is not supported"));
                        }
                        None => break,
                    },
                    c => out.push(c),
                }
            }
            Err("unterminated double-quoted string".into())
        }
        '\'' => {
            let mut out = String::new();
            let mut rest = &value[1..];
            loop {
                let Some(end) = rest.find('\'') else {
                    return Err("unterminated single-quoted string".into());
                };
                out.push_str(&rest[..end]);
                rest = &rest[end + 1..];
                if let Some(after) = rest.strip_prefix('\'') {
                    out.push('\'');
                    rest = after;
                } else {
                    return rest_is_empty(rest).map(|()| Some(out));
                }
            }
        }
        '[' | '{' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`' => Err(format!(
            "value {} is not supported (plain scalars only)",
            quote(value)
        )),
        '-' | '?' if value.len() == 1 || value[1..].starts_with(' ') => Err(format!(
            "value {} is not supported (plain scalars only)",
            quote(value)
        )),
        _ => Ok(Some(value.to_owned())),
    }
}

/// Nothing but whitespace may follow a closing quote.
fn rest_is_empty(rest: &str) -> Result<(), String> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unexpected {} after a quoted value",
            quote(rest.trim())
        ))
    }
}

/// A base16 hex value: optional `#`, six hex digits, either case.
fn parse_base16_hex(key: &str, value: &str) -> Result<Color, ImportError> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    Color::from_hex(&format!("#{digits}"))
        .filter(|_| digits.len() == 6)
        .ok_or_else(|| ImportError::BadHex {
            key: key.to_owned(),
            value: truncated(value, MAX_QUOTED_LEN),
        })
}

/// `text` in backticks, truncated for an error message.
fn quote(text: &str) -> String {
    format!("`{}`", truncated(text, MAX_QUOTED_LEN))
}
