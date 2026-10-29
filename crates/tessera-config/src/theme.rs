//! Colour themes: the `themes/*.toml` file format, the bundled themes and
//! the lookup behind `appearance.theme` (ADR 0019).
//!
//! A theme file is flat TOML; the file *is* the theme:
//!
//! ```toml
//! name = "Dracula"            # optional in a file; defaults to the file stem
//! foreground = "#f8f8f2"
//! background = "#282a36"
//! cursor = "#f8f8f2"          # optional; defaults to the foreground
//! selection = "#44475a"       # optional; defaults to the foreground at alpha 0x40
//! search_match = "#ffd86699"  # optional; defaults to the renderer's (#ffd866 at alpha 0.6)
//! search_match_current = "#ff9f43cc"  # optional; defaults to #ff9f43 at alpha 0.8
//! ansi = ["#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
//!         "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff"]
//! ```
//!
//! Colours are `#rrggbb` or `#rrggbbaa` sRGB strings (hex digits in either
//! case). Unknown keys are ignored, so a theme written by a newer version
//! still loads.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::load::{
    MAX_PARSE_ERROR_LEN, MAX_WARNING_PATH_LEN, ParseError, Warning, read_capped, truncated,
};

/// Largest theme file that is read, in bytes. A real theme is about 1 KiB;
/// a larger file is reported as an I/O error of kind `InvalidInput`.
pub const MAX_THEME_FILE_LEN: u64 = 64 * 1024;

/// Most `*.toml` files of the themes directory that one lookup reads, by
/// stem and by `name` key together (so also the most
/// [`Warning::ThemeInvalid`]s one lookup reports). Files past this, in
/// file-name order, are not considered.
pub const MAX_THEME_FILES: usize = 256;

/// Most directory entries one lookup lists. A themes directory with more
/// entries is searched only among the first this many the system returns.
pub const MAX_THEME_DIR_ENTRIES: usize = 4096;

/// Longest theme name the importers produce, in bytes; see
/// [`clip_theme_name`].
pub const MAX_THEME_NAME_LEN: usize = 256;

/// `name` cut to at most [`MAX_THEME_NAME_LEN`] bytes on a character
/// boundary, so a theme file written for it stays far below
/// [`MAX_THEME_FILE_LEN`] even with every character escaped.
#[must_use]
pub fn clip_theme_name(name: &str) -> &str {
    if name.len() <= MAX_THEME_NAME_LEN {
        return name;
    }
    let mut cut = MAX_THEME_NAME_LEN;
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    &name[..cut]
}

/// Longest [`slug`], in bytes.
pub const MAX_SLUG_LEN: usize = 64;

/// Alpha of a selection derived from the foreground when a theme or an
/// imported scheme has no selection colour (`0x40`, a quarter).
pub const DERIVED_SELECTION_ALPHA: u8 = 0x40;

/// Longest invalid value quoted in a [`ThemeError::InvalidColor`], in
/// bytes.
const MAX_QUOTED_VALUE_LEN: usize = 64;

/// An sRGB colour with straight (not premultiplied) alpha, written
/// `#rrggbb` or `#rrggbbaa` in theme files.
///
/// Serialises to lowercase hex, without the alpha digits when the colour
/// is opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    /// Red, sRGB-encoded.
    pub r: u8,
    /// Green, sRGB-encoded.
    pub g: u8,
    /// Blue, sRGB-encoded.
    pub b: u8,
    /// Alpha, `0xff` opaque.
    pub a: u8,
}

impl Color {
    /// Opaque black.
    pub const BLACK: Self = Self::rgb(0, 0, 0);

    /// An opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xff }
    }

    /// The same colour with alpha `a`.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Parses `#rrggbb` or `#rrggbbaa` (hex digits in either case).
    ///
    /// Nothing else is accepted: no surrounding whitespace, no missing
    /// `#`, no `#rgb` shorthand and no colour names.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?.as_bytes();
        if !matches!(hex.len(), 6 | 8) || !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let byte = |i: usize| (hex_digit(hex[i]) << 4) | hex_digit(hex[i + 1]);
        let a = if hex.len() == 8 { byte(6) } else { 0xff };
        Some(Self {
            r: byte(0),
            g: byte(2),
            b: byte(4),
            a,
        })
    }
}

/// The value of one ASCII hex digit (the caller has checked it is one).
fn hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    }
}

impl fmt::Display for Color {
    /// `#rrggbb` when opaque, else `#rrggbbaa`; always lowercase.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)?;
        if self.a != 0xff {
            write!(f, "{:02x}", self.a)?;
        }
        Ok(())
    }
}

/// The error of parsing a [`Color`] from a string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid colour {value:?}; expected `#rrggbb` or `#rrggbbaa`")]
pub struct InvalidColor {
    /// The rejected text, truncated to 64 bytes.
    pub value: String,
}

impl FromStr for Color {
    type Err = InvalidColor;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_hex(text).ok_or_else(|| InvalidColor {
            value: truncated(text, MAX_QUOTED_VALUE_LEN),
        })
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Why a theme file could not be loaded or parsed.
///
/// Every variant about a value names its key (`foreground`, `ansi[3]`).
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum ThemeError {
    /// The file could not be read, is not UTF-8, is larger than
    /// [`MAX_THEME_FILE_LEN`] or is not valid TOML.
    #[error(transparent)]
    Load(#[from] crate::Error),
    /// A required key is absent.
    #[error("missing key `{0}`")]
    MissingKey(&'static str),
    /// A key holds a value of the wrong TOML type.
    #[error("`{key}`: expected {expected}, found {found}")]
    WrongType {
        /// The key, such as `foreground` or `ansi[3]`.
        key: String,
        /// What the key must hold.
        expected: &'static str,
        /// The TOML type found instead.
        found: &'static str,
    },
    /// A colour string that is not `#rrggbb` or `#rrggbbaa`.
    #[error("`{key}`: {error}")]
    InvalidColor {
        /// The key, such as `foreground` or `ansi[3]`.
        key: String,
        /// The rejected value.
        #[source]
        error: InvalidColor,
    },
    /// `ansi` does not hold exactly 16 colours.
    #[error("`ansi`: expected 16 colours, found {0}")]
    AnsiLength(usize),
}

impl From<ParseError> for ThemeError {
    fn from(error: ParseError) -> Self {
        Self::Load(crate::Error::Parse(error))
    }
}

/// A colour theme: the default colours and the 16 ANSI colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Display name, such as `Solarized Dark`.
    pub name: String,
    /// Default text colour.
    pub foreground: Color,
    /// Default background. Alpha below `0xff` makes it translucent (and is
    /// multiplied by `appearance.opacity`).
    pub background: Color,
    /// Cursor colour.
    pub cursor: Color,
    /// Selection highlight; its alpha is the highlight's opacity.
    pub selection: Color,
    /// Search match highlight, or `None` for the renderer's default
    /// (`#ffd866` at alpha 0.6). Its alpha is the highlight's opacity.
    pub search_match: Option<Color>,
    /// The current search match's highlight, or `None` for the renderer's
    /// default (`#ff9f43` at alpha 0.8).
    pub search_match_current: Option<Color>,
    /// ANSI colours 0-15: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright variants.
    pub ansi: [Color; 16],
}

/// The bundled themes' names in their documented order, with their files.
const BUNDLED_SOURCES: [(&str, &str); 10] = [
    ("Tessera Dark", include_str!("../themes/tessera-dark.toml")),
    (
        "Tessera Light",
        include_str!("../themes/tessera-light.toml"),
    ),
    (
        "Solarized Dark",
        include_str!("../themes/solarized-dark.toml"),
    ),
    (
        "Solarized Light",
        include_str!("../themes/solarized-light.toml"),
    ),
    ("Dracula", include_str!("../themes/dracula.toml")),
    ("Nord", include_str!("../themes/nord.toml")),
    ("Gruvbox Dark", include_str!("../themes/gruvbox-dark.toml")),
    ("One Dark", include_str!("../themes/one-dark.toml")),
    ("Tokyo Night", include_str!("../themes/tokyo-night.toml")),
    (
        "Catppuccin Mocha",
        include_str!("../themes/catppuccin-mocha.toml"),
    ),
];

/// [`BUNDLED_SOURCES`]' names.
const BUNDLED_NAMES: [&str; 10] = {
    let mut names = [""; 10];
    let mut i = 0;
    while i < names.len() {
        names[i] = BUNDLED_SOURCES[i].0;
        i += 1;
    }
    names
};

/// The bundled themes, parsed once. A test parses every source, so the
/// `expect` cannot fire in a build that passed its tests.
static BUNDLED: LazyLock<Vec<Theme>> = LazyLock::new(|| {
    BUNDLED_SOURCES
        .iter()
        .map(|(name, text)| {
            Theme::parse(text).unwrap_or_else(|e| panic!("bundled theme {name} is invalid: {e}"))
        })
        .collect()
});

impl Theme {
    /// The theme used when `appearance.theme` matches nothing, and the
    /// configuration default.
    pub const DEFAULT_NAME: &'static str = "Tessera Dark";

    /// Parses a theme file's text. The `name` key is required here (there
    /// is no file stem to default it to); see [`Theme::load`].
    pub fn parse(text: &str) -> Result<Self, ThemeError> {
        Self::parse_named(text, None)
    }

    /// Reads and parses the theme file at `path`. A file without a `name`
    /// key (or with an empty one) is named after its file stem.
    ///
    /// Only a regular file of at most [`MAX_THEME_FILE_LEN`] bytes is read
    /// (a FIFO cannot block the call; see [`crate::load_file`]).
    pub fn load(path: &Path) -> Result<Self, ThemeError> {
        let bytes = read_capped(path, MAX_THEME_FILE_LEN, "theme file")?;
        let text = std::str::from_utf8(&bytes).map_err(crate::Error::from)?;
        let stem = path.file_stem().map(|s| s.to_string_lossy());
        Self::parse_named(text, stem.as_deref())
    }

    /// Parses `text`, naming the theme `default_name` when it has no
    /// non-empty `name` key.
    fn parse_named(text: &str, default_name: Option<&str>) -> Result<Self, ThemeError> {
        let table: toml::Table = toml::from_str(text).map_err(ParseError::from)?;
        let color = |key: &'static str| -> Result<Option<Color>, ThemeError> {
            table
                .get(key)
                .map(|value| color_value(key, value))
                .transpose()
        };
        let foreground = color("foreground")?.ok_or(ThemeError::MissingKey("foreground"))?;
        let background = color("background")?.ok_or(ThemeError::MissingKey("background"))?;
        let cursor = color("cursor")?.unwrap_or(foreground);
        let selection =
            color("selection")?.unwrap_or_else(|| foreground.with_alpha(DERIVED_SELECTION_ALPHA));
        let search_match = color("search_match")?;
        let search_match_current = color("search_match_current")?;
        let ansi = match table.get("ansi") {
            None => return Err(ThemeError::MissingKey("ansi")),
            Some(toml::Value::Array(items)) => {
                if items.len() != 16 {
                    return Err(ThemeError::AnsiLength(items.len()));
                }
                let mut ansi = [Color::BLACK; 16];
                for (i, (slot, item)) in ansi.iter_mut().zip(items).enumerate() {
                    *slot = color_value(&format!("ansi[{i}]"), item)?;
                }
                ansi
            }
            Some(other) => {
                return Err(ThemeError::WrongType {
                    key: "ansi".into(),
                    expected: "an array of 16 colours",
                    found: other.type_str(),
                });
            }
        };
        let name = match table.get("name") {
            None => None,
            Some(toml::Value::String(name)) => Some(name.as_str()),
            Some(other) => {
                return Err(ThemeError::WrongType {
                    key: "name".into(),
                    expected: "a string",
                    found: other.type_str(),
                });
            }
        };
        let name = name
            .filter(|name| !name.trim().is_empty())
            .or(default_name)
            .ok_or(ThemeError::MissingKey("name"))?
            .to_owned();
        Ok(Self {
            name,
            foreground,
            background,
            cursor,
            selection,
            search_match,
            search_match_current,
            ansi,
        })
    }

    /// The theme as a theme file, in the shape [`Theme::parse`] reads
    /// back to an equal theme: lowercase colours, the alpha digits only
    /// when a colour is not opaque, the ANSI colours eight to a line.
    #[must_use]
    pub fn to_toml(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        // `Value`'s display quotes and escapes the name as TOML requires.
        let _ = writeln!(out, "name = {}", toml::Value::String(self.name.clone()));
        let _ = writeln!(out, "foreground = \"{}\"", self.foreground);
        let _ = writeln!(out, "background = \"{}\"", self.background);
        let _ = writeln!(out, "cursor = \"{}\"", self.cursor);
        let _ = writeln!(out, "selection = \"{}\"", self.selection);
        if let Some(color) = self.search_match {
            let _ = writeln!(out, "search_match = \"{color}\"");
        }
        if let Some(color) = self.search_match_current {
            let _ = writeln!(out, "search_match_current = \"{color}\"");
        }
        out.push_str("ansi = [\n");
        for row in self.ansi.chunks(8) {
            let row: Vec<String> = row.iter().map(|c| format!("\"{c}\"")).collect();
            let _ = writeln!(out, "    {},", row.join(", "));
        }
        out.push_str("]\n");
        out
    }

    /// The bundled theme called `name`, compared case-insensitively and by
    /// [`slug`] (so `solarized-dark` finds `Solarized Dark`).
    #[must_use]
    pub fn bundled(name: &str) -> Option<&'static Self> {
        BUNDLED.iter().find(|theme| names_match(name, &theme.name))
    }

    /// The bundled themes' names, in a fixed order with
    /// [`Theme::DEFAULT_NAME`] first.
    #[must_use]
    pub fn bundled_names() -> &'static [&'static str] {
        &BUNDLED_NAMES
    }

    /// Resolves the theme `name` (the value of `appearance.theme`) the way
    /// [`crate::ConfigStore::theme`] does:
    ///
    /// 1. a `*.toml` file in `themes_dir` whose file stem matches `name`
    ///    (files in file-name order; hidden files are skipped);
    /// 2. a `*.toml` file there whose `name` key matches;
    /// 3. a bundled theme ([`Theme::bundled`]);
    /// 4. [`Theme::DEFAULT_NAME`], with a [`Warning::ThemeNotFound`].
    ///
    /// Names match case-insensitively or by [`slug`]. A user file thus
    /// shadows a bundled theme of the same name. A file that matches by
    /// stem but does not load adds a [`Warning::ThemeInvalid`] and the
    /// search goes on. `name` is only ever compared with names, never
    /// joined onto a path, so `../config` cannot reach outside
    /// `themes_dir`. An empty (or blank) name, or one longer than
    /// [`MAX_THEME_NAME_LEN`] bytes, is not looked up and gives the default
    /// with a warning. Never fails and never panics; a missing or
    /// unreadable directory just has no user themes.
    #[must_use]
    pub fn resolve(themes_dir: &Path, name: &str) -> ResolvedTheme {
        let mut warnings = Vec::new();
        // A name longer than any imported theme's (the importers clip names
        // to this length) is not compared at all: doing so against every
        // file would cost time in proportion to its length. A hand-written
        // theme with a longer `name` is still found by its file stem or by
        // a name within the cap that has the same slug.
        if is_lookup_name(name) && name.len() <= MAX_THEME_NAME_LEN {
            if let Some((theme, path)) = find_user_theme(themes_dir, name, &mut warnings) {
                return ResolvedTheme {
                    theme,
                    source: ThemeSource::User(path),
                    warnings,
                };
            }
            if let Some(theme) = Self::bundled(name) {
                return ResolvedTheme {
                    theme: theme.clone(),
                    source: ThemeSource::Bundled,
                    warnings,
                };
            }
        }
        warnings.push(Warning::ThemeNotFound {
            name: truncated(name, MAX_WARNING_PATH_LEN),
        });
        ResolvedTheme {
            theme: Self::default(),
            source: ThemeSource::Default,
            warnings,
        }
    }
}

impl Default for Theme {
    /// The bundled [`Theme::DEFAULT_NAME`] theme.
    fn default() -> Self {
        BUNDLED[0].clone()
    }
}

/// The outcome of [`Theme::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTheme {
    /// The theme to draw with.
    pub theme: Theme,
    /// Where it came from.
    pub source: ThemeSource,
    /// Problems met on the way: user files that did not load, and
    /// [`Warning::ThemeNotFound`] when the default was used.
    pub warnings: Vec<Warning>,
}

/// Where a [`ResolvedTheme`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    /// A user file in the themes directory.
    User(PathBuf),
    /// A bundled theme.
    Bundled,
    /// Nothing matched; this is the default theme.
    Default,
}

/// `name` reduced to a file-name-safe identifier: letters and digits (any
/// script) lowercased, each `+` spelled out as the word `plus`, every run
/// of other characters replaced by one `-`, no `-` at either end, at most
/// [`MAX_SLUG_LEN`] bytes. `None` when nothing is left.
///
/// `Solarized Dark` becomes `solarized-dark`, `Rosé Pine` `rosé-pine`,
/// `Dracula+` `dracula-plus` (so it does not shadow Dracula) and
/// `../../etc/passwd` `etc-passwd`, so a slug never names a hidden file or
/// a path. Unicode is not normalised: a name typed in NFC (`é` as one
/// character) and the same name in NFD (`e` plus a combining accent, which
/// is not a letter and so separates) have different slugs; each matches
/// its own spelling.
#[must_use]
pub fn slug(name: &str) -> Option<String> {
    /// Appends `piece`, after a `-` when `separated`; `false` when it
    /// would make the slug longer than [`MAX_SLUG_LEN`].
    fn push(out: &mut String, piece: &str, separated: bool) -> bool {
        let dash = separated && !out.is_empty();
        if out.len() + usize::from(dash) + piece.len() > MAX_SLUG_LEN {
            return false;
        }
        if dash {
            out.push('-');
        }
        out.push_str(piece);
        true
    }

    let mut out = String::new();
    let mut separated = false;
    for c in name.chars() {
        if c == '+' {
            if !push(&mut out, "plus", true) {
                break;
            }
            separated = true;
            continue;
        }
        // Lowercasing can yield marks (`İ` is `i` plus a combining dot);
        // keeping only letters and digits makes `slug` idempotent.
        let lower: String = c.to_lowercase().filter(|l| l.is_alphanumeric()).collect();
        if !c.is_alphanumeric() || lower.is_empty() {
            separated = true;
            continue;
        }
        if !push(&mut out, &lower, separated) {
            break;
        }
        separated = false;
    }
    (!out.is_empty()).then_some(out)
}

/// Whether two theme names denote the same theme: equal ignoring case, or
/// with equal [`slug`]s.
fn names_match(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase() || slug(a).is_some_and(|s| slug(b) == Some(s))
}

/// Whether `name` is worth looking up: not empty or blank.
fn is_lookup_name(name: &str) -> bool {
    !name.trim().is_empty()
}

/// The theme a `*.toml` file in `dir` provides for `name`, by stem first
/// and then by `name` key; see [`Theme::resolve`].
fn find_user_theme(
    dir: &Path,
    name: &str,
    warnings: &mut Vec<Warning>,
) -> Option<(Theme, PathBuf)> {
    let files = theme_files(dir);
    let (by_stem, others): (Vec<_>, Vec<_>) =
        files.iter().partition(|(stem, _)| names_match(stem, name));
    let mut budget = MAX_THEME_FILES;
    for (_, path) in by_stem.into_iter().take(budget) {
        budget -= 1;
        match Theme::load(path) {
            Ok(theme) => return Some((theme, path.clone())),
            Err(error) => {
                tracing::warn!(
                    target: "tessera_config",
                    path = %path.display(),
                    %error,
                    "theme file could not be loaded"
                );
                warnings.push(Warning::ThemeInvalid {
                    path: truncated(&path.display().to_string(), MAX_WARNING_PATH_LEN),
                    error: truncated(&error.to_string(), MAX_PARSE_ERROR_LEN),
                });
            }
        }
    }
    for (_, path) in others.into_iter().take(budget) {
        match Theme::load(path) {
            Ok(theme) if names_match(&theme.name, name) => return Some((theme, path.clone())),
            Ok(_) => {}
            // A broken file that does not match by stem may not be the one
            // asked for; it is not reported.
            Err(error) => tracing::debug!(
                target: "tessera_config",
                path = %path.display(),
                %error,
                "theme file skipped"
            ),
        }
    }
    None
}

/// The `*.toml` files in `dir` whose file stem matches `name` (the files
/// [`Theme::resolve`] tries first), in file-name order, whether or not they
/// load. For the watcher, which keeps such a file registered while it
/// fails to parse (a half-written save), so the save that fixes it is seen.
pub(crate) fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    if !is_lookup_name(name) || name.len() > MAX_THEME_NAME_LEN {
        return Vec::new();
    }
    theme_files(dir)
        .into_iter()
        .filter(|(stem, _)| names_match(stem, name))
        .map(|(_, path)| path)
        .collect()
}

/// The `(stem, path)` of every non-hidden `*.toml` entry in `dir`, sorted
/// by file name; at most [`MAX_THEME_DIR_ENTRIES`] entries are listed.
fn theme_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    target: "tessera_config",
                    path = %dir.display(),
                    %error,
                    "themes directory could not be listed"
                );
            }
            return Vec::new();
        }
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .take(MAX_THEME_DIR_ENTRIES)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            let stem = file_name.strip_suffix(".toml")?;
            (!stem.is_empty() && !stem.starts_with('.')).then(|| (stem.to_owned(), entry.path()))
        })
        .collect();
    files.sort();
    files
}

/// `value` as the colour at `key`.
fn color_value(key: &str, value: &toml::Value) -> Result<Color, ThemeError> {
    let toml::Value::String(text) = value else {
        return Err(ThemeError::WrongType {
            key: key.to_owned(),
            expected: "a colour string",
            found: value.type_str(),
        });
    };
    text.parse().map_err(|error| ThemeError::InvalidColor {
        key: key.to_owned(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_examples() {
        assert_eq!(slug("Solarized Dark").as_deref(), Some("solarized-dark"));
        assert_eq!(slug("  Nord  ").as_deref(), Some("nord"));
        assert_eq!(slug("!!!"), None);
        let long = "ab ".repeat(100);
        let s = slug(&long).unwrap();
        assert!(s.len() <= MAX_SLUG_LEN && !s.ends_with('-'), "{s}");
    }

    #[test]
    fn hex_digits_of_both_cases() {
        assert_eq!(
            Color::from_hex("#aAfF09"),
            Some(Color::rgb(0xaa, 0xff, 0x09))
        );
    }

    /// The search highlight keys are optional; set, they parse like any
    /// colour, survive `to_toml`, and a bad value is an error naming the
    /// key.
    #[test]
    fn search_match_keys_are_optional() {
        let base = Theme::bundled("Dracula").unwrap().to_toml();
        let plain = Theme::parse(&base).unwrap();
        assert_eq!(
            (plain.search_match, plain.search_match_current),
            (None, None)
        );
        assert!(
            !base.contains("search_match"),
            "absent keys are not written"
        );
        let text =
            format!("{base}search_match = \"#112233\"\nsearch_match_current = \"#44556680\"\n");
        let theme = Theme::parse(&text).unwrap();
        assert_eq!(theme.search_match, Some(Color::rgb(0x11, 0x22, 0x33)));
        assert_eq!(
            theme.search_match_current,
            Some(Color::rgb(0x44, 0x55, 0x66).with_alpha(0x80))
        );
        assert_eq!(Theme::parse(&theme.to_toml()).unwrap(), theme, "round trip");
        let bad = format!("{base}search_match = \"red\"\n");
        let error = Theme::parse(&bad).unwrap_err().to_string();
        assert!(error.contains("search_match"), "{error}");
    }
}
