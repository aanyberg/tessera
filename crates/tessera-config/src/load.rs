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
/// Wraps the message of [`toml::de::Error`] truncated to
/// [`MAX_PARSE_ERROR_LEN`] bytes, so logging or displaying it is always
/// cheap. The byte span of the problem in the source, when known, is kept
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
            message: truncated(&error.to_string(), MAX_PARSE_ERROR_LEN),
            span: error.span(),
        }
    }
}

/// `text` cut to at most `max` bytes on a character boundary, with a marker
/// when anything was dropped.
pub(crate) fn truncated(text: &str, max: usize) -> String {
    const MARKER: &str = "… (truncated)";
    if text.len() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(MARKER.len());
    let mut cut = keep;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + MARKER.len());
    out.push_str(&text[..cut]);
    out.push_str(MARKER);
    out
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
        }
    }
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
        let de = toml::de::Deserializer::parse(text)?;
        let mut warnings = Vec::new();
        let mut omitted = 0usize;
        let mut config: Config = serde_ignored::deserialize(de, |path| {
            if warnings.len() < MAX_UNKNOWN_KEY_WARNINGS {
                warnings.push(Warning::UnknownKey {
                    path: truncated(&dotted_path(&path), MAX_WARNING_PATH_LEN),
                });
            } else {
                omitted += 1;
            }
        })?;
        if omitted > 0 {
            warnings.push(Warning::UnknownKeysOmitted { count: omitted });
        }
        warnings.extend(invalid_cursor_shapes(text));
        warnings.extend(config.normalize());
        for w in &warnings {
            tracing::warn!(target: "tessera_config", "{w}");
        }
        Ok(Loaded { config, warnings })
    }

    /// Clamps or resets values that are outside their valid range, returning a
    /// warning for each change.
    pub fn normalize(&mut self) -> Vec<Warning> {
        let mut warnings = Vec::new();
        normalize_appearance("appearance", &mut self.appearance, &mut warnings);
        for (i, profile) in self.profiles.iter_mut().enumerate() {
            if let Some(appearance) = profile.appearance.as_mut() {
                normalize_appearance(
                    &format!("profiles[{i}].appearance"),
                    appearance,
                    &mut warnings,
                );
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

/// Deserializes `appearance.cursor_shape` without failing: a value that is
/// not exactly one of [`CursorShape::as_str`](crate::CursorShape::as_str)
/// (another spelling, another case, another type) gives the default shape,
/// and [`invalid_cursor_shapes`] reports it. So a mistyped shape costs a
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

/// One [`Warning::OutOfRange`] for every `cursor_shape` in `text` that
/// [`lenient_cursor_shape`] replaced by the default: under `[appearance]`
/// and under each profile's `appearance`. Reads the document a second
/// time, only when it mentions the key at all.
fn invalid_cursor_shapes(text: &str) -> Vec<Warning> {
    const KEY: &str = "cursor_shape";
    if !text.contains(KEY) {
        return Vec::new();
    }
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let profiles = table
        .get("profiles")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let appearances = std::iter::once(("appearance".to_owned(), table.get("appearance"))).chain(
        profiles.iter().enumerate().map(|(i, profile)| {
            (
                format!("profiles[{i}].appearance"),
                profile.get("appearance"),
            )
        }),
    );
    appearances
        .filter_map(|(prefix, appearance)| {
            let value = appearance?.get(KEY)?;
            if value
                .as_str()
                .and_then(crate::CursorShape::from_name)
                .is_some()
            {
                return None;
            }
            let names: Vec<String> = crate::CursorShape::ALL
                .iter()
                .map(|shape| format!("\"{}\"", shape.as_str()))
                .collect();
            Some(Warning::OutOfRange {
                path: format!("{prefix}.{KEY}"),
                detail: format!(
                    "{} replaced by \"{}\" (expected {})",
                    truncated(&value.to_string(), MAX_WARNING_PATH_LEN),
                    crate::CursorShape::default().as_str(),
                    names.join(", ")
                ),
            })
        })
        .collect()
}

fn normalize_appearance(prefix: &str, a: &mut crate::Appearance, warnings: &mut Vec<Warning>) {
    use crate::Appearance;

    let defaults = Appearance::default();
    if !a.font_size.is_finite() {
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.font_size"),
            detail: format!("{} replaced by {}", a.font_size, defaults.font_size),
        });
        a.font_size = defaults.font_size;
    } else if !(Appearance::MIN_FONT_SIZE..=Appearance::MAX_FONT_SIZE).contains(&a.font_size) {
        let clamped = a
            .font_size
            .clamp(Appearance::MIN_FONT_SIZE, Appearance::MAX_FONT_SIZE);
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.font_size"),
            detail: format!("{} clamped to {clamped}", a.font_size),
        });
        a.font_size = clamped;
    }
    if !(0.0..=1.0).contains(&a.opacity) {
        let clamped = if a.opacity.is_nan() {
            defaults.opacity
        } else {
            a.opacity.clamp(0.0, 1.0)
        };
        warnings.push(Warning::OutOfRange {
            path: format!("{prefix}.opacity"),
            detail: format!("{} clamped to {clamped}", a.opacity),
        });
        a.opacity = clamped;
    }
    // `-0.0` is in range but would round-trip as `-0.0`; store plain zero.
    if a.opacity == 0.0 {
        a.opacity = 0.0;
    }
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
    let bytes = read_capped(path, MAX_CONFIG_FILE_LEN, "config file")?;
    let text = std::str::from_utf8(&bytes)?;
    Config::from_toml_lenient(text)
}

/// Reads the regular file at `path` whole, refusing one larger than `limit`
/// bytes with an `InvalidInput` error that calls it a `what`. Never blocks
/// on a FIFO and never reads more than `limit + 1` bytes; see
/// [`load_file`] for the checks.
pub(crate) fn read_capped(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;

    check_regular_file(path, &std::fs::metadata(path)?, limit, what)?;
    let file = open_regular_file(path)?;
    let len = check_regular_file(path, &file.metadata()?, limit, what)?;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    (&file).take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(too_large(path, None, limit, what).into());
    }
    Ok(bytes)
}

/// Opens `path` for reading without blocking on a FIFO. On Unix the open
/// itself is non-blocking (`O_NONBLOCK`), which a regular file ignores and
/// which lets a FIFO open immediately so [`check_regular_file`] can reject
/// it from its handle.
fn open_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
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
        let appearance = loaded.config.profiles[0].appearance.as_ref().unwrap();
        assert_eq!(appearance.cursor_shape, CursorShape::Block);
        assert!(matches!(
            &loaded.warnings[..],
            [Warning::OutOfRange { path, .. }] if path == "profiles[0].appearance.cursor_shape"
        ));
        // A key that only mentions the name is not checked.
        let loaded = Config::from_toml_lenient("[appearance]\ntheme = \"cursor_shape\"\n").unwrap();
        assert!(matches!(&loaded.warnings[..], []), "{:?}", loaded.warnings);
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
        let file = open_regular_file(&path).expect("non-blocking open of a FIFO");
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
