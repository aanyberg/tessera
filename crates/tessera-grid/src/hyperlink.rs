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
//! entities that have an `id` and counts every live entity: it stops
//! creating them at [`MAX_LIVE_LINKS`], so a program cannot make the
//! terminal hold more links than that however much it writes (the cells and
//! scrollback lines that hold links are bounded by their own caps, and an
//! entity dies with the last cell that holds it).
//!
//! [`Cell`]: crate::Cell
//! [`CellFlags::LINK`]: crate::CellFlags::LINK
//! [`GraphemeTable`]: crate::GraphemeTable
//! [`Line`]: crate::Line

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

/// Most hyperlink entities alive at once: held by a cell, in scrollback or
/// as the active link. Past it [`Grid::begin_hyperlink`] refuses new ones.
///
/// [`Grid::begin_hyperlink`]: crate::Grid::begin_hyperlink
pub const MAX_LIVE_LINKS: usize = 65_536;

/// Longest URI accepted, in bytes (VTE and iTerm2: 2083). A longer one is
/// refused.
pub const MAX_URI_BYTES: usize = 2083;

/// Longest `id` kept, in bytes (VTE: 250). A longer one is dropped and the
/// link keeps no `id`.
pub const MAX_ID_BYTES: usize = 250;

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
    /// [`MAX_LIVE_LINKS`] entities are alive. `first` is true for the first
    /// refusal since a link last fitted, so the caller can log it once.
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
    /// The pool's count of live entities, decremented when this one dies.
    live: Arc<AtomicUsize>,
}

impl Drop for LinkData {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
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
/// ones against [`MAX_LIVE_LINKS`].
#[derive(Clone)]
pub(crate) struct LinkPool {
    /// Live and recently dead entities that have an `id`, by `(id, uri)`.
    /// Dead entries are swept when they outnumber the live ones (see
    /// [`LinkPool::sweep`]).
    by_id: HashMap<(Arc<str>, Arc<str>), Weak<LinkData>>,
    live: Arc<AtomicUsize>,
    /// Whether the refusal that came last was already reported.
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
            by_id: HashMap::new(),
            live: Arc::new(AtomicUsize::new(0)),
            refusal_reported: false,
        }
    }

    /// Hyperlink entities alive now.
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Entries of the `id` index, live or not yet swept (tests check the
    /// bound).
    #[cfg(test)]
    pub(crate) fn indexed(&self) -> usize {
        self.by_id.len()
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
        let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES) else {
            return self.create(Arc::from(uri), None);
        };
        let key: (Arc<str>, Arc<str>) = (Arc::from(id), Arc::from(uri));
        if let Some(link) = self.by_id.get(&key).and_then(Weak::upgrade) {
            return Ok(Hyperlink(link));
        }
        let link = self.create(key.1.clone(), Some(key.0.clone()))?;
        self.sweep();
        self.by_id.insert(key, Arc::downgrade(&link.0));
        Ok(link)
    }

    fn create(&mut self, uri: Arc<str>, id: Option<Arc<str>>) -> Result<Hyperlink, LinkRefused> {
        if self.live() >= MAX_LIVE_LINKS {
            let first = !self.refusal_reported;
            self.refusal_reported = true;
            return Err(LinkRefused::TableFull { first });
        }
        self.refusal_reported = false;
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(Hyperlink(Arc::new(LinkData {
            uri,
            id,
            live: Arc::clone(&self.live),
        })))
    }

    /// Drops the index entries of dead entities once they are at least as
    /// many as the live ones (and over a small floor): amortised O(1) per
    /// new entity, and the index never holds more than twice the live
    /// entities plus the floor.
    fn sweep(&mut self) {
        const FLOOR: usize = 1024;
        if self.by_id.len() >= FLOOR && self.by_id.len() >= 2 * self.live() {
            self.by_id.retain(|_, link| link.strong_count() > 0);
        }
    }
}

/// One link: its column and entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    col: u16,
    link: Hyperlink,
}

impl Entry {
    /// The column and link, by value.
    pub(crate) fn into_parts(self) -> (usize, Hyperlink) {
        (usize::from(self.col), self.link)
    }
}

/// The hyperlinks of a line's [`CellFlags::LINK`] cells, sorted by column:
/// the link half of the line's [`GraphemeTable`], which keeps it consistent
/// with the cells.
///
/// [`CellFlags::LINK`]: crate::CellFlags::LINK
/// [`GraphemeTable`]: crate::GraphemeTable
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LinkColumns(Vec<Entry>);

impl LinkColumns {
    /// The link at `col`.
    pub(crate) fn get(&self, col: usize) -> Option<&Hyperlink> {
        let col = u16::try_from(col).ok()?;
        let i = self.0.binary_search_by_key(&col, |e| e.col).ok()?;
        Some(&self.0[i].link)
    }

    /// True when no cell has a link.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of linked cells.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Every `(column, link)` pair, by ascending column.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, &Hyperlink)> + '_ {
        self.0.iter().map(|e| (usize::from(e.col), &e.link))
    }

    /// Stores `link` as the link at `col`, replacing any previous one.
    pub(crate) fn set(&mut self, col: usize, link: Hyperlink) {
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        match self.0.binary_search_by_key(&col, |e| e.col) {
            Ok(i) => self.0[i].link = link,
            Err(i) => self.0.insert(i, Entry { col, link }),
        }
    }

    /// Drops the link at `col`, if any.
    #[inline]
    pub(crate) fn remove(&mut self, col: usize) {
        if self.0.is_empty() {
            return;
        }
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        if let Ok(i) = self.0.binary_search_by_key(&col, |e| e.col) {
            self.0.remove(i);
        }
    }

    /// Keeps the links whose column `keep` accepts.
    pub(crate) fn retain_cols(&mut self, keep: &mut impl FnMut(usize) -> bool) {
        self.0.retain(|e| keep(usize::from(e.col)));
    }

    /// Drops every link whose column lies in `range`.
    #[inline]
    pub(crate) fn remove_range(&mut self, range: Range<usize>) {
        if self.0.is_empty() {
            return;
        }
        let start = self.0.partition_point(|e| usize::from(e.col) < range.start);
        let end = start + self.0[start..].partition_point(|e| usize::from(e.col) < range.end);
        self.0.drain(start..end);
    }

    /// Moves the links at columns `from..end` right by `n` (insertion of
    /// `n` cells at `from` that shifts cells up to `end`), dropping those
    /// that land at or past `end`. Links at `end` and beyond stay.
    pub(crate) fn shift_right(&mut self, from: usize, n: usize, end: usize) {
        self.0.retain_mut(|e| {
            let col = usize::from(e.col);
            if col < from || col >= end {
                return true;
            }
            match u16::try_from(col + n) {
                Ok(moved) if col + n < end => {
                    e.col = moved;
                    true
                }
                _ => false,
            }
        });
    }

    /// Removes the links at columns `at..at + n` and moves the ones after
    /// them, up to `end`, left by `n` (deletion of `n` cells at `at` that
    /// shifts cells up to `end`). Links at `end` and beyond stay.
    pub(crate) fn delete(&mut self, at: usize, n: usize, end: usize) {
        self.0.retain_mut(|e| {
            let col = usize::from(e.col);
            if col < at || col >= end {
                true
            } else if col < at + n {
                false
            } else {
                // `col - n >= at >= 0` and it was a `u16` before.
                e.col = u16::try_from(col - n).unwrap_or(e.col);
                true
            }
        });
    }

    /// Removes every link and hands them out, by ascending column.
    pub(crate) fn take(&mut self) -> Vec<Entry> {
        std::mem::take(&mut self.0)
    }

    /// Shrinks the storage to the entries.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
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
            t.set(col, open(pool, &format!("https://a.bc/{col}")));
        }
        t
    }

    fn cols(t: &LinkColumns) -> Vec<usize> {
        t.iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn entry_is_16_bytes() {
        assert_eq!(size_of::<Entry>(), 16);
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

    #[test]
    fn id_index_stays_bounded_under_a_flood_of_dying_links() {
        let mut pool = LinkPool::new();
        let mut peak = 0;
        for i in 0..1_000_000 {
            drop(pool.open(&format!("https://a.bc/{i}"), Some("k")).unwrap());
            peak = peak.max(pool.indexed());
        }
        assert_eq!(pool.live(), 0);
        assert!(peak <= 2048, "the index held {peak} entries");
    }

    #[test]
    fn set_get_remove_keep_order() {
        let mut pool = LinkPool::new();
        let mut t = table(&mut pool, &[5, 1]);
        assert_eq!(cols(&t), [1, 5]);
        assert_eq!(t.get(5).map(Hyperlink::uri), Some("https://a.bc/5"));
        assert_eq!(t.get(2), None);
        assert_eq!(t.get(usize::MAX), None);
        t.set(5, open(&mut pool, "https://a.bc/new"));
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
        let taken: Vec<usize> = t.take().into_iter().map(|e| e.into_parts().0).collect();
        assert_eq!(taken, [1, 4]);
        assert!(t.is_empty());
        assert_eq!(t.take().len(), 0);
    }
}
