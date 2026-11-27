//! Scrollback history: a capped store of lines that scrolled off the top of
//! the primary screen, the newest kept as [`Line`]s and the rest packed into
//! blocks (ADR 0051).
//!
//! [`Scrollback`] keeps at most [`Scrollback::cap`] lines. The newest
//! [`HOT_ROWS`] are ordinary [`Line`]s (the *hot* rows): a resize pulls them
//! back onto the screen and shell integration edits the newest of them.
//! Older rows are packed into blocks of 256 rows: their text as UTF-8,
//! their styles as runs into a per-block style table, and a sparse side
//! table for wide, spacer, padding, cluster and link cells, clusters,
//! hyperlinks and marks. Pushing a line packs at most one row and evicts at
//! most one, so both stay O(1), and the line that leaves the hot rows is
//! handed back for the caller to recycle.
//!
//! Readers get a [`LineRef`]: a borrowed hot row, or a packed row decoded
//! on demand. Random access ([`Scrollback::get`]) caches decoded rows, at
//! most [`MAX_UNPACKED_CELLS`] cells (and 1 024 rows) at a time; iteration
//! decodes each row once and caches nothing.
//!
//! # Memory budget
//!
//! Measured on `aarch64-apple-darwin` (the `heap_lines_*` and `rss_1m`
//! cases of the perf job, ADR 0049, and the unit tests below):
//!
//! | item | bytes |
//! |---|---|
//! | a hot row (as before ADR 0051: the 64-byte `Line` and 80 × 20 bytes of cells) | 1 664 |
//! | a packed row's metadata | 24 |
//! | a packed row's text: one byte per ASCII cell up to its last non-blank one | ≤ 80 |
//! | each style run (cells of one style in a row) | 4 |
//! | each wide, spacer, padding, cluster or link cell | 4 |
//! | each cluster | 8 + its UTF-8 |
//! | each distinct style in a block of 256 rows | 14 |
//!
//! A full plain 80-column ASCII line costs about 108 bytes packed; with the
//! hot rows spread over a history of 10 000 lines that is about 150 bytes a
//! line, and about 110 bytes a line over 1 000 000. Rows with hyperlinks or
//! marks add a small record each (the links themselves are shared
//! entities, ADR 0026).
//!
//! `terminal.scrollback_lines` in `tessera-config` defaults to 10 000 and is
//! capped at 10 000 000 on load.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, Mutex, PoisonError};

use crate::Line;
use crate::packed::{BLOCK_ROWS, Block};

/// Newest scrollback rows kept unpacked, as [`Line`]s.
pub const HOT_ROWS: usize = 256;

/// Most cells of decoded rows [`Scrollback::get`] keeps cached at once
/// (docs/security.md bounds). A row wider than this is decoded each time.
pub const MAX_UNPACKED_CELLS: usize = 65_536;

/// Most rows that cache keeps, whatever their width.
const MAX_UNPACKED_ROWS: usize = 1_024;

/// A scrollback or screen line: borrowed when it is stored as a [`Line`],
/// decoded when it was packed. Dereferences to [`Line`].
#[derive(Clone)]
pub struct LineRef<'a>(Repr<'a>);

#[derive(Clone)]
enum Repr<'a> {
    Borrowed(&'a Line),
    Owned(Box<Line>),
    Shared(Arc<Line>),
}

impl<'a> LineRef<'a> {
    /// A borrowed line.
    pub fn borrowed(line: &'a Line) -> Self {
        Self(Repr::Borrowed(line))
    }

    /// The line, owned (cloned when it is borrowed or shared).
    #[must_use]
    pub fn into_owned(self) -> Line {
        match self.0 {
            Repr::Borrowed(line) => line.clone(),
            Repr::Owned(line) => *line,
            Repr::Shared(line) => Arc::unwrap_or_clone(line),
        }
    }

    /// The borrowed line, when it is one: a hot row or a screen row.
    #[must_use]
    pub fn as_borrowed(&self) -> Option<&'a Line> {
        match self.0 {
            Repr::Borrowed(line) => Some(line),
            Repr::Owned(_) | Repr::Shared(_) => None,
        }
    }

    fn owned(line: Line) -> Self {
        Self(Repr::Owned(Box::new(line)))
    }
}

impl Deref for LineRef<'_> {
    type Target = Line;

    fn deref(&self) -> &Line {
        match &self.0 {
            Repr::Borrowed(line) => line,
            Repr::Owned(line) => line,
            Repr::Shared(line) => line,
        }
    }
}

impl AsRef<Line> for LineRef<'_> {
    fn as_ref(&self) -> &Line {
        self
    }
}

impl fmt::Debug for LineRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Line::fmt(self, f)
    }
}

impl PartialEq for LineRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for LineRef<'_> {}

impl PartialEq<Line> for LineRef<'_> {
    fn eq(&self, other: &Line) -> bool {
        **self == *other
    }
}

/// Decoded packed rows by sequence number, oldest first out.
#[derive(Default)]
struct RowCache {
    rows: BTreeMap<u64, Arc<Line>>,
    /// Insertion order; may name rows already forgotten (skipped on the
    /// way out).
    order: VecDeque<u64>,
    cells: usize,
}

impl RowCache {
    fn insert(&mut self, seq: u64, line: &Arc<Line>) {
        let n = line.cells.len();
        if n > MAX_UNPACKED_CELLS {
            return;
        }
        while self.cells + n > MAX_UNPACKED_CELLS || self.order.len() >= MAX_UNPACKED_ROWS {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.rows.remove(&old) {
                self.cells -= old.cells.len();
            }
        }
        self.rows.insert(seq, Arc::clone(line));
        self.order.push_back(seq);
        self.cells += n;
    }

    /// Forgets the rows below sequence number `seq` (evicted rows), so the
    /// cache keeps none of their cells or hyperlink entities alive.
    fn forget_before(&mut self, seq: u64) {
        if self
            .rows
            .first_key_value()
            .is_none_or(|(&first, _)| first >= seq)
        {
            return;
        }
        let kept = self.rows.split_off(&seq);
        for line in std::mem::replace(&mut self.rows, kept).into_values() {
            self.cells -= line.cells.len();
        }
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.order.clear();
        self.cells = 0;
    }
}

/// The rows of a [`Scrollback`] taken for a re-wrap: its packed blocks
/// (the first `skip` rows of the first block already evicted) and its hot
/// rows, oldest first.
pub(crate) struct Taken {
    /// Packed blocks, oldest first.
    pub(crate) blocks: VecDeque<Block>,
    /// Evicted rows at the start of the first block.
    pub(crate) skip: usize,
    /// Hot rows, oldest first.
    pub(crate) hot: VecDeque<Line>,
}

/// A capped store of scrolled-off lines (see the module docs).
///
/// Index `0` is the oldest retained line and `len() - 1` the newest. The
/// store never holds more than [`Scrollback::cap`] lines; a cap of zero
/// disables history.
pub struct Scrollback {
    /// Packed rows, oldest block first.
    blocks: VecDeque<Block>,
    /// Rows at the start of the first block that were evicted.
    skip: usize,
    /// Packed rows retained (all blocks' rows less `skip`).
    packed: usize,
    /// Hot rows, oldest first, after the packed ones.
    hot: VecDeque<Line>,
    /// Most hot rows: [`HOT_ROWS`], more while a re-wrap builds the store.
    hot_cap: usize,
    /// Maximum number of lines retained.
    cap: usize,
    /// Rows ever removed from the front: index `i` has sequence number
    /// `evicted + i`, the cache key.
    evicted: u64,
    /// Decoded packed rows ([`Scrollback::get`]).
    cache: Mutex<RowCache>,
}

impl Clone for Scrollback {
    fn clone(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            skip: self.skip,
            packed: self.packed,
            hot: self.hot.clone(),
            hot_cap: self.hot_cap,
            cap: self.cap,
            evicted: self.evicted,
            cache: Mutex::default(),
        }
    }
}

impl Scrollback {
    /// Creates an empty history that retains at most `cap` lines.
    ///
    /// No storage is reserved up front; it grows as lines arrive.
    pub fn new(cap: usize) -> Self {
        Self::with_hot_rows(cap, HOT_ROWS)
    }

    /// An empty history keeping its newest `hot` rows unpacked.
    pub(crate) fn with_hot_rows(cap: usize, hot: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            skip: 0,
            packed: 0,
            hot: VecDeque::new(),
            hot_cap: hot,
            cap,
            evicted: 0,
            cache: Mutex::default(),
        }
    }

    /// Number of lines currently retained.
    pub fn len(&self) -> usize {
        self.packed + self.hot.len()
    }

    /// True when no lines are retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of lines retained.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Lines held packed (the rest are hot).
    pub fn packed_len(&self) -> usize {
        self.packed
    }

    /// The block and row of packed row `index`.
    fn locate(&self, index: usize) -> (&Block, usize) {
        let front = self.blocks.front().map_or(0, |b| b.first);
        let target = front + (self.skip + index) as u64;
        let b = self.blocks.partition_point(|blk| blk.first <= target) - 1;
        let block = &self.blocks[b];
        // Below `BLOCK_ROWS`.
        #[allow(clippy::cast_possible_truncation)]
        let row = (target - block.first) as usize;
        (block, row)
    }

    /// The line at `index`; `0` is the oldest, `len() - 1` the newest.
    ///
    /// A packed row is decoded and cached (at most
    /// [`MAX_UNPACKED_CELLS`] cells stay cached).
    pub fn get(&self, index: usize) -> Option<LineRef<'_>> {
        if index >= self.packed {
            return self.hot.get(index - self.packed).map(LineRef::borrowed);
        }
        let seq = self.evicted + index as u64;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(line) = cache.rows.get(&seq) {
            return Some(LineRef(Repr::Shared(Arc::clone(line))));
        }
        let (block, row) = self.locate(index);
        let line = Arc::new(block.decode(row));
        cache.insert(seq, &line);
        Some(LineRef(Repr::Shared(line)))
    }

    /// Cells of decoded rows cached now (tests check the cap).
    #[doc(hidden)]
    pub fn cached_cells(&self) -> usize {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cells
    }

    /// Packs the oldest hot row into the newest open block and returns it
    /// for recycling.
    fn pack_oldest(&mut self) -> Option<Line> {
        let mut line = self.hot.pop_front()?;
        let open = self
            .blocks
            .back()
            .is_some_and(|b| b.is_open() && b.fits(&line));
        if !open {
            let first = self.blocks.back().map_or(0, |b| b.first + b.len() as u64);
            if let Some(back) = self.blocks.back_mut() {
                back.seal();
            }
            self.blocks.push_back(Block::open(first));
        }
        if let Some(block) = self.blocks.back_mut() {
            block.push(&mut line);
            if block.len() >= BLOCK_ROWS {
                block.seal();
            }
        }
        self.packed += 1;
        Some(line)
    }

    /// Removes the oldest `n` rows (at most `len()`). Returns the last hot
    /// row removed, for recycling.
    pub(crate) fn drop_oldest(&mut self, n: usize) -> Option<Line> {
        let n = n.min(self.len());
        self.evicted += n as u64;
        self.cache
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .forget_before(self.evicted);
        let from_packed = n.min(self.packed);
        self.packed -= from_packed;
        self.skip += from_packed;
        while let Some(front) = self.blocks.front_mut() {
            if self.skip < front.len() {
                front.release_before(self.skip);
                break;
            }
            self.skip -= front.len();
            self.blocks.pop_front();
        }
        if self.packed == 0 {
            self.blocks.clear();
            self.skip = 0;
        }
        let mut spare = None;
        for _ in from_packed..n {
            spare = self.hot.pop_front();
        }
        spare
    }

    /// Appends `line` as the newest entry in O(1).
    ///
    /// Returns a line the caller may recycle the allocations of: the
    /// evicted oldest line when the store was full and all hot, the row
    /// that was just packed when the hot rows overflowed, `line` itself
    /// when the cap is zero, and `None` otherwise. Its contents are
    /// unspecified.
    pub fn push(&mut self, line: Line) -> Option<Line> {
        if self.cap == 0 {
            return Some(line);
        }
        let mut spare = None;
        if self.len() >= self.cap {
            spare = self.drop_oldest(self.len() + 1 - self.cap);
        }
        self.hot.push_back(line);
        if self.hot.len() > self.hot_cap {
            spare = self.pack_oldest();
        }
        spare
    }

    /// Changes the cap. When the history holds more than `cap` lines the
    /// oldest are dropped first; a cap of zero empties it and releases its
    /// storage.
    pub fn set_cap(&mut self, cap: usize) {
        if cap == self.cap {
            return;
        }
        if self.len() > cap {
            drop(self.drop_oldest(self.len() - cap));
        }
        if cap == 0 {
            self.hot = VecDeque::new();
            self.blocks = VecDeque::new();
            self.cache
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
        self.hot.shrink_to(cap.min(self.hot_cap));
        self.cap = cap;
    }

    /// Drops every retained line. The cap is unchanged.
    pub fn clear(&mut self) {
        self.evicted += self.len() as u64;
        self.blocks.clear();
        self.skip = 0;
        self.packed = 0;
        self.hot.clear();
        self.cache
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Unpacks the newest packed row into the hot rows when there are no
    /// hot rows; false when the store is empty.
    fn ensure_hot_newest(&mut self) -> bool {
        if !self.hot.is_empty() {
            return true;
        }
        let Some(line) = self.blocks.back_mut().and_then(Block::pop) else {
            return false;
        };
        if self.blocks.back().is_some_and(|b| b.len() == 0) {
            self.blocks.pop_back();
        }
        self.packed -= 1;
        if self.packed == 0 {
            self.blocks.clear();
            self.skip = 0;
        }
        // A row left the packed rows other than from the front: its
        // sequence number will name another row.
        self.cache
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.hot.push_back(line);
        true
    }

    /// Removes and returns the newest line (the one most recently pushed),
    /// for a resize that pulls history back onto the screen. O(1) (a
    /// packed newest row is decoded).
    pub(crate) fn pop_newest(&mut self) -> Option<Line> {
        if !self.ensure_hot_newest() {
            return None;
        }
        self.hot.pop_back()
    }

    /// The newest line (the one most recently pushed), mutably, in O(1); a
    /// packed newest row is unpacked first.
    pub(crate) fn newest_mut(&mut self) -> Option<&mut Line> {
        if !self.ensure_hot_newest() {
            return None;
        }
        self.hot.back_mut()
    }

    /// Removes every retained line for a re-wrap, leaving the history empty
    /// and without storage. The cap is unchanged.
    pub(crate) fn take(&mut self) -> Taken {
        self.cache
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.evicted += self.len() as u64;
        self.packed = 0;
        Taken {
            blocks: std::mem::take(&mut self.blocks),
            skip: std::mem::take(&mut self.skip),
            hot: std::mem::take(&mut self.hot),
        }
    }

    /// The most hot rows: [`HOT_ROWS`] (`usize::MAX` in tests that keep
    /// every row unpacked as a reference).
    pub(crate) fn hot_rows(&self) -> usize {
        self.hot_cap
    }

    /// Makes `hot` the most hot rows, packing the oldest hot rows past it.
    pub(crate) fn set_hot_rows(&mut self, hot: usize) {
        self.hot_cap = hot;
        while self.hot.len() > self.hot_cap {
            drop(self.pack_oldest());
        }
    }

    /// Replaces this store's rows with `other`'s (a re-wrap's output),
    /// keeping this store's cap (the newest `cap` rows) and hot-row count.
    pub(crate) fn replace_with(&mut self, mut other: Scrollback) {
        let cap = self.cap;
        if other.len() > cap {
            drop(other.drop_oldest(other.len() - cap));
        }
        other.cap = cap;
        other.evicted = self.evicted;
        other.set_hot_rows(self.hot_cap);
        *self = other;
    }

    /// Iterates the retained lines from oldest to newest. Packed rows are
    /// decoded one at a time and not cached.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = LineRef<'_>> + Clone + '_ {
        (0..self.packed)
            .map(move |i| {
                let (block, row) = self.locate(i);
                LineRef::owned(block.decode(row))
            })
            .chain(self.hot.iter().map(LineRef::borrowed))
    }

    /// Heap bytes of the packed blocks (tests).
    #[cfg(test)]
    pub(crate) fn packed_heap_bytes(&self) -> usize {
        self.blocks.iter().map(Block::heap_bytes).sum()
    }
}

/// Prints the counts only: a full history would be enormous.
impl fmt::Debug for Scrollback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scrollback")
            .field("len", &self.len())
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, CellFlags, Mark, Style};

    fn line(n: usize) -> Line {
        let mut line = Line::blank(2, Style::DEFAULT);
        line.cells[0].ch = char::from_digit(u32::try_from(n % 10).unwrap(), 10).unwrap();
        if n.is_multiple_of(3) {
            line.cells[1] = Cell::new('\u{4e2d}', Style::DEFAULT.with(Style::BOLD));
            line.cells[1].flags = CellFlags::CLUSTER;
            line.graphemes.set(1, "\u{4e2d}\u{301}".into());
        }
        if n.is_multiple_of(4) {
            line.set_mark(Mark::PromptStart);
        }
        line.wrapped = n.is_multiple_of(5);
        line
    }

    fn texts(sb: &Scrollback) -> String {
        sb.iter().map(|l| l.cells[0].ch).collect()
    }

    /// Every hot-row count from all packed to all hot.
    fn stores(cap: usize) -> Vec<Scrollback> {
        [0, 1, 2, 3, usize::MAX]
            .iter()
            .map(|&hot| Scrollback::with_hot_rows(cap, hot))
            .collect()
    }

    #[test]
    fn push_until_full_then_evicts_oldest() {
        for mut sb in stores(3) {
            assert!(sb.is_empty());
            for n in 0..3 {
                sb.push(line(n));
            }
            assert_eq!(texts(&sb), "012");
            sb.push(line(3));
            sb.push(line(4));
            assert_eq!(texts(&sb), "234");
            assert_eq!(sb.len(), 3);
            assert_eq!(sb.get(0).unwrap().cells[0].ch, '2');
            assert_eq!(sb.get(2).unwrap().cells[0].ch, '4');
            assert_eq!(sb.get(3), None);
            let rev: String = sb.iter().rev().map(|l| l.cells[0].ch).collect();
            assert_eq!(rev, "432");
        }
    }

    #[test]
    fn packed_rows_read_back_exactly() {
        for mut sb in stores(700) {
            for n in 0..1_000 {
                sb.push(line(n));
            }
            for (i, got) in sb.iter().enumerate() {
                assert_eq!(*got, line(300 + i), "row {i}");
                assert_eq!(sb.get(i).unwrap(), line(300 + i), "row {i}");
            }
        }
    }

    #[test]
    fn hot_rows_stay_hot_and_the_rest_packed() {
        let mut sb = Scrollback::with_hot_rows(10_000, 4);
        for n in 0..600 {
            sb.push(line(n));
        }
        assert_eq!(sb.packed_len(), 596);
        assert!(sb.get(596).unwrap().as_borrowed().is_some());
        assert!(sb.get(595).unwrap().as_borrowed().is_none());
    }

    #[test]
    fn zero_cap_rejects_every_line() {
        let mut sb = Scrollback::new(0);
        assert_eq!(sb.push(line(7)).map(|l| l.cells[0].ch), Some('7'));
        assert!(sb.is_empty());
        assert_eq!(sb.get(0), None);
    }

    #[test]
    fn set_cap_trims_oldest() {
        for mut sb in stores(4) {
            for n in 0..6 {
                sb.push(line(n));
            }
            assert_eq!(texts(&sb), "2345");
            sb.set_cap(2);
            assert_eq!(texts(&sb), "45");
            sb.set_cap(5);
            sb.push(line(6));
            sb.push(line(7));
            assert_eq!(texts(&sb), "4567");
            sb.set_cap(0);
            assert!(sb.is_empty());
            assert_eq!(sb.cap(), 0);
        }
    }

    #[test]
    fn newest_mut_and_pop_newest_unpack_the_newest() {
        for mut sb in stores(10) {
            assert!(sb.newest_mut().is_none());
            for n in 0..6 {
                sb.push(line(n));
            }
            let newest = sb.newest_mut().map(|l| l.cells[0].ch);
            assert_eq!(newest, Some('5'));
            if let Some(line) = sb.newest_mut() {
                line.wrapped = true;
            }
            assert!(sb.get(sb.len() - 1).is_some_and(|l| l.wrapped));
            assert_eq!(sb.pop_newest().map(|l| l.cells[0].ch), Some('5'));
            assert_eq!(sb.pop_newest(), Some(line(4)));
            // The cache must not serve the popped rows' numbers again.
            let _ = sb.get(sb.len() - 1);
            sb.push(line(7));
            sb.push(line(8));
            assert_eq!(texts(&sb), "012378");
            assert_eq!(sb.get(4).unwrap().cells[0].ch, '7');
        }
        let mut empty = Scrollback::new(3);
        assert_eq!(empty.pop_newest(), None);
    }

    #[test]
    fn cache_holds_at_most_its_cap() {
        let mut sb = Scrollback::with_hot_rows(100_000, 0);
        let wide = Line::blank(1_000, Style::DEFAULT);
        for _ in 0..200 {
            sb.push(wide.clone());
        }
        for i in 0..200 {
            assert!(sb.get(i).is_some());
            assert!(sb.cached_cells() <= MAX_UNPACKED_CELLS);
        }
        assert_eq!(sb.cached_cells(), MAX_UNPACKED_CELLS / 1_000 * 1_000);
        let mut narrow = Scrollback::with_hot_rows(100_000, 0);
        for n in 0..3_000 {
            narrow.push(line(n));
        }
        for i in 0..3_000 {
            let _ = narrow.get(i);
        }
        let cache = narrow.cache.lock().unwrap();
        assert_eq!(cache.order.len(), MAX_UNPACKED_ROWS);
        assert_eq!(cache.rows.len(), MAX_UNPACKED_ROWS);
    }

    #[test]
    fn plain_lines_pack_under_the_bar() {
        let mut sb = Scrollback::with_hot_rows(10_000, 0);
        let mut plain = Line::blank(80, Style::DEFAULT);
        for (i, cell) in plain.cells.iter_mut().enumerate() {
            cell.ch = char::from(b'a' + u8::try_from(i % 26).unwrap());
        }
        for _ in 0..10_000 {
            sb.push(plain.clone());
        }
        let per_line = sb.packed_heap_bytes() / 10_000;
        assert!(per_line <= 24 + 80 + 4 + 4, "{per_line} B per packed line");
    }
}
