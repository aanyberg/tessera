//! `tessera-devtool theme import`: turns an iTerm2 `.itermcolors` file or a
//! base16 YAML scheme into a Tessera theme file (ADR 0019).
//!
//! The format follows the extension (`.itermcolors`; `.yaml` or `.yml`);
//! the content is not sniffed. The theme is written to
//! `<out>/<slug>.toml`, where `out` defaults to the current user's themes
//! directory and the slug comes from the theme's name
//! ([`tessera_config::slug`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tessera_config::{
    ConfigPaths, ImportError, Theme, clip_theme_name, import_base16, import_itermcolors,
    read_scheme_file, slug,
};

/// What to import and where to write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOptions {
    /// The `.itermcolors`, `.yaml` or `.yml` file to read.
    pub input: PathBuf,
    /// The theme's name; by default the file stem for iTerm2 and the
    /// scheme's own name (else the stem) for base16.
    pub name: Option<String>,
    /// The directory to write into; by default
    /// [`ConfigPaths::themes_dir`] of [`ConfigPaths::for_current_user`]. It
    /// must exist.
    pub out: Option<PathBuf>,
    /// Replace an existing theme file of the same slug.
    pub force: bool,
}

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ThemeImportError {
    /// The input's extension names no supported format.
    #[error("{0}: unrecognised theme format (expected .itermcolors, .yaml or .yml)")]
    UnknownFormat(PathBuf),
    /// The input could not be read: missing, not a regular file, or
    /// larger than [`tessera_config::MAX_IMPORT_LEN`].
    #[error("{path}: {source}")]
    Read {
        /// The input file.
        path: PathBuf,
        /// Why.
        #[source]
        source: tessera_config::Error,
    },
    /// The input is not UTF-8 (base16 schemes must be).
    #[error("{0}: not UTF-8")]
    NotUtf8(PathBuf),
    /// The input is not a valid scheme.
    #[error("{path}: {source}")]
    Import {
        /// The input file.
        path: PathBuf,
        /// Why.
        #[source]
        source: ImportError,
    },
    /// The theme's name has no letters or digits to make a file name of.
    #[error("theme name {0:?} gives an empty file name; pass --name")]
    EmptyName(String),
    /// No output directory was given and the home directory is unknown.
    #[error("cannot find the themes directory (no home directory); pass --out")]
    NoThemesDir,
    /// The output directory does not exist or is not a directory.
    #[error("{0}: output directory does not exist")]
    NoOutDir(PathBuf),
    /// The theme file exists and `force` is off.
    #[error("{0} already exists; pass --force to replace it")]
    Exists(PathBuf),
    /// Writing the theme file failed.
    #[error("{path}: {source}")]
    Write {
        /// The theme file.
        path: PathBuf,
        /// Why.
        #[source]
        source: tessera_config::Error,
    },
}

/// The importer an input path's extension selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    ITermColors,
    Base16,
}

fn format_of(path: &Path) -> Option<Format> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "itermcolors" => Some(Format::ITermColors),
        "yaml" | "yml" => Some(Format::Base16),
        _ => None,
    }
}

/// Imports `options.input` and writes it as a theme file; returns the
/// path written.
///
/// Nothing is written unless the whole import succeeds. With `force` the
/// file is replaced atomically ([`tessera_config::write_atomic`]).
/// Without it the file is created atomically and only if nothing is there:
/// the content goes to a temporary file that is then hard-linked to the
/// target, which fails rather than replaces, so two imports racing for one
/// name never overwrite each other. (A check for the file followed by a
/// rename would not give that guarantee; the Appearance pane must not
/// import that way either.) On a filesystem without hard links the file is
/// created exclusively in place instead (see `write_new`).
pub fn import(options: &ImportOptions) -> Result<PathBuf, ThemeImportError> {
    let input = &options.input;
    let format = format_of(input).ok_or_else(|| ThemeImportError::UnknownFormat(input.clone()))?;
    let bytes = read_scheme_file(input).map_err(|source| ThemeImportError::Read {
        path: input.clone(),
        source,
    })?;
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let imported = match format {
        Format::ITermColors => import_itermcolors(&bytes, &stem),
        Format::Base16 => {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| ThemeImportError::NotUtf8(input.clone()))?;
            import_base16(text, &stem)
        }
    };
    let mut theme = imported.map_err(|source| ThemeImportError::Import {
        path: input.clone(),
        source,
    })?;
    if let Some(name) = &options.name {
        clip_theme_name(name).clone_into(&mut theme.name);
    }
    let slug = slug(&theme.name).ok_or_else(|| ThemeImportError::EmptyName(theme.name.clone()))?;
    let out = match &options.out {
        Some(out) => out.clone(),
        None => ConfigPaths::for_current_user()
            .ok_or(ThemeImportError::NoThemesDir)?
            .themes_dir(),
    };
    if !out.is_dir() {
        return Err(ThemeImportError::NoOutDir(out));
    }
    let path = out.join(format!("{slug}.toml"));
    let file_name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let contents = theme_file(&theme, &file_name);
    let written = if options.force {
        tessera_config::write_atomic(&path, &contents)
    } else {
        match write_new(&path, &contents, hard_link) {
            Ok(NewFile::Exists) => return Err(ThemeImportError::Exists(path)),
            Ok(NewFile::Created) => Ok(()),
            Err(e) => Err(tessera_config::Error::from(e)),
        }
    };
    written.map_err(|source| ThemeImportError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// What [`write_new`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewFile {
    /// `path` was created with the contents.
    Created,
    /// Something (even a dangling symlink) was already at `path`; nothing
    /// was written there.
    Exists,
}

/// Numbers [`write_new`]'s temporary files within this process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many temporary names [`write_new`] tries before giving up; a name
/// can be taken by a temporary file a crashed import left behind (with a
/// process id since reused).
const TEMP_ATTEMPTS: usize = 16;

/// Creates `path` holding `contents`, only if nothing is there.
///
/// The content is written and flushed to a sibling temporary file of this
/// call's own, which `link` (`std::fs::hard_link`) then links to `path`:
/// the link fails with `AlreadyExists` rather than replacing, so the
/// creation is both exclusive and atomic. On a filesystem without hard
/// links (see [`NO_HARD_LINK_ERRNOS`]; verified against a FAT32 disk
/// image, whose `FSKit` driver on macOS returns `ENOTSUP`), `path` is
/// instead created with `create_new` and written in place, which is still
/// exclusive but not atomic: a crash mid-write can leave a partial theme
/// file, which the lookup then reports as invalid. Only temporary files
/// this call created are removed.
fn write_new(
    path: &Path,
    contents: &str,
    link: fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<NewFile> {
    let name = path
        .file_name()
        .map_or_else(|| "theme".into(), |n| n.to_string_lossy().into_owned());
    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let tmp = path.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::File::create_new(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let result = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| link(&tmp, path));
    drop(file);
    // Ours to remove whatever happened; a failure only leaves a hidden
    // temporary file behind, which the lookup skips.
    let _ = std::fs::remove_file(&tmp);
    match result {
        Ok(()) => Ok(NewFile::Created),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(NewFile::Exists),
        Err(e) if links_unsupported(&e) => create_in_place(path, contents),
        Err(e) => Err(e),
    }
}

/// [`std::fs::hard_link`] as a plain function pointer for [`write_new`].
fn hard_link(original: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::hard_link(original, link)
}

/// The `link(2)` errors that mean "this filesystem has no hard links" (as
/// opposed to a real failure such as `EACCES`): `EPERM`, `EXDEV`, `ENOSYS`
/// and `ENOTSUP`/`EOPNOTSUPP`. macOS keeps `ENOTSUP` (45, what FAT and
/// exFAT volumes return under `FSKit`) apart from `EOPNOTSUPP` (102), and the
/// standard library maps only the latter to `ErrorKind::Unsupported`, so
/// the numbers are listed rather than matched by kind.
#[cfg(target_os = "macos")]
const NO_HARD_LINK_ERRNOS: [i32; 5] = [1, 18, 78, 45, 102];

/// See the macOS table; on Linux `ENOTSUP` and `EOPNOTSUPP` are both 95.
#[cfg(target_os = "linux")]
const NO_HARD_LINK_ERRNOS: [i32; 4] = [1, 18, 38, 95];

/// Elsewhere only `EPERM` and `EXDEV`, which have these values on every
/// Unix; anything the standard library reports as unsupported also counts.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const NO_HARD_LINK_ERRNOS: [i32; 2] = [1, 18];

/// Whether a hard-link failure means the filesystem has no hard links.
fn links_unsupported(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
        || error
            .raw_os_error()
            .is_some_and(|errno| NO_HARD_LINK_ERRNOS.contains(&errno))
}

/// The fallback of [`write_new`]: exclusive `create_new`, then write and
/// flush in place. A failed write removes the file this call created.
fn create_in_place(path: &Path, contents: &str) -> std::io::Result<NewFile> {
    let mut file = match std::fs::File::create_new(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(NewFile::Exists),
        Err(e) => return Err(e),
    };
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        drop(file);
        // Created by this call just above, so ours to remove.
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(NewFile::Created)
}

/// The theme file for `theme`, with a header naming the file it came from.
fn theme_file(theme: &Theme, source: &str) -> String {
    // One line, so a crafted file name cannot start a TOML line of its own.
    let source: String = source.chars().filter(|c| !c.is_control()).collect();
    format!(
        "# Imported by tessera-devtool from {source}.\n{}",
        theme.to_toml()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_extension() {
        assert_eq!(
            format_of(Path::new("a.itermcolors")),
            Some(Format::ITermColors)
        );
        assert_eq!(format_of(Path::new("a.YML")), Some(Format::Base16));
        assert_eq!(format_of(Path::new("a.yaml")), Some(Format::Base16));
        assert_eq!(format_of(Path::new("a.txt")), None);
        assert_eq!(format_of(Path::new("yaml")), None);
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Temporary files a crashed import left under this process's id are
    /// neither mistaken for the target nor removed.
    #[test]
    fn leftover_temporary_files_are_skipped_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.toml");
        let pid = std::process::id();
        let next = TEMP_SEQUENCE.load(Ordering::Relaxed);
        let stale: Vec<PathBuf> = (next..next + 4)
            .map(|i| dir.path().join(format!(".x.toml.{pid}.{i}.tmp")))
            .collect();
        for file in &stale {
            std::fs::write(file, "stale").unwrap();
        }
        assert_eq!(
            write_new(&path, "new", hard_link).unwrap(),
            NewFile::Created
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        for file in &stale {
            assert_eq!(
                std::fs::read_to_string(file).unwrap(),
                "stale",
                "{}",
                file.display()
            );
        }
        assert_eq!(
            entries(dir.path()).len(),
            stale.len() + 1,
            "{:?}",
            entries(dir.path())
        );
        assert_eq!(
            write_new(&path, "again", hard_link).unwrap(),
            NewFile::Exists
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        // When every name it tries is taken, the call fails; it does not
        // claim the (absent) target exists.
        let other = dir.path().join("y.toml");
        let next = TEMP_SEQUENCE.load(Ordering::Relaxed);
        let width = u64::try_from(TEMP_ATTEMPTS).unwrap() + 4;
        for i in next..next + width {
            std::fs::write(dir.path().join(format!(".y.toml.{pid}.{i}.tmp")), "stale").unwrap();
        }
        let error = write_new(&other, "y", hard_link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!other.exists());
    }

    /// Without hard links the file is still created exclusively.
    #[test]
    fn without_hard_links_the_file_is_created_in_place() {
        fn unsupported(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn eperm(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(1))
        }
        /// What a FAT32 volume returns on macOS (`ENOTSUP`), which the
        /// standard library does not call `Unsupported`.
        fn enotsup(_: &Path, _: &Path) -> std::io::Result<()> {
            let errno = if cfg!(target_os = "macos") { 45 } else { 95 };
            Err(std::io::Error::from_raw_os_error(errno))
        }
        fn denied(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(13)) // EACCES: a real error
        }
        let dir = tempfile::tempdir().unwrap();
        for (i, link) in [unsupported as fn(&Path, &Path) -> _, eperm, enotsup]
            .into_iter()
            .enumerate()
        {
            let path = dir.path().join(format!("{i}.toml"));
            assert_eq!(
                write_new(&path, "fallback", link).unwrap(),
                NewFile::Created
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "fallback");
            assert_eq!(write_new(&path, "other", link).unwrap(), NewFile::Exists);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "fallback");
        }
        let path = dir.path().join("denied.toml");
        assert!(write_new(&path, "x", denied).is_err());
        assert!(!path.exists());
        assert_eq!(
            entries(dir.path()),
            ["0.toml", "1.toml", "2.toml"],
            "no temporary file left"
        );
    }

    #[test]
    fn the_header_is_one_line() {
        let text = theme_file(&Theme::default(), "evil\nname = \"x\".itermcolors");
        assert_eq!(Theme::parse(&text).unwrap(), Theme::default());
    }
}
