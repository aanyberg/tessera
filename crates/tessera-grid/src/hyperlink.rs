//! OSC 8 hyperlinks: the links themselves, their interning, and the per-line
//! table that attaches them to cells (ADR 0026).
//!
//! A [`Cell`] stays 20 bytes. A cell written while a link is active gets
//! [`CellFlags::LINK`] and the line's [`GraphemeTable`] stores the
//! [`Hyperlink`] under the cell's column, the same split ADR 0008 makes for
//! grapheme clusters (in the same allocation, so a [`Line`] does not grow).
//! Most lines have no links, so an empty table costs one pointer and no
//! allocation.
//!
//! A [`Hyperlink`] is one *entity*: all the cells that belong to one OSC 8
//! open share one allocation. Opening a link without an `id` makes a new
//! entity every time; opening one with an `id` finds the live entity with
//! the same `id` and URI, so cells written in separate runs group (hover
//! underlines them together). The grid's pool ([`LinkPool`]) interns the
//! entities that have an `id` and counts every live entity and the bytes
//! of their URIs and ids: it stops creating them at [`MAX_LIVE_LINKS`] or
//! [`MAX_LINK_TABLE_BYTES`], so a program cannot make the terminal hold
//! more links than that however much it writes (the cells and scrollback
//! lines that hold links are bounded by their own caps, and an entity and
//! its index entry die with the last cell that holds it).
//!
//! [`Cell`]: crate::Cell
//! [`CellFlags::LINK`]: crate::CellFlags::LINK
//! [`GraphemeTable`]: crate::GraphemeTable
//! [`Line`]: crate::Line

use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

/// Most hyperlink entities alive at once: held by a cell, in scrollback or
/// as the active link. Past it [`Grid::begin_hyperlink`] refuses new ones.
///
/// The cap holds per grid and for clones written on one thread. Clones of
/// a grid share the count but check it before they add to it, so clones
/// written on several threads at once can each add one past the cap
/// before either sees the other's (#277: no compare-and-swap, since
/// nothing in the product writes to a clone).
///
/// [`Grid::begin_hyperlink`]: crate::Grid::begin_hyperlink
pub const MAX_LIVE_LINKS: usize = 65_536;

/// Longest URI accepted, in bytes (VTE and iTerm2: 2083). A longer one is
/// refused.
pub const MAX_URI_BYTES: usize = 2083;

/// Longest `id` kept, in bytes (VTE: 250). A longer one is dropped and the
/// link keeps no `id`.
pub const MAX_ID_BYTES: usize = 250;

/// Most bytes of URI and `id` text the live hyperlink entities of a grid
/// hold together (16 MiB). Past it [`Grid::begin_hyperlink`] refuses new
/// ones, as at [`MAX_LIVE_LINKS`]. With maximal URIs and ids this is the
/// cap that binds: about 7 190 links of 2 083 + 250 bytes. Clones written
/// on several threads can pass it as they can [`MAX_LIVE_LINKS`].
///
/// [`Grid::begin_hyperlink`]: crate::Grid::begin_hyperlink
pub const MAX_LINK_TABLE_BYTES: usize = 16 * 1024 * 1024;

/// Live entities below which a refusal is reported anew: three quarters of
/// [`MAX_LIVE_LINKS`].
const REARM_LINKS: usize = MAX_LIVE_LINKS / 4 * 3;

/// Table bytes below which a refusal is reported anew: three quarters of
/// [`MAX_LINK_TABLE_BYTES`].
const REARM_BYTES: usize = MAX_LINK_TABLE_BYTES / 4 * 3;

/// Why [`Grid::begin_hyperlink`] did not open a link. The active link is
/// closed in every case, so text that follows is not taken for part of the
/// link that was open before.
///
/// [`Grid::begin_hyperlink`]: crate::Grid::begin_hyperlink
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkRefused {
    /// The URI is empty: that closes a link.
    EmptyUri,
    /// The URI is longer than [`MAX_URI_BYTES`].
    UriTooLong,
    /// The URI holds a control character (C0, DEL or C1), a bidirectional
    /// formatting character (U+061C, U+200E, U+200F, U+202A to U+202E,
    /// U+2066 to U+2069) or a line or paragraph separator (U+2028, U+2029),
    /// the set the OSC title filter drops. Such a link is refused rather
    /// than cleaned: a URI that reorders, hides or breaks part of itself
    /// when shown is not one to open. The text written under it still
    /// prints.
    ControlInUri,
    /// [`MAX_LIVE_LINKS`] entities are alive, or the new one's URI and `id`
    /// would take the table past [`MAX_LINK_TABLE_BYTES`]. `first` is true
    /// for the first refusal since the table was last below three quarters
    /// of both caps, so the caller can log a refill at the cap once.
    TableFull {
        /// Whether this is the first refusal of this run of refusals.
        first: bool,
    },
}

/// A hyperlink: a URI and an optional `id`, shared by every cell that
/// belongs to it.
///
/// Cloning is cheap (a reference count). Two clones are the same *entity*
/// ([`Hyperlink::same_link`]); `==` compares the URI and `id` instead, so
/// two opens of the same URI without an `id` are equal but not the same
/// link.
#[derive(Clone)]
pub struct Hyperlink(Arc<LinkData>);

struct LinkData {
    uri: Arc<str>,
    id: Option<Arc<str>>,
    /// The pool's counts and index, updated when this entity dies.
    shared: Arc<Shared>,
}

/// The entities that have an `id`, by `(id, uri)`.
type IdIndex = HashMap<(Arc<str>, Arc<str>), Weak<LinkData>>;

/// What a [`LinkPool`] shares with its entities, so that the last holder of
/// an entity can take it off the books when it drops.
struct Shared {
    /// Live entities.
    live: AtomicUsize,
    /// Bytes of URI and `id` text of the live entities ([`text_bytes`]).
    bytes: AtomicUsize,
    /// The live entities that have an `id`, by `(id, uri)`. The strings are
    /// the entity's own (shared, not copied). An entity's entry goes when
    /// the entity dies, so the index holds no dead entries (beyond the
    /// moment between the last holder letting go and its drop finishing).
    by_id: Mutex<IdIndex>,
}

impl Shared {
    /// The `id` index. A panic while it was held cannot leave it
    /// inconsistent (every update is one map call), so poisoning is
    /// ignored.
    fn index(&self) -> MutexGuard<'_, IdIndex> {
        self.by_id.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The bytes a link's text counts against [`MAX_LINK_TABLE_BYTES`].
fn text_bytes(uri: &str, id: Option<&str>) -> usize {
    uri.len() + id.map_or(0, str::len)
}

impl Drop for LinkData {
    fn drop(&mut self) {
        let shared = &self.shared;
        shared.live.fetch_sub(1, Ordering::Relaxed);
        shared
            .bytes
            .fetch_sub(text_bytes(&self.uri, self.id.as_deref()), Ordering::Relaxed);
        if self.id.is_some() {
            unindex(self);
        }
    }
}

/// Removes the index entry of `link`, which is dying and has an `id`.
///
/// `extern "C"` so that it cannot unwind (a panic in it aborts; nothing in
/// it panics), and out of line so the drop calls a function declared not
/// to unwind: if a link's drop could unwind (the map and lock calls), every
/// loop that drops links (the reflow's split, erases) would carry unwind
/// cleanup for it, which measured 3 to 4 % more instructions on a CJK
/// reflow (ADR 0026).
#[inline(never)]
extern "C" fn unindex(link: &LinkData) {
    let Some(id) = &link.id else { return };
    let key = (Arc::clone(id), Arc::clone(&link.uri));
    // The entry is this entity's unless the grid already reopened the `id`
    // (this one died on another thread meanwhile, and its successor took
    // the entry over): leave a live successor alone. `get` then `remove`,
    // not `entry`: an entry for a missing key reserves room for it, and a
    // drop must not allocate (#277).
    let mut index = link.shared.index();
    if index.get(&key).is_some_and(|slot| slot.strong_count() == 0) {
        index.remove(&key);
    }
}

impl Hyperlink {
    /// The URI as the program sent it (valid UTF-8, at most
    /// [`MAX_URI_BYTES`] bytes, never empty).
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.0.uri
    }

    /// The explicit `id` parameter, when the program gave one (at most
    /// [`MAX_ID_BYTES`] bytes, never empty).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.0.id.as_deref()
    }

    /// True when `other` is this very link: both come from the same OSC 8
    /// open, or from opens with the same `id` and URI while one of them was
    /// alive. Cells whose links are the same entity underline together.
    #[must_use]
    pub fn same_link(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq for Hyperlink {
    fn eq(&self, other: &Self) -> bool {
        self.same_link(other) || (self.0.uri == other.0.uri && self.0.id == other.0.id)
    }
}

impl Eq for Hyperlink {}

impl fmt::Debug for Hyperlink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id() {
            Some(id) => write!(f, "{}#{id}", self.uri()),
            None => f.write_str(self.uri()),
        }
    }
}

/// The links of a grid: interns the ones with an `id` and counts the live
/// ones against [`MAX_LIVE_LINKS`] and their text against
/// [`MAX_LINK_TABLE_BYTES`].
///
/// A clone shares the counts and the index with the original (the links of
/// a cloned grid are the original's entities, so they count once): a clone
/// of a grid at a cap is at the cap too, and an `id` opened in one finds the
/// live entity of the other. [`LinkPool::create`] checks the caps and then
/// adds to the counts, so clones creating links on several threads at once
/// can each pass a cap by one (see [`MAX_LIVE_LINKS`]).
#[derive(Clone)]
pub(crate) struct LinkPool {
    shared: Arc<Shared>,
    /// Whether a refusal was reported since the table was last below three
    /// quarters of both caps.
    refusal_reported: bool,
}

/// Opaque in `Debug`, like the other bookkeeping of a grid: a reset grid and
/// a fresh one hold the same screen and must print the same.
impl fmt::Debug for LinkPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkPool")
    }
}

impl LinkPool {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                live: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
                by_id: Mutex::new(HashMap::new()),
            }),
            refusal_reported: false,
        }
    }

    /// Hyperlink entities alive now.
    pub(crate) fn live(&self) -> usize {
        self.shared.live.load(Ordering::Relaxed)
    }

    /// Bytes of URI and `id` text the live entities hold, at most
    /// [`MAX_LINK_TABLE_BYTES`] (see [`MAX_LIVE_LINKS`] for clones on
    /// several threads).
    pub(crate) fn bytes(&self) -> usize {
        self.shared.bytes.load(Ordering::Relaxed)
    }

    /// Entries of the `id` index (tests check that dead entities leave it).
    #[cfg(test)]
    pub(crate) fn indexed(&self) -> usize {
        self.shared.index().len()
    }

    /// The entity for `uri` and `id`: the live one with the same `id` and
    /// URI, else a new one. An `id` that is empty or longer than
    /// [`MAX_ID_BYTES`] is dropped; a link without an `id` is always new.
    pub(crate) fn open(&mut self, uri: &str, id: Option<&str>) -> Result<Hyperlink, LinkRefused> {
        if uri.is_empty() {
            return Err(LinkRefused::EmptyUri);
        }
        if uri.len() > MAX_URI_BYTES {
            return Err(LinkRefused::UriTooLong);
        }
        if has_control(uri) {
            return Err(LinkRefused::ControlInUri);
        }
        let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES) else {
            return self.create(Arc::from(uri), None);
        };
        let key: (Arc<str>, Arc<str>) = (Arc::from(id), Arc::from(uri));
        let shared = Arc::clone(&self.shared);
        // Nothing below drops an entity while the index is locked (an
        // entity's drop locks it).
        let mut by_id = shared.index();
        match by_id.entry(key) {
            Slot::Occupied(mut slot) => {
                if let Some(link) = slot.get().upgrade() {
                    return Ok(Hyperlink(link));
                }
                // A dead entry whose drop has not finished: the new entity
                // takes over its strings, so the index holds one copy.
                let (id, uri) = slot.key();
                let link = self.create(Arc::clone(uri), Some(Arc::clone(id)))?;
                slot.insert(Arc::downgrade(&link.0));
                Ok(link)
            }
            Slot::Vacant(slot) => {
                let (id, uri) = slot.key();
                let link = self.create(Arc::clone(uri), Some(Arc::clone(id)))?;
                slot.insert(Arc::downgrade(&link.0));
                Ok(link)
            }
        }
    }

    fn create(&mut self, uri: Arc<str>, id: Option<Arc<str>>) -> Result<Hyperlink, LinkRefused> {
        let size = text_bytes(&uri, id.as_deref());
        let (live, bytes) = (self.live(), self.bytes());
        if live >= MAX_LIVE_LINKS || bytes + size > MAX_LINK_TABLE_BYTES {
            let first = !self.refusal_reported;
            self.refusal_reported = true;
            return Err(LinkRefused::TableFull { first });
        }
        // A program that frees a few links and refills the table is not
        // reported on every refill: only once the table has drained to
        // three quarters of both caps.
        if live < REARM_LINKS && bytes < REARM_BYTES {
            self.refusal_reported = false;
        }
        self.shared.live.fetch_add(1, Ordering::Relaxed);
        self.shared.bytes.fetch_add(size, Ordering::Relaxed);
        Ok(Hyperlink(Arc::new(LinkData {
            uri,
            id,
            shared: Arc::clone(&self.shared),
        })))
    }
}

/// Whether `uri` holds a character [`LinkRefused::ControlInUri`] names.
fn has_control(uri: &str) -> bool {
    // Printable ASCII, the common case, needs no decoding.
    uri.bytes().any(|b| !(0x20..0x7f).contains(&b))
        && uri.chars().any(|ch| {
            ch.is_control()
                || matches!(
                    ch,
                    '\u{061C}'
                        | '\u{200E}'
                        | '\u{200F}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
}

/// `v` as a column. Every caller passes a column or length already known
/// to fit (inside a run, or checked against `u16::MAX` on entry).
fn col16(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// One run of linked cells: `len` columns from `col`, all of one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    col: u16,
    /// At least 1.
    len: u16,
    link: Hyperlink,
}

impl Run {
    /// The first column.
    fn start(&self) -> usize {
        usize::from(self.col)
    }

    /// One past the last column.
    fn end(&self) -> usize {
        usize::from(self.col) + usize::from(self.len)
    }

    /// The columns of the run.
    fn cols(&self) -> Range<usize> {
        self.start()..self.end()
    }

    /// The columns and link, by value.
    pub(crate) fn into_parts(self) -> (Range<usize>, Hyperlink) {
        (self.cols(), self.link)
    }
}

/// The hyperlinks of a line's [`CellFlags::LINK`] cells: the link half of
/// the line's [`GraphemeTable`], which keeps it consistent with the cells.
///
/// Run-length (#411): one [`Run`] per stretch of adjacent cells of one
/// entity, sorted by column, never overlapping, and never two adjacent runs
/// of the same entity (every edit merges them), so a line linked from end
/// to end by one link is one 16-byte entry whatever its width. A table
/// holds at most one run per column of its line (one per cell when every
/// cell has a different link, the old per-cell cost), so it stays bounded
/// by the line's width (S5). Lookups are a binary search over the runs.
///
/// [`CellFlags::LINK`]: crate::CellFlags::LINK
/// [`GraphemeTable`]: crate::GraphemeTable
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LinkColumns(Vec<Run>);

impl LinkColumns {
    /// Index of the run that holds `col`.
    fn find(&self, col: usize) -> Option<usize> {
        let i = self
            .0
            .partition_point(|r| r.start() <= col)
            .checked_sub(1)?;
        (col < self.0[i].end()).then_some(i)
    }

    /// The link at `col`.
    pub(crate) fn get(&self, col: usize) -> Option<&Hyperlink> {
        self.find(col).map(|i| &self.0[i].link)
    }

    /// True when no cell has a link.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of runs.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Runs the storage has room for.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// Every `(column, link)` pair, by ascending column: one per linked
    /// cell.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, &Hyperlink)> + '_ {
        self.0
            .iter()
            .flat_map(|r| r.cols().map(move |col| (col, &r.link)))
    }

    /// Every run as its columns and link, by ascending column.
    pub(crate) fn runs(&self) -> impl Iterator<Item = (Range<usize>, &Hyperlink)> + '_ {
        self.0.iter().map(|r| (r.cols(), &r.link))
    }

    /// Stores `link` as the link at `col`, replacing any previous one.
    #[inline]
    pub(crate) fn set(&mut self, col: usize, link: &Hyperlink) {
        self.set_range(col..col.saturating_add(1), link);
    }

    /// Stores `link` as the link of every column in `range`, replacing the
    /// previous ones. A range reaching past column `u16::MAX` is ignored
    /// (no line is that wide).
    #[inline]
    pub(crate) fn set_range(&mut self, range: Range<usize>, link: &Hyperlink) {
        if range.is_empty() || range.end > usize::from(u16::MAX) {
            return;
        }
        // Text is written left to right: the cell after the last run, or
        // further right, is the common case.
        match self.0.last_mut() {
            Some(last) if range.start >= last.end() => {
                if range.start == last.end() && last.link.same_link(link) {
                    last.len = col16(range.end - last.start());
                } else {
                    self.0.push(Run {
                        col: col16(range.start),
                        len: col16(range.len()),
                        link: link.clone(),
                    });
                }
            }
            Some(_) => self.set_inside(range, link),
            None => self.0.push(Run {
                col: col16(range.start),
                len: col16(range.len()),
                link: link.clone(),
            }),
        }
    }

    /// [`LinkColumns::set_range`] for a range that does not start after
    /// the last run. Out of line: the append stays small.
    #[inline(never)]
    fn set_inside(&mut self, range: Range<usize>, link: &Hyperlink) {
        let (start, end) = (range.start, range.end);
        if let Some(i) = self.find(start) {
            let run = &self.0[i];
            if run.end() >= end && run.link.same_link(link) {
                // Already this link (text rewritten in place).
                return;
            }
        }
        self.remove_range(range);
        let i = self.0.partition_point(|r| r.start() < start);
        let joins_prev = i > 0 && {
            let prev = &self.0[i - 1];
            prev.end() == start && prev.link.same_link(link)
        };
        let joins_next = self
            .0
            .get(i)
            .is_some_and(|next| next.start() == end && next.link.same_link(link));
        match (joins_prev, joins_next) {
            (true, true) => {
                let next_end = self.0[i].end();
                let prev = &mut self.0[i - 1];
                prev.len = col16(next_end - prev.start());
                self.0.remove(i);
            }
            (true, false) => {
                let prev = &mut self.0[i - 1];
                prev.len = col16(end - prev.start());
            }
            (false, true) => {
                let next = &mut self.0[i];
                next.len = col16(next.end() - start);
                next.col = col16(start);
            }
            (false, false) => self.0.insert(
                i,
                Run {
                    col: col16(start),
                    len: col16(end - start),
                    link: link.clone(),
                },
            ),
        }
    }

    /// Drops the link at `col`, if any.
    #[inline]
    pub(crate) fn remove(&mut self, col: usize) {
        // Writing past the linked text, the common case, finds nothing.
        if self.0.last().is_none_or(|last| col >= last.end()) {
            return;
        }
        self.remove_range(col..col + 1);
    }

    /// Drops the link of every column in `range`, splitting a run that
    /// `range` cuts.
    pub(crate) fn remove_range(&mut self, range: Range<usize>) {
        if self.0.is_empty() || range.is_empty() {
            return;
        }
        let (start, end) = (range.start, range.end);
        // The runs `i..j` overlap `range`.
        let i = self.0.partition_point(|r| r.end() <= start);
        let j = i + self.0[i..].partition_point(|r| r.start() < end);
        if i == j {
            return;
        }
        let head = self.0[i].start() < start;
        let tail = self.0[j - 1].end() > end;
        if head && tail && i + 1 == j {
            // `range` lies inside one run: it splits in two.
            let run = &mut self.0[i];
            let rest = Run {
                col: col16(end),
                len: col16(run.end() - end),
                link: run.link.clone(),
            };
            run.len = col16(start - run.start());
            self.0.insert(i + 1, rest);
            return;
        }
        let (mut lo, mut hi) = (i, j);
        if head {
            let run = &mut self.0[i];
            run.len = col16(start - run.start());
            lo += 1;
        }
        if tail {
            let run = &mut self.0[j - 1];
            run.len = col16(run.end() - end);
            run.col = col16(end);
            hi -= 1;
        }
        self.0.drain(lo..hi);
    }

    /// Ends a run at `col`: a run that holds `col` past its first column
    /// is split in two there.
    fn split_at(&mut self, col: usize) {
        let Some(i) = self.find(col) else {
            return;
        };
        let run = &mut self.0[i];
        if run.start() == col {
            return;
        }
        let rest = Run {
            col: col16(col),
            len: col16(run.end() - col),
            link: run.link.clone(),
        };
        run.len = col16(col - run.start());
        self.0.insert(i + 1, rest);
    }

    /// Joins every two adjacent runs of one entity.
    fn merge_adjacent(&mut self) {
        self.0.dedup_by(|next, prev| {
            let joins = prev.end() == next.start() && prev.link.same_link(&next.link);
            if joins {
                prev.len = col16(next.end() - prev.start());
            }
            joins
        });
    }

    /// Keeps the links whose column `keep` accepts (asked once per linked
    /// column, by ascending column).
    pub(crate) fn retain_cols(&mut self, keep: &mut impl FnMut(usize) -> bool) {
        // Nothing moves until the first column `keep` refuses.
        let mut first = None;
        'runs: for (i, run) in self.0.iter().enumerate() {
            for col in run.cols() {
                if !keep(col) {
                    first = Some((i, col));
                    break 'runs;
                }
            }
        }
        let Some((i, refused)) = first else {
            return;
        };
        let rest = self.0.split_off(i);
        // Pushes the pieces of `run` from column `from` on that `keep`
        // accepts.
        let mut kept = |runs: &mut Vec<Run>, run: &Run, from: usize| {
            let mut piece = None;
            for col in from..run.end() {
                match (keep(col), piece) {
                    (true, None) => piece = Some(col),
                    (false, Some(start)) => {
                        runs.push(Run {
                            col: col16(start),
                            len: col16(col - start),
                            link: run.link.clone(),
                        });
                        piece = None;
                    }
                    _ => {}
                }
            }
            if let Some(start) = piece {
                runs.push(Run {
                    col: col16(start),
                    len: col16(run.end() - start),
                    link: run.link.clone(),
                });
            }
        };
        let mut rest = rest.iter();
        if let Some(run) = rest.next() {
            if refused > run.start() {
                self.0.push(Run {
                    col: run.col,
                    len: col16(refused - run.start()),
                    link: run.link.clone(),
                });
            }
            kept(&mut self.0, run, refused + 1);
        }
        for run in rest {
            kept(&mut self.0, run, run.start());
        }
    }

    /// Moves the links at columns `from..end` right by `n` (insertion of
    /// `n` cells at `from` that shifts cells up to `end`), dropping those
    /// that land at or past `end`. Links at `end` and beyond stay.
    pub(crate) fn shift_right(&mut self, from: usize, n: usize, end: usize) {
        if self.0.is_empty() || n == 0 || from >= end {
            return;
        }
        // Every run then lies wholly before `from`, inside `from..end` or
        // from `end` on.
        self.split_at(from);
        self.split_at(end);
        self.0.retain_mut(|r| {
            let start = r.start();
            if start < from || start >= end {
                return true;
            }
            let moved = start + n;
            if moved >= end {
                return false;
            }
            // Cut at `end`: `moved < end`, so some of it is left.
            r.len = col16((r.end() + n).min(end) - moved);
            r.col = col16(moved);
            true
        });
        self.merge_adjacent();
    }

    /// Removes the links at columns `at..at + n` and moves the ones after
    /// them, up to `end`, left by `n` (deletion of `n` cells at `at` that
    /// shifts cells up to `end`). Links at `end` and beyond stay.
    pub(crate) fn delete(&mut self, at: usize, n: usize, end: usize) {
        if self.0.is_empty() || n == 0 || at >= end {
            return;
        }
        let cut = at.saturating_add(n).min(end);
        self.split_at(at);
        self.split_at(cut);
        self.split_at(end);
        self.0.retain_mut(|r| {
            let start = r.start();
            if start < at || start >= end {
                true
            } else if start < cut {
                false
            } else {
                // `start - n >= at`: `start >= cut = at + n` here.
                r.col = col16(start - n);
                true
            }
        });
        self.merge_adjacent();
    }

    /// Removes every run and hands them out, by ascending column.
    pub(crate) fn take(&mut self) -> Vec<Run> {
        std::mem::take(&mut self.0)
    }

    /// Shrinks the storage to the runs.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }

    /// What breaks the run invariants (sorted, non-empty, not
    /// overlapping, no two adjacent runs of one entity), for
    /// [`Grid::check_invariants`].
    ///
    /// [`Grid::check_invariants`]: crate::Grid::check_invariants
    pub(crate) fn run_error(&self) -> Option<String> {
        if let Some(run) = self.0.iter().find(|r| r.len == 0) {
            return Some(format!("empty link run at column {}", run.col));
        }
        self.0.windows(2).find_map(|pair| {
            let [a, b] = pair else { return None };
            if a.end() > b.start() {
                Some(format!(
                    "link runs {:?} and {:?} overlap",
                    a.cols(),
                    b.cols()
                ))
            } else if a.end() == b.start() && a.link.same_link(&b.link) {
                Some(format!(
                    "link runs {:?} and {:?} not merged",
                    a.cols(),
                    b.cols()
                ))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(pool: &mut LinkPool, uri: &str) -> Hyperlink {
        pool.open(uri, None).expect("fits")
    }

    fn table(pool: &mut LinkPool, cols: &[usize]) -> LinkColumns {
        let mut t = LinkColumns::default();
        for &col in cols {
            t.set(col, &open(pool, &format!("https://a.bc/{col}")));
        }
        t
    }

    fn cols(t: &LinkColumns) -> Vec<usize> {
        t.iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn ids_intern_and_plain_links_do_not() {
        let mut pool = LinkPool::new();
        let a = pool.open("https://a.bc/", Some("k")).unwrap();
        let b = pool.open("https://a.bc/", Some("k")).unwrap();
        assert!(a.same_link(&b));
        assert_eq!(pool.live(), 1);
        let other_uri = pool.open("https://a.bc/x", Some("k")).unwrap();
        let other_id = pool.open("https://a.bc/", Some("K")).unwrap();
        assert!(!a.same_link(&other_uri) && !a.same_link(&other_id));
        let p = open(&mut pool, "https://a.bc/");
        let q = open(&mut pool, "https://a.bc/");
        assert!(!p.same_link(&q), "no id: one entity per open");
        assert_eq!(p, q, "but equal by content");
        assert_ne!(a, p, "an id is part of the content");
        assert_eq!(pool.live(), 5);
        drop((a, b));
        let again = pool.open("https://a.bc/", Some("k")).unwrap();
        assert_eq!(again.id(), Some("k"));
        assert_eq!(pool.live(), 5, "the dead entity was replaced, not added");
    }

    #[test]
    fn caps_on_uri_and_id() {
        let mut pool = LinkPool::new();
        assert_eq!(pool.open("", None).unwrap_err(), LinkRefused::EmptyUri);
        let at_cap = "u".repeat(MAX_URI_BYTES);
        assert!(pool.open(&at_cap, None).is_ok());
        let over = "u".repeat(MAX_URI_BYTES + 1);
        assert_eq!(pool.open(&over, None).unwrap_err(), LinkRefused::UriTooLong);
        let id = "i".repeat(MAX_ID_BYTES);
        let kept = pool.open("https://a.bc/", Some(&id)).unwrap();
        assert_eq!(kept.id(), Some(id.as_str()));
        let id = "i".repeat(MAX_ID_BYTES + 1);
        let dropped = pool.open("https://a.bc/", Some(&id)).unwrap();
        assert_eq!(dropped.id(), None, "the id goes, the link stays");
        let empty = pool.open("https://a.bc/", Some("")).unwrap();
        assert_eq!(empty.id(), None);
    }

    #[test]
    fn the_live_count_stops_new_links_and_reports_the_first_refusal_once() {
        let mut pool = LinkPool::new();
        let held: Vec<Hyperlink> = (0..MAX_LIVE_LINKS)
            .map(|i| open(&mut pool, &format!("https://a.bc/{i}")))
            .collect();
        assert_eq!(pool.live(), MAX_LIVE_LINKS);
        assert_eq!(
            pool.open("https://a.bc/x", None).unwrap_err(),
            LinkRefused::TableFull { first: true }
        );
        for _ in 0..3 {
            assert_eq!(
                pool.open("https://a.bc/y", Some("k")).unwrap_err(),
                LinkRefused::TableFull { first: false }
            );
        }
        // A few links let go and the table refills: still above three
        // quarters of the cap, so the refusals are not reported again.
        let mut held = held;
        for round in 0..100 {
            held.truncate(MAX_LIVE_LINKS - 1 - round % 7);
            while held.len() < MAX_LIVE_LINKS {
                held.push(open(&mut pool, "https://a.bc/refill"));
            }
            assert_eq!(
                pool.open("https://a.bc/x", None).unwrap_err(),
                LinkRefused::TableFull { first: false },
                "round {round}"
            );
        }
        // Down to just above three quarters: still the same run.
        held.truncate(REARM_LINKS);
        held.push(open(&mut pool, "https://a.bc/refill"));
        while held.len() < MAX_LIVE_LINKS {
            held.push(open(&mut pool, "https://a.bc/refill"));
        }
        assert_eq!(
            pool.open("https://a.bc/x", None).unwrap_err(),
            LinkRefused::TableFull { first: false }
        );
        drop(held);
        let id = pool.open("https://a.bc/", Some("k")).unwrap();
        assert!(
            pool.open("https://a.bc/", Some("k"))
                .unwrap()
                .same_link(&id)
        );
        // Room again: the next refusal is reported anew.
        assert_eq!(pool.live(), 1);
        let held: Vec<Hyperlink> = (0..MAX_LIVE_LINKS - 1)
            .map(|i| open(&mut pool, &format!("https://a.bc/{i}")))
            .collect();
        assert_eq!(
            pool.open("https://a.bc/x", None).unwrap_err(),
            LinkRefused::TableFull { first: true }
        );
        drop(held);
    }

    /// Holds links of `kept`, refills the table with `filler` links until
    /// a refusal, lets go of all but `kept` and refills again: whether the
    /// second run's first refusal is reported anew.
    fn reported_anew_after_draining_to(kept: &[String], filler: &str) -> bool {
        let mut pool = LinkPool::new();
        let mut held: Vec<Hyperlink> = kept.iter().map(|uri| open(&mut pool, uri)).collect();
        let mut refill = |held: &mut Vec<Hyperlink>| loop {
            match pool.open(filler, None) {
                Ok(link) => held.push(link),
                Err(LinkRefused::TableFull { first }) => return first,
                Err(other) => panic!("{other:?}"),
            }
        };
        assert!(refill(&mut held), "the first refusal is reported");
        held.truncate(kept.len());
        refill(&mut held)
    }

    #[test]
    fn a_refusal_is_reported_anew_once_the_table_drops_below_three_quarters() {
        // The re-arm points as literals: 49 152 links (3/4 of 65 536) and
        // 12 MiB (3/4 of 16 MiB). A create re-arms the report only if the
        // table is below both before it.
        let short = |n: usize| vec!["https://a.bc/".to_owned(); n];
        assert!(reported_anew_after_draining_to(
            &short(49_151),
            "https://a.bc/"
        ));
        assert!(!reported_anew_after_draining_to(
            &short(49_152),
            "https://a.bc/"
        ));
        let page = "u".repeat(2048);
        let mut kept = vec![page.clone(); 6143];
        kept.push("u".repeat(2047));
        assert_eq!(
            kept.iter().map(String::len).sum::<usize>(),
            12 * 1024 * 1024 - 1
        );
        assert!(reported_anew_after_draining_to(&kept, &page));
        let kept = vec![page.clone(); 6144];
        assert_eq!(
            kept.iter().map(String::len).sum::<usize>(),
            12 * 1024 * 1024
        );
        assert!(!reported_anew_after_draining_to(&kept, &page));
    }

    #[test]
    fn a_link_dying_on_another_thread_leaves_its_reopened_successor_indexed() {
        let (uri, id) = ("https://a.bc/", "k");
        let key: (Arc<str>, Arc<str>) = (Arc::from(id), Arc::from(uri));
        let mut pool = LinkPool::new();
        let shared = Arc::clone(&pool.shared);
        let (to_worker, links) = std::sync::mpsc::channel::<Hyperlink>();
        // Sends `link` to the worker to drop and waits, holding the index,
        // until it is dead and its drop stands at the index lock. Owns the
        // sender: dropping it ends the worker.
        let (by_key, shared) = (&key, &shared);
        let hold_until_dead = move |link: Hyperlink| {
            let by_id = shared.index();
            to_worker.send(link).unwrap();
            while by_id.get(by_key).expect("indexed").strong_count() > 0 {
                std::hint::spin_loop();
            }
            by_id
        };
        let (done, dropped) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                for link in links {
                    drop(link);
                    done.send(()).unwrap();
                }
            });
            // Sure: the successor takes the dead entry over while the
            // dying link waits for the lock, as `open` does.
            let link = pool.open(uri, Some(id)).unwrap();
            let mut by_id = hold_until_dead(link);
            let successor = pool
                .create(Arc::clone(&key.1), Some(Arc::clone(&key.0)))
                .unwrap();
            by_id.insert(key.clone(), Arc::downgrade(&successor.0));
            drop(by_id);
            dropped.recv().unwrap();
            assert_eq!(pool.indexed(), 1, "the successor kept its entry");
            assert!(pool.open(uri, Some(id)).unwrap().same_link(&successor));
            drop(successor);
            // Through `open`: it usually takes the lock before the waiting
            // drop does and reaches its dead-entry branch.
            for i in 0..2000 {
                let link = pool.open(uri, Some(id)).unwrap();
                drop(hold_until_dead(link));
                let successor = pool.open(uri, Some(id)).unwrap();
                dropped.recv().unwrap();
                assert_eq!(pool.indexed(), 1, "at {i}");
                assert!(
                    pool.open(uri, Some(id)).unwrap().same_link(&successor),
                    "at {i}"
                );
            }
            drop(hold_until_dead);
        });
        assert_eq!((pool.live(), pool.indexed()), (0, 0));
    }

    #[test]
    fn id_index_loses_each_entry_with_its_last_holder() {
        let mut pool = LinkPool::new();
        for i in 0..1_000_000 {
            let link = pool.open(&format!("https://a.bc/{i}"), Some("k")).unwrap();
            assert_eq!(pool.indexed(), 1, "at {i}");
            drop(link);
            assert_eq!(pool.indexed(), 0, "at {i}");
        }
        assert_eq!(pool.live(), 0);
        assert_eq!(pool.bytes(), 0);
    }

    /// A URI of `len` bytes, distinct per `i`.
    fn long_uri(i: usize, len: usize) -> String {
        let head = format!("https://a.bc/{i}/");
        format!("{head}{}", "u".repeat(len - head.len()))
    }

    #[test]
    fn the_byte_cap_stops_maximal_links_long_before_the_count_cap() {
        let mut pool = LinkPool::new();
        let id = "i".repeat(MAX_ID_BYTES);
        let size = MAX_URI_BYTES + MAX_ID_BYTES;
        let mut held = Vec::new();
        let mut refused = 0;
        for i in 0..MAX_LIVE_LINKS {
            match pool.open(&long_uri(i, MAX_URI_BYTES), Some(&id)) {
                Ok(link) => held.push(link),
                Err(LinkRefused::TableFull { first }) => {
                    assert_eq!(first, refused == 0);
                    refused += 1;
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(pool.bytes() <= MAX_LINK_TABLE_BYTES);
        }
        assert_eq!(held.len(), MAX_LINK_TABLE_BYTES / size);
        assert_eq!(refused, MAX_LIVE_LINKS - held.len());
        assert_eq!(pool.bytes(), held.len() * size);
        assert_eq!(pool.indexed(), held.len());
        // A short link still fits in what is left; then nothing does.
        let left = MAX_LINK_TABLE_BYTES - pool.bytes();
        let fits = pool.open(&"u".repeat(left), None).unwrap();
        assert_eq!(pool.bytes(), MAX_LINK_TABLE_BYTES);
        assert!(matches!(
            pool.open("u", None),
            Err(LinkRefused::TableFull { first: false })
        ));
        drop((held, fits));
        assert_eq!((pool.live(), pool.bytes(), pool.indexed()), (0, 0, 0));
    }

    #[test]
    fn churning_maximal_id_links_at_the_byte_cap_keeps_the_table_within_it() {
        let mut pool = LinkPool::new();
        let id = |i: usize| format!("{i:0>MAX_ID_BYTES$}");
        let size = MAX_URI_BYTES + MAX_ID_BYTES;
        // The table full but for one link, which the churn replaces.
        let held: Vec<Hyperlink> = (0..MAX_LINK_TABLE_BYTES / size - 1)
            .map(|i| {
                pool.open(&long_uri(i, MAX_URI_BYTES), Some(&id(i)))
                    .unwrap()
            })
            .collect();
        let mut cell = None;
        for i in held.len()..held.len() + 20_000 {
            // The cell's old link dies as a new one is written over it.
            drop(cell.take());
            let link = pool.open(&long_uri(i, MAX_URI_BYTES), Some(&id(i)));
            cell = Some(link.expect("the dead link's bytes were given back"));
            assert!(pool.bytes() <= MAX_LINK_TABLE_BYTES);
            assert_eq!(pool.indexed(), held.len() + 1);
        }
        drop((held, cell));
        assert_eq!((pool.live(), pool.bytes(), pool.indexed()), (0, 0, 0));
    }

    #[test]
    fn a_reopened_id_counts_its_bytes_once() {
        let mut pool = LinkPool::new();
        let a = pool.open("https://a.bc/", Some("k")).unwrap();
        let b = pool.open("https://a.bc/", Some("k")).unwrap();
        assert!(a.same_link(&b));
        assert_eq!(pool.bytes(), "https://a.bc/".len() + 1);
        let c = pool.open("https://a.bc/", None).unwrap();
        assert_eq!(pool.bytes(), 2 * "https://a.bc/".len() + 1);
        drop((a, c));
        assert_eq!(pool.bytes(), "https://a.bc/".len() + 1, "b holds it");
        drop(b);
        assert_eq!(pool.bytes(), 0);
    }

    #[test]
    fn controls_and_bidi_formatting_in_a_uri_refuse_the_link() {
        let mut pool = LinkPool::new();
        for ch in [
            '\0', '\u{1b}', '\u{7f}', '\u{80}', '\u{85}', '\u{9b}', '\u{9f}', '\u{61c}',
            '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202a}', '\u{202e}', '\u{2066}',
            '\u{2069}',
        ] {
            assert_eq!(
                pool.open(&format!("https://a.bc/{ch}x"), None).unwrap_err(),
                LinkRefused::ControlInUri,
                "{ch:?}"
            );
        }
        for uri in [
            "https://a.bc/~x",
            "https://a.bc/\u{a0}\u{e9}\u{200d}\u{2065}\u{206a}\u{202f}\u{4e16}",
            "file:///tmp/a%7F%C2%9B",
        ] {
            assert!(pool.open(uri, None).is_ok(), "{uri:?}");
        }
        assert_eq!(pool.live(), 0);
    }

    #[test]
    fn a_dying_link_whose_entry_is_gone_does_not_grow_the_index() {
        // The entry of a dying entity can be missing: its successor took it
        // over and died first (two threads). `entry` would reserve a slot
        // for the missing key, which allocates when the map is full (#277).
        let mut pool = LinkPool::new();
        let dying = pool.open("https://a.bc/", Some("dying")).unwrap();
        pool.shared.index().clear();
        let mut held = Vec::new();
        let mut i = 0;
        while held.is_empty() || pool.indexed() < pool.shared.index().capacity() {
            held.push(pool.open("https://a.bc/", Some(&format!("k{i}"))).unwrap());
            i += 1;
        }
        let capacity = pool.shared.index().capacity();
        drop(dying);
        assert_eq!(pool.shared.index().capacity(), capacity);
        assert_eq!(pool.indexed(), held.len());
    }

    #[test]
    fn a_clone_shares_the_counts_and_the_index() {
        let mut pool = LinkPool::new();
        let a = pool.open("https://a.bc/", Some("k")).unwrap();
        let mut clone = pool.clone();
        let b = clone.open("https://a.bc/", Some("k")).unwrap();
        assert!(a.same_link(&b));
        let _c = clone.open("https://a.bc/x", None).unwrap();
        assert_eq!((pool.live(), clone.live()), (2, 2));
    }

    #[test]
    fn set_get_remove_keep_order() {
        let mut pool = LinkPool::new();
        let mut t = table(&mut pool, &[5, 1]);
        assert_eq!(cols(&t), [1, 5]);
        assert_eq!(t.get(5).map(Hyperlink::uri), Some("https://a.bc/5"));
        assert_eq!(t.get(2), None);
        assert_eq!(t.get(usize::MAX), None);
        t.set(5, &open(&mut pool, "https://a.bc/new"));
        assert_eq!(t.get(5).map(Hyperlink::uri), Some("https://a.bc/new"));
        assert_eq!(t.len(), 2);
        t.remove(1);
        t.remove(9);
        assert_eq!(cols(&t), [5]);
        t.remove(5);
        assert!(t.is_empty());
        assert_eq!(pool.live(), 0, "removing the last cell frees the link");
        let mut t = table(&mut pool, &(0..256).collect::<Vec<_>>());
        t.remove_range(1..256);
        t.shrink_to_fit();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn shifts_follow_insert_and_delete() {
        let mut pool = LinkPool::new();
        let mut t = table(&mut pool, &[0, 2, 6]);
        t.shift_right(1, 3, 8);
        assert_eq!(cols(&t), [0, 5], "column 6 + 3 fell off an 8-column line");
        let mut t = table(&mut pool, &[0, 2, 4]);
        t.delete(1, 2, 8);
        assert_eq!(cols(&t), [0, 2]);
        assert_eq!(t.get(2).map(Hyperlink::uri), Some("https://a.bc/4"));
        let mut t = table(&mut pool, &[1, 3, 6]);
        t.shift_right(1, 2, 5);
        assert_eq!(cols(&t), [3, 6], "column 3 + 2 fell off at end 5");
        let mut t = table(&mut pool, &[1, 3, 6]);
        t.delete(1, 1, 5);
        assert_eq!(cols(&t), [2, 6]);
        t.remove_range(0..10);
        assert!(t.is_empty());
    }

    #[test]
    fn take_empties_the_table_in_column_order() {
        let mut pool = LinkPool::new();
        let mut t = table(&mut pool, &[4, 1]);
        let taken: Vec<_> = t.take().into_iter().map(|r| r.into_parts().0).collect();
        assert_eq!(taken, [1..2, 4..5]);
        assert!(t.is_empty());
        assert_eq!(t.take().len(), 0);
    }

    #[test]
    fn run_is_16_bytes() {
        assert_eq!(size_of::<Run>(), 16);
    }

    #[test]
    fn one_link_over_a_whole_line_is_one_run_and_edits_split_and_merge_it() {
        let mut pool = LinkPool::new();
        let a = open(&mut pool, "https://a.bc/a");
        let b = open(&mut pool, "https://a.bc/b");
        let mut t = LinkColumns::default();
        for col in 0..2048 {
            t.set(col, &a);
        }
        assert_eq!(t.len(), 1);
        assert_eq!(
            t.runs().map(|(r, _)| (r.start, r.end)).collect::<Vec<_>>(),
            [(0, 2048)]
        );
        t.set(1000, &b);
        assert_eq!(t.len(), 3, "a cell of another link splits the run");
        t.set(1000, &a);
        assert_eq!(t.len(), 1, "and writing the first link back merges it");
        t.remove(5);
        assert_eq!(t.len(), 2);
        t.set(5, &a);
        assert_eq!(t.len(), 1);
        t.delete(10, 3, 2048);
        assert_eq!(
            t.runs().map(|(r, _)| (r.start, r.end)).collect::<Vec<_>>(),
            [(0, 2045)]
        );
        t.shift_right(100, 7, 2048);
        assert_eq!(
            t.runs().map(|(r, _)| r).collect::<Vec<_>>(),
            [0..100, 107..2048]
        );
        t.set_range(100..107, &a);
        assert_eq!(t.len(), 1, "filling the gap merges both sides");
        t.retain_cols(&mut |col| col % 1024 != 7);
        assert_eq!(
            t.runs().map(|(r, _)| r).collect::<Vec<_>>(),
            [0..7, 8..1031, 1032..2048]
        );
        assert_eq!(t.run_error(), None);
        drop(t);
        drop((a, b));
        assert_eq!(pool.live(), 0, "the runs held the links, and let go");
    }

    /// The per-cell model of a [`LinkColumns`] of `WIDTH` columns: the
    /// index of each cell's link.
    const WIDTH: usize = 40;

    #[derive(Debug, Clone)]
    enum Op {
        Set(usize, usize),
        SetRange(usize, usize, usize),
        Remove(usize),
        RemoveRange(usize, usize),
        Retain(u64),
        ShiftRight(usize, usize, usize),
        Delete(usize, usize, usize),
        Take,
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        let col = 0..WIDTH + 2;
        let link = 0usize..3;
        prop_oneof![
            8 => (col.clone(), link.clone()).prop_map(|(c, l)| Op::Set(c, l)),
            3 => (col.clone(), 0usize..12, link).prop_map(|(c, n, l)| Op::SetRange(c, n, l)),
            3 => col.clone().prop_map(Op::Remove),
            2 => (col.clone(), 0usize..12).prop_map(|(c, n)| Op::RemoveRange(c, n)),
            1 => any::<u64>().prop_map(Op::Retain),
            2 => (col.clone(), 0usize..6, col.clone())
                .prop_map(|(c, n, e)| Op::ShiftRight(c, n, e)),
            2 => (col.clone(), 0usize..6, col).prop_map(|(c, n, e)| Op::Delete(c, n, e)),
            1 => Just(Op::Take),
        ]
    }

    /// Applies `op` to the model, the per-cell table the run table
    /// replaced (#411): `None` or the index of a cell's link.
    fn apply_model(model: &mut [Option<usize>], op: &Op) {
        let width = model.len();
        match *op {
            Op::Set(c, l) => {
                if c < width {
                    model[c] = Some(l);
                }
            }
            Op::SetRange(c, n, l) => {
                for cell in model.iter_mut().take(c + n).skip(c) {
                    *cell = Some(l);
                }
            }
            Op::Remove(c) => {
                if c < width {
                    model[c] = None;
                }
            }
            Op::RemoveRange(c, n) => {
                for cell in model.iter_mut().take(c + n).skip(c) {
                    *cell = None;
                }
            }
            Op::Retain(mask) => {
                for (c, cell) in model.iter_mut().enumerate() {
                    if mask >> (c % 64) & 1 == 0 {
                        *cell = None;
                    }
                }
            }
            Op::ShiftRight(from, n, end) => {
                let end = end.min(width);
                if from < end {
                    let old = model.to_vec();
                    for (c, cell) in model.iter_mut().enumerate().take(end).skip(from) {
                        *cell = c.checked_sub(n).filter(|&s| s >= from).and_then(|s| old[s]);
                    }
                }
            }
            Op::Delete(at, n, end) => {
                let end = end.min(width);
                if at < end {
                    let old = model.to_vec();
                    for (c, cell) in model.iter_mut().enumerate().take(end).skip(at) {
                        *cell = old.get(c + n).copied().flatten().filter(|_| c + n < end);
                    }
                }
            }
            Op::Take => model.fill(None),
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2000))]

        /// The run table agrees cell for cell with the per-cell table it
        /// replaced, through any chain of edits, and stays canonical: one
        /// run per maximal stretch of one link.
        #[test]
        fn runs_agree_with_the_per_cell_model(
            ops in proptest::collection::vec(op(), 1..60),
        ) {
            let mut pool = LinkPool::new();
            let links: Vec<Hyperlink> =
                (0..3).map(|i| open(&mut pool, &format!("https://a.bc/{i}"))).collect();
            let mut t = LinkColumns::default();
            let mut model = vec![None; WIDTH];
            for op in &ops {
                match *op {
                    Op::Set(c, l) => {
                        if c < WIDTH {
                            t.set(c, &links[l]);
                        }
                    }
                    Op::SetRange(c, n, l) => t.set_range(c..(c + n).min(WIDTH), &links[l]),
                    Op::Remove(c) => t.remove(c),
                    Op::RemoveRange(c, n) => t.remove_range(c..c + n),
                    Op::Retain(mask) => t.retain_cols(&mut |c| mask >> (c % 64) & 1 == 1),
                    Op::ShiftRight(from, n, end) => {
                        let end = end.min(WIDTH);
                        if from < end {
                            t.shift_right(from, n, end);
                        }
                    }
                    Op::Delete(at, n, end) => {
                        let end = end.min(WIDTH);
                        if at < end {
                            t.delete(at, n, end);
                        }
                    }
                    Op::Take => drop(t.take()),
                }
                apply_model(&mut model, op);
                let cells: Vec<Option<usize>> = (0..WIDTH)
                    .map(|c| t.get(c).map(|l| links.iter().position(|k| k.same_link(l)).unwrap()))
                    .collect();
                proptest::prop_assert_eq!(&cells, &model, "after {:?}", op);
                let per_cell: Vec<(usize, usize)> = t
                    .iter()
                    .map(|(c, l)| (c, links.iter().position(|k| k.same_link(l)).unwrap()))
                    .collect();
                let expected: Vec<(usize, usize)> = model
                    .iter()
                    .enumerate()
                    .filter_map(|(c, l)| l.map(|l| (c, l)))
                    .collect();
                proptest::prop_assert_eq!(per_cell, expected);
                proptest::prop_assert_eq!(t.run_error(), None, "after {:?}", op);
                let stretches = model
                    .iter()
                    .enumerate()
                    .filter(|&(c, l)| l.is_some() && (c == 0 || model[c - 1] != *l))
                    .count();
                proptest::prop_assert_eq!(t.len(), stretches, "after {:?}", op);
            }
        }
    }

    /// Runs and room in the link table of screen row `row`.
    fn storage(grid: &crate::Grid, row: u16) -> (usize, usize) {
        grid.line(row).expect("row").graphemes.link_storage()
    }

    #[test]
    fn a_fully_linked_line_holds_one_run_at_80_and_2048_columns() {
        for cols in [80u16, 2048] {
            let mut grid = crate::Grid::new(2, cols);
            grid.begin_hyperlink("https://a.bc/", None).unwrap();
            grid.put_str(&"x".repeat(usize::from(cols)));
            let line = grid.line(0).unwrap();
            assert!((0..usize::from(cols)).all(|c| line.hyperlink(c).is_some()));
            assert_eq!(storage(&grid, 0).0, 1, "{cols} columns");
            // Wide characters link both halves, still one run.
            grid.carriage_return();
            grid.line_feed();
            grid.put_str(&"\u{4e16}".repeat(usize::from(cols) / 2));
            assert_eq!(storage(&grid, 1).0, 1, "{cols} columns, wide");
            // In history, compacted: one entry of room, 16 bytes.
            grid.line_feed();
            grid.line_feed();
            let kept = grid.scrollback_line(grid.scrollback_len() - 1).unwrap();
            assert_eq!(kept.graphemes.link_storage(), (1, 1), "{cols} columns");
        }
    }

    #[test]
    fn retain_asks_each_linked_column_once_in_order() {
        let mut pool = LinkPool::new();
        let (a, b) = (
            open(&mut pool, "https://a.bc/"),
            open(&mut pool, "https://b.cd/"),
        );
        let mut t = LinkColumns::default();
        t.set_range(2..9, &a);
        t.set_range(9..12, &b);
        t.set(20, &a);
        let mut asked = Vec::new();
        t.retain_cols(&mut |col| {
            asked.push(col);
            col != 4 && col != 10
        });
        let expected: Vec<usize> = (2..12).chain([20]).collect();
        assert_eq!(asked, expected);
        assert_eq!(
            t.runs().map(|(r, _)| (r.start, r.end)).collect::<Vec<_>>(),
            [(2, 4), (5, 9), (9, 10), (11, 12), (20, 21)]
        );
    }

    #[test]
    fn a_flood_of_new_links_over_one_line_stays_capped_by_its_columns() {
        // S5: 200 000 cells, each under a link of its own, written at
        // scattered columns of one line with inserts and deletes mixed in.
        // The table never holds more than one run per column, and its room
        // never more than twice that.
        let cols = 80u16;
        let mut grid = crate::Grid::new(1, cols);
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut peak = (0, 0);
        for n in 0..200_000u32 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let col = u16::try_from(seed % u64::from(cols)).unwrap();
            grid.goto(0, col);
            match seed >> 60 {
                0 => grid.insert_chars(1 + u16::try_from(seed >> 56 & 3).unwrap()),
                1 => grid.delete_chars(1 + u16::try_from(seed >> 56 & 3).unwrap()),
                _ => {
                    let id = n.to_string();
                    grid.begin_hyperlink("https://a.bc/", Some(id.as_str()))
                        .unwrap();
                    // A wide character at the last column would wrap and scroll.
                    let wide = seed & 1 == 1 && col + 1 < cols;
                    grid.put_str(if wide { "\u{4e16}" } else { "x" });
                }
            }
            let (len, room) = storage(&grid, 0);
            peak = (peak.0.max(len), peak.1.max(room));
            assert!(len <= usize::from(cols), "{len} runs on {cols} columns");
            assert!(room <= 2 * usize::from(cols), "room for {room} runs");
        }
        grid.check_invariants();
        assert!(
            peak.0 > usize::from(cols) / 2,
            "the flood fills the line: {peak:?}"
        );
        // Links that no cell shows are gone, so the flood never fills the
        // live-link table either.
        assert_eq!(grid.scrollback_len(), 0);
        assert!(grid.live_hyperlinks() <= usize::from(cols) + 1);
    }

    #[test]
    fn alternating_links_hold_one_run_each() {
        for (cols, every) in [(80u16, 1usize), (80, 3), (2048, 1), (2048, 5)] {
            let mut grid = crate::Grid::new(2, cols);
            let mut written = 0;
            let mut k = 0;
            while written < usize::from(cols) {
                // Two `id`s alternate: each open of one finds the live
                // entity, so the stretches alternate between two links.
                grid.begin_hyperlink("https://a.bc/", Some(["a", "b"][k % 2]))
                    .unwrap();
                let n = every.min(usize::from(cols) - written);
                grid.put_str(&"x".repeat(n));
                written += n;
                k += 1;
            }
            assert_eq!(grid.live_hyperlinks(), 2);
            assert_eq!(
                storage(&grid, 0).0,
                usize::from(cols).div_ceil(every),
                "{cols} columns, a new link every {every}"
            );
            grid.check_invariants();
        }
    }
}
