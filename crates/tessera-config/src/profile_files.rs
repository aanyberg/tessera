//! Profile files (#312, ADR 0037): `profiles/<stem>.toml` next to
//! `config.toml`, each holding one profile with the keys of one
//! `[[profiles]]` table at its top level, and how they join the
//! `[[profiles]]` entries.
//!
//! - A file without `name` is named after its stem (`work.toml` is
//!   `work`).
//! - The `[[profiles]]` entries come first, in file order, then the files
//!   in byte order of their stems. Of two profiles with one name the first
//!   is used and the second gets the duplicate-name warning, across both
//!   sources; `[general] default_profile` may name a file's profile.
//! - [`MAX_PROFILES`] counts both sources together, with one
//!   [`Warning::TooManyProfiles`] for all that are left out. At most
//!   [`MAX_PROFILE_FILES`] files are read (the first by stem; one
//!   [`Warning::TooManyProfileFiles`] for the rest), each at most
//!   [`MAX_PROFILE_FILE_LEN`] bytes, from the first
//!   [`MAX_PROFILE_DIR_ENTRIES`] entries the directory lists.
//! - Hidden entries (`.work.toml`), entries not ending in `.toml`, entries
//!   whose name is not UTF-8 and entries that are not regular files
//!   (followed through symbolic links: a FIFO, a directory, a dangling
//!   link) are skipped without a warning.
//! - A file that cannot be read or does not load as a profile gets one
//!   [`Warning::ProfileFileIgnored`] and is skipped; the others load. That
//!   warning names the kind of problem (and the line and column of a TOML
//!   error), never the file's text (S8). Every other warning about a file
//!   is the one the same key in a `[[profiles]]` entry gets, wrapped in a
//!   [`Warning::ProfileFile`] that names the file; at most
//!   [`MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS`] unknown keys are listed per
//!   file.
//!
//! Nothing here writes: the configuration editor edits `config.toml` only
//! and refuses an edit of a file's profile (ADR 0040).

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use toml::Spanned;
use toml::de::{DeArray, DeTable, DeValue};

use crate::load::{Logging, MAX_WARNING_PATH_LEN, Warning, escaped, read_capped};
use crate::{Config, Error, Loaded, MAX_PROFILES, Profile, ProfileSource};

/// Most profile files read from `profiles/`: the first by stem; the others
/// are reported in one [`Warning::TooManyProfileFiles`] (docs/security.md
/// S5).
pub const MAX_PROFILE_FILES: usize = 256;

/// Largest profile file read, in bytes (64 KiB, as a theme file); a larger
/// one is skipped with a [`Warning::ProfileFileIgnored`] without being
/// read (S5).
pub const MAX_PROFILE_FILE_LEN: u64 = 64 * 1024;

/// Most directory entries one listing of `profiles/` looks at, files of
/// every kind together; a directory with more is read only among the first
/// this many the system returns (S5).
pub const MAX_PROFILE_DIR_ENTRIES: usize = 4096;

/// Most unknown keys reported one by one per profile file; the others are
/// counted in one [`Warning::UnknownKeysOmitted`] of that file. With
/// [`MAX_PROFILE_FILES`] files this bounds what the files can add to the
/// warnings (S5).
pub const MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS: usize = 8;

/// The profile files of a directory, as [`list`] finds them.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    /// The stem and path of each profile file read, by stem in byte order:
    /// regular files (through symbolic links) only, at most
    /// [`MAX_PROFILE_FILES`].
    pub(crate) files: Vec<(String, PathBuf)>,
    /// How many `*.toml` entries past [`MAX_PROFILE_FILES`] were not
    /// looked at.
    pub(crate) dropped: usize,
    /// Why the directory could not be listed (not for a missing one).
    pub(crate) error: Option<std::io::ErrorKind>,
    /// The first [`MAX_DANGLING_LISTED`] `*.toml` entries, by stem, that
    /// are symbolic links to nothing that loads (a missing target, say):
    /// not profile files, but the watcher follows the way to their
    /// targets, so a target that appears is seen.
    pub(crate) dangling: Vec<PathBuf>,
}

/// Most dangling links [`list`] reports ([`Listing::dangling`]).
pub(crate) const MAX_DANGLING_LISTED: usize = 8;

/// The profile files of `dir` (`profiles/`): see the [module
/// documentation](self) for which entries count. A missing directory has
/// none.
pub(crate) fn list(dir: &Path) -> Listing {
    list_first(dir, MAX_PROFILE_FILES)
}

/// [`list`], stopping at the first `limit` profile files (at most
/// [`MAX_PROFILE_FILES`]); [`Listing::dropped`] counts the `*.toml`
/// entries past them. The watcher lists the files it follows this way, so
/// it `stat`s no more entries than it needs.
pub(crate) fn list_first(dir: &Path, limit: usize) -> Listing {
    let limit = limit.min(MAX_PROFILE_FILES);
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Listing::default(),
        Err(error) => {
            return Listing {
                error: Some(error.kind()),
                ..Listing::default()
            };
        }
    };
    let mut candidates: Vec<(String, PathBuf)> = entries
        .take(MAX_PROFILE_DIR_ENTRIES)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".toml")?;
            (!stem.is_empty() && !stem.starts_with('.')).then(|| (stem.to_owned(), entry.path()))
        })
        .collect();
    // Stems are unique within a directory (a name is its stem and `.toml`).
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut files = Vec::new();
    let mut dangling = Vec::new();
    let mut rest = candidates.into_iter();
    for (stem, path) in rest.by_ref() {
        // Following links: a link to a regular file is a profile file, a
        // FIFO, a directory or a dangling link is not.
        if std::fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
            files.push((stem, path));
            if files.len() >= limit {
                break;
            }
        } else if dangling.len() < MAX_DANGLING_LISTED
            && std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink())
        {
            dangling.push(path);
        }
    }
    Listing {
        files,
        dropped: rest.len(),
        error: None,
        dangling,
    }
}

/// One profile file, loaded.
#[derive(Debug)]
struct FileProfile {
    /// The file's label, `profiles/<stem>.toml`, escaped for warnings.
    file: String,
    /// The profile, `None` when the file did not load.
    profile: Option<Profile>,
    /// The file's warnings (a [`Warning::ProfileFileIgnored`] when it did
    /// not load), in document order.
    warnings: Vec<Warning>,
}

/// The profile files of a directory, loaded ([`load`]), for [`merge`].
#[derive(Debug, Default)]
pub(crate) struct ProfileFiles {
    /// The directory's own warnings: it could not be listed, or holds more
    /// than [`MAX_PROFILE_FILES`] files.
    warnings: Vec<Warning>,
    /// Each file read, by stem.
    files: Vec<FileProfile>,
}

impl ProfileFiles {
    /// How many files loaded as a profile, before [`MAX_PROFILES`] cuts
    /// them in [`merge`].
    pub(crate) fn count(&self) -> usize {
        self.files.iter().filter(|f| f.profile.is_some()).count()
    }

    /// The names of the files' profiles, by stem (duplicates included).
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter_map(|f| f.profile.as_ref().map(|p| p.name.as_str()))
    }
}

/// Loads the profile files of `dir` (`profiles/`), logging nothing: the
/// caller merges their warnings with `config.toml`'s ([`merge`]) and logs
/// them as it logs those.
pub(crate) fn load(dir: &Path) -> ProfileFiles {
    let listing = list(dir);
    let mut warnings = Vec::new();
    if let Some(kind) = listing.error {
        warnings.push(Warning::ProfileFileIgnored {
            file: "profiles/".to_owned(),
            reason: format!("the directory cannot be listed ({})", io_kind(kind)),
        });
    }
    if listing.dropped > 0 {
        warnings.push(Warning::TooManyProfileFiles {
            dropped: listing.dropped,
        });
    }
    let files = listing
        .files
        .into_iter()
        .map(|(stem, path)| {
            let source = ProfileSource::File(stem);
            let file = escaped(&source.label(), MAX_WARNING_PATH_LEN);
            match load_one(source, &path) {
                Ok((profile, inner)) => FileProfile {
                    profile: Some(profile),
                    warnings: inner
                        .into_iter()
                        .map(|warning| in_file(&file, warning))
                        .collect(),
                    file,
                },
                Err(reason) => FileProfile {
                    warnings: vec![Warning::ProfileFileIgnored {
                        file: file.clone(),
                        reason,
                    }],
                    profile: None,
                    file,
                },
            }
        })
        .collect();
    ProfileFiles { warnings, files }
}

/// Appends the profiles of `files` to `loaded` (`config.toml`'s, already
/// cut to [`MAX_PROFILES`]) and their warnings to its warnings: the
/// directory's first, then each file's in stem order, each followed by the
/// file's duplicate-name warning. Profiles past [`MAX_PROFILES`] in all
/// are left out and counted in `loaded`'s one
/// [`Warning::TooManyProfiles`] (added when it had none); their own
/// warnings go with them.
pub(crate) fn merge(loaded: &mut Loaded, files: ProfileFiles) {
    let profiles = &mut loaded.config.profiles;
    let mut room = MAX_PROFILES.saturating_sub(profiles.len());
    let mut dropped = 0usize;
    // The first profile of each name, by its label, as `cap_profiles`
    // leaves the `[[profiles]]` entries.
    let mut first: HashMap<String, String> = HashMap::new();
    for (i, profile) in profiles.iter().enumerate() {
        first
            .entry(profile.name.clone())
            .or_insert_with(|| format!("profiles[{i}]"));
    }
    let mut warnings = files.warnings;
    for file in files.files {
        let Some(profile) = file.profile else {
            warnings.extend(file.warnings);
            continue;
        };
        if room == 0 {
            dropped += 1;
            continue;
        }
        room -= 1;
        warnings.extend(file.warnings);
        match first.entry(profile.name.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(file.file.clone());
            }
            Entry::Occupied(entry) => warnings.push(Warning::ProfileFile {
                file: file.file.clone(),
                warning: Box::new(Warning::OutOfRange {
                    path: "name".to_owned(),
                    detail: format!("{} has this name too and is the one used", entry.get()),
                }),
            }),
        }
        profiles.push(profile);
    }
    if dropped > 0 {
        match loaded
            .warnings
            .iter_mut()
            .find_map(|warning| match warning {
                Warning::TooManyProfiles { dropped } => Some(dropped),
                _ => None,
            }) {
            Some(count) => *count += dropped,
            None => loaded.warnings.push(Warning::TooManyProfiles { dropped }),
        }
    }
    loaded.warnings.extend(warnings);
}

/// [`load`] and [`merge`] in one: what a load of `config.toml` does with
/// the profile files of `dir`.
pub(crate) fn load_and_merge(loaded: &mut Loaded, dir: &Path) {
    merge(loaded, load(dir));
}

/// Loads the profile file at `path`, whose source is `source`: the profile
/// and its warnings (paths as the loader writes them for `profiles[0]`), or
/// why it is skipped.
fn load_one(source: ProfileSource, path: &Path) -> Result<(Profile, Vec<Warning>), String> {
    let bytes = match read_capped(path, MAX_PROFILE_FILE_LEN, "profile file") {
        Ok(bytes) => bytes,
        Err(Error::Io(error)) => return Err(read_reason(&error)),
        Err(_) => return Err("cannot be read".to_owned()),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("not UTF-8 (at byte {})", error.valid_up_to()))?;
    let mut table =
        DeTable::parse(text).map_err(|error| toml_reason("not TOML", text, error.span()))?;
    let ProfileSource::File(stem) = &source else {
        return Err("not a profile file".to_owned());
    };
    if !table
        .get_ref()
        .iter()
        .any(|(key, _)| key.get_ref().as_ref() == "name")
    {
        // Inserted with an empty span: the stem is the name, and a
        // warning about it (one cut to `MAX_PROFILE_NAME_LEN`) is the
        // file's like any other; its place among them is the table's
        // order, not promised here.
        table.get_mut().insert(
            Spanned::new(0..0, Cow::Borrowed("name")),
            Spanned::new(0..0, DeValue::String(Cow::Owned(stem.clone()))),
        );
    }
    // The file's table as the one `[[profiles]]` entry of a document, its
    // spans still pointing into `text`, so the file gets exactly the
    // checks and warnings of a `[[profiles]]` entry.
    let span = table.span();
    let mut array = DeArray::new();
    array.push(Spanned::new(
        span.clone(),
        DeValue::Table(table.into_inner()),
    ));
    let mut root = DeTable::new();
    root.insert(
        Spanned::new(0..0, Cow::Borrowed("profiles")),
        Spanned::new(span, DeValue::Array(array)),
    );
    let document = Spanned::new(0..text.len(), root);
    let loaded = Config::from_document_lenient(
        text,
        document,
        Logging::Quiet,
        MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS,
    )
    .map_err(|error| match error {
        Error::Parse(error) => toml_reason("a value has the wrong type", text, error.span()),
        _ => "not a profile".to_owned(),
    })?;
    let mut profile = loaded
        .config
        .profiles
        .into_iter()
        .next()
        .ok_or_else(|| "not a profile".to_owned())?;
    profile.source = source;
    Ok((profile, loaded.warnings))
}

/// `warning`, about the profile `profiles[0]` of a profile file's document,
/// as a [`Warning::ProfileFile`] of `file` with its path relative to the
/// file.
fn in_file(file: &str, warning: Warning) -> Warning {
    let relative = |path: String| match path.strip_prefix("profiles[0].") {
        Some(rest) => rest.to_owned(),
        None => path,
    };
    let warning = match warning {
        Warning::UnknownKey { path } => Warning::UnknownKey {
            path: relative(path),
        },
        Warning::OutOfRange { path, detail } => Warning::OutOfRange {
            path: relative(path),
            detail,
        },
        other => other,
    };
    Warning::ProfileFile {
        file: file.to_owned(),
        warning: Box::new(warning),
    }
}

/// Why a profile file could not be read, by the error's kind only: its
/// message names the path (S8, and the file is named by the warning).
fn read_reason(error: &std::io::Error) -> String {
    match error.kind() {
        // `read_capped`'s refusal: too large, or not a regular file any
        // more (replaced since the listing).
        std::io::ErrorKind::InvalidInput => {
            format!("larger than {MAX_PROFILE_FILE_LEN} bytes, or not a regular file")
        }
        kind => format!("cannot be read ({})", io_kind(kind)),
    }
}

/// `kind` in words, for a reason.
fn io_kind(kind: std::io::ErrorKind) -> String {
    match kind {
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        std::io::ErrorKind::NotFound => "removed while it was read".to_owned(),
        kind => kind.to_string(),
    }
}

/// `what` and where `span` starts in `text` (line and column, from 1), or
/// `what` alone without a span. Never the parser's message, which quotes
/// the file (S8).
fn toml_reason(what: &str, text: &str, span: Option<std::ops::Range<usize>>) -> String {
    let Some(span) = span else {
        return what.to_owned();
    };
    let at = span.start.min(text.len());
    let mut start = at;
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let before = &text[..start];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before, |newline| &before[newline + 1..])
        .chars()
        .count()
        + 1;
    format!("{what} (line {line}, column {column})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn merged(config: &str, dir: &Path) -> Loaded {
        let mut loaded = Config::from_toml_lenient_logging(config, Logging::Quiet).unwrap();
        load_and_merge(&mut loaded, dir);
        loaded
    }

    /// S5: a flood of dangling links lists the first
    /// [`MAX_DANGLING_LISTED`] by stem and reads none of them; a regular
    /// file among them is still listed.
    #[test]
    fn dangling_links_are_listed_up_to_their_cap() {
        let dir = dir_with(&[("m.toml", "")]);
        for i in 0..40 {
            std::os::unix::fs::symlink(
                dir.path().join(format!("gone{i}")),
                dir.path().join(format!("d{i:02}.toml")),
            )
            .unwrap();
        }
        let listing = list(dir.path());
        assert_eq!(listing.files, [("m".to_owned(), dir.path().join("m.toml"))]);
        let want: Vec<PathBuf> = (0..MAX_DANGLING_LISTED)
            .map(|i| dir.path().join(format!("d{i:02}.toml")))
            .collect();
        assert_eq!(listing.dangling, want);
        assert_eq!(listing.dropped, 0);
    }

    #[test]
    fn toml_reason_counts_lines_and_columns_from_one() {
        assert_eq!(
            toml_reason("x", "ab\ncd", Some(4..5)),
            "x (line 2, column 2)"
        );
        assert_eq!(toml_reason("x", "", Some(0..0)), "x (line 1, column 1)");
        assert_eq!(toml_reason("x", "é=", Some(1..2)), "x (line 1, column 1)");
        assert_eq!(toml_reason("x", "a", None), "x");
    }

    /// A warning about a key of a file names the key as a `[[profiles]]`
    /// entry's would be named, relative to the file.
    #[test]
    fn a_file_warning_names_the_key_relative_to_the_file() {
        let dir = dir_with(&[(
            "work.toml",
            "colour = 1\n[appearance]\nfont_size = 900.0\nblur_radius = 3\n",
        )]);
        let loaded = merged("", dir.path());
        let shown: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
        assert_eq!(shown.len(), 3, "{shown:?}");
        assert!(
            shown[0].starts_with("profiles/work.toml: unknown key `colour`"),
            "{shown:?}"
        );
        assert!(
            shown[1].starts_with("profiles/work.toml: `appearance.font_size`"),
            "{shown:?}"
        );
        assert!(
            shown[2].starts_with("profiles/work.toml: `appearance.blur_radius`"),
            "{shown:?}"
        );
        assert_eq!(loaded.config.profiles[0].name, "work");
    }

    /// S5: the unknown keys of one file are listed up to their cap.
    #[test]
    fn the_unknown_keys_of_a_file_are_capped() {
        let keys = (0..50).fold(String::new(), |mut keys, i| {
            writeln!(keys, "k{i} = 1").unwrap();
            keys
        });
        let dir = dir_with(&[("a.toml", &keys)]);
        let loaded = merged("", dir.path());
        assert_eq!(
            loaded.warnings.len(),
            MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS + 1
        );
        assert!(matches!(
            loaded.warnings.last(),
            Some(Warning::ProfileFile { warning, .. })
                if **warning == Warning::UnknownKeysOmitted { count: 50 - MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS }
        ));
    }
}
