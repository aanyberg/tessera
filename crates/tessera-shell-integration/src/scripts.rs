//! The shell scripts, embedded in the crate and written to disk on demand.
//!
//! The scripts in the repository's `shell-integration/` directory are
//! compiled in ([`SCRIPTS`]) and [`materialise`] writes them under a
//! directory per [`SCRIPTS_VERSION`] (Tessera uses
//! `~/Library/Application Support/Tessera/shell-integration/`), so the app
//! and the development window share one copy without bundle resources.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Version of the embedded scripts, the name of the directory
/// [`materialise`] writes them to.
///
/// Bump it whenever a script changes, so a running Tessera never sees a
/// half-updated set and a new one writes its scripts next to the old ones; a
/// test compares [`digest`] with a recorded value and fails until it is
/// bumped.
pub const SCRIPTS_VERSION: u32 = 3;

/// The version the scripts announce in their handshake
/// (`OSC 1337 ; ShellIntegrationVersion=<n> ; shell=<name> ST`). It changes
/// only when what the scripts send changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// One embedded script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Script {
    /// Path under the version directory, `/`-separated.
    pub path: &'static str,
    /// The file's contents.
    pub contents: &'static str,
}

/// Every embedded script.
pub const SCRIPTS: &[Script] = &[
    Script {
        path: "zsh/.zshenv",
        contents: include_str!("../../../shell-integration/zsh/.zshenv"),
    },
    Script {
        path: "zsh/tessera-integration",
        contents: include_str!("../../../shell-integration/zsh/tessera-integration"),
    },
    Script {
        path: "bash/tessera.bash",
        contents: include_str!("../../../shell-integration/bash/tessera.bash"),
    },
    Script {
        path: "fish/vendor_conf.d/tessera-shell-integration.fish",
        contents: include_str!(
            "../../../shell-integration/fish/vendor_conf.d/tessera-shell-integration.fish"
        ),
    },
];

/// A fingerprint of [`SCRIPTS`] (64-bit FNV-1a over every path and its
/// contents), for the test that keeps [`SCRIPTS_VERSION`] honest.
#[must_use]
pub fn digest() -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for script in SCRIPTS {
        for byte in script
            .path
            .bytes()
            .chain([0])
            .chain(script.contents.bytes())
            .chain([0])
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

/// The directory [`materialise`] writes this version's scripts to:
/// `<dir>/<SCRIPTS_VERSION>`.
#[must_use]
pub fn version_dir(dir: &Path) -> PathBuf {
    dir.join(SCRIPTS_VERSION.to_string())
}

/// Whether every embedded script is a file under `version_dir` (one
/// `stat` per script).
#[must_use]
pub fn is_complete(version_dir: &Path) -> bool {
    SCRIPTS.iter().all(|script| {
        script
            .path
            .split('/')
            .fold(version_dir.to_path_buf(), |path, part| path.join(part))
            .is_file()
    })
}

/// Temporary directories older than this are left over from a crash and
/// removed by the next [`materialise`] that writes.
const STALE_TEMP_AGE: Duration = Duration::from_hours(1);

/// Prefix of the temporary directories [`materialise`] writes into.
const TEMP_PREFIX: &str = ".tmp-";

/// Writes the embedded scripts to [`version_dir`]`(dir)` unless that
/// directory exists with every script in it ([`is_complete`]), and returns
/// it. An existing directory that lacks a script (deleted by hand, or left
/// by an older tool) is replaced: renamed aside and removed after the new
/// copy is in place.
///
/// The scripts are written to a temporary directory next to it, which is
/// then renamed into place, so another process (the app and the development
/// window may start shells at the same time) sees either no directory or a
/// complete one; when two race, the first rename wins and both return it.
/// `dir` and the directories under it are created with mode `0o700`, the
/// files with `0o644`. After writing, directories of older versions (named
/// by a smaller number) and temporary directories left by a crash more than
/// an hour ago are removed; failing to remove them is not an error.
///
/// # Errors
///
/// Any I/O error creating `dir`, writing the files or renaming the
/// temporary directory (for example when `dir` is not writable, or a file
/// is in the way of the version directory).
pub fn materialise(dir: &Path) -> io::Result<PathBuf> {
    let target = version_dir(dir);
    let exists = fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir());
    if exists && is_complete(&target) {
        return Ok(target);
    }
    create_dir(dir, true)?;
    if exists {
        // Out of the way under a temporary name (pruned if we crash); a
        // concurrent repair may have moved it already.
        let stale = dir.join(temp_name());
        if fs::rename(&target, &stale).is_ok() {
            let _ = fs::remove_dir_all(&stale);
        }
    }
    let temp = dir.join(temp_name());
    create_dir(&temp, false)?;
    let written = write_scripts(&temp).and_then(|()| fs::rename(&temp, &target));
    if let Err(err) = written {
        let _ = fs::remove_dir_all(&temp);
        // Another process renamed its copy into place first.
        if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) && is_complete(&target) {
            return Ok(target);
        }
        return Err(err);
    }
    prune(dir);
    Ok(target)
}

/// A temporary directory name no other call (in this or another process)
/// uses.
fn temp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    format!(
        "{TEMP_PREFIX}{SCRIPTS_VERSION}-{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates `path` with mode `0o700` (whatever the umask), and its missing
/// parents too when `recursive`.
fn create_dir(path: &Path, recursive: bool) -> io::Result<()> {
    let existed = recursive && path.is_dir();
    DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(path)?;
    if !existed {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_scripts(root: &Path) -> io::Result<()> {
    for script in SCRIPTS {
        let path = script
            .path
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = path.parent()
            && !parent.is_dir()
        {
            create_dir(parent, true)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)?;
        file.write_all(script.contents.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
    }
    Ok(())
}

/// Removes older version directories and stale temporary directories.
fn prune(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Directories only, never through a symlink.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let remove = if let Ok(version) = name.parse::<u32>() {
            version < SCRIPTS_VERSION
        } else if name.starts_with(TEMP_PREFIX) {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STALE_TEMP_AGE)
        } else {
            false
        };
        if remove {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}
