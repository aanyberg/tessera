//! Writing settings back to `config.toml` without losing what the user
//! wrote by hand (ADR 0040).
//!
//! [`ConfigEditor`] opens the file, changes one key at a time
//! ([`ConfigEditor::set`], [`ConfigEditor::remove`], a profile's key by
//! the profile's name with [`ConfigEditor::set_in_profile`] and
//! [`ConfigEditor::remove_in_profile`], [`ConfigEditor::add_profile`], or
//! a whole new profile at once with
//! [`ConfigEditor::add_profile_with`]) and [`ConfigEditor::save`]s it. Every
//! comment, blank line, key order and spelling (quotes, dotted keys,
//! inline tables, number formats) that an edit does not touch comes out
//! byte for byte as it went in. An edit is checked before it is kept:
//!
//! * the lenient loader ([`Config::from_toml_lenient`]) reads the edited
//!   document, and an edit that makes it fail or adds a [`Warning`] (an
//!   unknown key, a value out of range, a key binding it would drop, a
//!   theme that does not exist) is refused with that warning;
//! * the edited document, read as plain TOML, must equal the old one with
//!   exactly this change applied, and must keep every comment of the old
//!   one except those of the lines it removed.
//!
//! Saving is atomic (a temporary file in the target's directory, flushed,
//! then renamed over it), follows a symlinked `config.toml` to its target,
//! keeps the file's permission bits, and refuses with
//! [`EditError::Conflict`] when the file changed on disk since it was
//! opened instead of overwriting that change.
//!
//! The module also lists the themes a settings window offers
//! ([`theme_entries`]) and imports colour schemes into `themes/`
//! ([`import_theme`]), with every name it hands out passed through
//! [`display_name`].

use std::collections::HashMap;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::load::{
    Logging, MAX_CONFIG_FILE_LEN, MAX_WARNING_PATH_LEN, cut_prefix, escaped, follow_links,
    needs_unicode_escape, read_capped,
};
use crate::theme::{MAX_THEME_DIR_ENTRIES, MAX_THEME_FILES, ThemeSource, clip_theme_name, slug};
use crate::{BUILT_IN_PROFILE_NAME, Config, Error, ParseError, Theme, Warning};

/// The value type [`ConfigEditor::set`] takes (`toml::Value`), so a
/// caller needs no `toml` dependency of its own (it deserializes from
/// JSON, for example).
pub use toml::Value as TomlValue;

/// Longest key path [`KeyPath`] parses, in bytes.
pub const MAX_KEY_PATH_LEN: usize = 1024;

/// Most steps (keys and indices) of a [`KeyPath`].
pub const MAX_KEY_PATH_STEPS: usize = 16;

/// Largest index a [`KeyPath`] holds (`profiles[N]`).
pub const MAX_KEY_PATH_INDEX: usize = 65_535;

/// Deepest nesting of arrays in a value [`ConfigEditor::set`] writes.
pub const MAX_VALUE_DEPTH: usize = 32;

/// Longest id [`import_theme`] returns (a [`slug`]), in
/// bytes.
pub const MAX_THEME_ID_LEN: usize = crate::theme::MAX_SLUG_LEN;

/// Longest name [`display_name`] returns, in bytes.
pub const MAX_DISPLAY_NAME_LEN: usize = 128;

/// Most keys besides the name [`ConfigEditor::add_profile_with`] sets in
/// the profile it adds: a profile has about twenty (S5).
pub const MAX_ADDED_PROFILE_KEYS: usize = 64;

/// A profile [`ConfigEditor::add_profile_with`] added.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddedProfile {
    /// Its place in the configuration the app reads (the `[[profiles]]`
    /// entries, then the profile files' profiles), which is also its key
    /// path index (`profiles[index].command`).
    pub index: usize,
    /// The name it was given: the base name, or the base name with the
    /// number that makes it unused.
    pub name: String,
}

/// One step of a [`KeyPath`]: a key of a table or an index into an array
/// (of values, or of tables such as `[[profiles]]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    /// A key of a table.
    Key(String),
    /// A zero-based array index.
    Index(usize),
}

/// The place of a value in `config.toml`, written as keys joined by `.`,
/// each bare (`A-Za-z0-9_-`) or in double quotes (with `\"`, `\\` and
/// `\u{XXXX}`, one to six hexadecimal digits, the only escapes), each
/// optionally followed by `[N]` indices:
/// `appearance.opacity`, `profiles[1].appearance.theme`, `keys."cmd+d"`.
///
/// At most [`MAX_KEY_PATH_LEN`] bytes, [`MAX_KEY_PATH_STEPS`] steps and
/// indices up to [`MAX_KEY_PATH_INDEX`]; a quoted key may not hold control
/// characters. Displays in the same syntax, so a path can be logged (S8: a
/// key path, never a value), with each character that shows as nothing
/// (bidi controls, zero-width and other default-ignorable characters) as
/// its `\u{XXXX}` escape, which parses back to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyPath(Vec<Step>);

impl KeyPath {
    /// The steps, first (outermost) first; never empty, and the first is
    /// always a [`Step::Key`].
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.0
    }
}

/// Why a string is not a [`KeyPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyPathError {
    /// The path is empty.
    #[error("empty key path")]
    Empty,
    /// The path is longer than [`MAX_KEY_PATH_LEN`] bytes.
    #[error("key path longer than {MAX_KEY_PATH_LEN} bytes")]
    TooLong,
    /// The path has more than [`MAX_KEY_PATH_STEPS`] steps.
    #[error("key path with more than {MAX_KEY_PATH_STEPS} steps")]
    TooManySteps,
    /// An index is larger than [`MAX_KEY_PATH_INDEX`].
    #[error("index larger than {MAX_KEY_PATH_INDEX}")]
    IndexTooLarge,
    /// The path does not follow the syntax at byte `at`.
    #[error("malformed key path at byte {at}")]
    Malformed {
        /// Byte offset of the problem.
        at: usize,
    },
}

impl FromStr for KeyPath {
    type Err = KeyPathError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(KeyPathError::Empty);
        }
        if text.len() > MAX_KEY_PATH_LEN {
            return Err(KeyPathError::TooLong);
        }
        let bytes = text.as_bytes();
        let mut at = 0;
        let mut steps = Vec::new();
        let push = |steps: &mut Vec<Step>, step: Step| {
            if steps.len() == MAX_KEY_PATH_STEPS {
                return Err(KeyPathError::TooManySteps);
            }
            steps.push(step);
            Ok(())
        };
        loop {
            let key = if bytes.get(at) == Some(&b'"') {
                let mut key = String::new();
                let mut chars = text[at + 1..].char_indices();
                loop {
                    let Some((i, c)) = chars.next() else {
                        return Err(KeyPathError::Malformed { at: text.len() });
                    };
                    match c {
                        '"' => {
                            at += 1 + i + 1;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some((_, escaped @ ('"' | '\\'))) => key.push(escaped),
                            Some((_, 'u')) => {
                                let malformed = KeyPathError::Malformed { at: at + 1 + i };
                                let rest = &text[at + 1 + i + 2..];
                                let digits = rest
                                    .strip_prefix('{')
                                    .and_then(|r| r.split_once('}'))
                                    .map(|(digits, _)| digits)
                                    .filter(|d| {
                                        (1..=6).contains(&d.len())
                                            && d.bytes().all(|b| b.is_ascii_hexdigit())
                                    })
                                    .ok_or(malformed.clone())?;
                                let c = u32::from_str_radix(digits, 16)
                                    .ok()
                                    .and_then(char::from_u32)
                                    .filter(|c| !c.is_control())
                                    .ok_or(malformed)?;
                                key.push(c);
                                // `{`, the digits and `}`, all ASCII.
                                for _ in 0..digits.len() + 2 {
                                    chars.next();
                                }
                            }
                            _ => return Err(KeyPathError::Malformed { at: at + 1 + i }),
                        },
                        c if c.is_control() => {
                            return Err(KeyPathError::Malformed { at: at + 1 + i });
                        }
                        c => key.push(c),
                    }
                }
                key
            } else {
                let start = at;
                while bytes.get(at).is_some_and(|&b| is_bare(b)) {
                    at += 1;
                }
                if at == start {
                    return Err(KeyPathError::Malformed { at });
                }
                text[start..at].to_owned()
            };
            push(&mut steps, Step::Key(key))?;
            while bytes.get(at) == Some(&b'[') {
                let start = at + 1;
                let mut end = start;
                while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                if end == start || bytes.get(end) != Some(&b']') {
                    return Err(KeyPathError::Malformed { at: end });
                }
                // At most six digits are read; more is too large anyway.
                if end - start > 6 {
                    return Err(KeyPathError::IndexTooLarge);
                }
                let index: usize = text[start..end]
                    .parse()
                    .map_err(|_| KeyPathError::Malformed { at: start })?;
                if index > MAX_KEY_PATH_INDEX {
                    return Err(KeyPathError::IndexTooLarge);
                }
                push(&mut steps, Step::Index(index))?;
                at = end + 1;
            }
            match bytes.get(at) {
                None => return Ok(Self(steps)),
                Some(b'.') => at += 1,
                Some(_) => return Err(KeyPathError::Malformed { at }),
            }
        }
    }
}

/// Whether `b` may appear in a bare key.
fn is_bare(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, step) in self.0.iter().enumerate() {
            match step {
                Step::Key(key) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    if !key.is_empty() && key.bytes().all(is_bare) {
                        f.write_str(key)?;
                    } else {
                        f.write_str("\"")?;
                        for c in key.chars() {
                            match c {
                                '"' | '\\' => write!(f, "\\{c}")?,
                                // Never parsed, but a path built in code is
                                // still displayed on one line (S8); and a
                                // caller's character that shows as nothing
                                // (bidi, zero-width) as its escape, as the
                                // loader writes the file's (#348).
                                c if c.is_control() || needs_unicode_escape(c) => {
                                    write!(f, "\\u{{{:x}}}", u32::from(c))?;
                                }
                                c => write!(f, "{c}")?,
                            }
                        }
                        f.write_str("\"")?;
                    }
                }
                Step::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// Why an edit, or opening or saving `config.toml`, failed. Nothing is
/// written for any of them.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// The file could not be read or written, or is not a TOML document
    /// (then it is left for the user to fix by hand).
    #[error(transparent)]
    File(#[from] Error),
    /// The key path does not parse.
    #[error(transparent)]
    KeyPath(#[from] KeyPathError),
    /// The file is TOML but cannot be edited: the TOML editor cannot read
    /// it or would not write it back byte for byte (mixed line endings, a
    /// CRLF inside a multi-line string). Refused at
    /// [`ConfigEditor::open`], before any key path is involved; the file is
    /// left for the user to edit by hand.
    #[error("config.toml cannot be edited here: {reason}")]
    NotEditable {
        /// Why, for a message.
        reason: &'static str,
    },
    /// The edit is not one the editor makes: the key path runs through a
    /// value that is not a table or array, a value would replace a table,
    /// a value holds a table, or the file would not survive an edit
    /// unchanged elsewhere.
    #[error("`{path}`: {reason}")]
    Unsupported {
        /// The key path.
        path: KeyPath,
        /// Why, for a message.
        reason: &'static str,
    },
    /// The loader would warn about the edited file: the edit is refused
    /// and the file keeps its old value.
    #[error("`{path}` refused: {}", list(.warnings))]
    Refused {
        /// The key path.
        path: KeyPath,
        /// What the loader (or the theme lookup) would report, in document
        /// order; never empty.
        warnings: Vec<Warning>,
    },
    /// The loader would fail on the edited file (a value of the wrong type
    /// for its key), or fails on the opened one and the edit would replace
    /// the list or table it rejected (see [`ConfigEditor::set`]): the edit
    /// is refused.
    #[error("`{path}` refused: {error}")]
    Invalid {
        /// The key path.
        path: KeyPath,
        /// The loader's error, bounded and escaped.
        error: ParseError,
    },
    /// The edited file would be larger than
    /// [`MAX_CONFIG_FILE_LEN`].
    #[error("the edited file would be larger than {MAX_CONFIG_FILE_LEN} bytes")]
    TooLarge,
    /// The file changed on disk (or was replaced, removed or re-linked)
    /// since it was opened: the external change is kept and nothing is
    /// written. Open the file again to edit the new version.
    #[error("config.toml changed on disk since it was opened")]
    Conflict,
    /// No `[[profiles]]` entry has the name an edit addressed
    /// ([`ConfigEditor::set_in_profile`], [`ConfigEditor::remove_in_profile`]),
    /// or fewer entries than its ordinal: it was renamed or removed.
    /// Nothing is written.
    #[error("no profile in config.toml is named \"{name}\"")]
    NoSuchProfile {
        /// The name, made safe to show ([`display_name`]).
        name: String,
    },
    /// A theme file with the imported theme's file name exists already.
    #[error("a theme file named {file_name} exists already")]
    ThemeExists {
        /// The existing file's name (a [`slug`] and `.toml`).
        file_name: String,
    },
}

/// `warnings` joined by `; `.
fn list(warnings: &[Warning]) -> String {
    warnings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<std::io::Error> for EditError {
    fn from(error: std::io::Error) -> Self {
        Self::File(error.into())
    }
}

/// An open `config.toml`, edited in memory and written back with
/// [`ConfigEditor::save`]. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct ConfigEditor {
    /// The path opened, maybe a symlink.
    path: PathBuf,
    /// The file the path resolved to when opened.
    target: PathBuf,
    /// The file's bytes when opened (or last saved).
    original: Vec<u8>,
    /// Whether the file starts with a byte order mark (kept on save).
    bom: bool,
    /// The file's line endings, kept on save.
    endings: Endings,
    /// The edited document.
    doc: DocumentMut,
    /// The document as plain TOML, for the check of each edit.
    plain: toml::Table,
    /// The document's comments, for the check of each edit.
    comments: Vec<String>,
    /// What the loader reports for the document; `None` when it fails.
    warnings: Option<Vec<Warning>>,
    /// The loader's error when it fails on the document.
    error: Option<ParseError>,
    /// The theme names the document uses.
    themes: Vec<String>,
    /// The configuration the document loads as; `None` when it fails.
    config: Option<Config>,
}

/// The byte order mark a file may start with.
const BOM: &str = "\u{feff}";

/// The line endings of a file, which the editor (always LF, always a
/// final newline) does not keep by itself.
#[derive(Debug, Clone, Copy)]
struct Endings {
    /// Every line ends in CRLF (edited as LF).
    crlf: bool,
    /// The last line ends in a newline (or the file is empty).
    final_newline: bool,
}

impl Endings {
    /// The endings of `body` and `body` with LF endings; a body with mixed
    /// endings stays as it is (and does not round-trip).
    fn of(body: &str) -> (Self, std::borrow::Cow<'_, str>) {
        let crlf =
            body.contains("\r\n") && body.matches('\n').count() == body.matches("\r\n").count();
        let final_newline = body.is_empty() || body.ends_with('\n');
        let lf = if crlf {
            body.replace("\r\n", "\n").into()
        } else {
            body.into()
        };
        (
            Self {
                crlf,
                final_newline,
            },
            lf,
        )
    }

    /// `text`, as the editor renders it, with these endings. Without a
    /// final newline every line break at the end goes, not only the one
    /// the editor adds: removing the last item of such a file can leave
    /// the blank line that was above it last (#324 item 22).
    fn apply(self, text: &str) -> String {
        let text = if self.final_newline {
            text
        } else {
            text.trim_end_matches('\n')
        };
        if self.crlf {
            text.replace('\n', "\r\n")
        } else {
            text.to_owned()
        }
    }
}

impl ConfigEditor {
    /// Opens the `config.toml` at `path` for editing. A symlink is followed
    /// (and stays a symlink on save).
    ///
    /// # Errors
    ///
    /// [`EditError::File`] when the file is missing (create it first, for
    /// example with [`crate::write_default_if_missing`]), is a dangling
    /// symlink or a symlink cycle, is not a regular file, is larger than
    /// [`MAX_CONFIG_FILE_LEN`], has more than
    /// one hard link (the rename on save would split them), is read-only,
    /// is not UTF-8 or not TOML. [`EditError::NotEditable`] when the TOML
    /// editor would not write the file back byte for byte (the file's own
    /// line endings and missing final newline are kept on save; mixed line
    /// endings are not, nor a CRLF in a multi-line string).
    pub fn open(path: &Path) -> Result<Self, EditError> {
        let target = resolve(path)?;
        let meta = std::fs::metadata(&target)?;
        check_writable(&target, &meta)?;
        let original = read_capped(&target, MAX_CONFIG_FILE_LEN, "config file")?;
        let text = std::str::from_utf8(&original).map_err(Error::from)?;
        let (bom, body) = match text.strip_prefix(BOM) {
            Some(body) => (true, body),
            None => (false, text),
        };
        let (endings, lf) = Endings::of(body);
        let state = State::of(&lf).map_err(|error| match error {
            Parsed::Toml(error) => EditError::File(Error::Parse(error)),
            Parsed::Editor => EditError::NotEditable {
                reason: "the TOML editor cannot read this file",
            },
        })?;
        if endings.apply(&state.doc.to_string()) != body {
            return Err(EditError::NotEditable {
                reason: "the TOML editor would not write this file back unchanged",
            });
        }
        // A CRLF inside a multi-line string is part of its value.
        if endings.crlf && !same_as_saved(&state.plain, body) {
            return Err(EditError::NotEditable {
                reason: "a multi-line string in this CRLF file would change",
            });
        }
        Ok(Self {
            path: path.to_owned(),
            target,
            bom,
            endings,
            original: original.clone(),
            doc: state.doc,
            plain: state.plain,
            comments: state.comments,
            warnings: state.warnings,
            error: state.error,
            themes: state.themes,
            config: state.config,
        })
    }

    /// The configuration the document (with the edits so far) loads as,
    /// defaults filled in; `None` when the loader fails on it (only
    /// possible for the file as opened: an edit that makes it fail is
    /// refused).
    #[must_use]
    pub fn config(&self) -> Option<&Config> {
        self.config.as_ref()
    }

    /// The path this editor opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The document as it would be saved.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let body = self.endings.apply(&self.doc.to_string());
        if self.bom {
            format!("{BOM}{body}")
        } else {
            body
        }
    }

    /// Whether the document differs from the file as opened (or last
    /// saved).
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.to_toml().as_bytes() != self.original.as_slice()
    }

    /// How many profiles (`[[profiles]]` entries) the document has.
    #[must_use]
    pub fn profile_count(&self) -> usize {
        match self.plain.get("profiles") {
            Some(toml::Value::Array(profiles)) => profiles.len(),
            _ => 0,
        }
    }

    /// Sets the value at `path` (see [`KeyPath`]), creating the tables on
    /// the way that are missing (a new table goes after the existing
    /// ones). An existing value is replaced in place and keeps its
    /// comments; an index one past the end of an array appends (so
    /// `profiles[N].name`, with `N` the number of profiles, adds one).
    ///
    /// # Errors
    ///
    /// [`EditError::Refused`] or [`EditError::Invalid`] when the loader
    /// would warn about or reject the result, [`EditError::Unsupported`]
    /// for an edit the editor does not make (`value` holds a table, the
    /// path ends at a table or runs through a value), [`EditError::KeyPath`]
    /// for a malformed path, [`EditError::TooLarge`]. The document is
    /// unchanged after any error.
    ///
    /// An integer set where the loader reads a float (`opacity`) is
    /// written as a float (`1.0`). Replacing an array keeps the comments
    /// between its brackets, each on a line of its own above the key.
    ///
    /// A profile that a file in `profiles/` defines is not edited here
    /// (#312): in the configuration the app reads, the files' profiles
    /// follow the `[[profiles]]` entries, and a path into one of them
    /// (`profiles[N]` and on, `N` the number of entries, as many as the
    /// files define) is refused with [`EditError::Unsupported`] and
    /// [`FILE_PROFILE_REFUSED`], also by [`ConfigEditor::remove`].
    /// [`ConfigEditor::add_profile`] still adds an entry, before them.
    ///
    /// An item past one of the loader's caps (a profile from the 257th,
    /// a smart rule from the 65th, a `[keys]` entry from the 513th) is not
    /// edited: the loader never reads it, so the check cannot see the edit
    /// (#449). [`EditError::Unsupported`] says "past the loader's cap of
    /// N"; [`ConfigEditor::remove`] refuses the same.
    ///
    /// On a file the loader fails on (a value of the wrong type), an edit
    /// that leaves it failing is refused with the error of the edited file.
    /// One that makes it load repairs it, a set or remove of a scalar
    /// (`opacity = "x"`), unless it sets, removes or edits inside a list or
    /// table the loader rejected (`enabled = "x"` under `[plugins]`, a key
    /// that holds a list): that is refused with the loader's error for the
    /// file, so a toggle never replaces what the user wrote (#489 48b m2).
    /// [`ConfigEditor::replace`] replaces it when the user asked for that.
    pub fn set(&mut self, path: &str, value: toml::Value) -> Result<(), EditError> {
        let path: KeyPath = path.parse()?;
        self.not_in_a_profile_file(&path)?;
        self.set_path(&path, value, Rejected::Refuse)
    }

    /// [`ConfigEditor::set`], except that it may make a file the loader
    /// fails on load: the value the loader rejected (`enabled = "x"`, an
    /// array of the wrong type) is replaced by `value`, which must be well
    /// typed. For an action where the user asked to replace what the file
    /// holds; a toggle or field calls [`ConfigEditor::set`] and gets the
    /// loader's error. Otherwise the same checks and errors as
    /// [`ConfigEditor::set`]; the edited file must then load without any
    /// warning, since the opened one had none to keep.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::set`].
    pub fn replace(&mut self, path: &str, value: toml::Value) -> Result<(), EditError> {
        let path: KeyPath = path.parse()?;
        self.not_in_a_profile_file(&path)?;
        self.set_path(&path, value, Rejected::Replace)
    }

    /// [`ConfigEditor::set`] of a parsed `path`, which may be one past the
    /// last `[[profiles]]` entry with profile files after it
    /// ([`ConfigEditor::add_profile`]).
    fn set_path(
        &mut self,
        path: &KeyPath,
        value: toml::Value,
        rejected: Rejected,
    ) -> Result<(), EditError> {
        let path = path.clone();
        let state = self.prepare_set(&path, &value, rejected)?;
        // The loader takes an integer for a float key; the file should say
        // what the setting is (`opacity = 1.0`, #324 item 4).
        let state = match state
            .config
            .as_ref()
            .and_then(|config| floats_where_config_has_them(value, config, &path.0))
        {
            Some(value) => self.prepare_set(&path, &value, rejected)?,
            None => state,
        };
        self.keep(state);
        Ok(())
    }

    /// The document with `value` set at `path`, checked as
    /// [`ConfigEditor::commit`] checks it, not yet kept.
    fn prepare_set(
        &self,
        path: &KeyPath,
        value: &toml::Value,
        rejected: Rejected,
    ) -> Result<State, EditError> {
        let unsupported = |reason| EditError::Unsupported {
            path: path.clone(),
            reason,
        };
        past_loader_cap(&self.doc, path)?;
        let edit_value = edit_value(value, 0).map_err(unsupported)?;
        let mut doc = self.doc.clone();
        // The comments inside a replaced value are moved, not removed.
        set_table(
            doc.as_table_mut(),
            false,
            &path.0,
            edit_value,
            &mut Vec::new(),
        )
        .map_err(unsupported)?;
        let retired = retire_0_2_blur_radius_comment(&mut doc, &path.0);
        let mut expected = toml::Value::Table(self.plain.clone());
        plain_edit(&mut expected, &path.0, Some(value.clone()));
        let before = self.warnings.clone().unwrap_or_default();
        // A new key in an unknown table adds no warning (the loader names
        // the table once), but it is one more unknown key (#324 item 2),
        // whether or not the table's warning is listed (#430).
        if plain_at(&self.plain, &path.0).is_none()
            && let Some(unknown) = unknown_above(&path.0, value)
        {
            return Err(EditError::Refused {
                path: path.clone(),
                warnings: vec![unknown],
            });
        }
        self.check(
            path,
            &doc.to_string(),
            &expected,
            &retired,
            &before,
            rejected,
        )
    }

    /// Removes the value, table or array element at `path`; `Ok(false)`
    /// when there is nothing there. Removing an array element
    /// (`profiles[1]`) moves the later ones down.
    ///
    /// No comment is lost with a value: the comment lines above a removed
    /// key stay where they were, and a comment on its line stays as a line
    /// of its own in its place. A removed table or profile takes the
    /// comments inside it (its header line and body) with it; those above
    /// its header stay.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::set`].
    pub fn remove(&mut self, path: &str) -> Result<bool, EditError> {
        let path: KeyPath = path.parse()?;
        self.remove_path(&path)
    }

    /// [`ConfigEditor::remove`] of a parsed `path`.
    fn remove_path(&mut self, path: &KeyPath) -> Result<bool, EditError> {
        let unsupported = |reason| EditError::Unsupported {
            path: path.clone(),
            reason,
        };
        past_loader_cap(&self.doc, path)?;
        self.not_in_a_profile_file(path)?;
        let mut doc = self.doc.clone();
        let carried = cut_leading_decor(&mut doc, &path.0);
        let marked = doc.to_string();
        let mut removed = Vec::new();
        if !remove_table(doc.as_table_mut(), &path.0, &mut removed).map_err(unsupported)? {
            return Ok(false);
        }
        let mut text = doc.to_string();
        if let Some(carried) = carried {
            // Everything before the removed item renders as it did, so
            // the comments it kept go back exactly where they were.
            let at = marked
                .find(&carried.mark)
                .ok_or_else(|| unsupported("internal: the edit mark was not rendered"))?;
            if !text.starts_with(&marked[..at]) {
                return Err(unsupported("internal: the text before the edit changed"));
            }
            text.insert_str(at, &carried.kept);
        }
        let mut expected = toml::Value::Table(self.plain.clone());
        plain_edit(&mut expected, &path.0, None);
        let before = renumbered(self.warnings.as_deref().unwrap_or_default(), path);
        self.commit(path, &text, &expected, &removed, &before)?;
        Ok(true)
    }

    /// Appends a profile named `name` (a new `[[profiles]]` entry after
    /// the last one) and returns its index.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::set`]; [`EditError::Unsupported`] when the
    /// file has [`MAX_PROFILES`](crate::MAX_PROFILES) profiles already (a
    /// set anywhere in a profile past the cap is refused the same way), or
    /// has, with the profiles of the files in `profiles/` (which come after
    /// the entries and move up one), that many already.
    pub fn add_profile(&mut self, name: &str) -> Result<usize, EditError> {
        let files = self.file_profiles();
        self.add_entry(name, files)
    }

    /// [`ConfigEditor::add_profile`] with `files` profile files' profiles
    /// (read by the caller).
    fn add_entry(&mut self, name: &str, files: usize) -> Result<usize, EditError> {
        let index = self.profile_count();
        let path: KeyPath = format!("profiles[{index}].name").parse()?;
        // The new entry goes before the profile files' profiles, which
        // move up one; with the cap reached, the last of them would drop.
        if index < crate::MAX_PROFILES && index.saturating_add(files) >= crate::MAX_PROFILES {
            return Err(EditError::Unsupported {
                path,
                reason: "the first 256 profiles, with those in profiles/, are read; one more would push one out",
            });
        }
        self.set_path(
            &path,
            toml::Value::String(name.to_owned()),
            Rejected::Refuse,
        )?;
        Ok(index)
    }

    /// The first of `base`, `base 2`, `base 3`, … that no profile of the
    /// configuration the app would read now is named: the `[[profiles]]`
    /// entries of the document (with the edits so far), each as written
    /// and as the loader reads it (`"A\0"` loads as `"A\u{FFFD}"`), and the
    /// profiles of the files in `profiles/` (#312). Names compare byte for
    /// byte, as the loader's duplicate-name check does. At most one more
    /// try than there are names.
    #[must_use]
    pub fn unused_profile_name(&self, base: &str) -> String {
        self.unused_name(base, &self.load_profile_files())
    }

    /// The profile files next to the opened path, loaded now.
    fn load_profile_files(&self) -> crate::profile_files::ProfileFiles {
        crate::profile_files::load(&self.path.with_file_name("profiles"))
    }

    /// [`ConfigEditor::unused_profile_name`] with `files` read by the
    /// caller.
    fn unused_name(&self, base: &str, files: &crate::profile_files::ProfileFiles) -> String {
        // The loader's names are the ones a new name must differ from; a
        // file it fails on has none, and the names as written still count.
        let written = self
            .plain
            .get("profiles")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.get("name").and_then(toml::Value::as_str));
        let loaded = self
            .config
            .iter()
            .flat_map(|config| &config.profiles)
            .map(|p| p.name.as_str());
        let taken: std::collections::HashSet<&str> =
            written.chain(loaded).chain(files.names()).collect();
        let mut name = base.to_owned();
        let mut number = 2usize;
        while taken.contains(name.as_str()) {
            name = format!("{base} {number}");
            number += 1;
        }
        name
    }

    /// Adds a `[[profiles]]` entry named after `base` and sets `keys` in
    /// it, all or nothing (#312): the profile Settings adds or duplicates,
    /// whatever profile files exist. The name is
    /// [`ConfigEditor::unused_profile_name`] of `base`, so it never
    /// shadows a profile of either source; each key is a key path within
    /// the profile (`command`, `appearance.theme`), set as
    /// [`ConfigEditor::set`] sets it. The entry goes after the last entry,
    /// before the profile files' profiles, which move up one; its index in
    /// the configuration the app reads, also its key path index, is the
    /// number of entries before the add.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::add_profile`] (the cap of
    /// [`MAX_PROFILES`](crate::MAX_PROFILES) counting both sources) and
    /// [`ConfigEditor::set`] for each key, naming that key's full path
    /// (`profiles[3].appearance.theme`); [`EditError::Unsupported`] for more
    /// than [`MAX_ADDED_PROFILE_KEYS`] keys, a key `name` (the name is
    /// `base`'s), or a name longer than
    /// [`MAX_PROFILE_NAME_LEN`](crate::MAX_PROFILE_NAME_LEN) bytes (`base`,
    /// or `base` with the number that makes it unused);
    /// [`EditError::KeyPath`] for a key that does not parse or makes a path
    /// of more than [`MAX_KEY_PATH_STEPS`] steps or [`MAX_KEY_PATH_LEN`]
    /// bytes. The document is unchanged after any error.
    pub fn add_profile_with(
        &mut self,
        base: &str,
        keys: &[(String, toml::Value)],
    ) -> Result<AddedProfile, EditError> {
        let index = self.profile_count();
        let unsupported = |suffix: &str, reason| -> Result<AddedProfile, EditError> {
            Err(EditError::Unsupported {
                path: format!("profiles[{index}]{suffix}").parse()?,
                reason,
            })
        };
        if keys.len() > MAX_ADDED_PROFILE_KEYS {
            return unsupported("", "a new profile takes at most 64 keys besides its name");
        }
        const {
            assert!(
                crate::MAX_PROFILE_NAME_LEN == 128 && MAX_ADDED_PROFILE_KEYS == 64,
                "the reasons name the caps"
            );
        };
        if base.len() > crate::MAX_PROFILE_NAME_LEN {
            return unsupported(".name", "a profile name is at most 128 bytes");
        }
        // One read of `profiles/` for the name and the cap.
        let files = self.load_profile_files();
        let name = self.unused_name(base, &files);
        if name.len() > crate::MAX_PROFILE_NAME_LEN {
            return unsupported(
                ".name",
                "the name is taken, and with a number that makes it unused it is longer than 128 bytes",
            );
        }
        let mut paths = Vec::with_capacity(keys.len());
        for (key, _) in keys {
            let relative: KeyPath = key.parse()?;
            if relative.0.first() == Some(&Step::Key("name".to_owned())) {
                return unsupported(".name", "the name is given as the base name, not as a key");
            }
            if relative.0.len() + 2 > MAX_KEY_PATH_STEPS {
                return Err(KeyPathError::TooManySteps.into());
            }
            let mut steps = vec![Step::Key("profiles".to_owned()), Step::Index(index)];
            steps.extend(relative.0);
            // The full path is a key path too: a 1024-byte key makes one
            // longer than MAX_KEY_PATH_LEN with `profiles[N].` (#312 m3).
            let path = KeyPath(steps);
            if path.to_string().len() > MAX_KEY_PATH_LEN {
                return Err(KeyPathError::TooLong.into());
            }
            paths.push(path);
        }
        let before = self.clone();
        let added = self.add_entry(&name, files.count()).and_then(|at| {
            debug_assert_eq!(at, index);
            for (path, (_, value)) in paths.iter().zip(keys) {
                self.set_path(path, value.clone(), Rejected::Refuse)?;
            }
            Ok(AddedProfile { index, name })
        });
        if added.is_err() {
            *self = before;
        }
        added
    }

    /// [`ConfigEditor::set`] of `key`, a key path within a profile
    /// (`command`, `appearance.theme`), in the `[[profiles]]` entry named
    /// `name`, the `ordinal`-th (from 0, in file order) of the entries the
    /// caller showed with that name (#565). The profile is looked up in
    /// this document, the one being written, so an edit made against an
    /// older read of the file still lands on its profile when a hand edit
    /// has added, removed or moved other profiles since: the ordinal tells
    /// two entries of one name apart wherever they are now. Names compare
    /// byte for byte, each entry's as written and as the loader reads it;
    /// an entry without a name is [`BUILT_IN_PROFILE_NAME`], as the loader
    /// names it.
    ///
    /// # Errors
    ///
    /// [`EditError::NoSuchProfile`] when fewer than `ordinal + 1` entries
    /// have that name;
    /// [`EditError::Unsupported`] with [`FILE_PROFILE_REFUSED`] when only a
    /// profile of `profiles/` has it; [`EditError::KeyPath`] for a `key`
    /// that does not parse (an empty one too) or makes a path of more than
    /// [`MAX_KEY_PATH_STEPS`] steps or [`MAX_KEY_PATH_LEN`] bytes; else as
    /// for [`ConfigEditor::set`]. The document is unchanged after any
    /// error.
    pub fn set_in_profile(
        &mut self,
        name: &str,
        ordinal: usize,
        key: &str,
        value: toml::Value,
    ) -> Result<(), EditError> {
        let relative: KeyPath = key.parse()?;
        let path = self.path_in_profile(name, ordinal, relative.0)?;
        self.set_path(&path, value, Rejected::Refuse)
    }

    /// [`ConfigEditor::remove`] of `key` in the `[[profiles]]` entry named
    /// `name` of that `ordinal`, found as [`ConfigEditor::set_in_profile`]
    /// finds it; an empty `key` removes the entry itself, and the later
    /// ones move down.
    /// `Ok(false)` when the entry has nothing at `key`.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::set_in_profile`] (an empty `key` is the
    /// entry) and [`ConfigEditor::remove`].
    pub fn remove_in_profile(
        &mut self,
        name: &str,
        ordinal: usize,
        key: &str,
    ) -> Result<bool, EditError> {
        let relative = if key.is_empty() {
            Vec::new()
        } else {
            key.parse::<KeyPath>()?.0
        };
        let path = self.path_in_profile(name, ordinal, relative)?;
        self.remove_path(&path)
    }

    /// The key path of `relative` in the `[[profiles]]` entry named `name`
    /// of that `ordinal`, in this document.
    fn path_in_profile(
        &self,
        name: &str,
        ordinal: usize,
        relative: Vec<Step>,
    ) -> Result<KeyPath, EditError> {
        if relative.len() + 2 > MAX_KEY_PATH_STEPS {
            return Err(KeyPathError::TooManySteps.into());
        }
        let count = self.profile_count();
        // Without a name, an entry is the loader's "Default", also while
        // the file does not load.
        let written = |i: usize| {
            let profile = self.plain.get("profiles")?.get(i)?;
            match profile.get("name") {
                None => Some(BUILT_IN_PROFILE_NAME),
                Some(name) => name.as_str(),
            }
        };
        // Entries keep their places in the loaded configuration, the
        // profile files' profiles after them.
        let loaded = |i: usize| {
            self.config
                .as_ref()
                .and_then(|config| config.profiles.get(i))
                .map(|profile| profile.name.as_str())
        };
        let named = |i: usize| written(i) == Some(name) || loaded(i) == Some(name);
        let found = (0..count).filter(|&i| named(i)).nth(ordinal);
        let Some(index) = found else {
            let files = self.load_profile_files();
            let Some(at) = files.names().position(|file| file == name) else {
                return Err(EditError::NoSuchProfile {
                    name: display_name(name),
                });
            };
            let mut steps = vec![
                Step::Key("profiles".to_owned()),
                Step::Index(count.saturating_add(at)),
            ];
            steps.extend(relative);
            return Err(EditError::Unsupported {
                path: KeyPath(steps),
                reason: FILE_PROFILE_REFUSED,
            });
        };
        let mut steps = vec![Step::Key("profiles".to_owned()), Step::Index(index)];
        steps.extend(relative);
        let path = KeyPath(steps);
        if path.to_string().len() > MAX_KEY_PATH_LEN {
            return Err(KeyPathError::TooLong.into());
        }
        Ok(path)
    }

    /// [`EditError::Unsupported`] when `path` is in a profile that a file
    /// in `profiles/` defines (#312): in the configuration the app reads,
    /// the profiles of the files follow the `[[profiles]]` entries, so
    /// `profiles[i]` with `i` from the number of entries on is a file's.
    /// The editor writes `config.toml` only, and an edit there would
    /// change (or add) another profile than the one asked for, so it is
    /// refused; the file is edited by hand (ADR 0040).
    fn not_in_a_profile_file(&self, path: &KeyPath) -> Result<(), EditError> {
        if let [Step::Key(key), Step::Index(index), ..] = &path.0[..]
            && key == "profiles"
            && *index >= self.profile_count()
            && *index - self.profile_count() < self.file_profiles()
        {
            return Err(EditError::Unsupported {
                path: path.clone(),
                reason: FILE_PROFILE_REFUSED,
            });
        }
        Ok(())
    }

    /// How many profiles the files in `profiles/` next to the opened path
    /// define (those that load), read now; see
    /// [`ConfigEditor::not_in_a_profile_file`].
    fn file_profiles(&self) -> usize {
        self.load_profile_files().count()
    }

    /// Checks the edited document `text` and keeps it, or returns why not.
    /// `before` are the warnings of the document before the edit, named
    /// as the loader names them after it.
    fn commit(
        &mut self,
        path: &KeyPath,
        text: &str,
        expected: &toml::Value,
        removed: &[String],
        before: &[Warning],
    ) -> Result<(), EditError> {
        let state = self.check(path, text, expected, removed, before, Rejected::Refuse)?;
        self.keep(state);
        Ok(())
    }

    /// Whether an edit at `path` that makes the failing document load as
    /// `after` sets, removes or edits inside a list or table the loader
    /// rejected: the value at the loader's error, its key a list or table
    /// by default, before or after the edit, and `path` at, under or above
    /// it. Also when the error points into no value (nothing tells what
    /// would be lost).
    fn replaces_a_rejected_list(&self, path: &KeyPath, after: &toml::Table) -> bool {
        let Some(span) = self.error.as_ref().and_then(ParseError::span) else {
            return true;
        };
        // The document is the one opened: an edit that leaves it failing
        // is never kept, so the error's offsets are its own.
        let Some(rejected) = rejected_value(&self.doc.to_string(), span.start) else {
            return true;
        };
        if !(path.0.starts_with(&rejected) || rejected.starts_with(&path.0)) {
            return false;
        }
        let compound = |v: Option<&toml::Value>| {
            matches!(v, Some(toml::Value::Array(_) | toml::Value::Table(_)))
        };
        let defaults = toml::Table::try_from(Config::default()).unwrap_or_default();
        compound(plain_at(&defaults, &rejected))
            || compound(plain_at(&self.plain, &rejected))
            || compound(plain_at(after, &rejected))
    }

    /// Makes the checked document `state` the one being edited.
    fn keep(&mut self, state: State) {
        self.doc = state.doc;
        self.plain = state.plain;
        self.comments = state.comments;
        self.warnings = state.warnings;
        self.error = state.error;
        self.themes = state.themes;
        self.config = state.config;
    }

    /// Checks the edited document `text`: it parses, it is `expected` as
    /// plain TOML, it keeps every comment but those of `removed` and adds
    /// none, and the loader and the theme lookup warn about nothing that
    /// `before` does not hold. With the loader failing on the document
    /// before the edit, the edited one must fail too unless `rejected`
    /// says to replace.
    fn check(
        &self,
        path: &KeyPath,
        text: &str,
        expected: &toml::Value,
        removed: &[String],
        before: &[Warning],
        rejected: Rejected,
    ) -> Result<State, EditError> {
        let unsupported = |reason| EditError::Unsupported {
            path: path.clone(),
            reason,
        };
        let saved = self.endings.apply(text);
        if saved.len() + if self.bom { BOM.len() } else { 0 }
            > usize::try_from(MAX_CONFIG_FILE_LEN).unwrap_or(usize::MAX)
        {
            return Err(EditError::TooLarge);
        }
        let state = State::of(text).map_err(|_| unsupported("internal: the edit is not TOML"))?;
        if self.endings.crlf && !same_as_saved(&state.plain, &saved) {
            return Err(unsupported(
                "a multi-line string would change with CRLF line endings",
            ));
        }
        if !same(
            normalized(&toml::Value::Table(state.plain.clone())).as_ref(),
            normalized(expected).as_ref(),
        ) {
            return Err(unsupported(
                "internal: the edit would change other settings",
            ));
        }
        // Every comment stays but those of the lines removed, and none
        // appears that was not there.
        let mut gone = multiset(&self.comments);
        for comment in &state.comments {
            match gone.get_mut(comment.as_str()) {
                Some(n) if *n > 0 => *n -= 1,
                _ => return Err(unsupported("internal: the edit would add a comment")),
            }
        }
        let mut allowed = multiset(removed);
        for (comment, n) in gone {
            for _ in 0..n {
                match allowed.get_mut(comment) {
                    Some(m) if *m > 0 => *m -= 1,
                    _ => return Err(unsupported("internal: the edit would lose a comment")),
                }
            }
        }
        let Some(after) = &state.warnings else {
            return Err(match state.error {
                Some(error) => EditError::Invalid {
                    path: path.clone(),
                    error,
                },
                None => unsupported("internal: the loader failed"),
            });
        };
        // The edit makes a file the loader failed on load: it replaced
        // the value the loader rejected. A list or table the user wrote is
        // not replaced by a toggle (#489 48b m2); a scalar is repaired.
        if self.warnings.is_none()
            && rejected == Rejected::Refuse
            && self.replaces_a_rejected_list(path, &state.plain)
        {
            return Err(match &self.error {
                Some(error) => EditError::Invalid {
                    path: path.clone(),
                    error: error.clone(),
                },
                None => unsupported("internal: the loader failed"),
            });
        }
        let mut new = new_warnings(before, after);
        // A theme the edit names must exist (`ConfigStore::theme` would
        // fall back to the default with a warning). Each use of a missing
        // theme the document already has may stay, but not a further one
        // (#324 item 2).
        if let Some(dir) = self.path.parent() {
            let themes_dir = dir.join("themes");
            let mut known = multiset(&self.themes);
            for name in &state.themes {
                if let Some(n) = known.get_mut(name.as_str())
                    && *n > 0
                {
                    *n -= 1;
                    continue;
                }
                let resolved = Theme::resolve(&themes_dir, name);
                if resolved.source == ThemeSource::Default {
                    new.extend(
                        resolved
                            .warnings
                            .into_iter()
                            .filter(|w| matches!(w, Warning::ThemeNotFound { .. })),
                    );
                }
            }
        }
        if !new.is_empty() {
            return Err(EditError::Refused {
                path: path.clone(),
                warnings: new,
            });
        }
        Ok(state)
    }

    /// Writes the document back when it changed; `Ok(false)` when there
    /// was nothing to write.
    ///
    /// The document goes to a new temporary file in the directory of the
    /// file (the symlink's target when `config.toml` is a symlink), with
    /// the file's permission bits and, when the owner may set it, its
    /// group (not its extended attributes or ACLs), which is flushed to disk and then
    /// renamed over the file; the directory is flushed after. A crash
    /// leaves the old file or the new one, never a mix (a temporary file
    /// may stay behind: the next save removes it, see
    /// [`MAX_STALE_EDITS_SWEPT`]). Just before the rename the file is read again: if
    /// it is not byte for byte what was opened (or the symlink now points
    /// elsewhere), nothing is replaced and the result is
    /// [`EditError::Conflict`].
    ///
    /// # Errors
    ///
    /// [`EditError::Conflict`], or [`EditError::File`] when the file can no
    /// longer be read or the temporary file not written or renamed.
    pub fn save(&mut self) -> Result<bool, EditError> {
        let text = self.to_toml();
        if text.as_bytes() == self.original.as_slice() {
            return Ok(false);
        }
        if follow_links(&self.path)?.as_deref() != Some(self.target.as_path()) {
            return Err(EditError::Conflict);
        }
        let meta = match std::fs::metadata(&self.target) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EditError::Conflict);
            }
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            // A regular file when opened: replaced since.
            return Err(EditError::Conflict);
        }
        check_writable(&self.target, &meta)?;
        let (tmp, mut file) = temporary_file(&self.target)?;
        let owner = owner_of(&file);
        carry_group(&file, &meta);
        let written = file
            .set_permissions(meta.permissions())
            .and_then(|()| file.write_all(text.as_bytes()))
            .and_then(|()| file.sync_all());
        drop(file);
        let replaced = written.map_err(EditError::from).and_then(|()| {
            #[cfg(test)]
            tests::before_rename();
            self.unchanged_on_disk()?;
            std::fs::rename(&tmp, &self.target).map_err(EditError::from)
        });
        if let Err(error) = replaced {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        sync_dir(&self.target);
        self.original = text.into_bytes();
        if let Some(owner) = owner {
            let swept = sweep_stale_edits(&self.target, owner, process_exists);
            if swept > 0 {
                tracing::debug!(
                    target: "tessera_config",
                    swept,
                    "stale temporary files of earlier edits removed"
                );
            }
        }
        Ok(true)
    }

    /// `Err(Conflict)` unless the file on disk is what was opened.
    fn unchanged_on_disk(&self) -> Result<(), EditError> {
        if follow_links(&self.path)?.as_deref() != Some(self.target.as_path()) {
            return Err(EditError::Conflict);
        }
        match read_capped(&self.target, MAX_CONFIG_FILE_LEN, "config file") {
            Ok(bytes) if bytes == self.original => Ok(()),
            Ok(_) => Err(EditError::Conflict),
            // Removed, replaced by something that is not a regular file,
            // or grown past the cap: changed either way.
            Err(Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                ) =>
            {
                Err(EditError::Conflict)
            }
            Err(error) => Err(error.into()),
        }
    }
}

/// The final target of `path`, which must exist.
fn resolve(path: &Path) -> Result<PathBuf, EditError> {
    let target = follow_links(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a symlink cycle", path.display()),
        )
    })?;
    Ok(target)
}

/// Fails unless `meta` (of `target`) is a regular file the owner may
/// write, with a single hard link.
fn check_writable(target: &Path, meta: &std::fs::Metadata) -> Result<(), EditError> {
    let refuse = |what: &str| {
        EditError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} {what}", target.display()),
        ))
    };
    if !meta.is_file() {
        return Err(refuse("is not a regular file"));
    }
    if meta.permissions().readonly() {
        return Err(refuse("is read-only"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.nlink() > 1 {
            return Err(refuse(
                "has more than one hard link; saving would split them",
            ));
        }
    }
    Ok(())
}

/// Gives `file` the group of the file it replaces (`meta`), best effort:
/// the owner may not be in that group, and then the new file keeps the
/// directory's (ADR 0040 amendment, #324 item 7). Extended attributes and
/// ACLs are not carried.
fn carry_group(file: &std::fs::File, meta: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let gid = meta.gid();
        let current = file.metadata().map(|m| m.gid());
        if current.is_ok_and(|g| g == gid) {
            return;
        }
        if let Err(error) = std::os::unix::fs::fchown(file, None, Some(gid)) {
            tracing::debug!(
                target: "tessera_config",
                gid,
                %error,
                "configuration file group not kept"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (file, meta);
}

/// Numbers the temporary files within this process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The `<name>` of [`temporary_file`]'s names for `target`.
fn temporary_base(target: &Path) -> String {
    target
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned())
}

/// A new temporary file next to `target`, named
/// `.<name>.<pid>.<n>.edit`.
fn temporary_file(target: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    /// Names tried before giving up: a stale one (left by a crash of an
    /// earlier process with the same id) is skipped.
    const ATTEMPTS: u32 = 16;

    let name = temporary_base(target);
    let mut attempts = 0;
    loop {
        let tmp = target.with_file_name(format!(
            ".{name}.{}.{}.edit",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::File::create_new(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < ATTEMPTS => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Most stale temporary files of earlier edits one save removes
/// (docs/security.md S5): a crash between creating the temporary file and
/// the rename leaves `.<name>.<pid>.<n>.edit` behind (#324 item 6).
pub const MAX_STALE_EDITS_SWEPT: usize = 16;

/// Most entries of the target's directory a sweep for stale temporary
/// files reads (docs/security.md S5).
pub const MAX_STALE_SWEEP_ENTRIES: usize = 4096;

/// The user id that owns `file`, the temporary file this process just
/// created; `None` where there are no user ids.
fn owner_of(file: &std::fs::File) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        file.metadata().ok().map(|meta| meta.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        None
    }
}

/// Removes the temporary files that earlier saves of `target` left
/// behind (a crash before the rename) and returns how many: entries of
/// the target's directory named exactly as [`temporary_file`] names them
/// for this target, that are regular files (a symlink is neither followed
/// nor removed, nor is a directory) owned by `owner` (this user), and
/// whose process id is not this process's and, by `exists`, no running
/// process's (a live one may still be saving). At most
/// [`MAX_STALE_SWEEP_ENTRIES`] entries are read and
/// [`MAX_STALE_EDITS_SWEPT`] candidates checked. Best effort: an entry
/// that cannot be read or removed is left. Logs nothing about the files.
fn sweep_stale_edits(target: &Path, owner: u32, exists: impl Fn(u32) -> bool) -> usize {
    let Some(dir) = target.parent() else { return 0 };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let names = entries.map(|entry| entry.ok().map(|entry| entry.file_name()));
    sweep_names(dir, &temporary_base(target), names, owner, exists)
}

/// [`sweep_stale_edits`] of the entries `names` of `dir` (`None` for one
/// that could not be read), for the target file named `name`. Reads at
/// most [`MAX_STALE_SWEEP_ENTRIES`] of them, unreadable ones included.
fn sweep_names(
    dir: &Path,
    name: &str,
    names: impl Iterator<Item = Option<std::ffi::OsString>>,
    owner: u32,
    exists: impl Fn(u32) -> bool,
) -> usize {
    let prefix = format!(".{name}.");
    let mut checked = 0;
    let mut removed = 0;
    for file_name in names.take(MAX_STALE_SWEEP_ENTRIES).flatten() {
        if checked == MAX_STALE_EDITS_SWEPT {
            break;
        }
        let Some(pid) = file_name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix.as_str()))
            .and_then(stale_pid)
        else {
            continue;
        };
        if pid == 0 || pid == std::process::id() {
            continue;
        }
        let path = dir.join(&file_name);
        // Not followed: a symlink of that name is not a file of ours.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() || !owned_by(&meta, owner) {
            continue;
        }
        checked += 1;
        if exists(pid) {
            continue;
        }
        // `remove_file` never follows a symlink, should one replace the
        // file meanwhile, and fails on a directory.
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The process id in `rest`, the part of a temporary file's name after
/// `.<name>.`, when it is exactly `<pid>.<n>.edit` (decimal digits, a
/// pid up to `i32::MAX` and a `u64`).
fn stale_pid(rest: &str) -> Option<u32> {
    let (pid, rest) = rest.split_once('.')?;
    let n = rest.strip_suffix(".edit")?;
    let digits = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(pid, 10) || !digits(n, 20) {
        return None;
    }
    n.parse::<u64>().ok()?;
    // A pid is a positive `pid_t`: past `i32::MAX` no process wrote it,
    // and `ps -p` reports it as not running.
    pid.parse::<i32>().ok()?.try_into().ok()
}

/// Whether `meta` belongs to the user `owner`.
fn owned_by(meta: &std::fs::Metadata, owner: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        meta.uid() == owner
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, owner);
        false
    }
}

/// Whether a process with id `pid` runs, asked of `/bin/ps` (this crate
/// does not call `kill(2)`, which needs `unsafe`, S7). Anything but "no
/// such process" (`ps` failing to start, an unexpected exit) counts as
/// running, so a file is only removed when its process is known gone.
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |status| status.code() != Some(1))
}

/// Flushes the directory of `path`, so a rename in it survives a power
/// loss. Best effort: not every file system supports it.
fn sync_dir(path: &Path) {
    let Some(dir) = path.parent() else { return };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    if let Err(error) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::debug!(
            target: "tessera_config",
            path = %dir.display(),
            %error,
            "configuration directory not flushed"
        );
    }
}

/// Whether an edit may make a file the loader fails on load
/// ([`ConfigEditor::replace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejected {
    /// No: the edited file must fail as the opened one does.
    Refuse,
    /// Yes: the edit replaces the value the loader rejected.
    Replace,
}

/// A document parsed every way the checks need.
struct State {
    doc: DocumentMut,
    plain: toml::Table,
    comments: Vec<String>,
    /// `None` when the loader fails, with its error in `error`.
    warnings: Option<Vec<Warning>>,
    error: Option<ParseError>,
    themes: Vec<String>,
    config: Option<Config>,
}

/// Why [`State::of`] failed.
enum Parsed {
    /// Not TOML.
    Toml(ParseError),
    /// TOML the editor cannot read.
    Editor,
}

impl State {
    fn of(text: &str) -> Result<Self, Parsed> {
        let plain: toml::Table = toml::from_str(text).map_err(|e| Parsed::Toml(e.into()))?;
        let doc: DocumentMut = text.parse().map_err(|_| Parsed::Editor)?;
        let comments = doc_comments(&doc);
        // The loader logs what it warns about; a check is not a load, and
        // a refused value must not reach the log (S8).
        let loaded = Config::from_toml_lenient_logging(text, Logging::Quiet);
        let (warnings, error, themes, config) = match loaded {
            Ok(loaded) => {
                let themes = theme_names(&loaded.config);
                (Some(loaded.warnings), None, themes, Some(loaded.config))
            }
            Err(Error::Parse(error)) => (None, Some(error), Vec::new(), None),
            Err(_) => (None, None, Vec::new(), None),
        };
        Ok(Self {
            doc,
            plain,
            comments,
            warnings,
            error,
            themes,
            config,
        })
    }
}

/// The theme names `config` uses: its own and its profiles'.
fn theme_names(config: &Config) -> Vec<String> {
    let mut names = vec![config.appearance.theme.clone()];
    names.extend(
        config
            .profiles
            .iter()
            .filter_map(|p| p.appearance.as_ref().and_then(|a| a.theme.clone())),
    );
    names
}

/// Whether `saved` (the document with its line endings) reads as `plain`.
fn same_as_saved(plain: &toml::Table, saved: &str) -> bool {
    toml::from_str::<toml::Table>(saved).is_ok_and(|disk| {
        same(
            normalized(&toml::Value::Table(disk)).as_ref(),
            normalized(&toml::Value::Table(plain.clone())).as_ref(),
        )
    })
}

/// `warnings` as the loader names them once the array element at `path`
/// is removed: those of the element are gone and those of the later
/// elements move down one index (`profiles[2].bogus` becomes
/// `profiles[1].bogus` when `profiles[0]` goes). Unchanged when `path`
/// does not end at an array element.
fn renumbered(warnings: &[Warning], path: &KeyPath) -> Vec<Warning> {
    let Some((Step::Index(removed), array)) = path.0.split_last() else {
        return warnings.to_vec();
    };
    // The array's path as the loader writes it (`profiles`).
    let prefix = loader_path(array);
    // `Some(None)` for a warning of the removed element.
    let shift = |path: &str| -> Option<Option<String>> {
        let rest = path.strip_prefix(prefix.as_str())?.strip_prefix('[')?;
        let (index, rest) = rest.split_once(']')?;
        if !(rest.is_empty() || rest.starts_with(['.', '['])) {
            return None;
        }
        let index: usize = index.parse().ok()?;
        match index.cmp(removed) {
            std::cmp::Ordering::Less => None,
            std::cmp::Ordering::Equal => Some(None),
            std::cmp::Ordering::Greater => Some(Some(format!("{prefix}[{}]{rest}", index - 1))),
        }
    };
    warnings
        .iter()
        .filter_map(|warning| match warning {
            Warning::UnknownKey { path } => match shift(path) {
                None => Some(warning.clone()),
                Some(None) => None,
                Some(Some(path)) => Some(Warning::UnknownKey { path }),
            },
            Warning::OutOfRange { path, detail } => match shift(path) {
                None => Some(warning.clone()),
                Some(None) => None,
                Some(Some(path)) => Some(Warning::OutOfRange {
                    path,
                    detail: detail.clone(),
                }),
            },
            // One profile fewer past the cap.
            Warning::TooManyProfiles { dropped } if prefix == "profiles" => {
                (*dropped > 1).then(|| Warning::TooManyProfiles {
                    dropped: dropped - 1,
                })
            }
            Warning::UnknownKeysOmitted { .. }
            | Warning::TooManyProfiles { .. }
            | Warning::ProfileNotFound { .. }
            | Warning::ThemeNotFound { .. }
            | Warning::ThemeInvalid { .. }
            | Warning::KeyBinding { .. }
            | Warning::TooManyKeyBindings { .. }
            | Warning::ProfileFile { .. }
            | Warning::ProfileFileIgnored { .. }
            | Warning::TooManyProfileFiles { .. } => Some(warning.clone()),
        })
        .collect()
}

/// `steps` as the loader writes a path (`profiles[0].appearance`), before
/// escaping and cutting.
fn loader_path(steps: &[Step]) -> String {
    use std::fmt::Write as _;

    let mut path = String::new();
    for step in steps {
        match step {
            Step::Key(key) => {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
            }
            Step::Index(index) => {
                let _ = write!(path, "[{index}]");
            }
        }
    }
    path
}

/// The value at `steps` in `table`, `None` when there is none.
fn plain_at<'a>(table: &'a toml::Table, steps: &[Step]) -> Option<&'a toml::Value> {
    let (Step::Key(first), rest) = steps.split_first()? else {
        return None;
    };
    let mut node = table.get(first)?;
    for step in rest {
        node = match (step, node) {
            (Step::Key(key), toml::Value::Table(t)) => t.get(key)?,
            (Step::Index(i), toml::Value::Array(a)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(node)
}

/// The reason of the [`EditError::Unsupported`] for an edit of a profile
/// that a file in `profiles/` defines.
pub const FILE_PROFILE_REFUSED: &str =
    "this profile is defined in a file in profiles/, not in config.toml; edit that file instead";

/// A list the loader reads only the first [`LoaderCap::limit`] items of,
/// or a list of its warnings it cuts there. The loader never checks what
/// lies past such a cap, so neither can an edit's check: an edit there is
/// refused ([`past_loader_cap`]) or checked another way. Every such cap is
/// a row of [`LOADER_CAPS`], and a test iterates the table (a [`Cap`]
/// variant added without a row, or a row without a refusal test, fails
/// it). A new loader cap that gets neither a variant nor a row is not
/// caught by that test: it cannot see the loader's caps (#526 M9).
#[derive(Debug, Clone, Copy)]
struct LoaderCap {
    /// Which cap, for the test that iterates the table.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the test that iterates the table")
    )]
    cap: Cap,
    /// What the cap counts.
    counts: Counts,
    /// How many the loader reads.
    limit: usize,
    /// The reason of the refusal, naming the cap ("past the loader's cap
    /// of N").
    reason: &'static str,
}

/// The rows of [`LOADER_CAPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cap {
    /// [`MAX_PROFILES`](crate::MAX_PROFILES) `[[profiles]]` entries.
    Profiles,
    /// [`MAX_SMART_RULES`](crate::MAX_SMART_RULES) smart selection rules.
    SmartRules,
    /// [`MAX_KEY_BINDINGS`](crate::MAX_KEY_BINDINGS) `[keys]` entries.
    KeyBindings,
    /// [`MAX_UNKNOWN_KEY_WARNINGS`](crate::MAX_UNKNOWN_KEY_WARNINGS)
    /// unknown-key warnings.
    UnknownKeys,
}

/// What a [`LoaderCap`] counts.
#[derive(Debug, Clone, Copy)]
enum Counts {
    /// The elements of the array at this key path: an edit at or under an
    /// index from the cap on is refused.
    Elements(&'static [&'static str]),
    /// The entries of the table at this key path, in document order: an
    /// edit of an entry from the cap on, or a new entry with the table at
    /// the cap, is refused.
    Entries(&'static [&'static str]),
    /// The unknown keys the loader names: those past the cap are counted
    /// ([`Warning::UnknownKeysOmitted`]), and [`new_warnings`] takes the
    /// count as that many present, [`unknown_above`] asks about a new
    /// key's table without the rest of the file. Nothing is refused for it.
    UnknownKeyWarnings,
}

/// The loader's caps (docs/security.md S5) that an edit's check meets.
const LOADER_CAPS: [LoaderCap; 4] = [
    LoaderCap {
        cap: Cap::Profiles,
        counts: Counts::Elements(&["profiles"]),
        limit: crate::MAX_PROFILES,
        reason: "past the loader's cap of 256 profiles: the app never reads this one",
    },
    LoaderCap {
        cap: Cap::SmartRules,
        counts: Counts::Elements(&["selection", "smart_rules"]),
        limit: crate::MAX_SMART_RULES,
        reason: "past the loader's cap of 64 smart rules: the app never reads this one",
    },
    LoaderCap {
        cap: Cap::KeyBindings,
        counts: Counts::Entries(&["keys"]),
        limit: crate::MAX_KEY_BINDINGS,
        reason: "past the loader's cap of 512 key bindings: the app never reads this one",
    },
    LoaderCap {
        cap: Cap::UnknownKeys,
        counts: Counts::UnknownKeyWarnings,
        limit: crate::MAX_UNKNOWN_KEY_WARNINGS,
        reason: "past the loader's cap of 64 unknown-key warnings",
    },
];

const _: () = {
    assert!(
        crate::MAX_PROFILES == 256
            && crate::MAX_SMART_RULES == 64
            && crate::MAX_KEY_BINDINGS == 512
            && crate::MAX_UNKNOWN_KEY_WARNINGS == 64,
        "the reasons in LOADER_CAPS name the caps"
    );
};

/// [`EditError::Unsupported`] when `path`, in `doc` (the document before
/// the edit), is in an item past one of the [`LOADER_CAPS`]. The loader
/// drops those unread, so the checks cannot see an edit of one:
/// `profiles[256].name` on a file with 300 would rename its 257th entry
/// unseen (#342 M7), `selection.smart_rules[64].regex = "("` would be
/// accepted where `[63]` is refused (#449), and removing `profiles[257]`
/// or a key in it would change a profile the app never reads (#430).
fn past_loader_cap(doc: &DocumentMut, path: &KeyPath) -> Result<(), EditError> {
    for row in &LOADER_CAPS {
        let past = match row.counts {
            Counts::Elements(at) => match path.0.get(at.len()) {
                Some(Step::Index(index)) if starts_with_keys(&path.0, at) => *index >= row.limit,
                _ => false,
            },
            Counts::Entries(at) => match path.0.get(at.len()) {
                Some(Step::Key(key)) if starts_with_keys(&path.0, at) => {
                    entry_position(doc, at, key) >= row.limit
                }
                _ => false,
            },
            Counts::UnknownKeyWarnings => false,
        };
        if past {
            return Err(EditError::Unsupported {
                path: path.clone(),
                reason: row.reason,
            });
        }
    }
    Ok(())
}

/// The key path of the value of `text` that holds byte offset `at`: the
/// value of a `key = value` line, an inline table's entry as a key of its
/// own, an inline array's inline-table element as an index (its entries
/// searched in turn), and any other array as one value, so a scalar or
/// nested array that a list holds is the list. `None` when `at` is in no
/// value or `text` does not parse.
fn rejected_value(text: &str, at: usize) -> Option<Vec<Step>> {
    fn in_table(table: &dyn TableLike, at: usize, path: &mut Vec<Step>) -> bool {
        for (key, item) in table.iter() {
            path.push(Step::Key(key.to_owned()));
            if in_item(item, at, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    fn in_item(item: &Item, at: usize, path: &mut Vec<Step>) -> bool {
        match item {
            Item::Value(value) => in_value(value, at, path),
            Item::Table(table) => in_table(table, at, path),
            Item::ArrayOfTables(tables) => tables.iter().enumerate().any(|(index, table)| {
                path.push(Step::Index(index));
                in_table(table, at, path) || {
                    path.pop();
                    false
                }
            }),
            Item::None => false,
        }
    }
    fn in_value(value: &Value, at: usize, path: &mut Vec<Step>) -> bool {
        match value {
            Value::InlineTable(table) => in_table(table, at, path),
            Value::Array(array) => {
                if !value.span().is_some_and(|span| span.contains(&at)) {
                    return false;
                }
                for (index, element) in array.iter().enumerate() {
                    if matches!(element, Value::InlineTable(_)) {
                        path.push(Step::Index(index));
                        if in_value(element, at, path) {
                            return true;
                        }
                        path.pop();
                    }
                }
                true
            }
            value => value.span().is_some_and(|span| span.contains(&at)),
        }
    }
    let doc = toml_edit::Document::parse(text).ok()?;
    let mut path = Vec::new();
    in_table(doc.as_table(), at, &mut path).then_some(path)
}

/// Whether `steps` starts with the keys `keys`.
fn starts_with_keys(steps: &[Step], keys: &[&str]) -> bool {
    steps.len() >= keys.len()
        && steps
            .iter()
            .zip(keys)
            .all(|(step, key)| matches!(step, Step::Key(k) if k == key))
}

/// The position of the entry `key` among the entries of the table at
/// `table` in `doc`, in document order (the loader's order, for `[keys]`):
/// the number of entries when it has none (a new entry goes last), and 0
/// without the table.
fn entry_position(doc: &DocumentMut, table: &[&str], key: &str) -> usize {
    let mut node: Option<&dyn TableLike> = Some(doc.as_table());
    for step in table {
        node = node.and_then(|t| t.get(step)).and_then(Item::as_table_like);
    }
    let Some(node) = node else { return 0 };
    node.iter()
        .position(|(k, _)| k == key)
        .unwrap_or_else(|| node.len())
}

/// The [`Warning::UnknownKey`] for the table (or array) `steps` lies in,
/// when the loader does not know it.
///
/// Asked of the loader with a document that holds `value` at `steps` and
/// nothing else (every index made 0), not read off the file's warnings:
/// those list at most [`crate::MAX_UNKNOWN_KEY_WARNINGS`] unknown keys,
/// and a table hidden past them would let the key in (#430). The answer
/// does not depend on the rest of the file: whether the loader knows a
/// key depends only on the keys above it. Bounded: one key per step of
/// a [`KeyPath`].
fn unknown_above(steps: &[Step], value: &toml::Value) -> Option<Warning> {
    let mut probe = value.clone();
    for step in steps.iter().rev() {
        probe = match step {
            Step::Key(key) => toml::Value::Table(toml::Table::from_iter([(key.clone(), probe)])),
            Step::Index(_) => toml::Value::Array(vec![probe]),
        };
    }
    let toml::Value::Table(probe) = probe else {
        return None;
    };
    let text = toml::to_string(&probe).ok()?;
    let warnings = Config::from_toml_lenient_logging(&text, Logging::Quiet)
        .ok()?
        .warnings;
    let zeroed: Vec<Step> = steps
        .iter()
        .map(|step| match step {
            Step::Key(key) => Step::Key(key.clone()),
            Step::Index(_) => Step::Index(0),
        })
        .collect();
    (1..steps.len()).find_map(|len| {
        let above = escaped(&loader_path(&zeroed[..len]), MAX_WARNING_PATH_LEN);
        warnings
            .iter()
            .any(|w| matches!(w, Warning::UnknownKey { path } if same_path(path, &above)))
            .then(|| Warning::UnknownKey {
                path: escaped(&loader_path(&steps[..len]), MAX_WARNING_PATH_LEN),
            })
    })
}

/// The warnings of `after` that `before` does not hold, compared as
/// multisets (a second copy of a warning is new).
///
/// - A path the loader cut (at [`MAX_WARNING_PATH_LEN`]) matches a path
///   that starts with its uncut part, so a renumbered path, which the
///   loader cuts one character later or not at all, is not new (#324
///   items 15 and 17).
/// - The unknown keys past [`crate::MAX_UNKNOWN_KEY_WARNINGS`], which `before`
///   only counts ([`Warning::UnknownKeysOmitted`]), are present too: an
///   edit that makes one of them listed adds nothing (item 18), and
///   `after`'s count is new only when it exceeds what is left of
///   `before`'s (item 14: a removal that lowers it is not new, an edit
///   that adds an unknown key past the cap is).
fn new_warnings(before: &[Warning], after: &[Warning]) -> Vec<Warning> {
    let mut hidden = omitted(before);
    // Warnings are few (the loader bounds them); a list will do.
    let mut before: Vec<&Warning> = before
        .iter()
        .filter(|w| !matches!(w, Warning::UnknownKeysOmitted { .. }))
        .collect();
    let mut new = Vec::new();
    for warning in after {
        if let Warning::UnknownKeysOmitted { .. } = warning {
            continue;
        }
        match before.iter().position(|w| same_warning(w, warning)) {
            Some(at) => {
                before.swap_remove(at);
            }
            None if hidden > 0 && matches!(warning, Warning::UnknownKey { .. }) => hidden -= 1,
            None => new.push(warning.clone()),
        }
    }
    if let count @ 1.. = omitted(after)
        && count > hidden
    {
        new.push(Warning::UnknownKeysOmitted { count });
    }
    new
}

/// The count of [`Warning::UnknownKeysOmitted`] in `warnings` (0 without
/// one).
fn omitted(warnings: &[Warning]) -> usize {
    warnings
        .iter()
        .map(|w| match w {
            Warning::UnknownKeysOmitted { count } => *count,
            _ => 0,
        })
        .sum()
}

/// Whether `a` and `b` are the same warning, a cut path matching any path
/// that starts with its uncut part (see [`new_warnings`]).
fn same_warning(a: &Warning, b: &Warning) -> bool {
    match (a, b) {
        (Warning::UnknownKey { path: a }, Warning::UnknownKey { path: b }) => same_path(a, b),
        (
            Warning::OutOfRange { path: a, detail: x },
            Warning::OutOfRange { path: b, detail: y },
        ) => x == y && same_path(a, b),
        _ => a == b,
    }
}

/// Whether two warning paths name the same key: equal, or one was cut and
/// the other starts with its uncut part.
fn same_path(a: &str, b: &str) -> bool {
    match (cut_prefix(a), cut_prefix(b)) {
        (None, None) => a == b,
        (Some(cut), None) => b.starts_with(cut),
        (None, Some(cut)) => a.starts_with(cut),
        (Some(x), Some(y)) => x.starts_with(y) || y.starts_with(x),
    }
}

/// How many times each string occurs.
fn multiset(items: &[String]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for item in items {
        *counts.entry(item.as_str()).or_insert(0) += 1;
    }
    counts
}

/// `value` with each integer that `config` (the document loaded with
/// `value` set at `steps`) holds as a float made a float; `None` when
/// there is none. The loader reads `1` as `1.0` for a float setting, and
/// the file then says `opacity = 1.0` (#324 item 4); an integer setting
/// keeps its integer.
fn floats_where_config_has_them(
    value: toml::Value,
    config: &Config,
    steps: &[Step],
) -> Option<toml::Value> {
    /// `value` against `loaded`, the setting as the loader read it, and
    /// whether anything was turned into a float.
    fn coerce(value: toml::Value, loaded: &toml::Value) -> (toml::Value, bool) {
        match (value, loaded) {
            #[expect(
                clippy::cast_precision_loss,
                reason = "as the loader reads an integer for a float"
            )]
            (toml::Value::Integer(i), toml::Value::Float(_)) => {
                (toml::Value::Float(i as f64), true)
            }
            (toml::Value::Array(items), toml::Value::Array(loaded)) => {
                let mut changed = false;
                let items = items
                    .into_iter()
                    .zip(loaded.iter().map(Some).chain(std::iter::repeat(None)))
                    .map(|(item, loaded)| match loaded {
                        Some(loaded) => {
                            let (item, coerced) = coerce(item, loaded);
                            changed |= coerced;
                            item
                        }
                        None => item,
                    })
                    .collect();
                (toml::Value::Array(items), changed)
            }
            (value, _) => (value, false),
        }
    }

    if !holds_integer(&value) {
        return None;
    }
    let mut loaded = toml::Value::try_from(config).ok()?;
    for step in steps {
        loaded = match (step, loaded) {
            (Step::Key(key), toml::Value::Table(mut table)) => table.remove(key)?,
            (Step::Index(index), toml::Value::Array(mut array)) => {
                (*index < array.len()).then(|| array.swap_remove(*index))?
            }
            _ => return None,
        };
    }
    let (value, changed) = coerce(value, &loaded);
    changed.then_some(value)
}

/// Whether `value` is or holds an integer.
fn holds_integer(value: &toml::Value) -> bool {
    match value {
        toml::Value::Integer(_) => true,
        toml::Value::Array(items) => items.iter().any(holds_integer),
        _ => false,
    }
}

/// `value` as a TOML editor value.
fn edit_value(value: &toml::Value, depth: usize) -> Result<Value, &'static str> {
    if depth > MAX_VALUE_DEPTH {
        return Err("the value is nested too deeply");
    }
    Ok(match value {
        toml::Value::String(s) => Value::from(s.as_str()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Datetime(d) => Value::from(
            d.to_string()
                .parse::<toml_edit::Datetime>()
                .map_err(|_| "the date-time cannot be written")?,
        ),
        toml::Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(edit_value(item, depth + 1)?);
            }
            Value::Array(array)
        }
        toml::Value::Table(_) => return Err("a table is set key by key"),
    })
}

/// A new, empty container for the key whose path continues with `rest`,
/// in a table that is `inline` or `dotted`.
fn container(inline: bool, dotted: bool, rest: &[Step]) -> Item {
    let tables = match rest {
        [Step::Key(_), ..] => {
            return if inline {
                Item::Value(Value::InlineTable(InlineTable::new()))
            } else {
                let mut table = Table::new();
                table.set_dotted(dotted);
                Item::Table(table)
            };
        }
        [Step::Index(_), Step::Key(_), ..] => true,
        _ => false,
    };
    if tables && !inline {
        Item::ArrayOfTables(ArrayOfTables::new())
    } else {
        Item::Value(Value::Array(Array::new()))
    }
}

/// Sets `steps` (starting at a key of `table`) to `value`. A replaced
/// value keeps the comments around it; those inside it (between an
/// array's brackets) are pushed onto `moved` and put on lines of their own
/// above the nearest key of a table that is not inline (#324 item 21):
/// the key replaced, or the one holding the inline table or array it is
/// in.
fn set_table(
    table: &mut dyn TableLike,
    inline: bool,
    steps: &[Step],
    value: Value,
    moved: &mut Vec<String>,
) -> Result<(), &'static str> {
    let Some((Step::Key(key), rest)) = steps.split_first() else {
        return Err("an index into a table");
    };
    if rest.is_empty() {
        match table.get_mut(key) {
            Some(Item::Value(old)) if !matches!(old, Value::InlineTable(_)) => {
                inner_comments(old, moved);
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
            }
            Some(Item::None) | None => {
                table.insert(key, Item::Value(value));
            }
            Some(_) => return Err("a value cannot replace a table"),
        }
        place_above(table, inline, key, moved);
        return Ok(());
    }
    if table.get(key).is_none_or(Item::is_none) {
        let dotted = table.is_dotted();
        table.insert(key, container(inline, dotted, rest));
    }
    match table.get_mut(key) {
        Some(Item::Table(child)) => set_table(child, false, rest, value, moved),
        Some(Item::Value(Value::InlineTable(child))) => set_table(child, true, rest, value, moved),
        Some(Item::Value(Value::Array(child))) => set_array(child, rest, value, moved),
        Some(Item::ArrayOfTables(child)) => set_tables(child, rest, value, moved),
        _ => Err("the path runs through a value"),
    }?;
    place_above(table, inline, key, moved);
    Ok(())
}

/// Pushes the comments inside `value` (between an array's brackets), not
/// those around it.
fn inner_comments(value: &Value, out: &mut Vec<String>) {
    if let Value::Array(array) = value {
        for item in array {
            value_comments(item, out);
        }
        raw_comments(array.trailing(), out);
    }
}

/// Takes the 0.2.x template's comment ([`crate::load::BLUR_RADIUS_0_2_COMMENT`],
/// "No effect in 0.2.0 ...") off the lines above `[appearance]
/// blur_radius` when `steps` sets that key, returning the lines taken: the
/// user chose the value, so it is no longer the template's 0, which the
/// loader reads as the default (#491), and the comment is wrong since 0.3.0.
fn retire_0_2_blur_radius_comment(doc: &mut DocumentMut, steps: &[Step]) -> Vec<String> {
    let [Step::Key(table), Step::Key(key)] = steps else {
        return Vec::new();
    };
    if table != "appearance" || key != "blur_radius" {
        return Vec::new();
    }
    let Some(mut key) = doc
        .get_mut("appearance")
        .and_then(Item::as_table_like_mut)
        .and_then(|appearance| appearance.key_mut("blur_radius"))
    else {
        return Vec::new();
    };
    let [first, second] = crate::load::BLUR_RADIUS_0_2_COMMENT;
    let prefix = raw_text(key.leaf_decor().prefix());
    let lines = format!("{first}\n{second}\n");
    let Some(kept) = prefix.strip_suffix(lines.as_str()) else {
        return Vec::new();
    };
    if !(kept.is_empty() || kept.ends_with('\n')) {
        return Vec::new();
    }
    let kept = kept.to_owned();
    key.leaf_decor_mut().set_prefix(kept);
    vec![first.to_owned(), second.to_owned()]
}

/// Puts `moved` on lines of their own above `key` of `table`, indented
/// like the key, unless `table` is inline (they go above the key holding
/// it). Its line's prefix holds the lines above it, also for a dotted key.
fn place_above(table: &mut dyn TableLike, inline: bool, key: &str, moved: &mut Vec<String>) {
    if inline || moved.is_empty() {
        return;
    }
    let Some(mut key) = table.key_mut(key) else {
        return;
    };
    let prefix = raw_text(key.leaf_decor().prefix()).to_owned();
    let at = prefix.rfind('\n').map_or(0, |nl| nl + 1);
    let indent = &prefix[at..];
    let mut lines = prefix[..at].to_owned();
    for comment in moved.drain(..) {
        lines.push_str(indent);
        lines.push_str(&comment);
        lines.push('\n');
    }
    lines.push_str(indent);
    key.leaf_decor_mut().set_prefix(lines);
}

/// [`set_table`] for a step into an array of values.
fn set_array(
    array: &mut Array,
    steps: &[Step],
    value: Value,
    moved: &mut Vec<String>,
) -> Result<(), &'static str> {
    let Some((&Step::Index(index), rest)) = steps.split_first() else {
        return Err("a key into an array");
    };
    if rest.is_empty() {
        return match array.get(index) {
            Some(Value::InlineTable(_)) => Err("a value cannot replace a table"),
            Some(old) => {
                inner_comments(old, moved);
                array.replace_formatted(index, {
                    let mut value = value;
                    *value.decor_mut() = old.decor().clone();
                    value
                });
                Ok(())
            }
            None if index == array.len() => {
                push_element(array, value);
                Ok(())
            }
            None => Err("no such array element"),
        };
    }
    if index == array.len() && matches!(rest, [Step::Key(_), ..]) {
        array.push(InlineTable::new());
    }
    match array.get_mut(index) {
        Some(Value::InlineTable(child)) => set_table(child, true, rest, value, moved),
        Some(Value::Array(child)) => set_array(child, rest, value, moved),
        Some(_) => Err("the path runs through a value"),
        None => Err("no such array element"),
    }
}

/// Appends `value` to `array`. In an array written over several lines the
/// new element goes on a line of its own, indented like the last one,
/// after the comment on the last element's line; the comment lines before
/// the `]` stay last.
fn push_element(array: &mut Array, mut value: Value) {
    let mut tail = raw_text(Some(array.trailing())).to_owned();
    let n = array.len();
    let mut indent = String::from("  ");
    if let Some(last) = n.checked_sub(1).and_then(|i| array.get_mut(i)) {
        let prefix = raw_text(last.decor().prefix());
        if let Some(nl) = prefix.rfind('\n') {
            prefix[nl + 1..].clone_into(&mut indent);
        }
        let suffix = raw_text(last.decor().suffix()).to_owned();
        if suffix.contains('\n') {
            // No comma after the last element: what follows it goes after
            // the new one's comma.
            last.decor_mut().set_suffix("");
            tail.insert_str(0, &suffix);
        }
    }
    if let Some(nl) = tail.find('\n') {
        value
            .decor_mut()
            .set_prefix(format!("{}{indent}", &tail[..=nl]));
        value.decor_mut().set_suffix("");
        array.set_trailing(format!("\n{}", &tail[nl + 1..]));
    }
    array.push_formatted(value);
}

/// [`set_table`] for a step into an array of tables.
fn set_tables(
    tables: &mut ArrayOfTables,
    steps: &[Step],
    value: Value,
    moved: &mut Vec<String>,
) -> Result<(), &'static str> {
    let Some((&Step::Index(index), rest)) = steps.split_first() else {
        return Err("a key into an array");
    };
    if rest.is_empty() {
        return Err("a value cannot replace a table");
    }
    if index == tables.len() {
        tables.push(Table::new());
    }
    match tables.get_mut(index) {
        Some(child) => set_table(child, false, rest, value, moved),
        None => Err("no such array element"),
    }
}

/// Removes `steps` (starting at a key of `table`); `Ok(false)` when there
/// is nothing there. The removed item's comments are pushed onto
/// `removed`.
fn remove_table(
    table: &mut dyn TableLike,
    steps: &[Step],
    removed: &mut Vec<String>,
) -> Result<bool, &'static str> {
    let Some((Step::Key(key), rest)) = steps.split_first() else {
        return Ok(false);
    };
    if rest.is_empty() {
        if let Some(k) = table.key(key) {
            decor_comments(k.leaf_decor(), removed);
            decor_comments(k.dotted_decor(), removed);
        }
        return Ok(match table.remove(key) {
            Some(Item::None) | None => false,
            Some(item) => {
                item_comments(&item, removed);
                true
            }
        });
    }
    match table.get_mut(key) {
        Some(Item::Table(child)) => remove_table(child, rest, removed),
        Some(Item::Value(Value::InlineTable(child))) => remove_table(child, rest, removed),
        Some(Item::Value(Value::Array(child))) => remove_array(child, rest, removed),
        Some(Item::ArrayOfTables(child)) => {
            let Some((&Step::Index(index), rest)) = rest.split_first() else {
                return Ok(false);
            };
            if index >= child.len() {
                return Ok(false);
            }
            if rest.is_empty() {
                let gone = child.remove(index);
                table_comments(&gone, removed);
                return Ok(true);
            }
            match child.get_mut(index) {
                Some(element) => remove_table(element, rest, removed),
                None => Ok(false),
            }
        }
        Some(Item::Value(_)) => Err("the path runs through a value"),
        Some(Item::None) | None => Ok(false),
    }
}

/// [`remove_table`] for a step into an array of values.
fn remove_array(
    array: &mut Array,
    steps: &[Step],
    removed: &mut Vec<String>,
) -> Result<bool, &'static str> {
    let Some((&Step::Index(index), rest)) = steps.split_first() else {
        return Ok(false);
    };
    if index >= array.len() {
        return Ok(false);
    }
    if rest.is_empty() {
        let gone = array.remove(index);
        value_comments(&gone, removed);
        return Ok(true);
    }
    match array.get_mut(index) {
        Some(Value::InlineTable(child)) => remove_table(child, rest, removed),
        Some(Value::Array(child)) => remove_array(child, rest, removed),
        Some(_) => Err("the path runs through a value"),
        None => Ok(false),
    }
}

/// The comments a removal keeps, and the mark that tells where they go.
struct Carried {
    /// The lines above the removed item, and the comment on its line when
    /// it is a value.
    kept: String,
    /// A comment line, found nowhere else in the document, that replaced
    /// them in the removed item's leading decoration.
    mark: String,
}

/// Before `steps` is removed: takes the lines above the item (and the
/// comment on its line when it is a value) to keep, and puts a mark where
/// they were. The indentation of the item's own line goes with the item.
/// `None` when no comment is kept (or there is no such item).
fn cut_leading_decor(doc: &mut DocumentMut, steps: &[Step]) -> Option<Carried> {
    let mark = unused_mark(&doc.to_string());
    let mut kept = None;
    with_leading_decor(doc.as_table_mut(), steps, &mut |decor, trailing| {
        let prefix = decor
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_owned();
        let (above, indent) = prefix.split_at(prefix.rfind('\n').map_or(0, |i| i + 1));
        let mut keep = above.to_owned();
        if let Some(at) = trailing.and_then(|t| t.find('#').map(|at| &t[at..])) {
            keep.push_str(indent);
            keep.push_str(at.trim_end());
            keep.push('\n');
        }
        if keep.contains('#') {
            decor.set_prefix(format!("{mark}{indent}"));
            kept = Some(keep);
        }
    });
    kept.map(|kept| Carried { kept, mark })
}

/// What every edit mark starts with; a hexadecimal number and a line
/// break follow.
const MARK_START: &str = "#tessera-edit-";

/// The first mark (`#tessera-edit-<n>` and a line break, `n` in lowercase
/// hexadecimal from 0) found nowhere in `text`. One pass over `text`: the
/// marks it holds are collected, then the first number missing is taken
/// (a text with 20 000 of them took seconds when each candidate was
/// searched for in turn, #324 item 3).
fn unused_mark(text: &str) -> String {
    let mut taken = std::collections::HashSet::new();
    for (at, _) in text.match_indices(MARK_START) {
        let rest = &text[at + MARK_START.len()..];
        let digits = rest
            .bytes()
            .take_while(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            .count();
        if rest[digits..].starts_with('\n')
            && let Ok(n) = u64::from_str_radix(&rest[..digits], 16)
            // `#tessera-edit-01` is not the mark of 1.
            && format!("{n:x}").len() == digits
        {
            taken.insert(n);
        }
    }
    // Of the numbers 0..=taken.len(), at least one is free.
    let n = (0..=taken.len() as u64)
        .find(|n| !taken.contains(n))
        .unwrap_or_default();
    format!("{MARK_START}{n:x}\n")
}

/// Calls `f` with the decoration that holds the lines above the item at
/// `steps` (of `table`) and, when the item is a value, the text after it
/// (where a comment on its line is). Not called for a dotted table, whose
/// lines need not be together, nor when there is no such item.
fn with_leading_decor(
    table: &mut dyn TableLike,
    steps: &[Step],
    f: &mut dyn FnMut(&mut toml_edit::Decor, Option<&str>),
) {
    let Some((Step::Key(key), rest)) = steps.split_first() else {
        return;
    };
    if rest.is_empty() {
        match table.get_mut(key) {
            Some(Item::Table(t)) if !t.is_dotted() => f(t.decor_mut(), None),
            Some(Item::Value(value)) => {
                let trailing = raw_text(value.decor().suffix()).to_owned();
                if let Some(mut k) = table.key_mut(key) {
                    f(k.leaf_decor_mut(), Some(&trailing));
                }
            }
            _ => {}
        }
        return;
    }
    match table.get_mut(key) {
        Some(Item::Table(child)) => with_leading_decor(child, rest, f),
        Some(Item::Value(Value::InlineTable(child))) => with_leading_decor(child, rest, f),
        Some(Item::Value(Value::Array(child))) => with_leading_decor_array(child, rest, f),
        Some(Item::ArrayOfTables(child)) => {
            let Some((&Step::Index(index), rest)) = rest.split_first() else {
                return;
            };
            let Some(element) = child.get_mut(index) else {
                return;
            };
            if rest.is_empty() {
                f(element.decor_mut(), None);
            } else {
                with_leading_decor(element, rest, f);
            }
        }
        _ => {}
    }
}

/// [`with_leading_decor`] for a step into an array of values.
fn with_leading_decor_array(
    array: &mut Array,
    steps: &[Step],
    f: &mut dyn FnMut(&mut toml_edit::Decor, Option<&str>),
) {
    let Some((&Step::Index(index), rest)) = steps.split_first() else {
        return;
    };
    match array.get_mut(index) {
        Some(value) if rest.is_empty() => {
            let trailing = raw_text(value.decor().suffix()).to_owned();
            f(value.decor_mut(), Some(&trailing));
        }
        Some(Value::InlineTable(child)) => with_leading_decor(child, rest, f),
        Some(Value::Array(child)) => with_leading_decor_array(child, rest, f),
        _ => {}
    }
}

/// The text of a decoration part (empty when there is none).
fn raw_text(raw: Option<&toml_edit::RawString>) -> &str {
    raw.and_then(toml_edit::RawString::as_str)
        .unwrap_or_default()
}

/// Pushes the comments in the decoration `decor`.
fn decor_comments(decor: &toml_edit::Decor, out: &mut Vec<String>) {
    for raw in [decor.prefix(), decor.suffix()].into_iter().flatten() {
        raw_comments(raw, out);
    }
}

/// Pushes the comments in `raw`, whitespace and comments between tokens:
/// each `#` there starts one, which runs to the end of the line.
fn raw_comments(raw: &toml_edit::RawString, out: &mut Vec<String>) {
    let Some(text) = raw.as_str() else { return };
    for line in text.lines() {
        if let Some(at) = line.find('#') {
            out.push(line[at..].trim_end().to_owned());
        }
    }
}

/// Pushes the comments of `item` and everything in it.
fn item_comments(item: &Item, out: &mut Vec<String>) {
    match item {
        Item::Value(value) => value_comments(value, out),
        Item::Table(table) => table_comments(table, out),
        Item::ArrayOfTables(tables) => {
            for table in tables {
                table_comments(table, out);
            }
        }
        Item::None => {}
    }
}

/// Pushes the comments of `table` (its header and everything in it).
fn table_comments(table: &Table, out: &mut Vec<String>) {
    decor_comments(table.decor(), out);
    for (key, item) in table {
        if let Some(k) = table.key(key) {
            decor_comments(k.leaf_decor(), out);
            decor_comments(k.dotted_decor(), out);
        }
        item_comments(item, out);
    }
}

/// Pushes the comments of `value` and everything in it.
fn value_comments(value: &Value, out: &mut Vec<String>) {
    decor_comments(value.decor(), out);
    match value {
        Value::Array(array) => {
            for item in array {
                value_comments(item, out);
            }
            raw_comments(array.trailing(), out);
        }
        Value::InlineTable(table) => {
            for (key, item) in table {
                if let Some(k) = table.key(key) {
                    decor_comments(k.leaf_decor(), out);
                    decor_comments(k.dotted_decor(), out);
                }
                value_comments(item, out);
            }
            raw_comments(table.trailing(), out);
        }
        _ => {}
    }
}

/// Every comment of `doc`.
fn doc_comments(doc: &DocumentMut) -> Vec<String> {
    let mut out = Vec::new();
    decor_comments(doc.decor(), &mut out);
    table_comments(doc.as_table(), &mut out);
    raw_comments(doc.trailing(), &mut out);
    out
}

/// Applies the edit to the plain TOML tree `node`: sets `steps` to `new`,
/// or removes it when `new` is `None`, creating containers on the way for
/// a set as [`ConfigEditor::set`] does. The reference the edited document
/// is checked against.
fn plain_edit(node: &mut toml::Value, steps: &[Step], new: Option<toml::Value>) {
    let Some((step, rest)) = steps.split_first() else {
        return;
    };
    if rest.is_empty() {
        match (node, step, new) {
            (toml::Value::Table(t), Step::Key(k), Some(v)) => {
                t.insert(k.clone(), v);
            }
            (toml::Value::Table(t), Step::Key(k), None) => {
                t.remove(k);
            }
            (toml::Value::Array(a), &Step::Index(i), Some(v)) => {
                if i < a.len() {
                    a[i] = v;
                } else if i == a.len() {
                    a.push(v);
                }
            }
            (toml::Value::Array(a), &Step::Index(i), None) if i < a.len() => {
                a.remove(i);
            }
            _ => {}
        }
        return;
    }
    let child = match (node, step) {
        (toml::Value::Table(t), Step::Key(k)) => {
            if new.is_some() && !t.contains_key(k) {
                let empty = match rest[0] {
                    Step::Key(_) => toml::Value::Table(toml::Table::new()),
                    Step::Index(_) => toml::Value::Array(Vec::new()),
                };
                t.insert(k.clone(), empty);
            }
            t.get_mut(k)
        }
        (toml::Value::Array(a), &Step::Index(i)) => {
            if new.is_some() && i == a.len() && matches!(rest[0], Step::Key(_)) {
                a.push(toml::Value::Table(toml::Table::new()));
            }
            a.get_mut(i)
        }
        _ => None,
    };
    if let Some(child) = child {
        plain_edit(child, rest, new);
    }
}

/// `value` without empty tables and arrays (recursively), which the
/// document need not spell out (an array of tables with no elements is
/// not written at all).
fn normalized(value: &toml::Value) -> Option<toml::Value> {
    match value {
        toml::Value::Table(table) => {
            let table: toml::Table = table
                .iter()
                .filter_map(|(k, v)| normalized(v).map(|v| (k.clone(), v)))
                .collect();
            (!table.is_empty()).then_some(toml::Value::Table(table))
        }
        toml::Value::Array(items) => {
            (!items.is_empty()).then(|| toml::Value::Array(items.iter().map(keep_shape).collect()))
        }
        other => Some(other.clone()),
    }
}

/// An array element, normalised without dropping it (an index means
/// something).
fn keep_shape(value: &toml::Value) -> toml::Value {
    normalized(value).unwrap_or_else(|| match value {
        toml::Value::Table(_) => toml::Value::Table(toml::Table::new()),
        _ => toml::Value::Array(Vec::new()),
    })
}

/// Whether `a` and `b` are the same TOML, a NaN equal to a NaN (a
/// document with `nan` in it is still editable).
fn same(a: Option<&toml::Value>, b: Option<&toml::Value>) -> bool {
    fn eq(a: &toml::Value, b: &toml::Value) -> bool {
        match (a, b) {
            (toml::Value::Float(x), toml::Value::Float(y)) => {
                x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
            }
            (toml::Value::Array(x), toml::Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(x, y)| eq(x, y))
            }
            (toml::Value::Table(x), toml::Value::Table(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| eq(v, w)))
            }
            _ => a == b,
        }
    }
    match (a, b) {
        (Some(a), Some(b)) => eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// `name` made safe to show (S3): without control characters (C0, DEL,
/// C1) and bidirectional formatting characters, at most
/// [`MAX_DISPLAY_NAME_LEN`] bytes (cut on a character boundary).
#[must_use]
pub fn display_name(name: &str) -> String {
    let mut out = String::new();
    for c in name
        .chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
    {
        if out.len() + c.len_utf8() > MAX_DISPLAY_NAME_LEN {
            break;
        }
        out.push(c);
    }
    out
}

/// Whether `c` changes the direction of text around it.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Where a [`ThemeEntry`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ThemeOrigin {
    /// A theme bundled with Tessera.
    Bundled,
    /// A file in the themes directory.
    User,
}

/// A theme a settings window offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    /// The name to show ([`display_name`]).
    pub name: String,
    /// The value for `appearance.theme` that selects this theme: a
    /// bundled theme's name, a user theme's file stem.
    pub id: String,
    /// Where it comes from.
    pub origin: ThemeOrigin,
}

/// The themes `appearance.theme` can name: the bundled ones (but those a
/// user file shadows) and then the loadable `*.toml` files of
/// `themes_dir` in file-name order (at most
/// [`MAX_THEME_FILES`] files of the first
/// [`MAX_THEME_DIR_ENTRIES`] entries). A
/// user file is left out when its stem would not show as written
/// ([`display_name`] would change it), since its id would not select it.
#[must_use]
pub fn theme_entries(themes_dir: &Path) -> Vec<ThemeEntry> {
    let mut stems: Vec<(String, PathBuf)> =
        std::fs::read_dir(themes_dir).map_or_default(|entries| {
            entries
                .take(MAX_THEME_DIR_ENTRIES)
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let file_name = entry.file_name().into_string().ok()?;
                    let stem = file_name.strip_suffix(".toml")?;
                    (!stem.is_empty() && !stem.starts_with('.'))
                        .then(|| (stem.to_owned(), entry.path()))
                })
                .collect()
        });
    stems.sort();
    let mut user = Vec::new();
    let mut shadowing = Vec::new();
    for (stem, path) in stems.into_iter().take(MAX_THEME_FILES) {
        let Ok(theme) = Theme::load(&path) else {
            continue;
        };
        shadowing.push(stem.clone());
        shadowing.push(theme.name.clone());
        if display_name(&stem) != stem {
            continue;
        }
        let name = display_name(&theme.name);
        user.push(ThemeEntry {
            name: if name.trim().is_empty() {
                stem.clone()
            } else {
                name
            },
            id: stem,
            origin: ThemeOrigin::User,
        });
    }
    let shadowed = |bundled: &str| {
        shadowing.iter().any(|name| {
            name.to_lowercase() == bundled.to_lowercase()
                || slug(name).is_some_and(|s| slug(bundled) == Some(s))
        })
    };
    let mut entries: Vec<ThemeEntry> = Theme::bundled_names()
        .iter()
        .filter(|name| !shadowed(name))
        .map(|&name| ThemeEntry {
            name: display_name(name),
            id: name.to_owned(),
            origin: ThemeOrigin::Bundled,
        })
        .collect();
    entries.extend(user);
    entries
}

/// A theme [`import_theme`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTheme {
    /// The theme's name to show ([`display_name`]).
    pub name: String,
    /// The value for `appearance.theme` that selects it: its file stem, a
    /// [`slug`] of the name.
    pub id: String,
    /// The file written.
    pub path: PathBuf,
}

/// Imports the colour scheme file `source` (`.itermcolors`, or base16
/// `.yaml` / `.yml`, by extension) into `themes_dir` as `<slug>.toml`,
/// named after the file's stem (or the scheme's own name, for base16),
/// clipped to [`MAX_THEME_NAME_LEN`](crate::MAX_THEME_NAME_LEN) bytes.
///
/// The source is read as the importers read it: a regular file of at
/// most [`MAX_IMPORT_LEN`](crate::MAX_IMPORT_LEN) bytes, never blocking
/// on a FIFO. The theme file is written to a temporary file and linked
/// into place, so it never replaces an existing file and never appears
/// half-written; `themes_dir` is created when missing.
///
/// # Errors
///
/// [`EditError::Unsupported`] for an unknown extension or a scheme the
/// importer rejects, [`EditError::ThemeExists`] when the file is there
/// already, [`EditError::File`] when reading or writing fails.
pub fn import_theme(themes_dir: &Path, source: &Path) -> Result<ImportedTheme, EditError> {
    let unsupported = |reason| EditError::Unsupported {
        path: KeyPath(vec![
            Step::Key("appearance".into()),
            Step::Key("theme".into()),
        ]),
        reason,
    };
    let extension = source
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let bytes = crate::read_scheme_file(source)?;
    let stem = source
        .file_stem()
        .map_or_default(|s| s.to_string_lossy().into_owned());
    let imported = match extension.as_deref() {
        Some("itermcolors") => crate::import_itermcolors(&bytes, clip_theme_name(&stem)),
        Some("yaml" | "yml") => {
            let text = std::str::from_utf8(&bytes).map_err(Error::from)?;
            crate::import_base16(text, clip_theme_name(&stem))
        }
        _ => return Err(unsupported("not an .itermcolors, .yaml or .yml file")),
    };
    let mut theme = imported.map_err(|_| unsupported("the colour scheme does not import"))?;
    let clipped = clip_theme_name(&theme.name).to_owned();
    theme.name = clipped;
    let id = slug(&theme.name).ok_or_else(|| unsupported("the scheme's name has no letters"))?;
    std::fs::create_dir_all(themes_dir)?;
    let file_name = format!("{id}.toml");
    let path = themes_dir.join(&file_name);
    let source_name = source
        .file_name()
        .map_or_default(|n| display_name(&n.to_string_lossy()));
    // One line: a crafted file name cannot start a TOML line of its own.
    let contents = format!(
        "# Imported by Tessera from {source_name}.\n{}",
        theme.to_toml()
    );
    if !write_new(&path, &contents)? {
        return Err(EditError::ThemeExists { file_name });
    }
    Ok(ImportedTheme {
        name: display_name(&theme.name),
        id,
        path,
    })
}

/// Creates `path` holding `contents` unless something (even a dangling
/// symlink) is there; `Ok(false)` then. Written to a temporary file and
/// hard-linked into place; where the file system has no hard links,
/// created exclusively and written in place.
fn write_new(path: &Path, contents: &str) -> std::io::Result<bool> {
    /// `errno` values read as "no hard links on this file system", as
    /// `crate::load` reads them: `EPERM`, `EXDEV`, `ENOSYS`, `ENOTSUP`,
    /// `EOPNOTSUPP`.
    #[cfg(not(target_os = "linux"))]
    const NO_HARD_LINK_ERRNOS: [i32; 5] = [1, 18, 78, 45, 102];
    #[cfg(target_os = "linux")]
    const NO_HARD_LINK_ERRNOS: [i32; 4] = [1, 18, 38, 95];

    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let (tmp, mut file) = temporary_file(path)?;
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let linked = written.map(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(Ok(())) => {
            sync_dir(path);
            Ok(true)
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Ok(Err(e))
            if e.raw_os_error()
                .is_some_and(|n| NO_HARD_LINK_ERRNOS.contains(&n)) =>
        {
            let mut file = match std::fs::File::create_new(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
                Err(e) => return Err(e),
            };
            if let Err(e) = file
                .write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
            Ok(true)
        }
        Err(e) | Ok(Err(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        /// Called by `save` between flushing the temporary file and the
        /// rename, on this thread.
        static BEFORE_RENAME: Cell<Option<fn()>> = const { Cell::new(None) };
    }

    pub(super) fn before_rename() {
        if let Some(hook) = BEFORE_RENAME.with(Cell::get) {
            hook();
        }
    }

    /// #324 item 3: the mark is the first number whose mark the text does
    /// not hold; text that only looks like a mark (a leading zero, no line
    /// break after it, upper case) does not take its number.
    #[test]
    fn the_mark_is_the_first_unused_one() {
        assert_eq!(unused_mark(""), "#tessera-edit-0\n");
        let mut taken = String::new();
        for n in 0..300u32 {
            std::fmt::Write::write_fmt(&mut taken, format_args!("#tessera-edit-{n:x}\n"))
                .expect("writing to a String");
        }
        assert_eq!(unused_mark(&taken), "#tessera-edit-12c\n");
        let gap = "x = \"#tessera-edit-0\n\"\n#tessera-edit-2\n#tessera-edit-1";
        assert_eq!(unused_mark(gap), "#tessera-edit-1\n");
        let lookalikes = "#tessera-edit-00\n#tessera-edit-A\n#tessera-edit-\n#tessera-edit-0x\n";
        assert_eq!(unused_mark(lookalikes), "#tessera-edit-0\n");
        let huge = "#tessera-edit-fffffffffffffffffffff\n#tessera-edit-0\n";
        assert_eq!(unused_mark(huge), "#tessera-edit-1\n");
    }

    fn editor_for(text: &str) -> (tempfile::TempDir, PathBuf, ConfigEditor) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let editor = ConfigEditor::open(&path).unwrap();
        (dir, path, editor)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A crash between the flushed temporary file and the rename (the
    /// hook panics there) leaves the old file exactly as it was; the next
    /// save works and replaces it.
    #[test]
    fn a_crash_before_the_rename_leaves_the_old_file() {
        let old = "# mine\n[appearance]\nopacity = 1.0\n";
        let (dir, path, mut editor) = editor_for(old);
        editor
            .set("appearance.opacity", toml::Value::Float(0.5))
            .unwrap();
        BEFORE_RENAME.with(|hook| hook.set(Some(|| panic!("simulated crash"))));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| editor.save()));
        BEFORE_RENAME.with(|hook| hook.set(None));
        assert!(crashed.is_err(), "the hook ran");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        // The flushed temporary file is what a crash leaves behind.
        let left = names(dir.path());
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(
            left[0].starts_with(".config.toml.")
                && Path::new(&left[0]).extension().is_some_and(|e| e == "edit")
        );
        let tmp = std::fs::read_to_string(dir.path().join(&left[0])).unwrap();
        assert_eq!(tmp, "# mine\n[appearance]\nopacity = 0.5\n");

        let mut again = ConfigEditor::open(&path).unwrap();
        again
            .set("appearance.opacity", toml::Value::Float(0.5))
            .unwrap();
        assert!(again.save().unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[appearance]\nopacity = 0.5\n"
        );
    }

    /// The last guard before an edit is kept: an edited text that loses a
    /// comment the edit did not remove, adds one, or changes a setting the
    /// edit did not name is refused as an internal error, and the
    /// document stays as it was.
    #[test]
    fn the_commit_guard_refuses_damage() {
        let old = "# a\n[appearance]\nopacity = 1.0 # b\n[terminal]\nscrollback_lines = 5\n";
        let (_dir, _path, mut editor) = editor_for(old);
        let path: KeyPath = "appearance.opacity".parse().unwrap();
        let expected: toml::Value =
            toml::from_str("appearance.opacity = 0.5\nterminal.scrollback_lines = 5\n").unwrap();
        let damaged = [
            // Loses "# a".
            "[appearance]\nopacity = 0.5 # b\n[terminal]\nscrollback_lines = 5\n",
            // Adds "# c".
            "# a\n[appearance]\nopacity = 0.5 # b\n# c\n[terminal]\nscrollback_lines = 5\n",
            // Changes another setting.
            "# a\n[appearance]\nopacity = 0.5 # b\n[terminal]\nscrollback_lines = 6\n",
        ];
        for text in damaged {
            let refused = editor.commit(&path, text, &expected, &[], &[]);
            assert!(
                matches!(refused, Err(EditError::Unsupported { .. })),
                "{text:?}: {refused:?}"
            );
            assert_eq!(editor.to_toml(), old);
        }
        // The same text with the comment the edit removed is kept.
        let ok = "[appearance]\nopacity = 0.5 # b\n[terminal]\nscrollback_lines = 5\n";
        editor
            .commit(&path, ok, &expected, &["# a".to_owned()], &[])
            .unwrap();
        assert_eq!(editor.to_toml(), ok);
    }

    /// An external write that lands after the temporary file is written
    /// (the last moment before the rename) is kept: `Conflict`, and no
    /// temporary file stays.
    #[test]
    fn an_edit_just_before_the_rename_is_a_conflict() {
        let (dir, path, mut editor) = editor_for("[appearance]\nopacity = 1.0\n");
        editor
            .set("appearance.opacity", toml::Value::Float(0.5))
            .unwrap();
        thread_local! {
            static PATH: std::cell::RefCell<PathBuf> = const { std::cell::RefCell::new(PathBuf::new()) };
        }
        PATH.with(|p| p.replace(path.clone()));
        BEFORE_RENAME.with(|hook| {
            hook.set(Some(|| {
                PATH.with(|p| std::fs::write(&*p.borrow(), "# theirs\n").unwrap());
            }));
        });
        let saved = editor.save();
        BEFORE_RENAME.with(|hook| hook.set(None));
        assert!(matches!(saved, Err(EditError::Conflict)), "{saved:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# theirs\n");
        assert_eq!(names(dir.path()), ["config.toml"]);
    }

    /// `count` distinct `[keys]` entries with `cmd`, each a valid binding.
    pub(crate) fn key_entries(count: usize) -> Vec<String> {
        let keys = ('a'..='z')
            .map(String::from)
            .chain(('0'..='9').map(String::from))
            .chain((1..=35).map(|n| format!("f{n}")));
        let keys: Vec<String> = keys.collect();
        let mods = [
            "",
            "ctrl+",
            "alt+",
            "shift+",
            "ctrl+alt+",
            "ctrl+shift+",
            "alt+shift+",
            "ctrl+alt+shift+",
        ];
        mods.iter()
            .flat_map(|m| keys.iter().map(move |k| format!("cmd+{m}{k}")))
            .take(count)
            .collect()
    }

    /// A `[keys]` table of `count` entries, all `new_tab`.
    fn keys_file(count: usize) -> String {
        use std::fmt::Write as _;

        let mut text = String::from("[keys]\n");
        for chord in key_entries(count) {
            writeln!(text, "\"{chord}\" = \"new_tab\"").unwrap();
        }
        text
    }

    #[track_caller]
    fn unsupported(result: Result<(), EditError>, reason: &str, what: &str) {
        match result {
            Err(EditError::Unsupported { reason: r, .. }) => assert_eq!(r, reason, "{what}"),
            other => panic!("{what}: {other:?}"),
        }
    }

    #[track_caller]
    fn refused(result: Result<(), EditError>, what: &str) {
        match result {
            Err(EditError::Refused { .. }) => {}
            other => panic!("{what}: {other:?}"),
        }
    }

    /// #449: every row of [`LOADER_CAPS`] has a refusal test, picked by an
    /// exhaustive match (a new [`Cap`] without one does not compile), and
    /// names its cap in its reason. Each test floods a file one past the
    /// cap: an edit of the item past it is refused, set and remove, with a
    /// valid and an invalid value, and the item before it keeps today's
    /// behaviour (a valid value set, an invalid one refused with the
    /// loader's warning).
    #[test]
    fn every_loader_cap_refuses_an_edit_past_it() {
        let mut seen = Vec::new();
        for row in &LOADER_CAPS {
            assert!(!seen.contains(&row.cap), "{:?} twice", row.cap);
            seen.push(row.cap);
            assert!(
                row.reason
                    .contains(&format!("past the loader's cap of {}", row.limit)),
                "{:?}: {}",
                row.cap,
                row.reason
            );
            match row.cap {
                Cap::Profiles => profiles_past_the_cap(row),
                Cap::SmartRules => smart_rules_past_the_cap(row),
                Cap::KeyBindings => key_bindings_past_the_cap(row),
                Cap::UnknownKeys => unknown_keys_past_the_cap(row),
            }
        }
        // Every cap has a row.
        for cap in [
            Cap::Profiles,
            Cap::SmartRules,
            Cap::KeyBindings,
            Cap::UnknownKeys,
        ] {
            match cap {
                Cap::Profiles | Cap::SmartRules | Cap::KeyBindings | Cap::UnknownKeys => {
                    assert!(seen.contains(&cap), "{cap:?} has no row");
                }
            }
        }
    }

    fn profiles_past_the_cap(row: &LoaderCap) {
        use std::fmt::Write as _;

        assert_eq!(row.limit, crate::MAX_PROFILES);
        let mut text = String::new();
        for i in 0..=row.limit + 1 {
            writeln!(text, "[[profiles]]\nname = \"p{i}\"").unwrap();
        }
        let (_dir, _path, mut editor) = editor_for(&text);
        let past = row.limit;
        for value in [s("beam"), s("zzz")] {
            unsupported(
                editor.set(&format!("profiles[{past}].appearance.cursor_shape"), value),
                row.reason,
                "set past",
            );
        }
        for path in [
            format!("profiles[{past}]"),
            format!("profiles[{}]", past + 1),
            format!("profiles[{}].name", past + 1),
        ] {
            unsupported(editor.remove(&path).map(drop), row.reason, &path);
        }
        assert_eq!(editor.to_toml(), text);
        let last = row.limit - 1;
        refused(
            editor.set(
                &format!("profiles[{last}].appearance.cursor_shape"),
                s("zzz"),
            ),
            "invalid before",
        );
        editor
            .set(
                &format!("profiles[{last}].appearance.cursor_shape"),
                s("beam"),
            )
            .unwrap();
        editor
            .remove(&format!("profiles[{last}].appearance"))
            .unwrap();
    }

    fn smart_rules_past_the_cap(row: &LoaderCap) {
        use std::fmt::Write as _;

        assert_eq!(row.limit, crate::MAX_SMART_RULES);
        let mut text = String::new();
        for i in 0..=row.limit {
            writeln!(text, "[[selection.smart_rules]]\nregex = \"x{i}\"").unwrap();
        }
        let (_dir, _path, mut editor) = editor_for(&text);
        let past = row.limit;
        for value in ["(", "a"] {
            unsupported(
                editor.set(&format!("selection.smart_rules[{past}].regex"), s(value)),
                row.reason,
                value,
            );
        }
        for path in [
            format!("selection.smart_rules[{past}]"),
            format!("selection.smart_rules[{past}].regex"),
        ] {
            unsupported(editor.remove(&path).map(drop), row.reason, &path);
        }
        assert_eq!(editor.to_toml(), text);
        let last = row.limit - 1;
        refused(
            editor.set(&format!("selection.smart_rules[{last}].regex"), s("(")),
            "invalid before",
        );
        editor
            .set(&format!("selection.smart_rules[{last}].regex"), s("a"))
            .unwrap();
    }

    fn key_bindings_past_the_cap(row: &LoaderCap) {
        assert_eq!(row.limit, crate::MAX_KEY_BINDINGS);
        let chords = key_entries(row.limit + 2);
        // At the cap: a new entry would be dropped.
        let text = keys_file(row.limit);
        let (_dir, _path, mut editor) = editor_for(&text);
        assert_eq!(editor.warnings.as_deref(), Some(&[][..]));
        let new = &chords[row.limit];
        unsupported(
            editor.set(&format!("keys.\"{new}\""), s("copy")),
            row.reason,
            "new at the cap",
        );
        let last = &chords[row.limit - 1];
        refused(
            editor.set(&format!("keys.\"{last}\""), toml::Value::Integer(3)),
            "invalid before",
        );
        editor.set(&format!("keys.\"{last}\""), s("copy")).unwrap();
        // Past the cap: the entries from the cap on are not read.
        let text = keys_file(row.limit + 1);
        let (_dir, _path, mut editor) = editor_for(&text);
        for value in [s("copy"), toml::Value::Integer(3)] {
            unsupported(
                editor.set(&format!("keys.\"{new}\""), value),
                row.reason,
                "set past",
            );
        }
        unsupported(
            editor.remove(&format!("keys.\"{new}\"")).map(drop),
            row.reason,
            "remove past",
        );
        let newer = &chords[row.limit + 1];
        unsupported(
            editor.set(&format!("keys.\"{newer}\""), s("copy")),
            row.reason,
            "new past",
        );
        assert_eq!(editor.to_toml(), text);
        refused(
            editor.set(&format!("keys.\"{last}\""), toml::Value::Integer(3)),
            "invalid before",
        );
        editor.set(&format!("keys.\"{last}\""), s("copy")).unwrap();
    }

    /// The unknown keys past the cap are not refused for (nothing is
    /// edited there), but checked: a new key in a table hidden past them,
    /// or a new unknown key, is still refused.
    fn unknown_keys_past_the_cap(row: &LoaderCap) {
        use std::fmt::Write as _;

        assert_eq!(row.limit, crate::MAX_UNKNOWN_KEY_WARNINGS);
        let mut text = String::new();
        for i in 0..row.limit + 6 {
            writeln!(text, "unknown{i} = {i}").unwrap();
        }
        text.push_str("[zzz]\na = 1\n");
        let (_dir, _path, mut editor) = editor_for(&text);
        let before = editor.warnings.clone().unwrap();
        assert!(
            before
                .iter()
                .any(|w| matches!(w, Warning::UnknownKeysOmitted { .. })),
            "{before:?}"
        );
        let result = editor.set("zzz.newkey", toml::Value::Integer(1));
        assert!(
            matches!(&result, Err(EditError::Refused { warnings, .. })
                if matches!(&warnings[..], [Warning::UnknownKey { path }] if path == "zzz")),
            "{result:?}"
        );
        refused(
            editor.set("general.newkey", toml::Value::Integer(1)),
            "new unknown key",
        );
        assert_eq!(editor.to_toml(), text);
        editor.set("zzz.a", toml::Value::Integer(2)).unwrap();
    }

    fn s(text: &str) -> toml::Value {
        toml::Value::String(text.into())
    }

    /// `n` names of temporary files of `config.toml` for `pid`.
    fn temporaries(dir: &Path, pid: u32, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let path = dir.join(format!(".config.toml.{pid}.{i}.edit"));
                std::fs::write(&path, "stale").unwrap();
                path
            })
            .collect()
    }

    fn uid(dir: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(dir).unwrap().uid()
    }

    /// #324 item 6: the sweep removes a stale temporary file of this target
    /// only: not one of a running process, of this process, of another
    /// target or another name pattern, a symlink or a directory of that
    /// name, or a file of another user (`owner` another uid). At most
    /// [`MAX_STALE_EDITS_SWEPT`] candidates are checked per sweep.
    #[test]
    fn the_sweep_removes_only_stale_temporary_files_of_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, "").unwrap();
        let dead = 4_000_001;
        let live = 4_000_002;
        let exists = |pid: u32| pid == live;
        let stale = temporaries(dir.path(), dead, 1);
        let mine = temporaries(dir.path(), std::process::id(), 1);
        let running = temporaries(dir.path(), live, 1);
        let others = [
            ".config.toml.2147483648.0.edit",
            ".config.toml.4294967295.0.edit",
            ".other.toml.4000001.0.edit",
            ".config.toml.4000001.0.edit.bak",
            ".config.toml.4000001.edit",
            ".config.toml.40x0001.0.edit",
            ".config.toml.4000001.0x.edit",
            ".config.toml.0.0.edit",
            ".config.toml.40000010000.0.edit",
            "config.toml.4000001.0.edit",
        ];
        for name in others {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let link = dir.path().join(".config.toml.4000001.1.edit");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let folder = dir.path().join(".config.toml.4000001.2.edit");
        std::fs::create_dir(&folder).unwrap();
        // Another user's: every file here is this user's, so the sweep of
        // another uid sees them as someone else's.
        assert_eq!(sweep_stale_edits(&target, uid(dir.path()) + 1, exists), 0);
        assert!(stale[0].exists());
        assert_eq!(sweep_stale_edits(&target, uid(dir.path()), exists), 1);
        assert!(!stale[0].exists());
        assert!(mine[0].exists() && running[0].exists());
        for name in others {
            assert!(dir.path().join(name).exists(), "{name}");
        }
        assert!(link.symlink_metadata().is_ok() && target.exists());
        assert!(folder.is_dir());
    }

    /// S5: one sweep checks at most [`MAX_STALE_EDITS_SWEPT`] candidates
    /// (20 stale files take two saves) and reads at most
    /// [`MAX_STALE_SWEEP_ENTRIES`] entries.
    #[test]
    fn the_sweep_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let stale = temporaries(dir.path(), 4_000_001, 20);
        let asked = Cell::new(0);
        let exists = |_| {
            asked.set(asked.get() + 1);
            false
        };
        assert_eq!(
            sweep_stale_edits(&target, uid(dir.path()), exists),
            MAX_STALE_EDITS_SWEPT
        );
        assert_eq!(asked.get(), MAX_STALE_EDITS_SWEPT);
        assert_eq!(stale.iter().filter(|p| p.exists()).count(), 4);
        assert_eq!(sweep_stale_edits(&target, uid(dir.path()), |_| false), 4);
        // Running candidates count too: 20 live ones cost 16 checks.
        temporaries(dir.path(), 4_000_002, 20);
        let asked = Cell::new(0);
        let running = |_| {
            asked.set(asked.get() + 1);
            true
        };
        assert_eq!(sweep_stale_edits(&target, uid(dir.path()), running), 0);
        assert_eq!(asked.get(), MAX_STALE_EDITS_SWEPT);
        // The entry cap: a stale name read as the last entry the cap allows
        // is removed, one after it is not.
        for (at, swept) in [
            (MAX_STALE_SWEEP_ENTRIES - 1, 1),
            (MAX_STALE_SWEEP_ENTRIES, 0),
        ] {
            let stale = temporaries(dir.path(), 4_000_001, 1);
            let pulled = Cell::new(0);
            let names = (0..)
                .map(|i| {
                    pulled.set(pulled.get() + 1);
                    if i == at {
                        Some(stale[0].file_name().unwrap().to_owned())
                    } else if i % 7 == 0 {
                        None
                    } else {
                        Some(std::ffi::OsString::from(format!("junk{i}")))
                    }
                })
                .take(MAX_STALE_SWEEP_ENTRIES + 10);
            assert_eq!(
                sweep_names(dir.path(), "config.toml", names, uid(dir.path()), |_| false),
                swept,
                "{at}"
            );
            assert!(pulled.get() <= MAX_STALE_SWEEP_ENTRIES);
            let _ = std::fs::remove_file(&stale[0]);
        }
    }
}
