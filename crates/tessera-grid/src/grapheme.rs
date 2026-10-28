//! Grapheme clusters that do not fit in one [`Cell::ch`], and the width
//! queries the grid uses to place characters.
//!
//! A [`Cell`] holds one scalar. When further scalars join it to form one
//! grapheme cluster (UAX #29: a combining mark, a variation selector, an
//! emoji modifier, a ZWJ sequence, the second regional indicator of a
//! flag), the cell gets [`CellFlags::CLUSTER`] and the line's
//! [`GraphemeTable`] stores the whole cluster text, base included, under
//! the cell's column. Most lines have no clusters, so an empty table costs
//! one pointer and no allocation.
//!
//! [`Cell`]: crate::Cell
//! [`Cell::ch`]: crate::Cell::ch
//! [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER

use std::fmt;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

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

/// The cluster text of every [`CellFlags::CLUSTER`] cell in a [`Line`],
/// keyed by column.
///
/// Each entry holds the whole cluster, base scalar included, and at least
/// two scalars (a lone scalar is a plain cell). The grid keeps it
/// consistent with the cells: every mutation that overwrites, clears or
/// moves cells updates the table too, so its keys are exactly the columns
/// whose cell has [`CellFlags::CLUSTER`].
///
/// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
/// [`Line`]: crate::Line
#[derive(Clone, Default, Eq)]
pub struct GraphemeTable {
    /// Entries sorted by column; `None` until the line first gets a cluster
    /// (and again after [`GraphemeTable::clear`] or when an emptied table
    /// is compacted), so a line
    /// without clusters allocates nothing.
    #[expect(
        clippy::box_collection,
        reason = "one pointer in every Line instead of a three-word Vec header; \
                  most lines, including every scrollback line, have no clusters"
    )]
    entries: Option<Box<Vec<Entry>>>,
}

/// Prints the clusters as a `{column: text}` map, the same for a table
/// that never held one and one whose kept vector is empty.
impl fmt::Debug for GraphemeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl PartialEq for GraphemeTable {
    fn eq(&self, other: &Self) -> bool {
        self.slice() == other.slice()
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

    /// True when the line has no clusters.
    pub fn is_empty(&self) -> bool {
        self.slice().is_empty()
    }

    /// Every `(column, cluster text)` pair, by ascending column.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.slice().iter().map(|e| (usize::from(e.col), &*e.text))
    }

    fn slice(&self) -> &[Entry] {
        self.entries.as_deref().map_or(&[], Vec::as_slice)
    }

    /// Stores `text` as the cluster at `col`, replacing any previous one.
    pub(crate) fn set(&mut self, col: usize, text: Box<str>) {
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        let entries = self.entries.get_or_insert_with(Box::default);
        match entries.binary_search_by_key(&col, |e| e.col) {
            Ok(i) => entries[i].text = text,
            Err(i) => entries.insert(i, Entry { col, text }),
        }
    }

    /// Drops the cluster at `col`, if any.
    #[inline]
    pub(crate) fn remove(&mut self, col: usize) {
        let Ok(col) = u16::try_from(col) else {
            return;
        };
        self.edit(|entries| {
            if let Ok(i) = entries.binary_search_by_key(&col, |e| e.col) {
                entries.remove(i);
            }
        });
    }

    /// Drops every cluster whose column lies in `range`.
    #[inline]
    pub(crate) fn remove_range(&mut self, range: Range<usize>) {
        self.edit(|entries| {
            let start = entries.partition_point(|e| usize::from(e.col) < range.start);
            let end = start + entries[start..].partition_point(|e| usize::from(e.col) < range.end);
            entries.drain(start..end);
        });
    }

    /// Drops every cluster.
    pub(crate) fn clear(&mut self) {
        self.entries = None;
    }

    /// Moves the clusters at columns `>= from` right by `n` (insertion of
    /// `n` cells at `from`), dropping those that land at or past `cols`.
    pub(crate) fn shift_right(&mut self, from: usize, n: usize, cols: usize) {
        self.edit(|entries| {
            entries.retain_mut(|e| {
                let col = usize::from(e.col);
                if col < from {
                    return true;
                }
                match u16::try_from(col + n) {
                    Ok(moved) if col + n < cols => {
                        e.col = moved;
                        true
                    }
                    _ => false,
                }
            });
        });
    }

    /// Removes the clusters at columns `at..at + n` and moves the ones
    /// after them left by `n` (deletion of `n` cells at `at`).
    pub(crate) fn delete(&mut self, at: usize, n: usize) {
        self.edit(|entries| {
            entries.retain_mut(|e| {
                let col = usize::from(e.col);
                if col < at {
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
    /// by ascending column, without copying the texts. The table is left
    /// empty and without storage, like a new one.
    pub(crate) fn take(&mut self) -> impl Iterator<Item = (usize, Box<str>)> + use<> {
        self.entries
            .take()
            .into_iter()
            .flat_map(|entries| *entries)
            .map(|e| (usize::from(e.col), e.text))
    }

    /// Trims the storage to the entries: frees an emptied table and shrinks
    /// a non-empty one to fit. Lines on screen keep their vector as it
    /// grew, so text that keeps rewriting clusters on the same row does not
    /// reallocate it each time; a line calls this when it scrolls into
    /// history, so a scrollback line pays only for the clusters it holds.
    pub(crate) fn compact(&mut self) {
        match &mut self.entries {
            Some(entries) if entries.is_empty() => self.entries = None,
            Some(entries) => entries.shrink_to_fit(),
            None => {}
        }
    }

    /// Runs `f` on the entries, if any. An emptied vector is kept for
    /// reuse (see [`GraphemeTable::compact`]).
    #[inline]
    fn edit(&mut self, f: impl FnOnce(&mut Vec<Entry>)) {
        if let Some(entries) = self.entries.as_mut() {
            f(entries);
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
        let capacity = t.entries.as_ref().map_or(0, |e| e.capacity());
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
        t.delete(1, 2);
        assert_eq!(cols(&t), [0, 2]);
        assert_eq!(t.get(2), Some("c\u{301}"));
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
