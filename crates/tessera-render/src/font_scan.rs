//! The installed font database: system fonts mapped, user fonts read into
//! memory (#683, ADR 0007).
//!
//! fontdb's own scan (`Database::load_system_fonts`) opens every font file
//! with a plain blocking `open(2)` and memory maps it, and cosmic-text maps
//! each face in use for shaping and rasterisation. A mapped file that
//! another process truncates in place raises `SIGBUS` on the next touch of
//! a page past its new end, which kills the app, and a FIFO put at a font
//! path blocks the open forever. The system font directories
//! ([`SYSTEM_FONT_DIRS`]) are protected by SIP and only ever replaced
//! whole, so fontdb still scans and maps them. Every other font directory
//! (`/Library/Fonts`, `/Network/Library/Fonts`, `~/Library/Fonts`) is
//! walked here instead:
//!
//! - each file is opened non-blocking and checked by `fstat` of the
//!   descriptor to be a regular file ([`open_regular_file`]), so a FIFO,
//!   socket or device never blocks the scan;
//! - the scan reads only what fontdb's face parse looks at (the collection
//!   header, the table records ttf-parser's lookup visits, searched step
//!   for step as it searches so an unsorted directory gives the same
//!   tables, and the `OS/2`, `name` and `post` tables, each capped), never
//!   the whole file and never through a map;
//! - a file of at most [`MAX_OWNED_FONT_BYTES`] becomes an
//!   [`OwnedFontFile`]: its faces' data is read into memory, whole, the
//!   first time a face of it is used ([`OwnedFaces::admit`]), and never
//!   mapped, while the process's owned bytes stay within
//!   [`MAX_OWNED_TOTAL_BYTES`]. A larger file, or one whose read would
//!   pass that total, is a `Source::File`, which cosmic-text maps when a
//!   face of it is used: the documented residual of ADR 0007;
//! - a symlink into a system directory is left to the system pass
//!   ([`in_system_pass`]), so its faces load once, mapped.

// The walk is macOS's directory list; elsewhere only the tests use it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use cosmic_text::{FontSystem, fontdb};

use crate::font::{is_sfnt, open_regular_file, read_exact_at, read_u32s};

/// Largest user font file whose data is read into memory (8 MiB, the
/// #683 decision: almost every Latin, Nerd Font and icon font). A larger
/// one is mapped by cosmic-text when used.
pub(crate) const MAX_OWNED_FONT_BYTES: u64 = 8 << 20;

/// Most user font bytes the process holds in memory at once (32 MiB, the
/// #683 decision), counted when a file is read. A file whose read would
/// pass it is mapped instead, as a larger file is.
pub(crate) const MAX_OWNED_TOTAL_BYTES: u64 = 32 << 20;

/// The owned bytes of every font database in the process.
static PROCESS_BUDGET: OwnedBudget = OwnedBudget::new(MAX_OWNED_TOTAL_BYTES);

/// Directories fontdb scans and maps: SIP-protected, replaced only whole
/// by the system, so never truncated in place. Downloadable system fonts
/// live in the `com_apple_MobileAsset_Font*` directories of
/// [`SYSTEM_ASSET_DIR`].
const SYSTEM_FONT_DIRS: &[&str] = &["/System/Library/Fonts"];

/// Where the downloadable system fonts live (fontdb's macOS list).
const SYSTEM_ASSET_DIR: &str = "/System/Library/AssetsV2";

/// Directories below a user font directory the scan descends at most
/// (fontdb sets no limit; a symlink loop is stopped by the seen set).
const MAX_SCAN_DEPTH: usize = 16;

/// Faces read from one collection (`.ttc`) at most; a collection header
/// may claim up to 2^32. The largest installed on macOS 26 holds 20-odd.
const MAX_SCAN_FACES: u32 = 256;

/// Largest `name` table the scan reads for a face: 256 KiB, several times
/// the largest installed. A face with a larger one is left out.
const MAX_SCAN_NAME_BYTES: u32 = 256 << 10;

/// Bytes of the `OS/2` and `post` tables the scan reads: their fixed
/// headers are 100 and 32 bytes, and fontdb's parse reads nothing past
/// them.
const SCAN_TABLE_PREFIX: u32 = 1024;

/// The tables fontdb's face parse reads, in tag order (a table directory
/// is sorted by tag, and the parse binary-searches it), each with the
/// most the scan reads of it and whether a larger table is refused
/// (`true`) or read only up to that.
const SCAN_TABLES: [([u8; 4], u32, bool); 3] = [
    (*b"OS/2", SCAN_TABLE_PREFIX, false),
    (*b"name", MAX_SCAN_NAME_BYTES, true),
    (*b"post", SCAN_TABLE_PREFIX, false),
];

/// A cosmic-text font system over the installed fonts, loaded as the
/// module documentation describes, in fontdb's macOS order.
///
/// The locale is fixed: cosmic-text uses it only for the fallback lists of
/// its own shaping (`Buffer`, `ShapeLine`), which this crate never calls;
/// [`crate::Fonts`] picks every face itself.
///
/// The faces it records against a lazily read file come back with it:
/// [`OwnedFaces::admit`] must see each before the font system loads it.
pub(crate) fn installed_font_system() -> (FontSystem, OwnedFaces) {
    let mut db = fontdb::Database::new();
    #[cfg(target_os = "macos")]
    let owned = {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        load_installed_fonts(&mut db, home.as_deref(), &PROCESS_BUDGET)
    };
    // The directory list below is macOS's; elsewhere fontdb's own scan.
    #[cfg(not(target_os = "macos"))]
    let owned = {
        db.load_system_fonts();
        OwnedFaces::default()
    };
    // What `FontSystem::new` sets; no lookup here uses generic families.
    db.set_monospace_family("Noto Sans Mono");
    db.set_sans_serif_family("Open Sans");
    db.set_serif_family("DejaVu Serif");
    (
        FontSystem::new_with_locale_and_db("en-US".into(), db),
        owned,
    )
}

/// Loads every installed font into `db`, in the order fontdb's macOS
/// `load_system_fonts` uses: `/Library/Fonts`, the system directories,
/// `/Network/Library/Fonts`, then `home`'s `Library/Fonts`, the user
/// files' owned bytes counted against `budget`.
#[cfg(target_os = "macos")]
fn load_installed_fonts(
    db: &mut fontdb::Database,
    home: Option<&Path>,
    budget: &'static OwnedBudget,
) -> OwnedFaces {
    let started = std::time::Instant::now();
    let mut scan = Scan::new(db, budget);
    scan.root(db, Path::new("/Library/Fonts"));
    for dir in SYSTEM_FONT_DIRS {
        db.load_fonts_dir(dir);
    }
    if let Ok(assets) = std::fs::read_dir(SYSTEM_ASSET_DIR) {
        for entry in assets.flatten() {
            if is_font_asset_dir(entry.file_name().as_ref()) {
                db.load_fonts_dir(entry.path());
            }
        }
    }
    scan.see_loaded(db);
    scan.root(db, Path::new("/Network/Library/Fonts"));
    if let Some(home) = home {
        scan.root(db, &home.join("Library/Fonts"));
    }
    tracing::debug!(
        target: "tessera_render",
        faces = db.len(),
        user_files = scan.files,
        owned = scan.owned,
        elapsed = ?started.elapsed(),
        "loaded the installed fonts"
    );
    scan.faces
}

/// Whether `name`, an entry of [`SYSTEM_ASSET_DIR`], is a font asset
/// directory the system pass loads.
fn is_font_asset_dir(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .starts_with("com_apple_MobileAsset_Font")
}

/// Whether the canonical `path` lies in a directory the system pass of
/// [`load_installed_fonts`] loads (mapped). A user directory's symlink to
/// such a file (macOS ships `/Library/Fonts/Arial Unicode.ttf` as one) is
/// skipped, so the face is loaded once, by that pass, and not again here.
fn in_system_pass(path: &Path) -> bool {
    SYSTEM_FONT_DIRS.iter().any(|dir| path.starts_with(dir))
        || path
            .strip_prefix(SYSTEM_ASSET_DIR)
            .ok()
            .and_then(|rest| rest.iter().next())
            .is_some_and(is_font_asset_dir)
}

/// Loads the fonts below `dir` into `db` as a user font directory: read,
/// not mapped, as the module documentation describes, its owned bytes
/// counted against `budget`.
#[cfg(test)]
pub(crate) fn load_user_fonts_dir(
    db: &mut fontdb::Database,
    dir: &Path,
    budget: &'static OwnedBudget,
) -> OwnedFaces {
    let mut scan = Scan::new(db, budget);
    scan.root(db, dir);
    scan.faces
}

/// One walk of user font directories.
struct Scan {
    /// Canonical paths already loaded or walked, so a file reached twice
    /// (through a symlink) loads once and a symlink loop ends.
    seen: HashSet<PathBuf>,
    /// Parses one face's tables into a [`fontdb::FaceInfo`]; emptied after
    /// each face.
    scratch: fontdb::Database,
    /// Font files opened.
    files: usize,
    /// Of them, files whose data is read into memory when used.
    owned: usize,
    /// The faces recorded against those files.
    faces: OwnedFaces,
    /// What their reads count against.
    budget: &'static OwnedBudget,
}

impl Scan {
    fn new(db: &fontdb::Database, budget: &'static OwnedBudget) -> Self {
        let mut scan = Self {
            seen: HashSet::new(),
            scratch: fontdb::Database::new(),
            files: 0,
            owned: 0,
            faces: OwnedFaces::default(),
            budget,
        };
        scan.see_loaded(db);
        scan
    }

    /// Marks the files of every face in `db` as seen.
    fn see_loaded(&mut self, db: &fontdb::Database) {
        for info in db.faces() {
            if let fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) = &info.source
                && !self.seen.contains(path)
            {
                self.seen.insert(path.clone());
            }
        }
    }

    /// Walks the font directory `dir`.
    fn root(&mut self, db: &mut fontdb::Database, dir: &Path) {
        if let Ok(dir) = std::fs::canonicalize(dir)
            && self.seen.insert(dir.clone())
        {
            self.dir(db, &dir, 0);
        }
    }

    /// Loads the fonts in `dir`, `depth` directories below a root.
    fn dir(&mut self, db: &mut fontdb::Database, dir: &Path, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            // Follows symlinks, as fontdb's scan does; a dangling one is
            // skipped.
            let Ok(path) = std::fs::canonicalize(entry.path()) else {
                continue;
            };
            if self.seen.contains(&path) || in_system_pass(&path) {
                continue;
            }
            // `stat` never blocks, whatever the path is now; the open
            // below checks the descriptor, not this.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if depth < MAX_SCAN_DEPTH {
                    self.seen.insert(path.clone());
                    self.dir(db, &path, depth + 1);
                } else {
                    tracing::debug!(
                        target: "tessera_render",
                        path = %path.display(),
                        "a font directory is nested too deep; not scanned"
                    );
                }
            } else if has_font_extension(&path) {
                self.seen.insert(path.clone());
                self.file(db, &path);
            }
        }
    }

    /// Loads the faces of the font file at `path`.
    fn file(&mut self, db: &mut fontdb::Database, path: &Path) {
        #[cfg(test)]
        test_hook::before_open(path);
        let file = match open_regular_file(path) {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(
                    target: "tessera_render",
                    path = %path.display(),
                    %error,
                    "a user font file did not open; skipped"
                );
                return;
            }
        };
        self.files += 1;
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return;
        };
        let owned =
            (len <= MAX_OWNED_FONT_BYTES).then(|| Arc::new(OwnedFontFile::new(path, self.budget)));
        let source = match &owned {
            Some(file) => {
                self.owned += 1;
                fontdb::Source::SharedFile(path.to_path_buf(), file.clone())
            }
            None => fontdb::Source::File(path.to_path_buf()),
        };
        for (face_index, base) in face_offsets(&file, path).into_iter().enumerate() {
            let Some(tables) = read_face_tables(&file, base, len, path) else {
                continue;
            };
            let ids = self
                .scratch
                .load_font_source(fontdb::Source::Binary(Arc::new(tables)));
            for id in ids {
                let Some(mut info) = self.scratch.face(id).cloned() else {
                    continue;
                };
                self.scratch.remove_face(id);
                info.source = source.clone();
                // At most MAX_SCAN_FACES, so it fits.
                info.index = u32::try_from(face_index).unwrap_or(u32::MAX);
                let id = db.push_face_info(info);
                if let Some(file) = &owned {
                    self.faces.0.insert(id, Arc::clone(file));
                }
            }
        }
    }
}

/// Whether fontdb's scan would load `path`: its extensions.
fn has_font_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ttf" | "ttc" | "TTF" | "TTC" | "otf" | "otc" | "OTF" | "OTC")
    )
}

/// Where each face's table directory starts in `file`: 0 for a single
/// font, the collection header's offsets (at most [`MAX_SCAN_FACES`]) for
/// a collection, none for anything else.
fn face_offsets(file: &std::fs::File, path: &Path) -> Vec<u32> {
    const TTCF: u32 = u32::from_be_bytes(*b"ttcf");
    let Some([tag, _, count]) = read_u32s::<3>(file, 0) else {
        return Vec::new();
    };
    if is_sfnt(tag) {
        return vec![0];
    }
    if tag != TTCF {
        return Vec::new();
    }
    if count > MAX_SCAN_FACES {
        tracing::warn!(
            target: "tessera_render",
            path = %path.display(),
            faces = count,
            cap = MAX_SCAN_FACES,
            "a font collection claims too many faces; only the first are loaded"
        );
    }
    let count = count.min(MAX_SCAN_FACES) as usize;
    let mut bytes = vec![0; 4 * count];
    if read_exact_at(file, 12, &mut bytes).is_err() {
        return Vec::new();
    }
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_be_bytes(*chunk))
        .collect()
}

/// The face whose table directory is at `base` in `file`, as a font that
/// holds only the tables fontdb's face parse reads ([`SCAN_TABLES`]): an
/// sfnt header, one record per table found, then the tables. `None` when
/// there is no table directory there or its records run past the file's
/// `file_len` bytes (fontdb's parse refuses both), a read fails or comes
/// back short (the file changed), or the `name` table is over its cap.
fn read_face_tables(
    file: &std::fs::File,
    base: u32,
    file_len: u64,
    path: &Path,
) -> Option<Vec<u8>> {
    let [tag, tables, _] = read_u32s::<3>(file, u64::from(base))?;
    if !is_sfnt(tag) {
        return None;
    }
    let count = u64::from(tables >> 16);
    // ttf-parser refuses a face whose table records run past the data.
    if u64::from(base) + 12 + 16 * count > file_len {
        return None;
    }
    let mut found: Vec<([u8; 4], u32, u32)> = Vec::with_capacity(SCAN_TABLES.len());
    for (want, cap, refuse) in SCAN_TABLES {
        let Some((start, len)) = table_range(file, u64::from(base) + 12, count, want, file_len)
        else {
            continue;
        };
        if refuse && len > cap {
            tracing::warn!(
                target: "tessera_render",
                path = %path.display(),
                table = %String::from_utf8_lossy(&want),
                bytes = len,
                cap,
                "a font table is too large to scan; its face is left out"
            );
            return None;
        }
        found.push((want, start, len.min(cap)));
    }
    // Every length is at most MAX_SCAN_NAME_BYTES, so these fit u32.
    let header = 12 + 16 * found.len();
    let total = header + found.iter().map(|&(_, _, len)| len as usize).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
    out.extend_from_slice(&u16::try_from(found.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    let mut at = header;
    for &(tag, _, len) in &found {
        out.extend_from_slice(&tag);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&u32::try_from(at).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        at += len as usize;
    }
    for &(_, start, len) in &found {
        let from = out.len();
        out.resize(from + len as usize, 0);
        read_exact_at(file, u64::from(start), &mut out[from..]).ok()?;
    }
    Some(out)
}

/// The file range (offset, length) of table `tag` in the table directory
/// whose `count` records start at `records`, in a file of `file_len`
/// bytes: ttf-parser's `RawFace::table`, step for step, so the scan finds
/// exactly the tables fontdb's parse does. Its search is std's
/// `binary_search_by` (base and size halving, then one equality check at
/// the end), which on an unsorted directory finds tables that a low/high
/// search misses and misses others; a table whose end is past the file is
/// no table, as `data.get(offset..end)` gives none.
fn table_range(
    file: &std::fs::File,
    records: u64,
    count: u64,
    tag: [u8; 4],
    file_len: u64,
) -> Option<(u32, u32)> {
    let tag = u32::from_be_bytes(tag);
    let record = |index: u64| read_u32s::<4>(file, records + 16 * index);
    let mut size = count;
    if size == 0 {
        return None;
    }
    let mut base = 0;
    while size > 1 {
        let half = size / 2;
        let mid = base + half;
        let [record_tag, ..] = record(mid)?;
        if record_tag.cmp(&tag) != std::cmp::Ordering::Greater {
            base = mid;
        }
        size -= half;
    }
    let [record_tag, _, start, len] = record(base)?;
    let end = u64::from(start) + u64::from(len);
    (record_tag == tag && end <= file_len).then_some((start, len))
}

/// The faces of a font database recorded against an [`OwnedFontFile`],
/// by face, until each is admitted.
#[derive(Default)]
pub(crate) struct OwnedFaces(HashMap<fontdb::ID, Arc<OwnedFontFile>>);

impl OwnedFaces {
    /// Readies face `id` of `system` for its first load: reads its file
    /// into memory, or, when the file is now over [`MAX_OWNED_FONT_BYTES`]
    /// or the read would pass the process's [`MAX_OWNED_TOTAL_BYTES`],
    /// turns every face of that file back into a mapped `Source::File`.
    /// Decided once per file; free once nothing is left to admit.
    pub(crate) fn admit(&mut self, system: &mut FontSystem, id: fontdb::ID) {
        if self.0.is_empty() {
            return;
        }
        let Some(file) = self.0.get(&id).cloned() else {
            return;
        };
        self.0.retain(|_, other| !Arc::ptr_eq(other, &file));
        if !file.admit() {
            system.db_mut().make_face_data_unshared(id);
        }
    }

    /// Faces not admitted yet.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// A count of owned font bytes held against a cap.
pub(crate) struct OwnedBudget {
    cap: u64,
    used: AtomicU64,
}

impl OwnedBudget {
    /// A budget of `cap` bytes, none used.
    pub(crate) const fn new(cap: u64) -> Self {
        Self {
            cap,
            used: AtomicU64::new(0),
        }
    }

    /// Takes `bytes`, or nothing and `false` when they would pass the cap.
    fn reserve(&self, bytes: u64) -> bool {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|&total| total <= self.cap)
            })
            .is_ok()
    }

    /// Gives back `bytes` taken by [`Self::reserve`].
    fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }

    /// Bytes held now.
    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// A user font file read whole into memory the first time a face of it is
/// used ([`OwnedFaces::admit`]), then never touched again: a truncation in
/// place after that changes nothing, and is never a `SIGBUS`.
///
/// The read opens the file non-blocking and refuses anything that is not
/// a regular file; a refused or failed read leaves the data empty, so the
/// file's faces fail to load and their characters fall back to the next
/// face. A file now over [`MAX_OWNED_FONT_BYTES`], or one whose bytes the
/// budget refuses, is not read: its faces are mapped instead. The outcome
/// is kept for the life of the font database; the bytes count against the
/// budget until then.
pub(crate) struct OwnedFontFile {
    path: PathBuf,
    budget: &'static OwnedBudget,
    /// `None`: not read, mapped instead.
    data: OnceLock<Option<Vec<u8>>>,
}

impl OwnedFontFile {
    /// The file at `path`, not read yet, its bytes to count against
    /// `budget`.
    pub(crate) fn new(path: &Path, budget: &'static OwnedBudget) -> Self {
        Self {
            path: path.to_path_buf(),
            budget,
            data: OnceLock::new(),
        }
    }

    /// Reads the file if that is not decided yet: `false` when it is to be
    /// mapped instead.
    pub(crate) fn admit(&self) -> bool {
        self.data().is_some()
    }

    fn data(&self) -> Option<&[u8]> {
        self.data
            .get_or_init(|| match read_owned(&self.path, self.budget) {
                Ok(Some(data)) => Some(data),
                Ok(None) => {
                    tracing::debug!(
                        target: "tessera_render",
                        path = %self.path.display(),
                        "a user font file is over its own or the total owned size; mapped"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        target: "tessera_render",
                        path = %self.path.display(),
                        %error,
                        "a font file could not be read; its faces fall back"
                    );
                    Some(Vec::new())
                }
            })
            .as_deref()
    }
}

impl AsRef<[u8]> for OwnedFontFile {
    /// The file's bytes; empty when it was not read (its faces are then
    /// mapped, and nothing reads this).
    fn as_ref(&self) -> &[u8] {
        self.data().unwrap_or_default()
    }
}

impl Drop for OwnedFontFile {
    fn drop(&mut self) {
        if let Some(Some(data)) = self.data.get() {
            self.budget.release(data.len() as u64);
        }
    }
}

/// The whole file at `path`, its bytes taken from `budget`: `None` when it
/// is over [`MAX_OWNED_FONT_BYTES`] or `budget` refuses them.
fn read_owned(path: &Path, budget: &OwnedBudget) -> std::io::Result<Option<Vec<u8>>> {
    let file = open_regular_file(path)?;
    let len = file.metadata()?.len();
    if len > MAX_OWNED_FONT_BYTES || !budget.reserve(len) {
        return Ok(None);
    }
    // `len` is at most MAX_OWNED_FONT_BYTES, so this fits usize.
    let mut data = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    let read = (&file).take(len + 1).read_to_end(&mut data);
    let held = data.len() as u64;
    if let Err(error) = read {
        budget.release(len);
        return Err(error);
    }
    if held > len {
        budget.release(len);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the file grew while it was read",
        ));
    }
    // Shrunk since the `fstat`: hold only what was read.
    budget.release(len - held);
    data.shrink_to_fit();
    Ok(Some(data))
}

/// Test-only hook the scan calls before it opens each font file, on the
/// scanning thread.
#[cfg(test)]
mod test_hook {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static BEFORE_OPEN: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs `hook` before each font file open on this thread until the
    /// next call; `None` removes it.
    pub(super) fn set(hook: Option<Hook>) {
        BEFORE_OPEN.with(|slot| *slot.borrow_mut() = hook);
    }

    pub(super) fn before_open(path: &Path) {
        BEFORE_OPEN.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(path);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const MONACO: &str = "/System/Library/Fonts/Monaco.ttf";

    /// A fresh directory for one test's fonts, removed when it drops.
    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("tessera-font-scan-{name}-"))
            .tempdir()
            .expect("temp dir")
    }

    /// A budget of `cap` bytes for one test.
    fn budget(cap: u64) -> &'static OwnedBudget {
        Box::leak(Box::new(OwnedBudget::new(cap)))
    }

    fn copy_monaco(to: &Path) {
        std::fs::copy(MONACO, to).expect("Monaco is installed");
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo {}", path.display());
    }

    /// What fontdb's parse gives for a face, without its source.
    type Parsed = (Vec<String>, String, String, u16, u16, bool, u32);

    fn parsed(info: &fontdb::FaceInfo) -> Parsed {
        (
            info.families.iter().map(|(name, _)| name.clone()).collect(),
            info.post_script_name.clone(),
            format!("{:?}", info.style),
            info.weight.0,
            info.stretch.to_number(),
            info.monospaced,
            info.index,
        )
    }

    /// A [`test_hook`] hook that can move to the scanning thread.
    type SendHook = Box<dyn FnMut(&Path) + Send>;

    /// The user font directory `dir`, scanned into a fresh database on a
    /// thread, waited for at most 60 s (`None` past that): the database's
    /// faces then.
    fn scan_bounded(dir: &Path, hook: Option<SendHook>) -> Option<Vec<fontdb::FaceInfo>> {
        let (sent, got) = mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            if let Some(mut hook) = hook {
                test_hook::set(Some(Box::new(move |path| hook(path))));
            }
            let mut db = fontdb::Database::new();
            let _ = load_user_fonts_dir(&mut db, &dir, budget(MAX_OWNED_TOTAL_BYTES));
            test_hook::set(None);
            let _ = sent.send(db.faces().cloned().collect::<Vec<_>>());
        });
        got.recv_timeout(Duration::from_secs(60)).ok()
    }

    /// Lets a scan blocked in the open of the FIFO at `path` go on: a
    /// writer arrives.
    fn release(path: &Path) {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path);
    }

    /// A user directory's symlink to a file the system pass loads is left
    /// to that pass, so each of its faces loads once, mapped.
    #[test]
    fn a_user_symlink_to_a_system_font_leaves_it_to_the_system_pass() {
        let dir = scratch("system-link");
        let menlo = Path::new("/System/Library/Fonts/Menlo.ttc");
        std::os::unix::fs::symlink(menlo, dir.path().join("Menlo.ttc")).expect("symlink");
        let mut db = fontdb::Database::new();
        let mut scan = Scan::new(&db, budget(MAX_OWNED_TOTAL_BYTES));
        // The order of `load_installed_fonts`: `/Library/Fonts` is walked
        // before the system pass.
        scan.root(&mut db, dir.path());
        assert_eq!(db.len(), 0, "the link is not loaded from the user dir");
        db.load_fonts_dir("/System/Library/Fonts");
        let menlo_faces: Vec<u32> = db
            .faces()
            .filter(|info| matches!(&info.source, fontdb::Source::File(at) if at == menlo))
            .map(|info| info.index)
            .collect();
        assert_eq!(menlo_faces, [0, 1, 2, 3], "each Menlo face once, mapped");
        assert!(!in_system_pass(Path::new(
            "/System/Library/AssetsV2/com_apple_MobileAssetX/a.ttf"
        )));
        assert!(in_system_pass(Path::new(
            "/System/Library/AssetsV2/com_apple_MobileAsset_Font8/x/a.ttf"
        )));
    }

    /// The scan agrees with fontdb's own parse (the reference path, which
    /// maps the whole file) on every face of every installed font file:
    /// the system fonts and whatever user fonts this machine has. Files up
    /// to the cap are owned, larger ones stay file sources.
    #[test]
    fn the_scan_parses_every_installed_face_as_fontdb_does() {
        let mut files = Vec::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut dirs = vec![
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
        ];
        dirs.extend(home.map(|home| home.join("Library/Fonts")));
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.is_file() && has_font_extension(&path) {
                    files.push(path);
                }
            }
        }
        assert!(files.len() > 50, "the system fonts: {}", files.len());
        let (mut faces, mut owned) = (0, 0);
        for path in &files {
            let mut reference = fontdb::Database::new();
            if reference.load_font_file(path).is_err() {
                continue;
            }
            let want: Vec<Parsed> = reference.faces().map(parsed).collect();
            let mut db = fontdb::Database::new();
            let mut scan = Scan::new(&db, budget(MAX_OWNED_TOTAL_BYTES));
            scan.file(&mut db, path);
            let got: Vec<Parsed> = db.faces().map(parsed).collect();
            assert_eq!(got, want, "{}", path.display());
            let len = std::fs::metadata(path).expect("stat").len();
            for info in db.faces() {
                match &info.source {
                    fontdb::Source::SharedFile(at, _) if len <= MAX_OWNED_FONT_BYTES => {
                        assert_eq!(at, path);
                        owned += 1;
                    }
                    fontdb::Source::File(at) if len > MAX_OWNED_FONT_BYTES => {
                        assert_eq!(at, path);
                    }
                    _ => panic!("{} ({len} bytes): wrong source kind", path.display()),
                }
            }
            faces += got.len();
        }
        assert!(faces > 100 && owned > 0, "{faces} faces, {owned} owned");
    }

    /// #683: a FIFO named like a font in a scanned directory neither
    /// blocks the scan nor stops the fonts next to it from loading.
    #[test]
    fn a_fifo_in_a_scanned_font_dir_does_not_block_the_scan() {
        let dir = scratch("fifo");
        let fifo = dir.path().join("a.ttf");
        mkfifo(&fifo);
        copy_monaco(&dir.path().join("b.ttf"));
        let Some(faces) = scan_bounded(dir.path(), None) else {
            release(&fifo);
            panic!("the scan blocked on the FIFO");
        };
        let names: Vec<Parsed> = faces.iter().map(parsed).collect();
        assert_eq!(faces.len(), 1, "{names:?}");
        assert_eq!(faces[0].families[0].0, "Monaco");
    }

    /// #683: a font file swapped for a FIFO after the walk listed and
    /// stat'ed it, just before the open, does not block the scan: the open
    /// is non-blocking and the descriptor is what is checked.
    #[test]
    fn a_font_swapped_for_a_fifo_before_its_open_does_not_block_the_scan() {
        let dir = scratch("swap");
        let swap = dir.path().join("swap.ttf");
        copy_monaco(&swap);
        copy_monaco(&dir.path().join("kept.ttf"));
        let hook = Box::new(|path: &Path| {
            if path.file_name().is_some_and(|name| name == "swap.ttf") {
                std::fs::remove_file(path).expect("remove the font");
                mkfifo(path);
            }
        });
        let Some(faces) = scan_bounded(dir.path(), Some(hook)) else {
            release(
                &std::fs::canonicalize(dir.path())
                    .expect("dir")
                    .join("swap.ttf"),
            );
            panic!("the scan blocked on the FIFO");
        };
        let paths: Vec<_> = faces
            .iter()
            .filter_map(|info| match &info.source {
                fontdb::Source::SharedFile(path, _) => path.file_name().map(PathBuf::from),
                _ => None,
            })
            .collect();
        assert_eq!(paths, [PathBuf::from("kept.ttf")]);
    }

    /// S5: a user font file is owned up to `MAX_OWNED_FONT_BYTES`; one
    /// byte more and it stays a file source (mapped when used, the ADR
    /// 0007 residual). The large files are sparse copies of Monaco.
    #[test]
    fn a_user_font_is_owned_up_to_the_cap_and_mapped_past_it() {
        let dir = scratch("cap");
        for (name, len) in [
            ("small.ttf", None),
            ("at-cap.ttf", Some(MAX_OWNED_FONT_BYTES)),
            ("over-cap.ttf", Some(MAX_OWNED_FONT_BYTES + 1)),
        ] {
            let path = dir.path().join(name);
            copy_monaco(&path);
            if let Some(len) = len {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .expect("open the copy");
                file.set_len(len).expect("grow the copy");
            }
        }
        let faces = scan_bounded(dir.path(), None).expect("the scan ends");
        let mut kinds: Vec<(String, &str)> = faces
            .iter()
            .map(|info| match &info.source {
                fontdb::Source::SharedFile(path, _) => (path, "owned"),
                fontdb::Source::File(path) => (path, "file"),
                fontdb::Source::Binary(_) => panic!("a binary source"),
            })
            .map(|(path, kind)| {
                let name = path.file_name().expect("a name").to_string_lossy();
                (name.into_owned(), kind)
            })
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                ("at-cap.ttf".into(), "owned"),
                ("over-cap.ttf".into(), "file"),
                ("small.ttf".into(), "owned"),
            ]
        );
    }

    /// An owned file is read whole on first use and kept: a truncation in
    /// place afterwards changes nothing in memory, and its bytes count
    /// against the budget until it drops. A file grown past the per-file
    /// cap since the scan is left to be mapped; a FIFO or a missing file
    /// reads as empty, without blocking.
    #[test]
    fn an_owned_font_file_reads_once_and_refuses_what_it_cannot_own() {
        let dir = scratch("owned");
        let path = dir.path().join("copy.ttf");
        copy_monaco(&path);
        let budget = budget(MAX_OWNED_TOTAL_BYTES);
        let owned = OwnedFontFile::new(&path, budget);
        let want = std::fs::read(MONACO).expect("Monaco");
        assert!(owned.admit());
        assert_eq!(owned.as_ref(), want.as_slice());
        assert_eq!(budget.used(), want.len() as u64, "counted when read");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_len(0))
            .expect("truncate");
        assert_eq!(owned.as_ref(), want.as_slice(), "kept in memory");
        let truncated = OwnedFontFile::new(&path, budget);
        assert!(
            truncated.admit() && truncated.as_ref().is_empty(),
            "now empty"
        );
        drop((owned, truncated));
        assert_eq!(budget.used(), 0, "given back when dropped");

        let big = dir.path().join("big.ttf");
        copy_monaco(&big);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .and_then(|file| file.set_len(MAX_OWNED_FONT_BYTES + 1))
            .expect("grow");
        let big = OwnedFontFile::new(&big, budget);
        assert!(!big.admit(), "over the per-file cap: mapped instead");
        assert_eq!(big.as_ref(), [0_u8; 0]);
        assert_eq!(budget.used(), 0);

        let fifo = dir.path().join("fifo.ttf");
        mkfifo(&fifo);
        let (sent, got) = mpsc::channel();
        let at = fifo.clone();
        std::thread::spawn(move || {
            let fifo = OwnedFontFile::new(&at, budget);
            let _ = sent.send(fifo.admit() && fifo.as_ref().is_empty());
        });
        let Ok(empty) = got.recv_timeout(Duration::from_secs(60)) else {
            release(&fifo);
            panic!("the read blocked on the FIFO");
        };
        assert!(empty, "a FIFO reads as nothing");
        let gone = OwnedFontFile::new(&dir.path().join("gone.ttf"), budget);
        assert_eq!(gone.as_ref(), [0_u8; 0], "a missing file reads as nothing");
    }

    /// S5: the owned bytes of every file stop at the budget: the file whose
    /// read would pass it is mapped instead (its faces turned back into
    /// `Source::File`, which cosmic-text maps), and still loads. Each file
    /// is decided once, and its bytes are given back when the font system
    /// drops.
    #[test]
    fn the_owned_total_stops_at_its_budget_and_the_next_file_is_mapped() {
        let dir = scratch("budget");
        for name in ["a.ttf", "b.ttf", "c.ttf"] {
            copy_monaco(&dir.path().join(name));
        }
        let len = std::fs::metadata(MONACO).expect("Monaco").len();
        // Room for exactly two copies, and one byte short of three.
        for cap in [2 * len, 3 * len - 1] {
            let budget = budget(cap);
            let mut db = fontdb::Database::new();
            let mut owned = load_user_fonts_dir(&mut db, dir.path(), budget);
            let mut ids: Vec<(PathBuf, fontdb::ID)> = db
                .faces()
                .map(|info| match &info.source {
                    fontdb::Source::SharedFile(path, _) => (path.clone(), info.id),
                    _ => panic!("{info:?} is not owned before its first use"),
                })
                .collect();
            ids.sort();
            assert_eq!(ids.len(), 3);
            assert_eq!(owned.len(), 3);
            assert_eq!(budget.used(), 0, "nothing is read by the scan");
            let mut system = FontSystem::new_with_locale_and_db("en-US".into(), db);
            let mut kinds = Vec::new();
            for (path, id) in &ids {
                owned.admit(&mut system, *id);
                let kind = match system.db().face(*id).map(|info| &info.source) {
                    Some(fontdb::Source::SharedFile(..)) => "owned",
                    Some(fontdb::Source::File(..)) => "mapped",
                    _ => "other",
                };
                let loads = system.get_font(*id, fontdb::Weight::NORMAL).is_some();
                let name = path.file_name().expect("a name").to_string_lossy();
                kinds.push((name.into_owned(), kind, loads));
            }
            assert_eq!(
                kinds,
                [
                    ("a.ttf".into(), "owned", true),
                    ("b.ttf".into(), "owned", true),
                    ("c.ttf".into(), "mapped", true),
                ],
                "cap {cap}"
            );
            assert_eq!(budget.used(), 2 * len, "two files held");
            assert_eq!(owned.len(), 0, "each file decided once");
            drop((system, owned));
            assert_eq!(budget.used(), 0, "given back with the font system");
        }
    }

    /// S5: the walk descends `MAX_SCAN_DEPTH` directories below a root and
    /// no further; a symlink loop ends and a font reached twice through a
    /// symlink loads once.
    #[test]
    fn the_walk_stops_at_its_depth_cap_and_follows_each_path_once() {
        let dir = scratch("depth");
        let mut at = dir.path().to_path_buf();
        for depth in 0..=MAX_SCAN_DEPTH + 1 {
            copy_monaco(&at.join(format!("depth-{depth}.ttf")));
            at = at.join(format!("d{depth}"));
            std::fs::create_dir(&at).expect("mkdir");
        }
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).expect("symlink the root");
        std::os::unix::fs::symlink(dir.path().join("depth-0.ttf"), dir.path().join("alias.ttf"))
            .expect("symlink a font");
        let faces = scan_bounded(dir.path(), None).expect("the scan ends");
        let mut names: Vec<String> = faces
            .iter()
            .filter_map(|info| match &info.source {
                fontdb::Source::SharedFile(path, _) => path.file_stem(),
                _ => None,
            })
            .map(|stem| stem.to_string_lossy().into_owned())
            .collect();
        names.sort_by_key(|name| name.trim_start_matches("depth-").parse::<usize>().ok());
        let want: Vec<String> = (0..=MAX_SCAN_DEPTH).map(|d| format!("depth-{d}")).collect();
        assert_eq!(names, want);
    }

    /// Monaco as a collection (`.ttc`) of `faces` faces, all one table
    /// directory: the collection header, then Monaco with its table
    /// offsets moved past the header.
    fn monaco_collection(faces: u32) -> Vec<u8> {
        let mut monaco = std::fs::read(MONACO).expect("Monaco");
        let header = 12 + 4 * faces;
        let tables = usize::from(u16::from_be_bytes([monaco[4], monaco[5]]));
        for record in (0..tables).map(|t| 12 + 16 * t) {
            let at = &mut monaco[record + 8..record + 12];
            let offset = u32::from_be_bytes(at.try_into().expect("4 bytes")) + header;
            at.copy_from_slice(&offset.to_be_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        out.extend_from_slice(&faces.to_be_bytes());
        for _ in 0..faces {
            out.extend_from_slice(&header.to_be_bytes());
        }
        out.extend_from_slice(&monaco);
        out
    }

    /// S5: a collection header that claims more than `MAX_SCAN_FACES`
    /// faces loads the first `MAX_SCAN_FACES`; one at the cap loads all.
    #[test]
    fn a_collection_loads_at_most_the_face_cap() {
        let dir = scratch("faces");
        for (name, faces) in [
            ("at.ttc", MAX_SCAN_FACES),
            ("over.ttc", MAX_SCAN_FACES + 44),
        ] {
            std::fs::write(dir.path().join(name), monaco_collection(faces)).expect("write");
        }
        let faces = scan_bounded(dir.path(), None).expect("the scan ends");
        let count = |name: &str| {
            faces
                .iter()
                .filter(|info| {
                    matches!(&info.source, fontdb::Source::SharedFile(path, _)
                        if path.file_name().is_some_and(|n| n == name))
                })
                .count()
        };
        assert_eq!(count("at.ttc"), MAX_SCAN_FACES as usize);
        assert_eq!(count("over.ttc"), MAX_SCAN_FACES as usize);
        let indexes: Vec<u32> = faces.iter().take(3).map(|info| info.index).collect();
        assert_eq!(indexes, [0, 1, 2]);
    }

    /// S5: a face whose `name` table is over `MAX_SCAN_NAME_BYTES` is left
    /// out without reading it; one at the cap loads. The copies point
    /// Monaco's `name` record at that many bytes (the file padded to fit).
    #[test]
    fn a_name_table_over_its_cap_leaves_the_face_out() {
        let dir = scratch("name");
        for (file, len) in [
            ("at.ttf", MAX_SCAN_NAME_BYTES),
            ("over.ttf", MAX_SCAN_NAME_BYTES + 1),
        ] {
            let mut font = std::fs::read(MONACO).expect("Monaco");
            let tables = usize::from(u16::from_be_bytes([font[4], font[5]]));
            let mut end = 0;
            for record in (0..tables).map(|t| 12 + 16 * t) {
                if &font[record..record + 4] == b"name" {
                    let start = u32::from_be_bytes(
                        font[record + 8..record + 12].try_into().expect("4 bytes"),
                    );
                    font[record + 12..record + 16].copy_from_slice(&len.to_be_bytes());
                    end = start as usize + len as usize;
                }
            }
            font.resize(font.len().max(end), 0);
            std::fs::write(dir.path().join(file), font).expect("write");
        }
        let faces = scan_bounded(dir.path(), None).expect("the scan ends");
        let names: Vec<_> = faces
            .iter()
            .filter_map(|info| match &info.source {
                fontdb::Source::SharedFile(path, _) => path.file_name().map(PathBuf::from),
                _ => None,
            })
            .collect();
        assert_eq!(names, [PathBuf::from("at.ttf")]);
        assert_eq!(faces[0].families[0].0, "Monaco");
    }

    /// Monaco's bytes, its table records' offset and count.
    fn monaco_directory() -> (Vec<u8>, usize, usize) {
        let data = std::fs::read(MONACO).expect("Monaco is installed");
        let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
        assert!(count > 20, "Monaco's tables: {count}");
        (data, 12, count)
    }

    /// `data` with its `count` table records at `records` put in `order`
    /// (indices into the records as they are); the tables stay where they
    /// are.
    fn reorder(data: &[u8], records: usize, count: usize, order: &[usize]) -> Vec<u8> {
        let mut out = data.to_vec();
        for (to, &from) in order.iter().enumerate() {
            let from = records + 16 * from;
            out[records + 16 * to..][..16].copy_from_slice(&data[from..from + 16]);
        }
        assert_eq!(out.len(), data.len());
        assert_eq!(order.len(), count, "a whole order");
        out
    }

    /// A table's (offset, length) in its font.
    type Span = (usize, usize);

    /// The table `tag` as the scan's lookup finds it in `file` (whose
    /// bytes are `data`) and as ttf-parser's does: (offset, length) each.
    fn both_lookups(
        file: &std::fs::File,
        data: &[u8],
        tag: [u8; 4],
    ) -> (Option<Span>, Option<Span>) {
        let count = u64::from(u16::from_be_bytes([data[4], data[5]]));
        let len = u64::try_from(data.len()).expect("fits");
        let ours = table_range(file, 12, count, tag, len).map(|(start, len)| {
            (
                usize::try_from(start).expect("fits"),
                usize::try_from(len).expect("fits"),
            )
        });
        let raw = ttf_parser::RawFace::parse(data, 0).expect("records fit");
        let theirs = raw
            .table(ttf_parser::Tag::from_bytes(&tag))
            .map(|table| (table.as_ptr().addr() - data.as_ptr().addr(), table.len()));
        (ours, theirs)
    }

    /// Monaco with its table records in an order where a low/high binary
    /// search misses `name` and `post` (and so the face) while ttf-parser's
    /// search, std's `binary_search_by`, finds all three tables fontdb's
    /// parse reads: the scan loads the face as fontdb does (#683).
    #[test]
    fn an_unsorted_table_directory_loads_as_fontdb_loads_it() {
        let (data, records, count) = monaco_directory();
        let tags: Vec<[u8; 4]> = (0..count)
            .map(|i| data[records + 16 * i..][..4].try_into().expect("4 bytes"))
            .collect();
        let shuffled = [
            "morx", "OS/2", "Zapf", "fond", "hmtx", "cmap", "hdmx", "prep", "loca", "maxp", "bdat",
            "gasp", "fpgm", "head", "glyf", "bsln", "bloc", "meta", "hhea", "prop", "fdsc", "feat",
            "cvt ", "name", "post",
        ];
        assert_eq!(shuffled.len(), count, "Monaco's tables changed");
        let order: Vec<usize> = shuffled
            .iter()
            .map(|tag| {
                tags.iter()
                    .position(|have| have == tag.as_bytes())
                    .unwrap_or_else(|| panic!("Monaco has no {tag}"))
            })
            .collect();
        let bytes = reorder(&data, records, count, &order);
        let dir = scratch("unsorted");
        let path = dir.path().join("unsorted.ttf");
        std::fs::write(&path, &bytes).expect("write");

        let mut reference = fontdb::Database::new();
        reference.load_font_data(bytes.clone());
        let theirs: Vec<Parsed> = reference.faces().map(parsed).collect();
        assert_eq!(theirs.len(), 1, "fontdb loads the unsorted font");
        assert_eq!(theirs[0].0, ["Monaco"]);

        let mut db = fontdb::Database::new();
        load_user_fonts_dir(&mut db, dir.path(), budget(u64::MAX));
        let ours: Vec<Parsed> = db.faces().map(parsed).collect();
        assert_eq!(ours, theirs);

        let file = std::fs::File::open(&path).expect("open");
        for (tag, _, _) in SCAN_TABLES {
            let (ours, theirs) = both_lookups(&file, &bytes, tag);
            assert!(theirs.is_some(), "ttf-parser finds {tag:?}");
            assert_eq!(ours, theirs, "{tag:?}");
        }
    }

    /// For 1000 orders of Monaco's table records, the scan's lookup finds
    /// each table, and misses each, exactly where ttf-parser's
    /// `RawFace::table` does: the three tables the scan reads, every other
    /// table Monaco has, and one it has not.
    #[test]
    fn the_table_lookup_agrees_with_ttf_parser_on_any_record_order() {
        use proptest::prelude::*;
        let (data, records, count) = monaco_directory();
        let mut tags: Vec<[u8; 4]> = (0..count)
            .map(|i| data[records + 16 * i..][..4].try_into().expect("4 bytes"))
            .collect();
        tags.push(*b"zzzz");
        let dir = scratch("orders");
        let path = dir.path().join("orders.ttf");
        std::fs::write(&path, &data).expect("write");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");
        let (found, missed) = (std::cell::Cell::new(0_u32), std::cell::Cell::new(0_u32));
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(1000));
        let orders = Just((0..count).collect::<Vec<usize>>()).prop_shuffle();
        runner
            .run(&orders, |order| {
                let bytes = reorder(&data, records, count, &order);
                std::os::unix::fs::FileExt::write_all_at(
                    &file,
                    &bytes[records..records + 16 * count],
                    12,
                )
                .expect("write the records");
                for &tag in &tags {
                    let (ours, theirs) = both_lookups(&file, &bytes, tag);
                    prop_assert_eq!(ours, theirs, "{:?} in order {:?}", tag, order);
                    if SCAN_TABLES.iter().any(|&(want, _, _)| want == tag) {
                        let count = if theirs.is_some() { &found } else { &missed };
                        count.set(count.get() + 1);
                    }
                }
                Ok(())
            })
            .expect("the lookups agree");
        // The orders reach both outcomes for the scan's tables.
        let (found, missed) = (found.get(), missed.get());
        assert!(found > 0 && missed > 0, "found {found}, missed {missed}");
    }

    /// For any small table directory, sorted or not, with tables inside,
    /// at the end of and past the data, the scan's lookup agrees with
    /// ttf-parser's on `name`, `OS/2` and `post`.
    #[test]
    fn the_table_lookup_agrees_with_ttf_parser_on_any_directory() {
        use proptest::prelude::*;
        let tag = prop_oneof![
            Just(*b"name"),
            Just(*b"OS/2"),
            Just(*b"post"),
            Just(*b"cmap"),
            any::<[u8; 4]>(),
        ];
        let end = prop_oneof![Just(0_u32), 0_u32..1024, Just(u32::MAX)];
        let record = (tag, end.clone(), end);
        let directory = (prop::collection::vec(record, 0..24), 0_usize..512);
        let dir = scratch("directories");
        let path = dir.path().join("directory.ttf");
        let (found, past) = (std::cell::Cell::new(0_u32), std::cell::Cell::new(0_u32));
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(1000));
        runner
            .run(&directory, |(records, padding)| {
                let mut bytes = 0x0001_0000_u32.to_be_bytes().to_vec();
                bytes.extend_from_slice(&u16::try_from(records.len()).expect("fits").to_be_bytes());
                bytes.extend_from_slice(&[0; 6]);
                for (tag, start, len) in &records {
                    bytes.extend_from_slice(tag);
                    bytes.extend_from_slice(&[0; 4]);
                    bytes.extend_from_slice(&start.to_be_bytes());
                    bytes.extend_from_slice(&len.to_be_bytes());
                }
                bytes.resize(bytes.len() + padding, 0);
                std::fs::write(&path, &bytes).expect("write");
                let file = std::fs::File::open(&path).expect("open");
                for (tag, _, _) in SCAN_TABLES {
                    let (ours, theirs) = both_lookups(&file, &bytes, tag);
                    prop_assert_eq!(ours, theirs, "{:?} in {:?}", tag, records);
                    found.set(found.get() + u32::from(theirs.is_some()));
                    past.set(
                        past.get()
                            + u32::from(records.iter().any(|&(have, start, len)| {
                                have == tag
                                    && u64::from(start) + u64::from(len)
                                        > u64::try_from(bytes.len()).expect("fits")
                            })),
                    );
                }
                Ok(())
            })
            .expect("the lookups agree");
        let (found, past) = (found.get(), past.get());
        assert!(found > 0 && past > 0, "found {found}, past the data {past}");
    }

    /// A collection whose one face's table records run past the end of the
    /// file, though the search never visits the missing one and the tables
    /// it finds (Monaco's own) lie inside: ttf-parser refuses the face, so
    /// fontdb loads nothing, and the scan loads nothing; with the last
    /// record whole, both load it.
    #[test]
    fn a_face_whose_table_records_run_past_the_file_is_refused_as_fontdb_refuses_it() {
        let (data, records, count) = monaco_directory();
        let table = |want: &[u8; 4]| {
            (0..count)
                .map(|i| &data[records + 16 * i..][..16])
                .find(|record| &record[..4] == want)
                .map(|record| {
                    let word = |at: usize| {
                        u32::from_be_bytes(record[at..at + 4].try_into().expect("4 bytes"))
                    };
                    &data[word(8) as usize..][..word(12) as usize]
                })
                .expect("Monaco has the table")
        };
        let mut bytes = b"ttcf".to_vec();
        bytes.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 4]);
        let mut directory = Vec::new();
        for tag in [b"OS/2", b"name", b"post"] {
            let at = u32::try_from(bytes.len()).expect("fits");
            let body = table(tag);
            directory.push((*tag, at, u32::try_from(body.len()).expect("fits")));
            bytes.extend_from_slice(body);
        }
        directory.push((*b"zzzz", 0, 0));
        let face = u32::try_from(bytes.len()).expect("fits");
        bytes[12..16].copy_from_slice(&face.to_be_bytes());
        bytes.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        bytes.extend_from_slice(&4_u16.to_be_bytes());
        bytes.extend_from_slice(&[0; 6]);
        for (tag, at, len) in directory {
            bytes.extend_from_slice(&tag);
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(&at.to_be_bytes());
            bytes.extend_from_slice(&len.to_be_bytes());
        }
        let cut = bytes.len() - 8;
        for (bytes, faces) in [(&bytes[..], 1), (&bytes[..cut], 0)] {
            let mut reference = fontdb::Database::new();
            reference.load_font_data(bytes.to_vec());
            let theirs: Vec<Parsed> = reference.faces().map(parsed).collect();
            assert_eq!(theirs.len(), faces, "fontdb, {} bytes", bytes.len());
            let dir = scratch("records-past");
            std::fs::write(dir.path().join("face.ttc"), bytes).expect("write");
            let mut db = fontdb::Database::new();
            load_user_fonts_dir(&mut db, dir.path(), budget(u64::MAX));
            let ours: Vec<Parsed> = db.faces().map(parsed).collect();
            assert_eq!(ours, theirs, "{} bytes", bytes.len());
        }
    }
}
