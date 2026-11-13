//! Lenient loading and first-launch defaults.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Config, Error};

/// The commented `config.toml` written on first launch.
///
/// Parsing it yields exactly [`Config::default()`]; a test enforces this.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

/// Upper bound on the text of a [`ParseError`], in bytes.
///
/// `toml` quotes the offending line in its message, so a syntax error on a
/// very long line would otherwise produce an error as large as the line.
pub const MAX_PARSE_ERROR_LEN: usize = 2048;

/// Largest `config.toml` that is read, in bytes.
///
/// A configuration file is a few kilobytes; one above this limit is almost
/// certainly the wrong file (or a symlink to one), and reading it whole on
/// every reload would cost that much memory each time. [`load_file`]
/// reports a larger file as an I/O error of kind `InvalidInput` without
/// opening it.
pub const MAX_CONFIG_FILE_LEN: u64 = 16 * 1024 * 1024;

/// Most symlink hops [`follow_links`] follows before giving up on a cycle.
const MAX_LINK_HOPS: usize = 40;

/// Most unknown keys reported individually by one load, in bytes of
/// [`Warning`]s rather than in warnings per key: a document with more of
/// them gets this many [`Warning::UnknownKey`]s and then one
/// [`Warning::UnknownKeysOmitted`] with the count of the rest, so a file
/// full of junk cannot produce a million warnings per reload.
pub const MAX_UNKNOWN_KEY_WARNINGS: usize = 64;

/// Upper bound on the `path` of a [`Warning::UnknownKey`], in bytes; a
/// longer key is truncated like a [`ParseError`].
pub const MAX_WARNING_PATH_LEN: usize = 256;

/// A TOML syntax error or a type mismatch on a known key.
///
/// Wraps the message of [`toml::de::Error`] on one line, with control
/// characters, line separators and bidirectional formatting characters
/// escaped (`toml` quotes the offending line of the file, and the message
/// spans several lines), truncated to [`MAX_PARSE_ERROR_LEN`] bytes, so
/// logging or displaying it is always cheap and safe. The byte span of the problem in the source, when known, is kept
/// intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
    span: Option<std::ops::Range<usize>>,
}

impl ParseError {
    /// The (possibly truncated) message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Byte range of the problem in the source document, when known.
    pub fn span(&self) -> Option<std::ops::Range<usize>> {
        self.span.clone()
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<toml::de::Error> for ParseError {
    fn from(error: toml::de::Error) -> Self {
        Self {
            // Escaped (S8): `toml` quotes the offending line, which may hold
            // any character the file does (#262 item 10).
            message: escaped(&error.to_string(), MAX_PARSE_ERROR_LEN),
            span: error.span(),
        }
    }
}

/// `text` cut to at most `max` bytes on a character boundary, with a marker
/// when anything was dropped.
pub(crate) fn truncated(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(TRUNCATION_MARKER.len());
    let mut cut = keep;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + TRUNCATION_MARKER.len());
    out.push_str(&text[..cut]);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// What [`truncated`] (and so [`escaped`]) puts at the end of a cut text.
pub(crate) const TRUNCATION_MARKER: &str = "… (truncated)";

/// The uncut start of `text` when [`truncated`] cut it (it ends with
/// [`TRUNCATION_MARKER`]), `None` when it is whole.
pub(crate) fn cut_prefix(text: &str) -> Option<&str> {
    text.strip_suffix(TRUNCATION_MARKER)
}

/// A non-fatal problem found while loading configuration.
///
/// Warnings never prevent a load; the affected value falls back to its
/// default (or is ignored, for unknown keys).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Warning {
    /// A key that is not part of the schema. The dotted `path` names it.
    UnknownKey {
        /// Dotted path such as `appearance.fnt_size`, truncated to
        /// [`MAX_WARNING_PATH_LEN`] bytes.
        path: String,
    },
    /// More unknown keys than [`MAX_UNKNOWN_KEY_WARNINGS`] were found; the
    /// first ones are reported individually and `count` others only here.
    UnknownKeysOmitted {
        /// How many unknown keys are not listed.
        count: usize,
    },
    /// A value outside its valid range; it was replaced.
    OutOfRange {
        /// Dotted path of the field.
        path: String,
        /// Human-readable description of the replacement.
        detail: String,
    },
    /// `appearance.theme` names no user theme file and no bundled theme;
    /// the default theme ([`Theme::DEFAULT_NAME`](crate::Theme::DEFAULT_NAME))
    /// is used instead.
    ThemeNotFound {
        /// The requested name, truncated to [`MAX_WARNING_PATH_LEN`] bytes.
        name: String,
    },
    /// A user theme file whose name matches `appearance.theme` could not be
    /// loaded; the lookup continued with the next candidate (another user
    /// file, then the bundled themes, then the default).
    ThemeInvalid {
        /// The file, truncated to [`MAX_WARNING_PATH_LEN`] bytes.
        path: String,
        /// Why it could not be loaded (bounded like a [`ParseError`]).
        error: String,
    },
    /// A `[keys]` entry was left out (a malformed chord or value, a chord
    /// already bound by an earlier entry, an unknown command) or is
    /// honoured with a caveat (it shadows plain typing or a menu item).
    /// The `reason` says which.
    KeyBinding {
        /// The table key as written, escaped and truncated to
        /// [`MAX_WARNING_PATH_LEN`] bytes.
        key: String,
        /// What is wrong, ending in `ignored` or `honoured`.
        reason: String,
    },
    /// The `[keys]` table has more than
    /// [`MAX_KEY_BINDINGS`](crate::MAX_KEY_BINDINGS) entries; the first
    /// ones are read and `dropped` others are left out.
    TooManyKeyBindings {
        /// How many entries were left out.
        dropped: usize,
    },
    /// The file has more than [`MAX_PROFILES`](crate::MAX_PROFILES)
    /// `[[profiles]]` entries; the first ones are read and `dropped`
    /// others are left out.
    TooManyProfiles {
        /// How many entries were left out.
        dropped: usize,
    },
    /// A session asked for a profile (or `[general] default_profile`
    /// names one) that no `[[profiles]]` entry has; `instead` is the one
    /// used, `None` for the built-in profile.
    ProfileNotFound {
        /// The name asked for, escaped and truncated to
        /// [`MAX_WARNING_PATH_LEN`] bytes.
        name: String,
        /// The profile used instead, escaped and truncated like `name`.
        instead: Option<String>,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey { path } => write!(f, "unknown key `{path}` ignored"),
            Self::UnknownKeysOmitted { count } => {
                write!(f, "{count} more unknown keys ignored")
            }
            Self::OutOfRange { path, detail } => write!(f, "`{path}` out of range: {detail}"),
            Self::ThemeNotFound { name } => write!(
                f,
                "theme `{name}` not found; using `{}`",
                crate::Theme::DEFAULT_NAME
            ),
            Self::ThemeInvalid { path, error } => write!(f, "theme file {path} ignored: {error}"),
            Self::KeyBinding { key, reason } => write!(f, "`keys.\"{key}\"`: {reason}"),
            Self::TooManyKeyBindings { dropped } => write!(
                f,
                "{dropped} `[keys]` entries past the first {} ignored",
                crate::MAX_KEY_BINDINGS
            ),
            Self::TooManyProfiles { dropped } => write!(
                f,
                "{dropped} `[[profiles]]` entries past the first {} ignored",
                crate::MAX_PROFILES
            ),
            Self::ProfileNotFound { name, instead } => match instead {
                Some(instead) => write!(f, "profile `{name}` not found; using `{instead}`"),
                None => write!(f, "profile `{name}` not found; using the built-in profile"),
            },
        }
    }
}

/// Whether a load logs its warnings ([`Config::from_toml_lenient_logging`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Logging {
    /// Each warning is logged (`tracing::warn!`), as a load does.
    Log,
    /// Nothing is logged; the caller gets the warnings and decides.
    Quiet,
}

/// A successfully loaded configuration together with its warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    /// The configuration with defaults filled in and values normalised.
    pub config: Config,
    /// Non-fatal problems, in document order.
    pub warnings: Vec<Warning>,
}

impl Config {
    /// Parses a TOML document, collecting unknown keys and out-of-range values
    /// as warnings instead of failing.
    ///
    /// Only syntax errors and type mismatches on known keys are errors. A
    /// type mismatch includes an integer that does not fit its field's type,
    /// such as a negative `terminal.scrollback_lines` or one above
    /// `u32::MAX`; values that fit the type but exceed a documented limit
    /// (for example [`Terminal::MAX_SCROLLBACK_LINES`](crate::Terminal::MAX_SCROLLBACK_LINES))
    /// are clamped with a warning.
    pub fn from_toml_lenient(text: &str) -> Result<Loaded, Error> {
        Self::from_toml_lenient_logging(text, Logging::Log)
    }

    /// [`Config::from_toml_lenient`] that logs no warning when `logging`
    /// is [`Logging::Quiet`]: for a check that is not a load (an edit's
    /// check, a reload that logs only the warnings it had not logged
    /// before), without a scoped subscriber, which would churn tracing's
    /// callsite cache on every call (#342 M6).
    pub(crate) fn from_toml_lenient_logging(text: &str, logging: Logging) -> Result<Loaded, Error> {
        // Parsed once (#233 item 6). The deserializer consumes its tree,
        // and the warnings need the document with spans afterwards (where
        // each warning's key is, for document order, and the values the
        // lenient keys replaced), so they get a copy of the part they can
        // look at: see `position_tree`.
        let document = toml::de::DeTable::parse(text)?;
        let root = position_tree(document.get_ref());
        let de = toml::de::Deserializer::from(document);
        let mut warnings = Vec::new();
        let mut omitted = 0usize;
        let mut unknown = 0usize;
        let config = serde_ignored::deserialize(de, |path| {
            let steps = path_steps(&path);
            if let Some((i, key)) = profile_window_key(&steps) {
                warnings.extend(profile_window_warning(text, &root, &steps, i, key));
            } else if unknown < MAX_UNKNOWN_KEY_WARNINGS {
                unknown += 1;
                warnings.push((
                    position(&root, &steps),
                    Warning::UnknownKey {
                        path: escaped(&dotted_path(&path), MAX_WARNING_PATH_LEN),
                    },
                ));
            } else {
                omitted += 1;
            }
        });
        // A deserializer made from a parsed tree does not know the text, so
        // its errors lack the quoted line `Deserializer::parse` gives them.
        let mut config: Config = config.map_err(|mut error| {
            error.set_input(Some(text));
            error
        })?;
        if omitted > 0 {
            warnings.push((usize::MAX, Warning::UnknownKeysOmitted { count: omitted }));
        }
        config.keys.put_in_document_order(|key| {
            lookup(&root, &[Step::Key("keys"), Step::Key(key)]).map(|(at, _)| at)
        });
        warnings.extend(config.keys.take_placed_warnings());
        warnings.extend(invalid_lenient_values(text, &root));
        warnings.extend(config.normalize().into_iter().map(|warning| {
            let at = match &warning {
                Warning::OutOfRange { path, .. } => position(&root, &dotted_steps(path)),
                _ => usize::MAX,
            };
            (at, warning)
        }));
        // Stable: warnings without a place keep the order they came in.
        warnings.sort_by_key(|&(at, _)| at);
        let warnings: Vec<Warning> = warnings.into_iter().map(|(_, warning)| warning).collect();
        if logging == Logging::Log {
            for w in &warnings {
                tracing::warn!(target: "tessera_config", "{w}");
            }
        }
        Ok(Loaded { config, warnings })
    }

    /// Clamps or resets values that are outside their valid range, returning a
    /// warning for each change.
    pub fn normalize(&mut self) -> Vec<Warning> {
        let mut warnings = Vec::new();
        normalize_appearance(&mut self.appearance, &mut warnings);
        self.selection.normalize(&mut warnings);
        crate::profile::cap_profiles(self, &mut warnings);
        for (i, profile) in self.profiles.iter_mut().enumerate() {
            if let Some(appearance) = profile.appearance.as_mut() {
                let prefix = format!("profiles[{i}].appearance");
                // Not a number, or infinite: unset, so `[appearance]`'s
                // value applies, as for a key left out (#333).
                for (key, value) in [
                    ("font_size", &mut appearance.font_size),
                    ("opacity", &mut appearance.opacity),
                ] {
                    if let Some(bad) = value.take_if(|v| !v.is_finite()) {
                        warnings.push(Warning::OutOfRange {
                            path: format!("{prefix}.{key}"),
                            detail: format!("{bad} ignored; [appearance] {key} applies"),
                        });
                    }
                }
                if let Some(size) = appearance.font_size.as_mut() {
                    *size = font_size_in_range(&prefix, *size, &mut warnings);
                }
                if let Some(opacity) = appearance.opacity.as_mut() {
                    *opacity = opacity_in_range(&prefix, *opacity, &mut warnings);
                }
            }
        }
        if self.terminal.scrollback_lines > crate::Terminal::MAX_SCROLLBACK_LINES {
            warnings.push(Warning::OutOfRange {
                path: "terminal.scrollback_lines".into(),
                detail: format!(
                    "{} capped to {}",
                    self.terminal.scrollback_lines,
                    crate::Terminal::MAX_SCROLLBACK_LINES
                ),
            });
            self.terminal.scrollback_lines = crate::Terminal::MAX_SCROLLBACK_LINES;
        }
        warnings
    }
}

/// The top-level keys of [`Config`], as written in `config.toml`. A test
/// checks them against the serialised defaults.
const CONFIG_KEYS: [&str; 7] = [
    "general",
    "appearance",
    "terminal",
    "profiles",
    "plugins",
    "keys",
    "selection",
];

/// The part of `document` the warnings look up, copied: every top-level
/// key with its span, and the value only under the keys of [`Config`].
///
/// Warnings name a key only where the deserializer looked, and it never
/// looks inside the value of an unknown top-level key (that key itself is
/// the warning), so such a value is replaced by a placeholder. A file full
/// of unknown tables thus costs one copy of its top-level keys rather than
/// of the whole tree. Copying is cheaper than parsing the text again (a
/// 16 MiB file loads in 0.53-0.73x the instructions it took with two
/// parses), and the copy borrows its strings from the text unless they
/// hold escapes.
fn position_tree<'i>(document: &toml::de::DeTable<'i>) -> toml::de::DeTable<'i> {
    document
        .iter()
        .map(|(key, value)| {
            let value = if CONFIG_KEYS.contains(&key.get_ref().as_ref()) {
                value.clone()
            } else {
                toml::Spanned::new(value.span(), toml::de::DeValue::Boolean(false))
            };
            (key.clone(), value)
        })
        .collect()
}

/// Renders a `serde_ignored` path the way [`Config::normalize`] names
/// fields: `profiles[0].appearance.bogus`, with `Option` and newtype
/// wrappers (which `serde_ignored` shows as `?`) left out.
fn dotted_path(path: &serde_ignored::Path<'_>) -> String {
    use std::fmt::Write as _;

    use serde_ignored::Path;

    fn render(path: &Path<'_>, out: &mut String) {
        match path {
            Path::Root => {}
            Path::Seq { parent, index } => {
                render(parent, out);
                let _ = write!(out, "[{index}]");
            }
            Path::Map { parent, key } => {
                render(parent, out);
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Path::Some { parent }
            | Path::NewtypeStruct { parent }
            | Path::NewtypeVariant { parent } => render(parent, out),
        }
    }

    let mut out = String::new();
    render(path, &mut out);
    out
}

/// One step of a path into the document: a key of a table or an element of
/// an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// The steps of a `serde_ignored` path, `Option` and newtype wrappers left
/// out (as in [`dotted_path`]).
fn path_steps<'a>(path: &'a serde_ignored::Path<'a>) -> Vec<Step<'a>> {
    use serde_ignored::Path;

    let mut steps = Vec::new();
    let mut at = path;
    loop {
        at = match at {
            Path::Root => break,
            Path::Seq { parent, index } => {
                steps.push(Step::Index(*index));
                parent
            }
            Path::Map { parent, key } => {
                steps.push(Step::Key(key));
                parent
            }
            Path::Some { parent }
            | Path::NewtypeStruct { parent }
            | Path::NewtypeVariant { parent } => parent,
        };
    }
    steps.reverse();
    steps
}

/// The steps of a path as [`Config::normalize`] writes it
/// (`profiles[0].appearance.font_size`; its keys never contain `.` or
/// `[`).
fn dotted_steps(path: &str) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    for part in path.split('.') {
        let mut pieces = part.split('[');
        if let Some(key) = pieces.next() {
            steps.push(Step::Key(key));
        }
        for index in pieces {
            if let Ok(index) = index.trim_end_matches(']').parse() {
                steps.push(Step::Index(index));
            }
        }
    }
    steps
}

/// Rebuilds `config`'s `[keys]`, deserialized from `text` in the parser's
/// order (sorted by key), in document order, as
/// [`Config::from_toml_lenient`] does: for [`Config::from_toml`].
pub(crate) fn put_keys_in_document_order(config: &mut Config, text: &str) {
    // `text` was parsed once already, so this parse fails only if that did.
    let Ok(document) = toml::de::DeTable::parse(text) else {
        return;
    };
    config.keys.put_in_document_order(|key| {
        lookup(document.get_ref(), &[Step::Key("keys"), Step::Key(key)]).map(|(at, _)| at)
    });
}

/// The value at `steps` in the document and where it is: the start of
/// its key (or of the array element, for a last step that is an index).
fn lookup<'a, 'i>(
    root: &'a toml::de::DeTable<'i>,
    steps: &[Step<'_>],
) -> Option<(usize, &'a toml::Spanned<toml::de::DeValue<'i>>)> {
    use toml::de::DeValue;

    let (first, rest) = steps.split_first()?;
    let Step::Key(key) = first else {
        return None;
    };
    let (key, mut value) = root.get_key_value(*key)?;
    let mut at = key.span().start;
    for step in rest {
        (at, value) = match (step, value.get_ref()) {
            (Step::Key(key), DeValue::Table(table)) => {
                let (key, value) = table.get_key_value(*key)?;
                (key.span().start, value)
            }
            (Step::Index(index), DeValue::Array(array)) => {
                let value = array.get(*index)?;
                (value.span().start, value)
            }
            _ => return None,
        };
    }
    Some((at, value))
}

/// Where the key at `steps` is in the document, for sorting warnings into
/// document order; `usize::MAX` (last) when it is not there.
fn position(root: &toml::de::DeTable<'_>, steps: &[Step<'_>]) -> usize {
    lookup(root, steps).map_or(usize::MAX, |(at, _)| at)
}

/// A lenient key's value as written, for its warning: escaped and capped
/// by [`escaped`], since a multi-line string or array would otherwise put
/// line breaks into the log line.
fn written(text: &str, value: &toml::Spanned<toml::de::DeValue<'_>>) -> String {
    let raw = text.get(value.span()).unwrap_or_default();
    escaped(raw, MAX_WARNING_PATH_LEN)
}

/// `text` with every control character (C0, DEL and C1: `\n`, `\t`,
/// `\u{1b}`, ...), the line and paragraph separators (U+2028, U+2029) and
/// every character that shows as nothing (`is_invisible`: the
/// bidirectional formatting characters U+061C, U+200E, U+200F,
/// U+202A..=U+202E and U+2066..=U+2069, the zero-width characters, the
/// variation selectors and the other default-ignorables) written as their
/// Rust escapes, so a warning stays on one line, cannot reorder the text
/// around it and never carries a character a reader cannot see, then
/// cut to at most `max` bytes on a character boundary (with a marker when
/// cut). Only as much of `text` is escaped as can survive the cut, so a
/// huge value costs at most `max` characters of work.
///
/// Every file-derived text this crate logs or reports goes through it
/// (docs/security.md S8); callers that log such text themselves (a theme
/// file's path or load error) use it too, with [`MAX_WARNING_PATH_LEN`] or
/// [`MAX_PARSE_ERROR_LEN`] as `max`.
pub fn escaped(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max + 1));
    for c in text.chars() {
        // Past `max`, `truncated` cuts the rest anyway (and adds its marker).
        if out.len() > max {
            break;
        }
        if c.is_control() {
            out.extend(c.escape_debug());
        } else if needs_unicode_escape(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    truncated(&out, max)
}

/// Whether [`escaped`] writes `c` (not a control character) as `\u{..}`:
/// a line or paragraph separator, which some log viewers break lines at,
/// or a character that shows as nothing ([`is_invisible`]), the
/// bidirectional formatting characters among them.
pub(crate) fn needs_unicode_escape(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}') || is_invisible(c)
}

/// The Unicode `Default_Ignorable_Code_Point` ranges (`DerivedCoreProperties`,
/// Unicode 16), inclusive and sorted: the characters a renderer shows as
/// nothing, the bidirectional formatting characters among them (U+061C,
/// U+200E, U+200F, U+202A..=U+202E, U+2066..=U+2069).
pub(crate) const DEFAULT_IGNORABLE: [(u32, u32); 17] = [
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1_BCA0, 0x1_BCA3),
    (0x1_D173, 0x1_D17A),
    (0xE_0000, 0xE_0FFF),
];

/// Whether `c` shows nothing: a default-ignorable character
/// ([`DEFAULT_IGNORABLE`]), the bidi, zero-width and variation-selector
/// characters included.
pub(crate) fn is_invisible(c: char) -> bool {
    let c = u32::from(c);
    let i = DEFAULT_IGNORABLE.partition_point(|&(_, last)| last < c);
    DEFAULT_IGNORABLE
        .get(i)
        .is_some_and(|&(first, _)| first <= c)
}

/// Deserializes `appearance.cursor_shape` without failing: a value that is
/// not exactly one of [`CursorShape::as_str`](crate::CursorShape::as_str)
/// (another spelling, another case, another type) gives the default shape,
/// and [`invalid_lenient_values`] reports it. So a mistyped shape costs a
/// warning, not the whole configuration.
pub(crate) fn lenient_cursor_shape<'de, D>(deserializer: D) -> Result<crate::CursorShape, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let value = toml::Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .and_then(crate::CursorShape::from_name)
        .unwrap_or_default())
}

/// Deserializes `general.remote_indicator` without failing: a value that
/// is not exactly one of
/// [`RemoteIndicator::as_str`](crate::RemoteIndicator::as_str) gives
/// `"title"`, and [`invalid_lenient_values`] reports it, as for
/// [`lenient_cursor_shape`].
pub(crate) fn lenient_remote_indicator<'de, D>(
    deserializer: D,
) -> Result<crate::RemoteIndicator, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let value = toml::Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .and_then(crate::RemoteIndicator::from_name)
        .unwrap_or_default())
}

/// Deserializes `appearance.app_appearance` without failing: a value that
/// is not exactly one of
/// [`AppAppearance::as_str`](crate::AppAppearance::as_str) gives `"dark"`,
/// and [`invalid_lenient_values`] reports it, as for
/// [`lenient_cursor_shape`].
pub(crate) fn lenient_app_appearance<'de, D>(
    deserializer: D,
) -> Result<crate::AppAppearance, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let value = toml::Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .and_then(crate::AppAppearance::from_name)
        .unwrap_or_default())
}

/// Deserializes `appearance.cursor_blink` without failing: anything but a
/// boolean gives the default (`false`), and [`invalid_lenient_values`]
/// reports it, as for [`lenient_cursor_shape`].
pub(crate) fn lenient_cursor_blink<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_bool(deserializer)?.unwrap_or(crate::Appearance::default().cursor_blink))
}

/// Deserializes a profile's `appearance.cursor_shape` without failing: a
/// value that is not a shape is left unset (so `[appearance]`'s is used),
/// and [`invalid_lenient_values`] reports it.
pub(crate) fn lenient_optional_cursor_shape<'de, D>(
    deserializer: D,
) -> Result<Option<crate::CursorShape>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let value = toml::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(crate::CursorShape::from_name))
}

/// Deserializes a profile's `appearance.cursor_blink` without failing:
/// anything but a boolean is left unset, as for
/// [`lenient_optional_cursor_shape`].
pub(crate) fn lenient_optional_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    lenient_bool(deserializer)
}

/// Deserializes `terminal.clipboard_write` without failing, and fails
/// closed (D-FAIL-CLOSED, ADR 0022): anything but a boolean is `false`, so
/// a mistyped switch never leaves clipboard writes on, and
/// [`invalid_lenient_values`] reports it.
pub(crate) fn lenient_clipboard_write<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_bool(deserializer)?.unwrap_or(false))
}

/// A boolean, or `None` for a value of any other type.
fn lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    Ok(toml::Value::deserialize(deserializer)?.as_bool())
}

/// The [`Warning::OutOfRange`] for `[general] remote_indicator` when its
/// value is not one of [`RemoteIndicator::ALL`](crate::RemoteIndicator::ALL),
/// with where the key is (see [`invalid_lenient_values`]).
fn invalid_remote_indicator(text: &str, root: &toml::de::DeTable<'_>) -> Option<(usize, Warning)> {
    use toml::de::DeValue;

    const REMOTE: &str = "remote_indicator";

    let (at, value) = lookup(root, &[Step::Key("general"), Step::Key(REMOTE)])?;
    if let DeValue::String(name) = value.get_ref()
        && crate::RemoteIndicator::from_name(name).is_some()
    {
        return None;
    }
    let names: Vec<String> = crate::RemoteIndicator::ALL
        .iter()
        .map(|value| format!("\"{}\"", value.as_str()))
        .collect();
    Some((
        at,
        Warning::OutOfRange {
            path: format!("general.{REMOTE}"),
            detail: format!(
                "{} replaced by \"{}\" (expected {})",
                written(text, value),
                crate::RemoteIndicator::default().as_str(),
                names.join(" or ")
            ),
        },
    ))
}

/// The [`Warning::OutOfRange`] for `[appearance] app_appearance` when its
/// value is not one of [`AppAppearance::ALL`](crate::AppAppearance::ALL),
/// with where the key is (see [`invalid_lenient_values`]). A profile's
/// `app_appearance` is not looked at here: it gets the window-level
/// warning instead ([`WINDOW_APPEARANCE_KEYS`]).
fn invalid_app_appearance(text: &str, root: &toml::de::DeTable<'_>) -> Option<(usize, Warning)> {
    use toml::de::DeValue;

    const APP: &str = "app_appearance";

    let (at, value) = lookup(root, &[Step::Key("appearance"), Step::Key(APP)])?;
    if let DeValue::String(name) = value.get_ref()
        && crate::AppAppearance::from_name(name).is_some()
    {
        return None;
    }
    let names: Vec<String> = crate::AppAppearance::ALL
        .iter()
        .map(|value| format!("\"{}\"", value.as_str()))
        .collect();
    Some((
        at,
        Warning::OutOfRange {
            path: format!("appearance.{APP}"),
            detail: format!(
                "{} replaced by \"{}\" (expected {})",
                written(text, value),
                crate::AppAppearance::default().as_str(),
                names.join(", ")
            ),
        },
    ))
}

/// One [`Warning::OutOfRange`], with where its key is, for every lenient
/// key whose value the lenient deserializers replaced:
/// `cursor_shape` and `cursor_blink` under `[appearance]` and under each
/// profile's `appearance`, `[appearance] app_appearance`, `[general]
/// remote_indicator` and `[terminal] clipboard_write`. Found in the
/// parsed document `root`, so a key written quoted or with escapes
/// (`"cursor\u005fshape"`) is found too, and a value that merely mentions
/// a key's name is not.
fn invalid_lenient_values(text: &str, root: &toml::de::DeTable<'_>) -> Vec<(usize, Warning)> {
    use toml::de::DeValue;

    const SHAPE: &str = "cursor_shape";
    const BLINK: &str = "cursor_blink";
    const CLIPBOARD: &str = "clipboard_write";

    let shape_names = || {
        let names: Vec<String> = crate::CursorShape::ALL
            .iter()
            .map(|shape| format!("\"{}\"", shape.as_str()))
            .collect();
        names.join(", ")
    };
    let profiles = match lookup(root, &[Step::Key("profiles")]) {
        Some((_, value)) => match value.get_ref() {
            DeValue::Array(array) => array.len(),
            _ => 0,
        },
        None => 0,
    };
    let mut warnings = Vec::new();
    let appearances = std::iter::once(("appearance".to_owned(), vec![Step::Key("appearance")]))
        .chain((0..profiles.min(crate::MAX_PROFILES)).map(|i| {
            (
                format!("profiles[{i}].appearance"),
                vec![
                    Step::Key("profiles"),
                    Step::Index(i),
                    Step::Key("appearance"),
                ],
            )
        }));
    for (prefix, mut steps) in appearances {
        // A profile's invalid value is left unset, so `[appearance]`'s is used.
        let instead = |default: &str| {
            if prefix == "appearance" {
                format!("replaced by {default}")
            } else {
                "ignored, `[appearance]`'s value used".to_owned()
            }
        };
        steps.push(Step::Key(SHAPE));
        if let Some((at, value)) = lookup(root, &steps) {
            let valid = match value.get_ref() {
                DeValue::String(name) => crate::CursorShape::from_name(name).is_some(),
                _ => false,
            };
            if !valid {
                warnings.push((
                    at,
                    Warning::OutOfRange {
                        path: format!("{prefix}.{SHAPE}"),
                        detail: format!(
                            "{} {} (expected {})",
                            written(text, value),
                            instead(&format!("\"{}\"", crate::CursorShape::default().as_str())),
                            shape_names()
                        ),
                    },
                ));
            }
        }
        steps.pop();
        steps.push(Step::Key(BLINK));
        if let Some((at, value)) = lookup(root, &steps)
            && !matches!(value.get_ref(), DeValue::Boolean(_))
        {
            warnings.push((
                at,
                Warning::OutOfRange {
                    path: format!("{prefix}.{BLINK}"),
                    detail: format!(
                        "{} {} (expected true or false)",
                        written(text, value),
                        instead(&crate::Appearance::default().cursor_blink.to_string())
                    ),
                },
            ));
        }
    }
    warnings.extend(invalid_app_appearance(text, root));
    warnings.extend(invalid_remote_indicator(text, root));
    if let Some((at, value)) = lookup(root, &[Step::Key("terminal"), Step::Key(CLIPBOARD)])
        && !matches!(value.get_ref(), DeValue::Boolean(_))
    {
        warnings.push((
            at,
            Warning::OutOfRange {
                path: format!("terminal.{CLIPBOARD}"),
                detail: format!(
                    "{} replaced by false, clipboard writes refused (expected true or false)",
                    written(text, value)
                ),
            },
        ));
    }
    warnings
}

/// `[appearance]`'s ranges: font size, opacity, blur radius and pane
/// dimming.
fn normalize_appearance(a: &mut crate::Appearance, warnings: &mut Vec<Warning>) {
    use crate::Appearance;

    const PREFIX: &str = "appearance";
    a.font_size = font_size_in_range(PREFIX, a.font_size, warnings);
    a.opacity = opacity_in_range(PREFIX, a.opacity, warnings);
    // A negative or fractional radius never gets here: it does not fit
    // the `u32` and fails the load (ADR 0045).
    if a.blur_radius > Appearance::MAX_BLUR_RADIUS {
        warnings.push(Warning::OutOfRange {
            path: format!("{PREFIX}.blur_radius"),
            detail: format!(
                "{} clamped to {}",
                a.blur_radius,
                Appearance::MAX_BLUR_RADIUS
            ),
        });
        a.blur_radius = Appearance::MAX_BLUR_RADIUS;
    }
    let max_dim = Appearance::MAX_DIM_INACTIVE_PANES;
    if !(0.0..=max_dim).contains(&a.dim_inactive_panes) {
        let clamped = if a.dim_inactive_panes.is_finite() {
            a.dim_inactive_panes.clamp(0.0, max_dim)
        } else {
            0.0
        };
        warnings.push(Warning::OutOfRange {
            path: format!("{PREFIX}.dim_inactive_panes"),
            detail: format!("{} clamped to {clamped}", a.dim_inactive_panes),
        });
        a.dim_inactive_panes = clamped;
    }
    // As for `opacity`: store plain zero for `-0.0`.
    if a.dim_inactive_panes == 0.0 {
        a.dim_inactive_panes = 0.0;
    }
}

/// `{prefix}.font_size` clamped to
/// [`Appearance::MIN_FONT_SIZE`](crate::Appearance::MIN_FONT_SIZE)..=[`Appearance::MAX_FONT_SIZE`](crate::Appearance::MAX_FONT_SIZE)
/// (not a number gives the default), with a warning when it changed.
fn font_size_in_range(prefix: &str, size: f32, warnings: &mut Vec<Warning>) -> f32 {
    use crate::Appearance;

    if !size.is_finite() {
        let default = Appearance::default().font_size;
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.font_size"),
            detail: format!("{size} replaced by {default}"),
        });
        default
    } else if (Appearance::MIN_FONT_SIZE..=Appearance::MAX_FONT_SIZE).contains(&size) {
        size
    } else {
        let clamped = size.clamp(Appearance::MIN_FONT_SIZE, Appearance::MAX_FONT_SIZE);
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.font_size"),
            detail: format!("{size} clamped to {clamped}"),
        });
        clamped
    }
}

/// `{prefix}.opacity` clamped to 0..=1 (not a number gives the default),
/// with a warning when it changed.
fn opacity_in_range(prefix: &str, opacity: f32, warnings: &mut Vec<Warning>) -> f32 {
    let mut opacity = if (0.0..=1.0).contains(&opacity) {
        opacity
    } else {
        let clamped = if opacity.is_nan() {
            crate::Appearance::default().opacity
        } else {
            opacity.clamp(0.0, 1.0)
        };
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.opacity"),
            detail: format!("{opacity} clamped to {clamped}"),
        });
        clamped
    };
    // `-0.0` is in range but would round-trip as `-0.0`; store plain zero.
    if opacity == 0.0 {
        opacity = 0.0;
    }
    opacity
}

/// The `[appearance]` keys the window draws, not the session, so a
/// profile's `appearance` table does not have them: pane dimming and the
/// window's blur radius, and the app's own appearance, which the whole app
/// draws.
const WINDOW_APPEARANCE_KEYS: [&str; 3] = ["dim_inactive_panes", "blur_radius", "app_appearance"];

/// The profile index and the key when `steps` is one of
/// [`WINDOW_APPEARANCE_KEYS`] in a profile's `appearance`.
fn profile_window_key(steps: &[Step<'_>]) -> Option<(usize, &'static str)> {
    match steps {
        [
            Step::Key("profiles"),
            Step::Index(i),
            Step::Key("appearance"),
            Step::Key(key),
        ] => WINDOW_APPEARANCE_KEYS
            .into_iter()
            .find(|window_key| window_key == key)
            .map(|key| (*i, key)),
        _ => None,
    }
}

/// The warning for profile `i`'s window-level `key` at `steps`: a
/// [`Warning::OutOfRange`] that says to set it in `[appearance]`, placed
/// where the key is. None past [`MAX_PROFILES`](crate::MAX_PROFILES): that
/// profile is left out, with one warning for all of them (S5).
fn profile_window_warning(
    text: &str,
    root: &toml::de::DeTable<'_>,
    steps: &[Step<'_>],
    i: usize,
    key: &str,
) -> Option<(usize, Warning)> {
    if i >= crate::MAX_PROFILES {
        return None;
    }
    let (at, value) = lookup(root, steps)?;
    Some((
        at,
        Warning::OutOfRange {
            path: format!("profiles[{i}].appearance.{key}"),
            detail: format!(
                "{} ignored (set it in [appearance], not in a profile)",
                written(text, value)
            ),
        },
    ))
}

/// Reads and leniently parses the file at `path`.
///
/// A file that is not UTF-8 is reported as [`Error::Utf8`] rather than as
/// an I/O error, since it is a problem with the document, not with reading
/// it.
///
/// Only a regular file (or a symlink to one) is read: a directory, FIFO,
/// socket or device is reported as an I/O error of kind `InvalidInput`.
/// The type is checked before the open and again on the open handle, and
/// on Unix the open is non-blocking, so a FIFO swapped in between the two
/// checks still cannot block the caller. A missing file, or a symlink
/// whose target is missing, is `NotFound`. A file larger than
/// [`MAX_CONFIG_FILE_LEN`] is `InvalidInput` as well: its size is checked
/// on the open handle and at most that many bytes are ever read, so a
/// file that grows during the read is rejected rather than read whole.
pub fn load_file(path: &Path) -> Result<Loaded, Error> {
    load_file_noting_empty(path, Logging::Log).map(|(loaded, _)| loaded)
}

/// [`load_file`], and whether the file held nothing but whitespace after
/// an optional byte order mark: TOML loads a BOM-only file as empty, so it
/// counts as empty too (#287).
pub(crate) fn load_file_noting_empty(
    path: &Path,
    logging: Logging,
) -> Result<(Loaded, bool), Error> {
    let bytes = read_capped(path, MAX_CONFIG_FILE_LEN, "config file")?;
    let text = std::str::from_utf8(&bytes)?;
    let empty = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .trim()
        .is_empty();
    Config::from_toml_lenient_logging(text, logging).map(|loaded| (loaded, empty))
}

/// Reads the regular file at `path` whole, refusing one larger than `limit`
/// bytes with an `InvalidInput` error that calls it a `what`. Never blocks
/// on a FIFO and never reads more than `limit + 1` bytes; see
/// [`load_file`] for the checks.
pub(crate) fn read_capped(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;

    check_regular_file(path, &std::fs::metadata(path)?, limit, what)?;
    let file = open_nonblocking(path)?;
    let len = check_regular_file(path, &file.metadata()?, limit, what)?;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    (&file).take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(too_large(path, None, limit, what).into());
    }
    Ok(bytes)
}

/// Opens `path` read-only without blocking on a FIFO: the one open for
/// every file the loader reads and every path the watcher registers
/// (#382 item 2; the directory `edit` syncs after a write is opened
/// apart).
/// On Unix the open itself is non-blocking (`O_NONBLOCK`), which a regular
/// file or a directory ignores and which lets a FIFO open at once, so the
/// caller can reject it from its handle ([`check_regular_file`] here, the
/// type check on the handle in the watcher's registration). Symlinks are
/// followed: a symlinked `config.toml` is read, and watched, through its
/// target.
pub(crate) fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(path)
}

/// Fails with `InvalidInput` unless `meta` describes a regular file of at
/// most `limit` bytes; returns its length.
fn check_regular_file(
    path: &Path,
    meta: &std::fs::Metadata,
    limit: u64,
    what: &str,
) -> std::io::Result<u64> {
    if !meta.is_file() {
        return Err(not_a_regular_file(path));
    }
    if meta.len() > limit {
        return Err(too_large(path, Some(meta.len()), limit, what));
    }
    Ok(meta.len())
}

fn too_large(path: &Path, len: Option<u64>, limit: u64, what: &str) -> std::io::Error {
    let size = len.map_or_else(String::new, |len| format!("is {len} bytes, "));
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "{} {size}larger than the {limit}-byte limit for a {what}",
            path.display()
        ),
    )
}

fn not_a_regular_file(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{} is not a regular file", path.display()),
    )
}

/// Follows symlinks from `path` to the first path that is not a symlink,
/// which need not exist (a dangling link resolves to its missing target).
///
/// Relative link targets are resolved against the link's directory, as the
/// kernel does. Returns `None` for a cycle or a chain longer than
/// [`MAX_LINK_HOPS`], and `Err` for an I/O error other than "not found".
pub(crate) fn follow_links(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(current)),
            Err(e) => return Err(e),
        };
        if !meta.is_symlink() {
            return Ok(Some(current));
        }
        let target = std::fs::read_link(&current)?;
        current = match current.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
    }
    Ok(None)
}

/// Writes [`DEFAULT_CONFIG_TOML`] to `path` unless something already exists
/// there.
///
/// The document is written and flushed to a temporary file, then
/// hard-linked to `path`. The link fails rather than replaces, so a file
/// another process or the user creates meanwhile is never overwritten, and
/// a watcher never sees a half-written document. On a file system without
/// hard links (FAT, some network shares) the file is created with
/// `create_new` and written in place: still never replacing, not atomic.
/// Returns whether a file was written. Only a
/// definite "not found" counts as missing: any other error while checking
/// (permission denied, a parent that is not a directory, ...) is returned so
/// an existing file is never replaced by mistake. A symlink counts as
/// present even when its target is missing, so a `config.toml` that points
/// into a dotfiles checkout is never replaced by a regular file; the user
/// (or their checkout) is expected to provide the target.
pub fn write_default_if_missing(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_new(path, DEFAULT_CONFIG_TOML)
}

/// Creates `path` holding `contents` unless something exists there; see
/// [`write_default_if_missing`]. Returns whether it was created.
fn write_new(path: &Path, contents: &str) -> Result<bool, Error> {
    use std::io::Write as _;

    /// Temporary names tried before giving up: a stale one (left by a
    /// crash of an earlier process with the same id) is skipped.
    const TEMP_ATTEMPTS: u32 = 16;

    let name = path
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned());
    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let tmp = path.with_file_name(format!(
            ".{name}.{}.{}.new",
            std::process::id(),
            NEW_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::File::create_new(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS => {
                attempts += 1;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    // Only the link's `AlreadyExists` means "something is at `path`".
    let linked = written.map(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        // No hard links here (FSKit's FAT driver: `ENOTSUP`): create in
        // place, still exclusive; a failed write removes the partial file.
        Ok(Err(e))
            if e.raw_os_error()
                .is_some_and(|n| NO_HARD_LINK_ERRNOS.contains(&n)) =>
        {
            let mut file = match std::fs::File::create_new(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            if let Err(e) = file
                .write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(e.into());
            }
            Ok(true)
        }
        // The temporary file could not be written, or the link failed.
        Err(e) | Ok(Err(e)) => Err(e.into()),
    }
}

/// Numbers [`write_new`]'s temporary files within this process.
static NEW_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The `errno` values [`write_new`] reads as "no hard links on this file
/// system", as `tessera-devtool theme import` does: `EPERM`, `EXDEV`,
/// `ENOSYS`, `ENOTSUP`, `EOPNOTSUPP` (macOS numbers; the crate has no
/// `libc` dependency).
#[cfg(not(target_os = "linux"))]
const NO_HARD_LINK_ERRNOS: [i32; 5] = [1, 18, 78, 45, 102];

/// See the macOS table; on Linux `ENOTSUP` and `EOPNOTSUPP` are both 95.
#[cfg(target_os = "linux")]
const NO_HARD_LINK_ERRNOS: [i32; 4] = [1, 18, 38, 95];

/// Writes `contents` to `path` via a sibling temporary file and a rename.
///
/// The temporary file is flushed to disk (`fsync`) before the rename, so a
/// crash or power loss leaves either the old file or the complete new one,
/// never an empty or truncated document. Each call uses its own temporary
/// name, so concurrent writers (threads or processes) never truncate each
/// other's temporary file; the last rename wins and the result is always
/// one complete document.
///
/// Symlinks are followed: when `path` is a link the write goes to its final
/// target, so a `config.toml` that points into a dotfiles checkout stays a
/// link. A link cycle is an `InvalidInput` error, and the target must be
/// either absent or a regular file.
///
/// An existing target's permission bits are carried over to the new file
/// (a `chmod 600 config.toml` stays private after a save); a new file gets
/// the process's default mode. Extended attributes and ACLs of the old file
/// are not preserved, since the rename replaces the inode.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), Error> {
    use std::io::Write as _;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let target = follow_links(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a symlink cycle", path.display()),
        )
    })?;
    let permissions = match std::fs::symlink_metadata(&target) {
        Ok(meta) if !meta.is_file() => return Err(not_a_regular_file(&target).into()),
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };

    let file_name = target
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned());
    let tmp = target.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::File::create_new(&tmp).and_then(|mut file| {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &target)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that appears between the existence check and the write (the
    /// user saving their own, another process) is never replaced, and no
    /// temporary file is left behind.
    /// #348 adversarial M2 (lead decision D2-3): a TOML parse error
    /// quotes the offending line with its zero-width character escaped,
    /// as every other warning does now.
    #[test]
    fn a_parse_error_escapes_zero_width_characters() {
        let Err(Error::Parse(error)) = Config::from_toml_lenient("a\u{200B} = 1\n") else {
            panic!("U+200B is not a bare key character");
        };
        assert!(
            error.message.contains("\\u{200b}") && !error.message.contains('\u{200B}'),
            "{}",
            error.message
        );
    }

    #[test]
    fn writing_the_default_never_replaces_a_file_created_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(write_new(&path, "first").unwrap());
        assert!(!write_new(&path, "second").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }

    /// A stale temporary file with the name the next write would use (left
    /// by a crash of an earlier process with the same id) is skipped, not
    /// mistaken for an existing `config.toml`.
    #[test]
    fn a_stale_temporary_file_does_not_count_as_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let next = NEW_FILE_SEQUENCE.load(Ordering::Relaxed);
        let stale: Vec<_> = (next..next + 4)
            .map(|n| {
                dir.path()
                    .join(format!(".config.toml.{}.{n}.new", std::process::id()))
            })
            .collect();
        for file in &stale {
            std::fs::write(file, "stale").unwrap();
        }
        assert!(write_new(&path, "fresh").unwrap(), "created");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        for file in &stale {
            assert_eq!(
                std::fs::read_to_string(file).unwrap(),
                "stale",
                "left alone"
            );
        }
    }

    #[test]
    fn default_template_matches_default_config() {
        let loaded = Config::from_toml_lenient(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(loaded.warnings, Vec::new());
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn unknown_keys_warn_and_do_not_fail() {
        let text = "[appearance]\nfnt_size = 3\nfont_size = 15\n[nope]\nx = 1\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(
            loaded.warnings,
            vec![
                Warning::UnknownKey {
                    path: "appearance.fnt_size".into()
                },
                Warning::UnknownKey {
                    path: "nope".into()
                },
            ]
        );
        assert_eq!(loaded.config.appearance.font_size, 15.0);
    }

    #[test]
    fn unknown_key_warnings_are_bounded_in_count_and_size() {
        use std::fmt::Write as _;

        let long = "k".repeat(4 << 20);
        let loaded = Config::from_toml_lenient(&format!("{long} = 1\n")).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        let Warning::UnknownKey { path } = &loaded.warnings[0] else {
            panic!("{:?}", loaded.warnings[0]);
        };
        assert!(path.len() <= MAX_WARNING_PATH_LEN, "{}", path.len());
        assert!(path.ends_with("(truncated)"));

        let mut many = String::new();
        for i in 0..MAX_UNKNOWN_KEY_WARNINGS + 10 {
            writeln!(many, "k{i} = 1").unwrap();
        }
        let loaded = Config::from_toml_lenient(&many).unwrap();
        assert_eq!(loaded.warnings.len(), MAX_UNKNOWN_KEY_WARNINGS + 1);
        assert_eq!(
            loaded.warnings[MAX_UNKNOWN_KEY_WARNINGS],
            Warning::UnknownKeysOmitted { count: 10 }
        );
        assert_eq!(
            loaded.warnings[MAX_UNKNOWN_KEY_WARNINGS].to_string(),
            "10 more unknown keys ignored"
        );
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn negative_zero_opacity_is_stored_as_zero() {
        let loaded = Config::from_toml_lenient("[appearance]\nopacity = -0.0\n").unwrap();
        assert_eq!(loaded.warnings, Vec::new());
        assert!(loaded.config.appearance.opacity.is_sign_positive());
        assert!(loaded.config.to_toml().unwrap().contains("opacity = 0.0"));
    }

    #[test]
    fn type_mismatch_is_an_error() {
        let err = Config::from_toml_lenient("[appearance]\nfont_size = \"big\"\n").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
        // The deserializer works on the parsed tree, not the text (#233
        // item 6); the error still names the line and quotes it.
        let shown = err.to_string();
        assert!(shown.contains("line 2"), "{shown}");
        assert!(shown.contains("font_size = \"big\""), "{shown}");
        let Error::Parse(parse) = &err else {
            panic!("{err:?}")
        };
        assert_eq!(parse.span(), Some(25..30));
    }

    /// [`CONFIG_KEYS`] are exactly the top-level keys of [`Config`], so
    /// the position tree keeps every value a warning can point into.
    #[test]
    fn config_keys_are_the_top_level_keys() {
        let table = toml::Table::try_from(Config::default()).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut expected = CONFIG_KEYS.to_vec();
        keys.sort_unstable();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    /// Unknown top-level tables keep their place among the warnings
    /// although their values are not copied for the lookups.
    #[test]
    fn unknown_top_level_tables_keep_document_order() {
        let text = "[zeta]\nk = 1\n[appearance]\nfnt = 1\n[alpha.beta]\nk = 1\n[terminal]\nclipboard_write = 1\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        let shown: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "unknown key `zeta` ignored",
                "unknown key `appearance.fnt` ignored",
                "unknown key `alpha` ignored",
                "`terminal.clipboard_write` out of range: 1 replaced by false, clipboard writes refused (expected true or false)",
            ]
        );
    }

    /// A lenient value written over several lines, or holding a tab or a
    /// C1 control, gives a one-line warning with the controls escaped
    /// (S8; #233 item 3).
    #[test]
    fn a_multi_line_value_gives_a_one_line_warning() {
        for (value, shown) in [
            ("\"\"\"a\nb\"\"\"", "\"\"\"a\\nb\"\"\""),
            ("[\n  1,\n  2,\n]", "[\\n  1,\\n  2,\\n]"),
            ("\"a\tb\"", "\"a\\tb\""),
            ("'a\u{85}b\u{9b}c'", "'a\\u{85}b\\u{9b}c'"),
            ("\"\"\"a\r\nb\"\"\"", "\"\"\"a\\r\\nb\"\"\""),
        ] {
            for key in [
                "[terminal]\nclipboard_write",
                "[appearance]\ncursor_blink",
                "[appearance]\ncursor_shape",
            ] {
                let text = format!("{key} = {value}\n");
                let loaded = Config::from_toml_lenient(&text).unwrap();
                let [warning @ Warning::OutOfRange { detail, .. }] = &loaded.warnings[..] else {
                    panic!("{text:?}: {:?}", loaded.warnings);
                };
                assert!(detail.starts_with(shown), "{text:?}: {detail}");
                let line = warning.to_string();
                assert!(!line.chars().any(char::is_control), "{text:?}: {line:?}");
            }
        }
    }

    /// A long multi-line value is escaped and still capped at
    /// [`MAX_WARNING_PATH_LEN`] bytes of value, with the truncation marker.
    #[test]
    fn an_escaped_value_stays_capped() {
        for unit in ["\n", "a", "\n\t", "é\n"] {
            let value = format!("\"\"\"{}\"\"\"", unit.repeat(1 << 16));
            let text = format!("[terminal]\nclipboard_write = {value}\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let [Warning::OutOfRange { detail, .. }] = &loaded.warnings[..] else {
                panic!("{:?}", loaded.warnings);
            };
            let written = detail
                .split_once(" replaced by")
                .map_or(detail.as_str(), |(written, _)| written);
            assert!(
                written.len() <= MAX_WARNING_PATH_LEN,
                "{unit:?}: {}",
                written.len()
            );
            assert!(written.ends_with("(truncated)"), "{unit:?}: {written}");
            assert!(!detail.chars().any(char::is_control), "{unit:?}");
        }
    }

    /// An unknown key with a control character in it (a quoted key with an
    /// escape) is escaped in its warning too.
    #[test]
    fn an_unknown_key_with_a_newline_gives_a_one_line_warning() {
        let loaded = Config::from_toml_lenient("[appearance]\n\"a\\nb\" = 1\n").unwrap();
        assert_eq!(
            loaded.warnings,
            [Warning::UnknownKey {
                path: "appearance.a\\nb".into()
            }]
        );
    }

    proptest::proptest! {
        /// [`escaped`] never leaves a control character, a line separator
        /// or a character that shows as nothing (bidi, zero-width, other
        /// default-ignorables), never exceeds its cap, and equals escaping
        /// the whole text before the cut.
        #[test]
        fn escaping_is_one_line_and_capped(
            text in "\\PC{0,40}|[\\x00-\\x7f\\u{80}-\\u{ad}é\\u{34f}\\u{61c}\\u{180b}-\\u{180f}\\u{200b}-\\u{200f}\\u{2028}-\\u{202f}\\u{2060}-\\u{2069}\\u{fe00}-\\u{fe0f}\\u{feff}\\u{e0001}\\u{e0100}]{0,600}",
            max in 16usize..300,
        ) {
            let out = escaped(&text, max);
            proptest::prop_assert!(out.len() <= max, "{} > {max}", out.len());
            proptest::prop_assert!(!out.chars().any(char::is_control), "{:?}", out);
            proptest::prop_assert!(
                !out.chars().any(|c| matches!(c, '\u{2028}' | '\u{2029}') || is_invisible(c)),
                "{:?}", out
            );
            // The reference: escape the whole text, then cut (for text
            // without controls, plain truncation). Spelled out rather than
            // through `needs_unicode_escape`, so a code point dropped from
            // it is caught.
            let full: String = text
                .chars()
                .flat_map(|c| {
                    let escape: Vec<char> = if c.is_control() {
                        c.escape_debug().collect()
                    } else if matches!(
                        c,
                        '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{115f}' | '\u{1160}'
                            | '\u{17b4}' | '\u{17b5}' | '\u{180b}'..='\u{180f}'
                            | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
                            | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}'
                            | '\u{feff}' | '\u{ffa0}' | '\u{fff0}'..='\u{fff8}'
                            | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
                            | '\u{e0000}'..='\u{e0fff}'
                    ) {
                        c.escape_unicode().collect()
                    } else {
                        vec![c]
                    };
                    escape
                })
                .collect();
            proptest::prop_assert_eq!(out, truncated(&full, max));
        }
    }

    /// #262 items 4 and 10: a line separator, a right-to-left override and
    /// a raw ESC reach neither a warning nor a parse error unescaped, and
    /// the parse error (which quotes the line) is one line.
    #[test]
    fn separators_bidi_controls_and_esc_are_escaped_in_warnings_and_parse_errors() {
        let hostile = "x\u{2028}y\u{202e}z\u{1b}[31m";
        let raw = |text: &str| {
            text.chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || is_invisible(c))
        };
        // A warning naming a key: an unknown quoted key, whose escapes
        // TOML decodes (raw controls are not valid inside a TOML string).
        let spelled = r"x\u2028y\u202Ez\u001B[31m";
        // A warning quoting the source text of a value: U+2028 and U+202E
        // are valid raw inside a TOML string, ESC only as an escape.
        let quoted = "x\u{2028}y\u{202e}z\\u001B[31m";
        for (text, expected) in [
            (
                format!("[appearance]\n\"{spelled}\" = 1\n"),
                "x\\u{2028}y\\u{202e}z\\u{1b}[31m",
            ),
            (
                format!("[terminal]\nclipboard_write = \"{quoted}\"\n"),
                "x\\u{2028}y\\u{202e}z",
            ),
        ] {
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let [warning] = &loaded.warnings[..] else {
                panic!("{text:?}: {:?}", loaded.warnings);
            };
            let line = warning.to_string();
            assert!(!raw(&line), "{text:?}: {line:?}");
            assert!(line.contains(expected), "{line}");
        }
        // A parse error quoting the line: a bare key with the hostile
        // characters in a comment after a syntax error on the same line.
        let text = format!("[terminal]\nclipboard_write = off # {hostile}\n");
        let error = Config::from_toml_lenient(&text).unwrap_err();
        let line = error.to_string();
        assert!(!raw(&line), "{line:?}");
        assert!(
            line.contains("\\u{2028}") && line.contains("\\u{1b}"),
            "{line}"
        );
    }

    #[test]
    fn syntax_error_is_an_error() {
        assert!(Config::from_toml_lenient("[appearance\n").is_err());
    }

    #[test]
    fn integer_outside_its_type_is_an_error() {
        for text in [
            "[terminal]\nscrollback_lines = -5\n",
            "[terminal]\nscrollback_lines = 99999999999\n",
        ] {
            let err = Config::from_toml_lenient(text).unwrap_err();
            assert!(matches!(err, Error::Parse(_)), "{text}: {err}");
        }
    }

    #[test]
    fn cursor_shape_parses_its_three_values_and_defaults_to_block() {
        use crate::CursorShape;

        assert_eq!(
            Config::default().appearance.cursor_shape,
            CursorShape::Block
        );
        for shape in CursorShape::ALL {
            let text = format!("[appearance]\ncursor_shape = \"{}\"\n", shape.as_str());
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.appearance.cursor_shape, shape);
            assert_eq!(loaded.warnings, []);
            // `as_str` is what serde writes, and it reads back.
            let written = loaded.config.to_toml().unwrap();
            assert!(written.contains(&text[13..]), "{written}");
            assert_eq!(Config::from_toml(&written).unwrap(), loaded.config);
        }
    }

    #[test]
    fn remote_indicator_parses_its_two_values_and_defaults_to_title() {
        use crate::RemoteIndicator;

        assert_eq!(
            Config::default().general.remote_indicator,
            RemoteIndicator::Title
        );
        for value in RemoteIndicator::ALL {
            let text = format!("[general]\nremote_indicator = \"{}\"\n", value.as_str());
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.general.remote_indicator, value);
            assert_eq!(loaded.warnings, []);
            let written = loaded.config.to_toml().unwrap();
            assert!(written.contains(&text[10..]), "{written}");
            assert_eq!(Config::from_toml(&written).unwrap(), loaded.config);
        }
    }

    /// #205: an unknown value falls back to `"title"` with a warning that
    /// names the key, in document order, and every other key applies.
    #[test]
    fn an_invalid_remote_indicator_warns_and_shows_the_prefix() {
        use crate::RemoteIndicator;

        for bad in ["\"Off\"", "\"none\"", "\"\"", "false", "0", "[\"off\"]"] {
            let text = format!(
                "[general]\nconfirm_close = false\nremote_indicator = {bad}\n\
                 [appearance]\ncursor_shape = \"bar\"\n"
            );
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let general = &loaded.config.general;
            assert_eq!(general.remote_indicator, RemoteIndicator::Title, "{bad}");
            assert!(!general.confirm_close, "{bad}");
            let paths: Vec<&str> = loaded
                .warnings
                .iter()
                .map(|w| match w {
                    Warning::OutOfRange { path, .. } => path.as_str(),
                    other => panic!("{bad}: {other:?}"),
                })
                .collect();
            assert_eq!(
                paths,
                ["general.remote_indicator", "appearance.cursor_shape"],
                "{bad}"
            );
            let Warning::OutOfRange { detail, .. } = &loaded.warnings[0] else {
                unreachable!();
            };
            assert!(detail.contains("replaced by \"title\""), "{bad}: {detail}");
            assert!(detail.contains("\"off\""), "{bad}: {detail}");
        }
        // A hostile value is escaped in the warning (S8).
        let loaded =
            Config::from_toml_lenient("[general]\nremote_indicator = \"a\\u001b[2Jb\"\n").unwrap();
        let Warning::OutOfRange { detail, .. } = &loaded.warnings[0] else {
            panic!("{:?}", loaded.warnings);
        };
        assert!(!detail.contains('\u{1b}'), "{detail}");
    }

    #[test]
    fn cursor_blink_parses_and_defaults_to_false() {
        assert!(!Config::default().appearance.cursor_blink);
        for blink in [true, false] {
            let text = format!("[appearance]\ncursor_blink = {blink}\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.appearance.cursor_blink, blink);
            assert_eq!(loaded.warnings, []);
        }
    }

    /// Unlike a mistyped `shell_integration`, a mistyped shape is not a
    /// type error: the key falls back to its default with a warning that
    /// names it, and every other key still applies.
    #[test]
    fn an_invalid_cursor_shape_warns_and_keeps_the_default() {
        use crate::CursorShape;

        for bad in [
            "\"bar\"",
            "\"Block\"",
            "\"\"",
            "3",
            "\"I-beam\"",
            "[\"beam\"]",
        ] {
            let text = format!(
                "[appearance]\nfont_size = 15\ncursor_shape = {bad}\ncursor_blink = true\n\
                 [terminal]\nscrollback_lines = 7\n"
            );
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let appearance = &loaded.config.appearance;
            assert_eq!(appearance.cursor_shape, CursorShape::Block, "{bad}");
            assert_eq!(appearance.font_size, 15.0, "{bad}");
            assert!(appearance.cursor_blink, "{bad}");
            assert_eq!(loaded.config.terminal.scrollback_lines, 7, "{bad}");
            assert_eq!(loaded.warnings.len(), 1, "{bad}: {:?}", loaded.warnings);
            let Warning::OutOfRange { path, detail } = &loaded.warnings[0] else {
                panic!("{bad}: {:?}", loaded.warnings);
            };
            assert_eq!(path, "appearance.cursor_shape", "{bad}");
            assert!(detail.contains("\"block\""), "{bad}: {detail}");
        }
        // In a profile too, named by its index.
        let text = "[[profiles]]\nname = \"a\"\n[profiles.appearance]\ncursor_shape = \"bar\"\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        // Left unset, so the profile uses `[appearance]`'s shape.
        let appearance = loaded.config.profiles[0].appearance.as_ref().unwrap();
        assert_eq!(appearance.cursor_shape, None);
        assert!(matches!(
            &loaded.warnings[..],
            [Warning::OutOfRange { path, .. }] if path == "profiles[0].appearance.cursor_shape"
        ));
        // A key that only mentions the name is not checked.
        let loaded = Config::from_toml_lenient("[appearance]\ntheme = \"cursor_shape\"\n").unwrap();
        assert!(matches!(&loaded.warnings[..], []), "{:?}", loaded.warnings);
    }

    #[test]
    fn clipboard_write_parses_and_defaults_to_true() {
        assert!(Config::default().terminal.clipboard_write);
        assert!(
            Config::from_toml_lenient("")
                .unwrap()
                .config
                .terminal
                .clipboard_write
        );
        for value in [true, false] {
            let text = format!("[terminal]\nclipboard_write = {value}\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.terminal.clipboard_write, value);
            assert_eq!(loaded.warnings, []);
        }
    }

    /// A clipboard setting that is not a boolean fails closed
    /// (D-FAIL-CLOSED): writes are refused, with a warning naming the key;
    /// every other key applies.
    #[test]
    fn an_invalid_clipboard_write_warns_and_refuses_writes() {
        for bad in ["\"false\"", "0", "\"yes\"", "\"no\"", "[false]", "1.5", "1"] {
            let text = format!(
                "[terminal]\nscrollback_lines = 7\nclipboard_write = {bad}\n\
                 [appearance]\nfont_size = 15\n"
            );
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert!(!loaded.config.terminal.clipboard_write, "{bad}");
            assert_eq!(loaded.config.terminal.scrollback_lines, 7, "{bad}");
            assert_eq!(loaded.config.appearance.font_size, 15.0, "{bad}");
            let [Warning::OutOfRange { path, detail }] = &loaded.warnings[..] else {
                panic!("{bad}: {:?}", loaded.warnings);
            };
            assert_eq!(path, "terminal.clipboard_write", "{bad}");
            assert!(detail.contains("true or false"), "{bad}: {detail}");
            assert!(detail.starts_with(bad), "{bad}: {detail}");
            assert!(detail.contains("replaced by false"), "{bad}: {detail}");
        }
        // A key that only mentions the name is not checked.
        let loaded =
            Config::from_toml_lenient("[appearance]\ntheme = \"clipboard_write\"\n").unwrap();
        assert!(matches!(&loaded.warnings[..], []), "{:?}", loaded.warnings);
    }

    /// The first load and a live reload go through the same lenient load:
    /// a mistyped switch refuses writes in both, where the last good value
    /// was `true`.
    #[test]
    fn clipboard_write_fails_closed_on_first_load_and_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::ConfigPaths::new(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        write_atomic(
            &paths.config_file(),
            "[terminal]\nclipboard_write = \"false\"\n",
        )
        .unwrap();
        let store = crate::ConfigStore::open(paths.clone()).unwrap();
        assert!(!store.config().terminal.clipboard_write, "first load");
        assert_eq!(store.warnings().len(), 1);

        write_atomic(&paths.config_file(), "[terminal]\nclipboard_write = true\n").unwrap();
        store.reload().unwrap();
        assert!(store.config().terminal.clipboard_write);
        for bad in ["\"yes\"", "0"] {
            write_atomic(
                &paths.config_file(),
                &format!("[terminal]\nclipboard_write = {bad}\n"),
            )
            .unwrap();
            store.reload().unwrap();
            assert!(!store.config().terminal.clipboard_write, "reload {bad}");
            assert!(
                matches!(
                    &store.warnings()[..],
                    [Warning::OutOfRange { path, .. }] if path == "terminal.clipboard_write"
                ),
                "{:?}",
                store.warnings()
            );
            write_atomic(&paths.config_file(), "[terminal]\nclipboard_write = true\n").unwrap();
            store.reload().unwrap();
        }
    }

    /// `cursor_blink` joins the lenient keys: a mistyped value warns and
    /// keeps the default instead of failing the whole load. `blur` keeps
    /// the strict path (a type mismatch is an error).
    #[test]
    fn an_invalid_cursor_blink_warns_and_keeps_the_default() {
        for bad in ["\"yes\"", "1", "\"true\"", "[true]"] {
            let text = format!(
                "[appearance]\ncursor_blink = {bad}\nfont_size = 15\ncursor_shape = \"beam\"\n"
            );
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let appearance = &loaded.config.appearance;
            assert!(!appearance.cursor_blink, "{bad}");
            assert_eq!(appearance.font_size, 15.0, "{bad}");
            assert_eq!(appearance.cursor_shape, crate::CursorShape::Beam, "{bad}");
            let [Warning::OutOfRange { path, detail }] = &loaded.warnings[..] else {
                panic!("{bad}: {:?}", loaded.warnings);
            };
            assert_eq!(path, "appearance.cursor_blink", "{bad}");
            assert!(detail.contains("true or false"), "{bad}: {detail}");
        }
        let text = "[[profiles]]\nname = \"a\"\n[profiles.appearance]\ncursor_blink = 2\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert!(matches!(
            &loaded.warnings[..],
            [Warning::OutOfRange { path, .. }] if path == "profiles[0].appearance.cursor_blink"
        ));
        assert!(Config::from_toml_lenient("[appearance]\nblur = \"yes\"\n").is_err());
    }

    /// Lenient keys are found in the parsed document, not in its text: a
    /// quoted key, or one spelled with an escape, warns too (#168).
    #[test]
    fn quoted_and_escaped_lenient_keys_warn() {
        for (key, value, path) in [
            ("\"cursor_shape\"", "\"bar\"", "appearance.cursor_shape"),
            (
                "\"cursor\\u005fshape\"",
                "\"bar\"",
                "appearance.cursor_shape",
            ),
            ("'cursor_blink'", "\"yes\"", "appearance.cursor_blink"),
            (
                "\"cursor\\u005Fblink\"",
                "\"yes\"",
                "appearance.cursor_blink",
            ),
        ] {
            let text = format!("[appearance]\n{key} = {value}\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert!(
                matches!(&loaded.warnings[..], [Warning::OutOfRange { path: p, .. }] if p == path),
                "{text}: {:?}",
                loaded.warnings
            );
        }
        for key in ["\"clipboard_write\"", "\"clipboard\\u005Fwrite\""] {
            let text = format!("[terminal]\n{key} = \"no\"\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert!(!loaded.config.terminal.clipboard_write, "{text}");
            assert_eq!(loaded.warnings.len(), 1, "{text}: {:?}", loaded.warnings);
        }
        // Dotted keys and inline tables reach the same values.
        let loaded = Config::from_toml_lenient(
            "appearance.cursor_shape = \"bar\"\nterminal = { clipboard_write = 0 }\n",
        )
        .unwrap();
        let paths: Vec<_> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, .. } => path.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            ["appearance.cursor_shape", "terminal.clipboard_write"]
        );
    }

    /// `Loaded::warnings` is in document order, whichever check found the
    /// problem: unknown keys, lenient values and normalised ranges
    /// interleave as their keys appear (#168).
    #[test]
    fn warnings_come_in_document_order() {
        let text = "\
            [[profiles]]\n\
            name = \"a\"\n\
            [profiles.appearance]\n\
            cursor_shape = \"bar\"\n\
            bogus_one = 1\n\
            font_size = 1000\n\
            [appearance]\n\
            cursor_blink = \"yes\"\n\
            bogus_two = 2\n\
            cursor_shape = \"Block\"\n\
            opacity = 2.0\n\
            [terminal]\n\
            clipboard_write = 0\n\
            scrollback_lines = 4000000000\n\
            [bogus_three]\n\
            x = 1\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        let paths: Vec<_> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, .. } | Warning::UnknownKey { path } => path.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                "profiles[0].appearance.cursor_shape",
                "profiles[0].appearance.bogus_one",
                "profiles[0].appearance.font_size",
                "appearance.cursor_blink",
                "appearance.bogus_two",
                "appearance.cursor_shape",
                "appearance.opacity",
                "terminal.clipboard_write",
                "terminal.scrollback_lines",
                "bogus_three",
            ]
        );
    }

    #[test]
    fn out_of_range_values_are_normalised() {
        let text =
            "[appearance]\nfont_size = 0\nopacity = 1.5\n[terminal]\nscrollback_lines = 99999999\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(
            loaded.config.appearance.font_size,
            crate::Appearance::MIN_FONT_SIZE
        );
        assert_eq!(loaded.config.appearance.opacity, 1.0);
        assert_eq!(
            loaded.config.terminal.scrollback_lines,
            crate::Terminal::MAX_SCROLLBACK_LINES
        );
        assert_eq!(loaded.warnings.len(), 3);
        assert!(
            matches!(&loaded.warnings[0], Warning::OutOfRange { path, .. } if path == "appearance.font_size")
        );
    }

    #[test]
    fn font_size_is_clamped_to_a_sane_range() {
        for (text, expected) in [
            (
                "[appearance]\nfont_size = 1e30\n",
                crate::Appearance::MAX_FONT_SIZE,
            ),
            (
                "[appearance]\nfont_size = 1e-30\n",
                crate::Appearance::MIN_FONT_SIZE,
            ),
            (
                "[appearance]\nfont_size = -7\n",
                crate::Appearance::MIN_FONT_SIZE,
            ),
            ("[appearance]\nfont_size = nan\n", 13.0),
            ("[appearance]\nfont_size = inf\n", 13.0),
        ] {
            let loaded = Config::from_toml_lenient(text).unwrap();
            assert_eq!(loaded.config.appearance.font_size, expected, "{text}");
            assert_eq!(loaded.warnings.len(), 1, "{text}");
            assert!(
                matches!(&loaded.warnings[0], Warning::OutOfRange { path, .. } if path == "appearance.font_size"),
                "{text}: {:?}",
                loaded.warnings
            );
        }
        let loaded = Config::from_toml_lenient("[appearance]\nfont_size = 4\n").unwrap();
        assert_eq!(loaded.warnings, Vec::new(), "bounds are inclusive");
        let loaded = Config::from_toml_lenient("[appearance]\nfont_size = 512\n").unwrap();
        assert_eq!(loaded.warnings, Vec::new(), "bounds are inclusive");
    }

    #[test]
    fn dim_inactive_panes_is_clamped_to_its_range() {
        let max = crate::Appearance::MAX_DIM_INACTIVE_PANES;
        for (text, expected) in [
            ("[appearance]\ndim_inactive_panes = 0.9\n", max),
            ("[appearance]\ndim_inactive_panes = 1e30\n", max),
            ("[appearance]\ndim_inactive_panes = -0.1\n", 0.0),
            ("[appearance]\ndim_inactive_panes = nan\n", 0.0),
            ("[appearance]\ndim_inactive_panes = inf\n", 0.0),
            ("[appearance]\ndim_inactive_panes = -inf\n", 0.0),
        ] {
            let loaded = Config::from_toml_lenient(text).unwrap();
            let dim = loaded.config.appearance.dim_inactive_panes;
            assert_eq!(dim.to_bits(), f32::to_bits(expected), "{text}: {dim}");
            assert!(
                matches!(&loaded.warnings[..], [Warning::OutOfRange { path, .. }] if path == "appearance.dim_inactive_panes"),
                "{text}: {:?}",
                loaded.warnings
            );
        }
        for (text, expected) in [
            ("", 0.0),
            ("[appearance]\ndim_inactive_panes = 0\n", 0.0),
            ("[appearance]\ndim_inactive_panes = -0.0\n", 0.0),
            ("[appearance]\ndim_inactive_panes = 0.3\n", 0.3),
            ("[appearance]\ndim_inactive_panes = 0.8\n", max),
        ] {
            let loaded = Config::from_toml_lenient(text).unwrap();
            let dim = loaded.config.appearance.dim_inactive_panes;
            assert_eq!(dim.to_bits(), f32::to_bits(expected), "{text}: {dim}");
            assert_eq!(loaded.warnings, Vec::new(), "{text}");
        }
    }

    /// Dimming belongs to the window, not to a session, so a profile's
    /// value is dropped with a warning rather than silently ignored.
    #[test]
    fn dim_inactive_panes_in_a_profile_is_ignored_with_a_warning() {
        let text = "[[profiles]]\nname = \"a\"\n[profiles.appearance]\ndim_inactive_panes = 0.5\n";
        let text = format!("[appearance]\ndim_inactive_panes = 0.25\n{text}");
        let loaded = Config::from_toml_lenient(&text).unwrap();
        assert_eq!(
            loaded.warnings,
            [Warning::OutOfRange {
                path: "profiles[0].appearance.dim_inactive_panes".into(),
                detail: "0.5 ignored (set it in [appearance], not in a profile)".into(),
            }]
        );
        // The window's value, whichever profile.
        let appearance = loaded.config.resolve_profile(Some("a")).appearance;
        assert_eq!(appearance.dim_inactive_panes.to_bits(), 0.25f32.to_bits());
        // Any value, also one that is not a number, warns and nothing more.
        let loaded = Config::from_toml_lenient(
            "[[profiles]]\nname = \"a\"\n[profiles.appearance]\ndim_inactive_panes = \"x\"\n",
        )
        .unwrap();
        assert!(
            matches!(&loaded.warnings[..], [Warning::OutOfRange { path, .. }] if path == "profiles[0].appearance.dim_inactive_panes"),
            "{:?}",
            loaded.warnings
        );
    }

    /// ADR 0045: an integer 0..=64, default 0; above 64 lowered with one
    /// warning; a negative or fractional value fails the load, as for
    /// `scrollback_lines`.
    #[test]
    fn blur_radius_is_an_integer_up_to_64() {
        let max = crate::Appearance::MAX_BLUR_RADIUS;
        assert_eq!(max, 64);
        assert_eq!(Config::default().appearance.blur_radius, 0);
        for (text, expected) in [
            ("", 0),
            ("[appearance]\nblur_radius = 0\n", 0),
            ("[appearance]\nblur_radius = 1\n", 1),
            ("[appearance]\nblur_radius = 20\n", 20),
            ("[appearance]\nblur_radius = 64\n", max),
        ] {
            let loaded = Config::from_toml_lenient(text).unwrap();
            assert_eq!(loaded.config.appearance.blur_radius, expected, "{text}");
            assert_eq!(loaded.warnings, Vec::new(), "{text}");
        }
        for (text, written) in [
            ("[appearance]\nblur_radius = 65\n", 65u64),
            ("[appearance]\nblur_radius = 1000\n", 1000),
            (
                "[appearance]\nblur_radius = 4294967295\n",
                u64::from(u32::MAX),
            ),
        ] {
            let loaded = Config::from_toml_lenient(text).unwrap();
            assert_eq!(loaded.config.appearance.blur_radius, max, "{text}");
            assert_eq!(
                loaded.warnings,
                [Warning::OutOfRange {
                    path: "appearance.blur_radius".into(),
                    detail: format!("{written} clamped to 64"),
                }],
                "{text}"
            );
        }
        for text in [
            "[appearance]\nblur_radius = -1\n",
            "[appearance]\nblur_radius = 20.0\n",
            "[appearance]\nblur_radius = 0.5\n",
            "[appearance]\nblur_radius = 4294967296\n",
            "[appearance]\nblur_radius = true\n",
            "[appearance]\nblur_radius = \"20\"\n",
        ] {
            let err = Config::from_toml_lenient(text).unwrap_err();
            assert!(matches!(err, Error::Parse(_)), "{text}: {err}");
        }
        // The clamped value is what is written back, and it reads back
        // without a warning.
        let loaded = Config::from_toml_lenient("[appearance]\nblur_radius = 99\n").unwrap();
        let written = loaded.config.to_toml().unwrap();
        assert!(written.contains("blur_radius = 64"), "{written}");
        assert_eq!(Config::from_toml_lenient(&written).unwrap().warnings, []);
    }

    /// The window is blurred, not a session, so a profile's radius is
    /// dropped with a warning and `[appearance]`'s applies to every
    /// profile, as for `dim_inactive_panes`.
    #[test]
    fn blur_radius_in_a_profile_is_ignored_with_a_warning() {
        let text = "[appearance]\nblur_radius = 12\n[[profiles]]\nname = \"a\"\n\
                    [profiles.appearance]\nblur_radius = 30\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(
            loaded.warnings,
            [Warning::OutOfRange {
                path: "profiles[0].appearance.blur_radius".into(),
                detail: "30 ignored (set it in [appearance], not in a profile)".into(),
            }]
        );
        assert_eq!(
            loaded
                .config
                .resolve_profile(Some("a"))
                .appearance
                .blur_radius,
            12
        );
        // Any value, also one that would fail `[appearance]`'s load, warns
        // and nothing more: the profile has no such key.
        for value in ["-1", "20.0", "\"x\"", "1000"] {
            let loaded = Config::from_toml_lenient(&format!(
                "[[profiles]]\nname = \"a\"\n[profiles.appearance]\nblur_radius = {value}\n"
            ))
            .unwrap();
            assert!(
                matches!(&loaded.warnings[..], [Warning::OutOfRange { path, .. }] if path == "profiles[0].appearance.blur_radius"),
                "{value}: {:?}",
                loaded.warnings
            );
            assert_eq!(
                loaded
                    .config
                    .resolve_profile(Some("a"))
                    .appearance
                    .blur_radius,
                0
            );
        }
        // Both window keys in one profile: one warning each, in order.
        let loaded = Config::from_toml_lenient(
            "[[profiles]]\nname = \"a\"\n[profiles.appearance]\nblur_radius = 5\n\
             dim_inactive_panes = 0.5\n",
        )
        .unwrap();
        let paths: Vec<_> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, .. } => path.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                "profiles[0].appearance.blur_radius",
                "profiles[0].appearance.dim_inactive_panes"
            ]
        );
    }

    /// #365: the three values read and write back, and a missing key is
    /// `"dark"`.
    #[test]
    fn app_appearance_parses_its_three_values_and_defaults_to_dark() {
        use crate::AppAppearance;

        assert_eq!(
            Config::default().appearance.app_appearance,
            AppAppearance::Dark
        );
        let loaded = Config::from_toml_lenient("[appearance]\nfont_size = 14.0\n").unwrap();
        assert_eq!(loaded.config.appearance.app_appearance, AppAppearance::Dark);
        assert_eq!(loaded.warnings, []);
        for (name, value) in [
            ("dark", AppAppearance::Dark),
            ("light", AppAppearance::Light),
            ("system", AppAppearance::System),
        ] {
            let text = format!("[appearance]\napp_appearance = \"{name}\"\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.appearance.app_appearance, value, "{name}");
            assert_eq!(loaded.warnings, [], "{name}");
            let written = loaded.config.to_toml().unwrap();
            assert!(written.contains(&text[13..]), "{written}");
            assert_eq!(Config::from_toml(&written).unwrap(), loaded.config);
        }
    }

    /// #365: an unknown value, another case or another type falls back to
    /// `"dark"` with one warning that names the key and the value as
    /// written, escaped (S8), and every other key applies.
    #[test]
    fn an_invalid_app_appearance_warns_once_and_is_dark() {
        use crate::AppAppearance;

        for bad in [
            "\"Light\"",
            "\"auto\"",
            "\"\"",
            "true",
            "1",
            "[\"light\"]",
            "{ a = 1 }",
        ] {
            let text = format!("[appearance]\nfont_size = 14.0\napp_appearance = {bad}\n");
            let loaded = Config::from_toml_lenient(&text).unwrap();
            let appearance = &loaded.config.appearance;
            assert_eq!(appearance.app_appearance, AppAppearance::Dark, "{bad}");
            assert!((appearance.font_size - 14.0).abs() < f32::EPSILON, "{bad}");
            assert_eq!(
                loaded.warnings,
                [Warning::OutOfRange {
                    path: "appearance.app_appearance".into(),
                    detail: format!(
                        "{bad} replaced by \"dark\" (expected \"dark\", \"light\", \"system\")"
                    ),
                }],
                "{bad}"
            );
        }
        // A bidi override and a newline in the value are escaped in the
        // warning, so it cannot reorder or break the log line.
        let text = "[appearance]\napp_appearance = \"li\u{202e}ght\\n\"\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(loaded.config.appearance.app_appearance, AppAppearance::Dark);
        let [Warning::OutOfRange { path, detail }] = &loaded.warnings[..] else {
            panic!("{:?}", loaded.warnings);
        };
        assert_eq!(path, "appearance.app_appearance");
        assert!(!detail.contains('\u{202e}'), "{detail:?}");
        assert!(
            detail.starts_with("\"li\\u{202e}ght\\n\" replaced by"),
            "{detail:?}"
        );
        // A huge value is capped as every warning value is.
        let text = format!(
            "[appearance]\napp_appearance = \"{}\"\n",
            "x".repeat(100_000)
        );
        let loaded = Config::from_toml_lenient(&text).unwrap();
        let [Warning::OutOfRange { detail, .. }] = &loaded.warnings[..] else {
            panic!("{:?}", loaded.warnings);
        };
        assert!(
            detail.len() < MAX_WARNING_PATH_LEN + 200,
            "{}",
            detail.len()
        );
    }

    /// #365: the app has one appearance, so a profile's is dropped with
    /// the window-level warning and `[appearance]`'s applies to every
    /// profile, as for `blur_radius`.
    #[test]
    fn app_appearance_in_a_profile_is_ignored_with_a_warning() {
        use crate::AppAppearance;

        let text = "[appearance]\napp_appearance = \"system\"\n[[profiles]]\nname = \"a\"\n\
                    [profiles.appearance]\napp_appearance = \"light\"\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(
            loaded.warnings,
            [Warning::OutOfRange {
                path: "profiles[0].appearance.app_appearance".into(),
                detail: "\"light\" ignored (set it in [appearance], not in a profile)".into(),
            }]
        );
        assert_eq!(
            loaded
                .config
                .resolve_profile(Some("a"))
                .appearance
                .app_appearance,
            AppAppearance::System
        );
        // Any value warns once, with the window-level warning only.
        for value in ["\"bogus\"", "3", "\"dark\""] {
            let loaded = Config::from_toml_lenient(&format!(
                "[[profiles]]\nname = \"a\"\n[profiles.appearance]\napp_appearance = {value}\n"
            ))
            .unwrap();
            assert!(
                matches!(&loaded.warnings[..], [Warning::OutOfRange { path, detail }]
                    if path == "profiles[0].appearance.app_appearance"
                        && detail.ends_with("ignored (set it in [appearance], not in a profile)")),
                "{value}: {:?}",
                loaded.warnings
            );
            assert_eq!(
                loaded
                    .config
                    .resolve_profile(Some("a"))
                    .appearance
                    .app_appearance,
                AppAppearance::Dark
            );
        }
    }

    #[test]
    fn unknown_key_paths_match_out_of_range_paths() {
        let text = "[[profiles]]\nname = \"a\"\n[profiles.appearance]\nbogus = 1\nfont_size = 0\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(
            loaded.warnings,
            vec![
                Warning::UnknownKey {
                    path: "profiles[0].appearance.bogus".into()
                },
                Warning::OutOfRange {
                    path: "profiles[0].appearance.font_size".into(),
                    detail: format!("0 clamped to {}", crate::Appearance::MIN_FONT_SIZE),
                },
            ]
        );
    }

    #[test]
    fn non_utf8_file_is_an_encoding_error_not_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"[appearance]\nfont_family = \"Men\xE9lo\"\n").unwrap();
        let err = load_file(&path).unwrap_err();
        assert!(matches!(err, Error::Utf8(_)), "{err}");
    }

    #[test]
    fn write_default_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        assert!(write_default_if_missing(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG_TOML);
        std::fs::write(&path, "[general]\nconfirm_close = false\n").unwrap();
        assert!(!write_default_if_missing(&path).unwrap());
        assert!(!load_file(&path).unwrap().config.general.confirm_close);
        assert_eq!(
            std::fs::read_dir(dir.path().join("nested"))
                .unwrap()
                .count(),
            1,
            "no temporary file left behind"
        );
    }

    #[test]
    fn parse_error_on_a_huge_line_is_bounded() {
        let text = format!("[appearance]\nfont_size = \"{}\n", "x".repeat(1 << 20));
        let err = Config::from_toml_lenient(&text).unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.len() <= MAX_PARSE_ERROR_LEN + "parse: ".len(),
            "{} bytes",
            shown.len()
        );
        assert!(shown.ends_with("(truncated)"), "{shown}");
        let Error::Parse(parse) = &err else {
            panic!("{err:?}")
        };
        assert_eq!(parse.message(), &shown["parse: ".len()..]);
        assert!(parse.span().is_some());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = "é".repeat(100);
        let out = truncated(&text, 20);
        assert!(out.len() <= 20);
        assert!(out.ends_with("… (truncated)"));
        assert_eq!(truncated("short", 20), "short");
    }

    #[test]
    fn concurrent_writers_never_corrupt_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let docs: Vec<String> = (0..8)
            .map(|i| format!("[appearance]\nfont_size = {}\n", 10 + i))
            .collect();
        for _round in 0..25 {
            std::thread::scope(|s| {
                for doc in &docs {
                    s.spawn(|| write_atomic(&path, doc).unwrap());
                }
            });
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert!(docs.contains(&on_disk), "corrupted: {on_disk:?}");
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary files left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let err = load_file(&path).unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput),
            "{err}"
        );
        let err = write_atomic(&path, "x").unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_written_through_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("tessera.toml");
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Dangling: counts as present, so the defaults are not written.
        assert!(!write_default_if_missing(&link).unwrap());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(!target.exists());

        // Writing through the link creates the target; the link survives.
        write_atomic(&link, "[appearance]\nfont_size = 15\n").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(load_file(&link).unwrap().config.appearance.font_size, 15.0);
        assert_eq!(
            load_file(&target).unwrap().config.appearance.font_size,
            15.0
        );
        assert_eq!(
            std::fs::read_dir(&dotfiles).unwrap().count(),
            1,
            "temporary file lives next to the target and is gone"
        );

        // A relative link, chained through another link, resolves the same.
        let hop = dir.path().join("hop.toml");
        std::os::unix::fs::symlink("dotfiles/tessera.toml", &hop).unwrap();
        let chained = dir.path().join("chained.toml");
        std::os::unix::fs::symlink("hop.toml", &chained).unwrap();
        assert_eq!(follow_links(&chained).unwrap().unwrap(), target);
        write_atomic(&chained, "[appearance]\nfont_size = 16\n").unwrap();
        assert_eq!(
            load_file(&target).unwrap().config.appearance.font_size,
            16.0
        );

        // A cycle is refused rather than looping.
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(follow_links(&a).unwrap().is_none());
        assert!(write_atomic(&a, "x").is_err());
    }

    #[test]
    fn oversized_file_is_rejected_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A sparse file: the size is set without writing the bytes.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_FILE_LEN + 1).unwrap();
        drop(file);
        let err = load_file(&path).unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput),
            "{err}"
        );
        assert!(err.to_string().contains("larger than"), "{err}");

        // Exactly at the limit is still read.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_FILE_LEN).unwrap();
        drop(file);
        let err = load_file(&path).unwrap_err();
        assert!(matches!(err, Error::Parse(_)), "NUL bytes: {err}");
    }

    /// The size guard holds on the open handle, not only on the `stat`
    /// before the open: a file that is small when checked and over the
    /// limit when read is rejected rather than read whole.
    #[test]
    fn oversized_handle_is_rejected_after_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_FILE_LEN + 1).unwrap();
        let err = check_regular_file(
            &path,
            &file.metadata().unwrap(),
            MAX_CONFIG_FILE_LEN,
            "config file",
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("larger than"), "{err}");
        assert_eq!(
            check_regular_file(
                &path,
                &std::fs::metadata(dir.path()).unwrap(),
                MAX_CONFIG_FILE_LEN,
                "config file"
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    /// A FIFO is rejected from its open handle too, without blocking, so
    /// one swapped in after the `stat` cannot hang the loader.
    #[cfg(unix)]
    #[test]
    fn fifo_handle_is_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let file = open_nonblocking(&path).expect("non-blocking open of a FIFO");
        let err = check_regular_file(
            &path,
            &file.metadata().unwrap(),
            MAX_CONFIG_FILE_LEN,
            "config file",
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_the_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&path, "[general]\nconfirm_close = false\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
        assert!(!load_file(&path).unwrap().config.general.confirm_close);

        // Through a symlink, the target's mode is what is kept.
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        write_atomic(&link, "[general]\nconfirm_close = true\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
    }

    #[test]
    fn write_default_reports_metadata_errors_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let err = write_default_if_missing(&not_a_dir.join("config.toml")).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&not_a_dir).unwrap(), "x");
    }
}
