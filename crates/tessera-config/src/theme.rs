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
//! mark_success = "#50fa7b"    # optional; defaults to ansi[2] (green)
//! mark_failure = "#ff5555"    # optional; defaults to ansi[1] (red)
//! ansi = ["#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
//!         "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff"]
//! ```
//!
//! Colours are `#rrggbb` or `#rrggbbaa` sRGB strings (hex digits in either
//! case). Unknown keys are ignored, so a theme written by a newer version
//! still loads. `mark_success` and `mark_failure` colour the exit-status
//! marks beside finished commands' prompts (#45b); they are drawn opaque,
//! and a command whose exit status is unknown is marked in `ansi[8]`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::load::{
    MAX_PARSE_ERROR_LEN, MAX_WARNING_PATH_LEN, ParseError, Warning, escaped, read_capped, truncated,
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
    /// The exit-status mark of a command that succeeded, or `None` for
    /// `ansi[2]` (green). Drawn opaque: the alpha digits are ignored.
    pub mark_success: Option<Color>,
    /// The exit-status mark of a command that failed, or `None` for
    /// `ansi[1]` (red). Drawn opaque.
    pub mark_failure: Option<Color>,
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
        #[cfg(test)]
        LOADS.with(|loads| loads.set(loads.get() + 1));
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
        let mark_success = color("mark_success")?;
        let mark_failure = color("mark_failure")?;
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
            mark_success,
            mark_failure,
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
        if let Some(color) = self.mark_success {
            let _ = writeln!(out, "mark_success = \"{color}\"");
        }
        if let Some(color) = self.mark_failure {
            let _ = writeln!(out, "mark_failure = \"{color}\"");
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
        let key = NameKey::of(name);
        BUNDLED
            .iter()
            .find(|theme| NameKey::of(&theme.name).matches(&key))
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
        Self::resolve_noting(themes_dir, name, &mut Vec::new())
    }

    /// [`Theme::resolve`], also pushing onto `unloadable` every user file
    /// the lookup read that did not load, whether or not it matched by
    /// stem (for the watcher; see [`unloadable_to_follow`]).
    fn resolve_noting(
        themes_dir: &Path,
        name: &str,
        unloadable: &mut Vec<PathBuf>,
    ) -> ResolvedTheme {
        #[cfg(test)]
        LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
        let mut warnings = Vec::new();
        // A name longer than any imported theme's (the importers clip names
        // to this length) is not compared at all: doing so against every
        // file would cost time in proportion to its length. A hand-written
        // theme with a longer `name` is still found by its file stem or by
        // a name within the cap that has the same slug.
        if is_lookup_name(name) && name.len() <= MAX_THEME_NAME_LEN {
            if let Some((theme, path)) =
                find_user_theme(themes_dir, name, &mut warnings, unloadable)
            {
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
            name: escaped(name, MAX_WARNING_PATH_LEN),
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

/// A theme name as lookups compare it, worked out once (#430: comparing
/// thousands of names, each pair recomputing both sides, cost seconds).
/// Two names denote the same theme when they are equal ignoring case or
/// have equal [`slug`]s.
struct NameKey {
    /// The name lowercased.
    lower: String,
    /// Its [`slug`].
    slug: Option<String>,
}

impl NameKey {
    fn of(name: &str) -> Self {
        Self {
            lower: name.to_lowercase(),
            slug: slug(name),
        }
    }

    /// Whether the two names denote the same theme.
    fn matches(&self, other: &Self) -> bool {
        self.lower == other.lower || (self.slug.is_some() && self.slug == other.slug)
    }
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
    unloadable: &mut Vec<PathBuf>,
) -> Option<(Theme, PathBuf)> {
    let files = theme_files(dir);
    let key = NameKey::of(name);
    let (by_stem, others): (Vec<_>, Vec<_>) = files
        .iter()
        .partition(|(stem, _)| NameKey::of(stem).matches(&key));
    let mut budget = MAX_THEME_FILES;
    for (_, path) in by_stem.into_iter().take(budget) {
        budget -= 1;
        match Theme::load(path) {
            Ok(theme) => return Some((theme, path.clone())),
            Err(error) => {
                // Escaped (S8): the error may quote the file, and a file
                // name may hold any character but `/`. Not logged here: a
                // lookup runs on every save the watch applies, and the
                // callers log what is new (#430: one line per broken
                // content, not one per lookup).
                warnings.push(Warning::ThemeInvalid {
                    path: escaped(&path.display().to_string(), MAX_WARNING_PATH_LEN),
                    error: escaped(&error.to_string(), MAX_PARSE_ERROR_LEN),
                });
                unloadable.push(path.clone());
            }
        }
    }
    for (_, path) in others.into_iter().take(budget) {
        match Theme::load(path) {
            Ok(theme) if NameKey::of(&theme.name).matches(&key) => {
                return Some((theme, path.clone()));
            }
            Ok(_) => {}
            // A broken file that does not match by stem may not be the one
            // asked for; it is not reported.
            Err(error) => {
                tracing::debug!(
                    target: "tessera_config",
                    path = %escaped(&path.display().to_string(), MAX_WARNING_PATH_LEN),
                    error = %escaped(&error.to_string(), MAX_PARSE_ERROR_LEN),
                    "theme file skipped"
                );
                unloadable.push(path.clone());
            }
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    /// How many theme lookups ([`Theme::resolve`]) ran on this thread.
    static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// How many theme files [`Theme::load`] read on this thread.
    static LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many theme files were loaded on this thread so far (#417).
#[cfg(test)]
pub(crate) fn loads_on_this_thread() -> usize {
    LOADS.with(std::cell::Cell::get)
}

/// How many theme lookups ran on this thread so far (#283 item 2).
#[cfg(test)]
pub(crate) fn lookups_on_this_thread() -> usize {
    LOOKUPS.with(std::cell::Cell::get)
}

/// The `*.toml` files in `dir` whose file stem matches `name` (the files
/// [`Theme::resolve`] tries first), in file-name order, whether or not they
/// load. For the watcher, which keeps such a file registered while it
/// fails to parse (a half-written save), so the save that fixes it is seen.
pub(crate) fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    if !is_lookup_name(name) || name.len() > MAX_THEME_NAME_LEN {
        return Vec::new();
    }
    let key = NameKey::of(name);
    theme_files(dir)
        .into_iter()
        .filter(|(stem, _)| NameKey::of(stem).matches(&key))
        .map(|(_, path)| path)
        .collect()
}

/// For the watcher: the user theme file each of `names` resolves to, as
/// [`Theme::resolve`] would find it (else its first stem match, which may
/// be a file that does not load yet), in the order of `names`, without
/// duplicates and at most `cap`.
///
/// Each name gets the budget of [`Theme::resolve`]: [`MAX_THEME_FILES`]
/// reads by stem and then by `name` key (#430). Unlike a lookup per name,
/// `dir` is listed once, each file is read at most once for all names,
/// each name and stem is lowercased and slugged once, and the names past
/// the one that settles the `cap`th file are not looked at. A name with
/// no stem match that names a bundled theme reads no file of its own: it
/// is searched by `name` key (a user file that shadows the bundled theme
/// under another stem) only when another name's search reads the files
/// anyway (#417 item 4, #430). All this reads at most
/// [`MAX_THEME_FILES`] times `cap + 1` files.
pub(crate) fn files_for_names(dir: &Path, names: &[&str], cap: usize) -> Vec<PathBuf> {
    /// Where a name's search ended.
    enum Outcome {
        /// The index of its file.
        File(usize),
        /// Nothing found.
        Nothing,
        /// A bundled theme's name with no stem match, searched by key last.
        Bundled(NameKey),
    }

    let names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| is_lookup_name(name) && name.len() <= MAX_THEME_NAME_LEN)
        .collect();
    if names.is_empty() || cap == 0 {
        return Vec::new();
    }
    let files = theme_files(dir);
    let stems: Vec<NameKey> = files.iter().map(|(stem, _)| NameKey::of(stem)).collect();
    let mut reads = Reads {
        files: &files,
        read: (0..files.len()).map(|_| Read::Not).collect(),
        left: MAX_THEME_FILES.saturating_mul(cap.saturating_add(1)),
    };
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(names.len());
    // Distinct files found so far; once `cap` are, no later name can
    // enter the result (a bundled name searched last may only push one
    // of them out).
    let mut settled: Vec<usize> = Vec::new();
    let mut key_pass = false;
    for name in &names {
        if settled.len() >= cap {
            break;
        }
        let key = NameKey::of(name);
        let matching: Vec<usize> = (0..files.len())
            .filter(|&i| stems[i].matches(&key))
            .collect();
        if matching.is_empty() && Theme::bundled(name).is_some() {
            outcomes.push(Outcome::Bundled(key));
            continue;
        }
        let mut budget = MAX_THEME_FILES;
        let mut file = None;
        for &i in &matching {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if reads.name_of(i).is_some() {
                file = Some(i);
                break;
            }
        }
        if file.is_none() {
            key_pass = true;
            file = (0..files.len())
                .filter(|&i| !stems[i].matches(&key))
                .take(budget)
                .find(|&i| reads.name_of(i).is_some_and(|theme| theme.matches(&key)))
                .or_else(|| matching.first().copied());
        }
        match file {
            Some(i) => {
                if !settled.contains(&i) {
                    settled.push(i);
                }
                outcomes.push(Outcome::File(i));
            }
            None => outcomes.push(Outcome::Nothing),
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for outcome in outcomes {
        let file = match outcome {
            Outcome::File(i) => Some(i),
            Outcome::Bundled(key) if key_pass => (0..files.len())
                .take(MAX_THEME_FILES)
                .find(|&i| reads.name_of(i).is_some_and(|theme| theme.matches(&key))),
            Outcome::Nothing | Outcome::Bundled(_) => None,
        };
        let Some(i) = file else { continue };
        if out.len() == cap {
            break;
        }
        if !out.contains(&files[i].1) {
            out.push(files[i].1.clone());
        }
    }
    out
}

/// The theme files of one [`files_for_names`] call, each read at most
/// once, within a total number of reads.
struct Reads<'a> {
    files: &'a [(String, PathBuf)],
    /// What reading each file gave.
    read: Vec<Read>,
    /// Reads left; past them a file not read yet counts as not loading.
    left: usize,
}

impl Reads<'_> {
    /// The key of the `name` of file `i`, `None` when it does not load.
    fn name_of(&mut self, i: usize) -> Option<&NameKey> {
        if matches!(self.read[i], Read::Not) {
            if self.left == 0 {
                return None;
            }
            self.left -= 1;
            self.read[i] = match Theme::load(&self.files[i].1) {
                Ok(theme) => Read::Named(NameKey::of(&theme.name)),
                Err(_) => Read::Broken,
            };
        }
        match &self.read[i] {
            Read::Named(key) => Some(key),
            Read::Not | Read::Broken => None,
        }
    }
}

/// What [`Reads`] knows of one file.
enum Read {
    /// Not read yet.
    Not,
    /// Read; it did not load.
    Broken,
    /// Read; the key of its `name`.
    Named(NameKey),
}

/// Most theme files that do not load which the native watcher keeps
/// registered while the theme in use is not a user file (see
/// [`unloadable_to_follow`]): one file descriptor each.
pub(crate) const MAX_UNLOADABLE_FOLLOWED: usize = 8;

/// For the watcher: the user file `name` resolves to (`None` for a bundled
/// theme or the default) and, when there is none, the theme files that the
/// lookup could not load, most recently modified first and at most
/// [`MAX_UNLOADABLE_FOLLOWED`].
///
/// An unloadable file may be the theme asked for, half-written (created
/// empty and then filled, deleted and recreated, saved in chunks): its
/// `name` key cannot be read yet, so nothing else ties it to `name`, and
/// an in-place write that completes it raises no event on the directory.
/// The watcher follows these files too, so that write is seen and the
/// lookup retried (#253 item 8).
pub(crate) fn unloadable_to_follow(dir: &Path, name: &str) -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut unloadable = Vec::new();
    if let ThemeSource::User(path) = Theme::resolve_noting(dir, name, &mut unloadable).source {
        return (Some(path), Vec::new());
    }
    let mut dated: Vec<(Option<std::time::SystemTime>, PathBuf)> = unloadable
        .into_iter()
        .map(|path| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            (modified, path)
        })
        .collect();
    // Newest first: a file just created or truncated is the likeliest.
    dated.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    (
        None,
        dated
            .into_iter()
            .take(MAX_UNLOADABLE_FOLLOWED)
            .map(|(_, path)| path)
            .collect(),
    )
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

    /// The exit-status mark keys (#45b) are optional like the search
    /// keys: absent they are `None` and not written; set, they parse, survive
    /// `to_toml`, and a bad value is an error naming the key.
    #[test]
    fn mark_keys_are_optional() {
        let base = Theme::bundled("Dracula").unwrap().to_toml();
        let plain = Theme::parse(&base).unwrap();
        assert_eq!((plain.mark_success, plain.mark_failure), (None, None));
        assert!(!base.contains("mark_"), "absent keys are not written");
        let text = format!("{base}mark_success = \"#00ff00\"\nmark_failure = \"#Ff000080\"\n");
        let theme = Theme::parse(&text).unwrap();
        assert_eq!(theme.mark_success, Some(Color::rgb(0, 0xff, 0)));
        assert_eq!(
            theme.mark_failure,
            Some(Color::rgb(0xff, 0, 0).with_alpha(0x80))
        );
        assert_eq!(Theme::parse(&theme.to_toml()).unwrap(), theme, "round trip");
        for key in ["mark_success", "mark_failure"] {
            let bad = format!("{base}{key} = \"green\"\n");
            let error = Theme::parse(&bad).unwrap_err().to_string();
            assert!(error.contains(key), "{error}");
        }
    }

    /// #262 items 4 and 5: a theme warning never carries a raw line
    /// separator, bidi override or escape, neither from the configured name
    /// ([`Warning::ThemeNotFound`]) nor from a theme file's name or the
    /// parse error that quotes its line ([`Warning::ThemeInvalid`]).
    #[test]
    fn theme_warnings_escape_separators_bidi_controls_and_esc() {
        const HOSTILE: [char; 3] = ['\u{2028}', '\u{202e}', '\u{1b}'];
        let raw = |text: &str| text.chars().any(|c| HOSTILE.contains(&c));
        let dir = tempfile::tempdir().unwrap();

        let missing = "gone\u{2028}x\u{202e}y\u{1b}[31m";
        let resolved = Theme::resolve(dir.path(), missing);
        let [Warning::ThemeNotFound { .. }] = resolved.warnings.as_slice() else {
            panic!("{:?}", resolved.warnings);
        };
        let text = resolved.warnings[0].to_string();
        assert!(!raw(&text), "{text:?}");
        assert!(text.contains(r"gone\u{2028}x\u{202e}y\u{1b}[31m"), "{text}");

        let name = "bad\u{2028}\u{202e}\u{1b}";
        std::fs::write(
            dir.path().join(format!("{name}.toml")),
            "background = nope\u{202e}\u{2028}\u{1b}[31m\n",
        )
        .unwrap();
        let resolved = Theme::resolve(dir.path(), name);
        let Some(Warning::ThemeInvalid { .. }) = resolved.warnings.first() else {
            panic!("{:?}", resolved.warnings);
        };
        for warning in &resolved.warnings {
            let text = warning.to_string();
            assert!(!raw(&text), "{text:?}");
        }
        let text = resolved.warnings[0].to_string();
        assert!(text.contains(r"bad\u{2028}\u{202e}\u{1b}.toml"), "{text}");
        assert!(text.contains(r"nope\u{202e}\u{2028}\u{1b}"), "{text}");
    }

    /// A theme document named `name`.
    fn named(name: &str) -> String {
        Theme {
            name: name.to_owned(),
            ..Theme::default()
        }
        .to_toml()
    }

    /// `dir` holding `files` as `(file name, contents)`.
    fn themes_dir(files: &[(String, String)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    /// The file names `files_for_names` gives for `names`.
    fn followed(dir: &Path, names: &[&str], cap: usize) -> Vec<String> {
        files_for_names(dir, names, cap)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// #430 (20.7 adv F2): each name gets [`Theme::resolve`]'s budget, so
    /// a file found only by its `name` key at sorted index 255 is found
    /// while another name took a stem load, and one at index 256 is not
    /// (as `resolve` finds it, or not).
    #[test]
    fn each_name_gets_the_lookup_budget_of_resolve() {
        let mut files: Vec<(String, String)> = (0..255)
            .map(|i| (format!("f{i:03}.toml"), named(&format!("F{i}"))))
            .collect();
        files.push(("g-keyed.toml".into(), named("Keyed")));
        let dir = themes_dir(&files);
        assert!(matches!(
            Theme::resolve(dir.path(), "Keyed").source,
            ThemeSource::User(path) if path.ends_with("g-keyed.toml")
        ));
        assert_eq!(
            followed(dir.path(), &["f001", "Keyed"], 8),
            ["f001.toml", "g-keyed.toml"]
        );
        assert_eq!(
            followed(dir.path(), &["f001", "f002", "f003", "Keyed"], 8),
            ["f001.toml", "f002.toml", "f003.toml", "g-keyed.toml"]
        );

        // One file more before it: index 256, past the budget, for
        // `resolve` and here alike.
        std::fs::write(dir.path().join("f255.toml"), named("F255")).unwrap();
        assert_eq!(
            Theme::resolve(dir.path(), "Keyed").source,
            ThemeSource::Default
        );
        assert_eq!(followed(dir.path(), &["f001", "Keyed"], 8), ["f001.toml"]);
    }

    /// #430 (20.7 m7): names that resolve to one file (they differ by
    /// case, or one is found by stem and the other by key) follow it once
    /// and take one slot.
    #[test]
    fn names_resolving_to_one_file_follow_it_once() {
        let dir = themes_dir(&[
            ("mine.toml".into(), named("Mine Keyed")),
            ("other.toml".into(), named("Other")),
        ]);
        assert_eq!(
            followed(dir.path(), &["mine", "MINE", "Mine Keyed", "other"], 2),
            ["mine.toml", "other.toml"]
        );
    }

    /// S5 at the key pass: a file is read for its `name` key only up to
    /// [`MAX_THEME_FILE_LEN`]; one byte more and it is not followed by
    /// its key.
    #[test]
    fn the_key_pass_reads_a_theme_up_to_its_size_cap() {
        let padded = |len: u64| {
            let mut text = named("Keyed");
            text.push('#');
            let len = usize::try_from(len).unwrap();
            text.extend(std::iter::repeat_n('x', len - text.len() - 1));
            text.push('\n');
            assert_eq!(text.len(), len);
            text
        };
        let at = themes_dir(&[("other.toml".into(), padded(MAX_THEME_FILE_LEN))]);
        assert_eq!(followed(at.path(), &["Keyed"], 8), ["other.toml"]);
        let past = themes_dir(&[("other.toml".into(), padded(MAX_THEME_FILE_LEN + 1))]);
        assert_eq!(followed(past.path(), &["Keyed"], 8), Vec::<String>::new());
    }

    /// #430 (20.7 m2): a user file that shadows a bundled theme only by
    /// its `name` key (another stem) is found once another name's search
    /// reads the files anyway; alone, a bundled name still reads nothing
    /// (#417 item 4).
    #[test]
    fn a_bundled_name_shadowed_by_key_is_found_when_the_files_are_read() {
        let dir = themes_dir(&[
            ("my-north.toml".into(), named("Nord")),
            ("other.toml".into(), named("Keyed")),
        ]);
        let loads = loads_on_this_thread();
        assert_eq!(followed(dir.path(), &["Nord"], 8), Vec::<String>::new());
        assert_eq!(loads_on_this_thread(), loads, "a bundled name alone");
        assert_eq!(
            followed(dir.path(), &["Nord", "Keyed"], 8),
            ["my-north.toml", "other.toml"]
        );
        // In profile order, and within the cap.
        assert_eq!(
            followed(dir.path(), &["Nord", "Keyed"], 1),
            ["my-north.toml"]
        );
    }

    /// #430 (20.7 m4, adv F1): once `cap` files are settled the later
    /// names read nothing, and each file is read at most once for all
    /// names: 16 names with 20 broken stem matches each and 256 files
    /// with other stems (sorted first, so each key window ends in them)
    /// read the 8 first names' 160 broken files and at most
    /// [`MAX_THEME_FILES`] others, not the later names' 160 too.
    #[test]
    fn the_search_stops_once_the_slots_are_settled() {
        let mut files: Vec<(String, String)> = Vec::new();
        let names: Vec<String> = (0..16).map(|n| format!("n{n:02}")).collect();
        for name in &names {
            for bang in 0..20 {
                files.push((format!("{name}{}.toml", "!".repeat(bang)), "x = ".into()));
            }
        }
        for i in 0..256 {
            files.push((format!("a{i:03}.toml"), named(&format!("A{i}"))));
        }
        let dir = themes_dir(&files);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let loads = loads_on_this_thread();
        let found = followed(dir.path(), &refs, 8);
        let read = loads_on_this_thread() - loads;
        let want: Vec<String> = names[..8].iter().map(|n| format!("{n}.toml")).collect();
        assert_eq!(found, want);
        assert!(read <= 8 * 20 + MAX_THEME_FILES, "{read} files read");
        assert!(read >= 8 * 20, "{read} files read");
    }

    /// #430 (20.7 adv F1): 256 long names that match nothing against 4096
    /// files with long stems: each stem and name is lowercased and
    /// slugged once, so this takes far less than the seconds it took.
    /// Release-only bar; debug just checks the outcome.
    #[test]
    fn many_long_names_against_many_long_stems_are_compared_once_each() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_THEME_DIR_ENTRIES {
            let stem = format!("{i:04}{}", "s".repeat(246));
            std::fs::write(dir.path().join(format!("{stem}.toml")), named("S")).unwrap();
        }
        let names: Vec<String> = (0..crate::MAX_PROFILES)
            .map(|i| format!("{i:03}{}", "N".repeat(MAX_THEME_NAME_LEN - 3)))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let started = std::time::Instant::now();
        assert_eq!(files_for_names(dir.path(), &refs, 8), Vec::<PathBuf>::new());
        let took = started.elapsed();
        if !cfg!(debug_assertions) {
            assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        }
    }
}
