//! Grapheme clusters that do not fit in one [`Cell::ch`], the hyperlinks of
//! linked cells, and the width queries the grid uses to place characters.
//!
//! A [`Cell`] holds one scalar. When further scalars join it to form one
//! grapheme cluster (UAX #29: a combining mark, a variation selector, an
//! emoji modifier, a ZWJ sequence, the second regional indicator of a
//! flag), the cell gets [`CellFlags::CLUSTER`] and the line's
//! [`GraphemeTable`] stores the whole cluster text, base included, under
//! the cell's column. A cell written while an OSC 8 hyperlink is open gets
//! [`CellFlags::LINK`] and the same table stores its [`Hyperlink`] under
//! the column (ADR 0026), in the same allocation: a line keeps one pointer
//! for both, which holds [`Line`] at 64 bytes (a size that is not a power of
//! two made every row access and every scroll measurably dearer). Most
//! lines have neither, so an empty table costs one pointer and no
//! allocation.
//!
//! [`Cell`]: crate::Cell
//! [`Cell::ch`]: crate::Cell::ch
//! [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
//! [`CellFlags::LINK`]: crate::CellFlags::LINK
//! [`Hyperlink`]: crate::Hyperlink
//! [`Line`]: crate::Line

use std::fmt;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::Hyperlink;
use crate::hyperlink::LinkColumns;

/// Most scalars one cluster may hold, base included.
///
/// The longest RGI emoji sequence has 10 scalars; the cap only stops
/// "Zalgo" text from growing a line's table without bound. Scalars that
/// arrive once a cluster is full are not attached (a zero-width one is
/// dropped).
pub const MAX_CLUSTER_SCALARS: usize = 32;

/// Columns a lone scalar occupies, per `unicode-width` 0.2
/// (`UnicodeWidthChar::width`, the non-CJK variant).
///
/// `None` for control characters (C0, DEL, C1), which never occupy a cell;
/// `Some(0)` for zero-width scalars (combining marks, ZWJ, variation
/// selectors, format characters), which can only join the cell to their
/// left; `Some(1)` or `Some(2)` otherwise. A cell is at most two columns,
/// so wider answers are clamped to 2: U+17D8 KHMER SIGN BEYYAL, the only
/// scalar `unicode-width` 0.2.2 calls three columns, takes two. The width
/// of a whole cluster is
/// not the sum of its scalars' widths (`👍` + `🏽` is 2): the grid asks
/// `unicode-width` for the cluster text when a scalar attaches.
#[inline]
pub fn char_width(ch: char) -> Option<u16> {
    ch.width()
        .map(|w| if w >= 2 { 2 } else { u16::from(w == 1) })
}

/// One cluster: its column and full text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    col: u16,
    text: Box<str>,
}

/// What a line's table holds: the clusters and the hyperlinks, each sorted
/// by column.
#[derive(Clone, Default, PartialEq, Eq)]
struct Side {
    clusters: Vec<Entry>,
    links: LinkColumns,
}

/// The cluster text of every [`CellFlags::CLUSTER`] cell in a [`Line`] and
/// the hyperlink of every [`CellFlags::LINK`] cell, keyed by column.
///
/// Each cluster entry holds the whole cluster, base scalar included, and at
/// least two scalars (a lone scalar is a plain cell). The grid keeps the
/// table consistent with the cells: every mutation that overwrites, clears
/// or moves cells updates the table too, so its keys are exactly the
/// columns whose cell has [`CellFlags::CLUSTER`] (clusters) and
/// [`CellFlags::LINK`] (links). A column can have both. Every edit of
/// the cells (overwrite, erase, insert, delete, scroll) applies to both kinds;
/// [`GraphemeTable::get`], [`GraphemeTable::len`] and
/// [`GraphemeTable::iter`] read the clusters, [`GraphemeTable::link`] and
/// [`GraphemeTable::links`] the links.
///
/// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
/// [`CellFlags::LINK`]: crate::CellFlags::LINK
/// [`Line`]: crate::Line
#[derive(Clone, Default, Eq)]
pub struct GraphemeTable {
    /// `None` until the line first gets a cluster or a link (and again
    /// after [`GraphemeTable::clear`] or when an emptied table is
    /// compacted), so a line without either allocates nothing.
    /// One pointer in every `Line` instead of two `Vec` headers: most lines,
    /// including every scrollback line, have neither.
    entries: Option<Box<Side>>,
}

/// Prints the clusters as a `{column: text}` map, the same for a table
/// that never held one and one whose kept vector is empty.
impl fmt::Debug for GraphemeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()?;
        if self.has_links() {
            f.write_str(" + links ")?;
            f.debug_map().entries(self.links()).finish()?;
        }
        Ok(())
    }
}

impl PartialEq for GraphemeTable {
    fn eq(&self, other: &Self) -> bool {
        self.slice() == other.slice() && self.link_slice_eq(other)
    }
}

impl GraphemeTable {
    /// An empty table.
    pub const fn new() -> Self {
        Self { entries: None }
    }

    /// The cluster text at `col`, base scalar included.
    pub fn get(&self, col: usize) -> Option<&str> {
        let col = u16::try_from(col).ok()?;
        let entries = self.slice();
        let i = entries.binary_search_by_key(&col, |e| e.col).ok()?;
        Some(&entries[i].text)
    }

    /// Number of clusters.
    pub fn len(&self) -> usize {
        self.slice().len()
    }

    /// True when the line has no clusters and no hyperlinks.
    pub fn is_empty(&self) -> bool {
        self.slice().is_empty() && !self.has_links()
    }

    /// Every `(column, cluster text)` pair, by ascending column.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.slice().iter().map(|e| (usize::from(e.col), &*e.text))
    }

    fn slice(&self) -> &[Entry] {
        self.entries.as_deref().map_or(&[], |side| &side.clusters)
    }

    /// The hyperlink at `col`.
    pub fn link(&self, col: usize) -> Option<&Hyperlink> {
        self.entries.as_deref()?.links.get(col)
    }

    /// True when the line has a hyperlink.
    pub fn has_links(&self) -> bool {
        self.entries
            .as_deref()
            .is_some_and(|side| !side.links.is_empty())
    }

    /// Every `(column, hyperlink)` pair, by ascending column.
    pub fn links(&self) -> impl Iterator<Item = (usize, &Hyperlink)> + '_ {
        self.entries
            .as_deref()
            .into_iter()
            .flat_map(|side| side.links.iter())
    }

    fn link_slice_eq(&self, other: &Self) -> bool {
        self.links().eq(other.links())
    }

    /// Stores `text` as the cluster at `col`, replacing any previous one.
    pub(crate) fn set(&mut self, col: usize, text: Box<str>) {
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        let side = self.entries.get_or_insert_with(Box::default);
        let entries = &mut side.clusters;
        match entries.binary_search_by_key(&col, |e| e.col) {
            Ok(i) => entries[i].text = text,
            Err(i) => entries.insert(i, Entry { col, text }),
        }
    }

    /// Stores `link` as the hyperlink at `col`, replacing any previous one.
    pub(crate) fn set_link(&mut self, col: usize, link: Hyperlink) {
        self.entries
            .get_or_insert_with(Box::default)
            .links
            .set(col, link);
    }

    /// Drops the cluster and the hyperlink at `col`, if any.
    #[inline]
    pub(crate) fn remove(&mut self, col: usize) {
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        self.edit(|side| {
            if let Ok(i) = side.clusters.binary_search_by_key(&col, |e| e.col) {
                side.clusters.remove(i);
            }
            side.links.remove(usize::from(col));
        });
    }

    /// Drops every cluster and hyperlink whose column lies in `range`.
    #[inline]
    pub(crate) fn remove_range(&mut self, range: Range<usize>) {
        if self.entries.is_none() {
            // The common case: a line without clusters or links.
            return;
        }
        self.edit(|side| {
            let entries = &mut side.clusters;
            let start = entries.partition_point(|e| usize::from(e.col) < range.start);
            let end = start + entries[start..].partition_point(|e| usize::from(e.col) < range.end);
            entries.drain(start..end);
            side.links.remove_range(range);
        });
    }

    /// Keeps the clusters and hyperlinks whose column `keep` accepts and
    /// drops the others, in one pass over each kind.
    pub(crate) fn retain_cols(&mut self, mut keep: impl FnMut(usize) -> bool) {
        self.edit(|side| {
            side.clusters.retain(|e| keep(usize::from(e.col)));
            side.links.retain_cols(&mut keep);
        });
    }

    /// Drops every cluster and hyperlink.
    pub(crate) fn clear(&mut self) {
        self.entries = None;
    }

    /// Moves the clusters and hyperlinks at columns `from..end` right by
    /// `n` (insertion of `n` cells at `from` that shifts cells up to
    /// `end`), dropping those that land at or past `end`. Entries at `end`
    /// and beyond stay.
    pub(crate) fn shift_right(&mut self, from: usize, n: usize, end: usize) {
        self.edit(|side| {
            side.links.shift_right(from, n, end);
            side.clusters.retain_mut(|e| {
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
        });
    }

    /// Removes the clusters and hyperlinks at columns `at..at + n` and
    /// moves the ones after them, up to `end`, left by `n` (deletion of `n`
    /// cells at `at` that shifts cells up to `end`). Entries at `end` and
    /// beyond stay.
    pub(crate) fn delete(&mut self, at: usize, n: usize, end: usize) {
        self.edit(|side| {
            side.links.delete(at, n, end);
            side.clusters.retain_mut(|e| {
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
        });
    }

    /// Removes every cluster and hands them out as `(column, text)` pairs
    /// by ascending column, without copying the texts. The hyperlinks stay
    /// (see [`GraphemeTable::take_links`]); a table without any is left
    /// without storage, like a new one.
    pub(crate) fn take(&mut self) -> impl Iterator<Item = (usize, Box<str>)> + use<> {
        let taken = self
            .entries
            .as_deref_mut()
            .map(|side| std::mem::take(&mut side.clusters));
        self.drop_if_empty();
        taken
            .into_iter()
            .flatten()
            .map(|e| (usize::from(e.col), e.text))
    }

    /// Removes every hyperlink and hands them out as `(column, link)`
    /// pairs by ascending column; the clusters stay.
    pub(crate) fn take_links(&mut self) -> impl Iterator<Item = (usize, Hyperlink)> + use<> {
        let taken = self.entries.as_deref_mut().map(|side| side.links.take());
        self.drop_if_empty();
        taken
            .into_iter()
            .flatten()
            .map(crate::hyperlink::Entry::into_parts)
    }

    /// Frees the storage when nothing is left in it.
    fn drop_if_empty(&mut self) {
        if self
            .entries
            .as_deref()
            .is_some_and(|side| side.clusters.is_empty() && side.links.is_empty())
        {
            self.entries = None;
        }
    }

    /// Trims the storage to the entries: frees an emptied table and shrinks
    /// a non-empty one to fit. Lines on screen keep their vectors as they
    /// grew, so text that keeps rewriting clusters or links on the same row
    /// does not reallocate them each time; a line calls this when it
    /// scrolls into history, so a scrollback line pays only for what it
    /// holds.
    pub(crate) fn compact(&mut self) {
        match &mut self.entries {
            Some(side) if side.clusters.is_empty() && side.links.is_empty() => {
                self.entries = None;
            }
            Some(side) => {
                side.clusters.shrink_to_fit();
                side.links.shrink_to_fit();
            }
            None => {}
        }
    }

    /// Runs `f` on the entries, if any. Emptied vectors are kept for reuse
    /// (see [`GraphemeTable::compact`]).
    #[inline]
    fn edit(&mut self, f: impl FnOnce(&mut Side)) {
        if let Some(side) = self.entries.as_mut() {
            f(side);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(usize, &str)]) -> GraphemeTable {
        let mut t = GraphemeTable::new();
        for &(col, text) in entries {
            t.set(col, text.into());
        }
        t
    }

    fn cols(t: &GraphemeTable) -> Vec<usize> {
        t.iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn set_get_remove_keep_order() {
        let mut t = table(&[(5, "e\u{301}"), (1, "a\u{302}")]);
        assert_eq!(cols(&t), [1, 5]);
        assert_eq!(t.get(5), Some("e\u{301}"));
        assert_eq!(t.get(2), None);
        assert_eq!(t.get(usize::MAX), None);
        t.set(5, "x\u{300}".into());
        assert_eq!(t.get(5), Some("x\u{300}"));
        assert_eq!(t.len(), 2);
        t.remove(1);
        t.remove(9);
        assert_eq!(cols(&t), [5]);
        t.remove(5);
        assert!(t.is_empty());
        assert_eq!(t, GraphemeTable::new(), "empty tables compare equal");
        assert_eq!(format!("{t:?}"), format!("{:?}", GraphemeTable::new()));
        assert_eq!(format!("{:?}", table(&[(2, "ab")])), "{2: \"ab\"}");
        assert!(t.entries.is_some(), "an emptied table keeps its storage");
        t.compact();
        assert!(t.entries.is_none(), "until it is compacted");
        let mut t = table(&(0..256).map(|c| (c, "a\u{301}")).collect::<Vec<_>>());
        t.remove_range(1..256);
        t.compact();
        assert_eq!(t.len(), 1, "a non-empty table keeps its entries");
        let capacity = t.entries.as_ref().map_or(0, |e| e.clusters.capacity());
        assert_eq!(capacity, 1, "and shrinks to them");
    }

    #[test]
    fn entry_is_24_bytes() {
        // The memory budget in `scrollback.rs` and ADR 0008 counts on it.
        assert_eq!(size_of::<Entry>(), 24);
    }

    #[test]
    fn shifts_follow_insert_and_delete() {
        let mut t = table(&[(0, "a\u{301}"), (2, "b\u{301}"), (6, "c\u{301}")]);
        t.shift_right(1, 3, 8);
        assert_eq!(cols(&t), [0, 5], "column 6 + 3 fell off an 8-column line");
        let mut t = table(&[(0, "a\u{301}"), (2, "b\u{301}"), (4, "c\u{301}")]);
        t.delete(1, 2, 8);
        assert_eq!(cols(&t), [0, 2]);
        assert_eq!(t.get(2), Some("c\u{301}"));
        // Between margins: clusters at and past `end` stay where they are.
        let mut t = table(&[(1, "a\u{301}"), (3, "b\u{301}"), (6, "c\u{301}")]);
        t.shift_right(1, 2, 5);
        assert_eq!(cols(&t), [3, 6], "column 3 + 2 fell off at end 5");
        let mut t = table(&[(1, "a\u{301}"), (3, "b\u{301}"), (6, "c\u{301}")]);
        t.delete(1, 1, 5);
        assert_eq!(cols(&t), [2, 6]);
        t.remove_range(0..10);
        assert!(t.is_empty());
        let mut t = table(&[(3, "a\u{301}")]);
        t.clear();
        assert!(t.is_empty());
    }

    #[test]
    fn take_empties_the_table_in_column_order() {
        let mut t = table(&[(4, "b\u{301}"), (1, "a\u{301}")]);
        let taken: Vec<(usize, Box<str>)> = t.take().collect();
        assert_eq!(taken, [(1, "a\u{301}".into()), (4, "b\u{301}".into())]);
        assert!(t.entries.is_none(), "no storage is left behind");
        assert_eq!(t.take().count(), 0);
    }

    #[test]
    fn char_width_follows_unicode_width() {
        assert_eq!(char_width('a'), Some(1));
        assert_eq!(char_width('\u{4E2D}'), Some(2));
        assert_eq!(char_width('\u{301}'), Some(0));
        assert_eq!(char_width('\u{0}'), None);
        assert_eq!(char_width('\u{7F}'), None);
        assert_eq!(char_width('\u{85}'), None);
        assert_eq!(char_width('\u{17D8}'), Some(2), "unicode-width says 3");
    }
}
