//! Writing settings back to `config.toml` without losing what the user
//! wrote by hand (ADR 0040).
//!
//! [`ConfigEditor`] opens the file, changes one key at a time
//! ([`ConfigEditor::set`], [`ConfigEditor::remove`],
//! [`ConfigEditor::add_profile`]) and [`ConfigEditor::save`]s it. Every
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

use crate::load::{MAX_CONFIG_FILE_LEN, follow_links, read_capped};
use crate::theme::{MAX_THEME_DIR_ENTRIES, MAX_THEME_FILES, ThemeSource, clip_theme_name, slug};
use crate::{Config, Error, ParseError, Theme, Warning};

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
/// each bare (`A-Za-z0-9_-`) or in double quotes (with `\"` and `\\` the
/// only escapes), each optionally followed by `[N]` indices:
/// `appearance.opacity`, `profiles[1].appearance.theme`, `keys."cmd+d"`.
///
/// At most [`MAX_KEY_PATH_LEN`] bytes, [`MAX_KEY_PATH_STEPS`] steps and
/// indices up to [`MAX_KEY_PATH_INDEX`]; a quoted key may not hold control
/// characters. Displays in the same syntax, so a path can be logged (S8: a
/// key path, never a value).
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
                                // still displayed on one line (S8).
                                c if c.is_control() => write!(f, "\\u{{{:x}}}", u32::from(c))?,
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
    /// for its key): the edit is refused.
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

    /// `text`, as the editor renders it, with these endings.
    fn apply(self, text: &str) -> String {
        let text = if self.final_newline {
            text
        } else {
            text.strip_suffix('\n').unwrap_or(text)
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
    /// is not UTF-8 or not TOML. [`EditError::Unsupported`] when the TOML
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
            Parsed::Editor => EditError::Unsupported {
                path: KeyPath(vec![Step::Key(String::new())]),
                reason: "the TOML editor cannot read this file",
            },
        })?;
        if endings.apply(&state.doc.to_string()) != body {
            return Err(EditError::Unsupported {
                path: KeyPath(vec![Step::Key(String::new())]),
                reason: "the TOML editor would not write this file back unchanged",
            });
        }
        // A CRLF inside a multi-line string is part of its value.
        if endings.crlf && !same_as_saved(&state.plain, body) {
            return Err(EditError::Unsupported {
                path: KeyPath(vec![Step::Key(String::new())]),
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
    pub fn set(&mut self, path: &str, value: toml::Value) -> Result<(), EditError> {
        let path: KeyPath = path.parse()?;
        let unsupported = |reason| EditError::Unsupported {
            path: path.clone(),
            reason,
        };
        let edit_value = edit_value(&value, 0).map_err(unsupported)?;
        let mut doc = self.doc.clone();
        let mut removed = Vec::new();
        set_table(doc.as_table_mut(), false, &path.0, edit_value, &mut removed)
            .map_err(unsupported)?;
        let mut expected = toml::Value::Table(self.plain.clone());
        plain_edit(&mut expected, &path.0, Some(value));
        let before = self.warnings.clone().unwrap_or_default();
        self.commit(&path, &doc.to_string(), &expected, &removed, &before)
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
        let unsupported = |reason| EditError::Unsupported {
            path: path.clone(),
            reason,
        };
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
        let before = renumbered(self.warnings.as_deref().unwrap_or_default(), &path);
        self.commit(&path, &text, &expected, &removed, &before)?;
        Ok(true)
    }

    /// Appends a profile named `name` (a new `[[profiles]]` entry after
    /// the last one) and returns its index.
    ///
    /// # Errors
    ///
    /// As for [`ConfigEditor::set`].
    pub fn add_profile(&mut self, name: &str) -> Result<usize, EditError> {
        let index = self.profile_count();
        self.set(
            &format!("profiles[{index}].name"),
            toml::Value::String(name.to_owned()),
        )?;
        Ok(index)
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
        // Warnings are few (the loader bounds them); a list will do.
        let mut before: Vec<&Warning> = before.iter().collect();
        let mut new = Vec::new();
        for warning in after {
            match before.iter().position(|w| *w == warning) {
                Some(at) => {
                    before.swap_remove(at);
                }
                None => new.push(warning.clone()),
            }
        }
        // A theme the edit names must exist (`ConfigStore::theme` would
        // fall back to the default with a warning).
        if let Some(dir) = self.path.parent() {
            let themes_dir = dir.join("themes");
            for name in &state.themes {
                if self.themes.contains(name) {
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
        self.doc = state.doc;
        self.plain = state.plain;
        self.comments = state.comments;
        self.warnings = state.warnings;
        self.themes = state.themes;
        self.config = state.config;
        Ok(())
    }

    /// Writes the document back when it changed; `Ok(false)` when there
    /// was nothing to write.
    ///
    /// The document goes to a new temporary file in the directory of the
    /// file (the symlink's target when `config.toml` is a symlink), with
    /// the file's permission bits, which is flushed to disk and then
    /// renamed over the file; the directory is flushed after. A crash
    /// leaves the old file or the new one, never a mix (a temporary file
    /// may stay behind). Just before the rename the file is read again: if
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

/// Numbers the temporary files within this process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A new temporary file next to `target`, named
/// `.<name>.<pid>.<n>.edit`.
fn temporary_file(target: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    /// Names tried before giving up: a stale one (left by a crash of an
    /// earlier process with the same id) is skipped.
    const ATTEMPTS: u32 = 16;

    let name = target
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned());
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
        let loaded =
            tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                Config::from_toml_lenient(text)
            });
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

/// How many times each string occurs.
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
    use std::fmt::Write as _;

    let Some((Step::Index(removed), array)) = path.0.split_last() else {
        return warnings.to_vec();
    };
    // The array's path as the loader writes it (`profiles`).
    let mut prefix = String::new();
    for step in array {
        match step {
            Step::Key(key) => {
                if !prefix.is_empty() {
                    prefix.push('.');
                }
                prefix.push_str(key);
            }
            Step::Index(index) => {
                let _ = write!(prefix, "[{index}]");
            }
        }
    }
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
            | Warning::TooManyKeyBindings { .. } => Some(warning.clone()),
        })
        .collect()
}

fn multiset(items: &[String]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for item in items {
        *counts.entry(item.as_str()).or_insert(0) += 1;
    }
    counts
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

/// Sets `steps` (starting at a key of `table`) to `value`; the comments of
/// a replaced value are pushed onto `removed`.
fn set_table(
    table: &mut dyn TableLike,
    inline: bool,
    steps: &[Step],
    value: Value,
    removed: &mut Vec<String>,
) -> Result<(), &'static str> {
    let Some((Step::Key(key), rest)) = steps.split_first() else {
        return Err("an index into a table");
    };
    if rest.is_empty() {
        return match table.get_mut(key) {
            Some(Item::Value(old)) if !matches!(old, Value::InlineTable(_)) => {
                value_comments(old, removed);
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
                Ok(())
            }
            Some(Item::None) | None => {
                table.insert(key, Item::Value(value));
                Ok(())
            }
            Some(_) => Err("a value cannot replace a table"),
        };
    }
    if table.get(key).is_none_or(Item::is_none) {
        let dotted = table.is_dotted();
        table.insert(key, container(inline, dotted, rest));
    }
    match table.get_mut(key) {
        Some(Item::Table(child)) => set_table(child, false, rest, value, removed),
        Some(Item::Value(Value::InlineTable(child))) => {
            set_table(child, true, rest, value, removed)
        }
        Some(Item::Value(Value::Array(child))) => set_array(child, rest, value, removed),
        Some(Item::ArrayOfTables(child)) => set_tables(child, rest, value, removed),
        _ => Err("the path runs through a value"),
    }
}

/// [`set_table`] for a step into an array of values.
fn set_array(
    array: &mut Array,
    steps: &[Step],
    value: Value,
    removed: &mut Vec<String>,
) -> Result<(), &'static str> {
    let Some((&Step::Index(index), rest)) = steps.split_first() else {
        return Err("a key into an array");
    };
    if rest.is_empty() {
        return match array.get(index) {
            Some(Value::InlineTable(_)) => Err("a value cannot replace a table"),
            Some(old) => {
                value_comments(old, removed);
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
        Some(Value::InlineTable(child)) => set_table(child, true, rest, value, removed),
        Some(Value::Array(child)) => set_array(child, rest, value, removed),
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
    removed: &mut Vec<String>,
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
        Some(child) => set_table(child, false, rest, value, removed),
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
    let text = doc.to_string();
    let mut n = 0u64;
    let mark = loop {
        let mark = format!("#tessera-edit-{n:x}\n");
        if !text.contains(&mark) {
            break mark;
        }
        n += 1;
    };
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
    let mut stems: Vec<(String, PathBuf)> = std::fs::read_dir(themes_dir)
        .map(|entries| {
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
        })
        .unwrap_or_default();
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
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
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
        .map(|n| display_name(&n.to_string_lossy()))
        .unwrap_or_default();
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
}
