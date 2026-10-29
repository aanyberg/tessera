//! Scrollback history: a capped ring of lines that scrolled off the top of
//! the primary screen.
//!
//! [`Scrollback`] keeps at most [`Scrollback::cap`] lines. Pushing onto a
//! full ring overwrites the oldest line in place and hands it back to the
//! caller, so both the push and the eviction are O(1) and the grid can reuse
//! the evicted line's cell allocation for the blank line that scrolls into
//! view. Storage grows lazily up to the cap and never beyond it, so a large
//! configured cap costs nothing until the history actually fills.
//!
//! # Memory budget
//!
//! Measured on `aarch64-apple-darwin` with rustc 1.99 when [`Cell`] grew to
//! 20 bytes for the underline colour (ADR 0013). The `footprint` unit test
//! prints the requested sizes with `--nocapture`; the `malloc_size` row and
//! the allocated total were measured outside it, with a C program calling
//! `malloc_size` on a block of each size:
//!
//! | item | bytes |
//! |---|---|
//! | `size_of::<Cell>()` | 20 |
//! | `size_of::<Line>()` (the ring slot: two `Vec` headers, the grapheme table pointer and a flag) | 64 |
//! | heap for `cells` of an 80-column line (`80 × 20`, requested exactly) | 1 600 |
//! | **one 80-column line, requested** | **1 664** |
//! | the same heap block as the allocator sizes it (`malloc_size`, macOS 26.6) | 1 792 |
//! | **one 80-column line, allocated (`malloc_size`)** | **1 856** |
//!
//! That is 16 640 000 bytes (≈ 15.9 MiB) requested and 18 560 000 bytes
//! (≈ 17.7 MiB) allocated (`malloc_size`) for the default 10 000 lines, and
//! 1 664 000 000 bytes (≈ 1 587 MiB) requested, 1 856 000 000 bytes
//! (≈ 1 770 MiB) allocated for 1 000 000 lines. With 16-byte cells the heap block was
//! 1 280 bytes, which macOS's allocator hands out exactly, so the line cost
//! 1 344 bytes; the larger cell costs 38 % more per 80-column line. Lines
//! without marks carry no `marks` heap allocation, and lines without
//! grapheme clusters (combining marks, emoji sequences) no table
//! allocation. A line with clusters adds, per cluster, its text (at most
//! 32 scalars, so at most 128 bytes) and a 24-byte table entry; the table
//! is shrunk to fit when the line scrolls off. The worst case, an
//! 80-column line of nothing but full clusters, is about 14 KB, eight
//! times a plain line. Wider lines scale linearly with the column count
//! (up to the allocator's size classes); a larger [`Cell`] scales the heap
//! part.
//!
//! `terminal.scrollback_lines` in `tessera-config` defaults to 10 000 and is
//! capped at 10 000 000 on load.
//!
//! [`Cell`]: crate::Cell

use std::{fmt, mem};

use crate::Line;

/// Smallest number of slots the ring grows by while it is filling up.
const MIN_GROWTH: usize = 64;

/// A capped ring buffer of scrolled-off lines.
///
/// Index `0` is the oldest retained line and `len() - 1` the newest. The
/// ring never holds more than [`Scrollback::cap`] lines; a cap of zero
/// disables history.
#[derive(Clone)]
pub struct Scrollback {
    /// Retained lines. Until the ring is full this is in age order; once
    /// `buf.len() == cap`, the oldest line sits at `head` and the order
    /// wraps around the end of the vector.
    buf: Vec<Line>,
    /// Index of the oldest line in `buf`. Always zero while the ring is not
    /// full.
    head: usize,
    /// Maximum number of lines retained.
    cap: usize,
}

impl Scrollback {
    /// Creates an empty history that retains at most `cap` lines.
    ///
    /// No storage is reserved up front; it grows as lines arrive.
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
            cap,
        }
    }

    /// Number of lines currently retained.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when no lines are retained.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Maximum number of lines retained.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The line at `index`; `0` is the oldest, `len() - 1` the newest.
    pub fn get(&self, index: usize) -> Option<&Line> {
        if index >= self.buf.len() {
            return None;
        }
        let slot = self.head + index;
        let slot = if slot >= self.buf.len() {
            slot - self.buf.len()
        } else {
            slot
        };
        self.buf.get(slot)
    }

    /// Appends `line` as the newest entry in O(1).
    ///
    /// Returns the line that no longer fits: the evicted oldest line when
    /// the ring was full, `line` itself when the cap is zero, and `None`
    /// otherwise. Callers may recycle the returned line's allocations.
    pub fn push(&mut self, line: Line) -> Option<Line> {
        if self.cap == 0 {
            return Some(line);
        }
        let len = self.buf.len();
        if len < self.cap {
            if len == self.buf.capacity() {
                // Grow geometrically, but never reserve past the cap.
                self.buf
                    .reserve_exact(len.max(MIN_GROWTH).min(self.cap - len));
            }
            self.buf.push(line);
            return None;
        }
        let evicted = mem::replace(&mut self.buf[self.head], line);
        self.head += 1;
        if self.head == len {
            self.head = 0;
        }
        Some(evicted)
    }

    /// Changes the cap. When the history holds more than `cap` lines the
    /// oldest are dropped first; a cap of zero empties it and releases its
    /// storage. Re-applying the current cap is O(1); any other change is
    /// O(len).
    pub fn set_cap(&mut self, cap: usize) {
        if cap == self.cap {
            return;
        }
        self.buf.rotate_left(self.head);
        self.head = 0;
        if self.buf.len() > cap {
            let excess = self.buf.len() - cap;
            self.buf.drain(..excess);
        }
        self.buf.shrink_to(cap);
        self.cap = cap;
    }

    /// Drops every retained line. The cap is unchanged and the slot storage
    /// (never more than `cap` slots) is kept, so a history refilled after a
    /// clear does not regrow it; [`Scrollback::set_cap`] with zero releases
    /// it.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    /// Removes and returns the newest line (the one most recently pushed),
    /// for a resize that pulls history back onto the screen.
    ///
    /// O(1), except that the first pop from a full ring whose order wraps
    /// around the end of its storage straightens it first (O(len), once:
    /// the ring is not full afterwards, so it stays straight until it fills
    /// up again).
    pub(crate) fn pop_newest(&mut self) -> Option<Line> {
        if self.head != 0 {
            self.buf.rotate_left(self.head);
            self.head = 0;
        }
        self.buf.pop()
    }

    /// The newest line (the one most recently pushed), mutably, in O(1).
    pub(crate) fn newest_mut(&mut self) -> Option<&mut Line> {
        let slot = self.head.checked_sub(1).or(self.buf.len().checked_sub(1))?;
        self.buf.get_mut(slot)
    }

    /// Removes every retained line and returns them oldest first, leaving
    /// the history empty and without storage. The cap is unchanged.
    pub(crate) fn take_lines(&mut self) -> Vec<Line> {
        let mut lines = mem::take(&mut self.buf);
        lines.rotate_left(self.head);
        self.head = 0;
        lines
    }

    /// Replaces the history with `lines` (oldest first), keeping only the
    /// newest [`Scrollback::cap`] of them. The storage is trimmed so it
    /// never exceeds the cap, as [`Scrollback::push`] guarantees.
    pub(crate) fn set_lines(&mut self, mut lines: Vec<Line>) {
        if lines.len() > self.cap {
            let excess = lines.len() - self.cap;
            lines.drain(..excess);
        }
        lines.shrink_to(self.cap);
        self.buf = lines;
        self.head = 0;
    }

    /// Iterates the retained lines from oldest to newest without
    /// allocating.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Line> + Clone + '_ {
        let (newer, older) = self.buf.split_at(self.head);
        older.iter().chain(newer)
    }
}

/// Prints the counts only: the raw slots are in ring order, not age order,
/// and a full history would be enormous.
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
    use std::mem::size_of;

    use super::*;
    use crate::{Cell, Style};

    fn line(n: usize) -> Line {
        let mut line = Line::blank(1, Style::DEFAULT);
        line.cells[0].ch = char::from_digit(u32::try_from(n % 10).unwrap(), 10).unwrap();
        line
    }

    fn texts(sb: &Scrollback) -> String {
        sb.iter().map(Line::text).collect()
    }

    #[test]
    fn push_until_full_then_evicts_oldest() {
        let mut sb = Scrollback::new(3);
        assert!(sb.is_empty());
        for n in 0..3 {
            assert_eq!(sb.push(line(n)), None);
        }
        assert_eq!(texts(&sb), "012");
        assert_eq!(sb.push(line(3)).map(|l| l.text()), Some("0".into()));
        assert_eq!(sb.push(line(4)).map(|l| l.text()), Some("1".into()));
        assert_eq!(texts(&sb), "234");
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.get(0).unwrap().text(), "2");
        assert_eq!(sb.get(2).unwrap().text(), "4");
        assert_eq!(sb.get(3), None);
        let rev: String = sb.iter().rev().map(Line::text).collect();
        assert_eq!(rev, "432");
    }

    #[test]
    fn zero_cap_rejects_every_line() {
        let mut sb = Scrollback::new(0);
        assert_eq!(sb.push(line(7)).map(|l| l.text()), Some("7".into()));
        assert!(sb.is_empty());
        assert_eq!(sb.get(0), None);
    }

    #[test]
    fn set_cap_trims_oldest_and_keeps_order_when_wrapped() {
        let mut sb = Scrollback::new(4);
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

    #[test]
    fn storage_never_exceeds_cap() {
        let mut sb = Scrollback::new(100);
        for n in 0..1000 {
            sb.push(line(n));
            assert!(sb.buf.capacity() <= 100, "capacity {}", sb.buf.capacity());
        }
        sb.clear();
        assert!(sb.is_empty());
        assert_eq!(sb.cap(), 100);
        assert_eq!(sb.buf.capacity(), 100, "clear keeps the slot storage");
        for n in 0..1000 {
            sb.push(line(n));
        }
        assert_eq!(sb.buf.capacity(), 100);
        sb.set_cap(0);
        assert_eq!(sb.buf.capacity(), 0, "cap zero releases the storage");
    }

    #[test]
    fn set_cap_to_the_current_cap_leaves_the_ring_untouched() {
        let mut sb = Scrollback::new(4);
        for n in 0..6 {
            sb.push(line(n));
        }
        assert_ne!(sb.head, 0, "ring has wrapped");
        let head = sb.head;
        sb.set_cap(4);
        assert_eq!(sb.head, head, "no rotation for an unchanged cap");
        assert_eq!(texts(&sb), "2345");
    }

    /// #86: the newest line is reachable in place, without straightening
    /// the ring, whatever its fill and rotation.
    #[test]
    fn newest_mut_is_the_newest_line_in_place() {
        assert!(Scrollback::new(0).newest_mut().is_none());
        assert!(Scrollback::new(3).newest_mut().is_none());
        for pushes in 1..10 {
            let mut sb = Scrollback::new(4);
            for n in 0..pushes {
                sb.push(line(n));
            }
            let head = sb.head;
            let newest = sb.newest_mut().map(|l| l.text());
            assert_eq!(newest, Some((pushes - 1).to_string()), "{pushes} pushes");
            assert_eq!(sb.head, head, "the ring is not rotated");
            if let Some(line) = sb.newest_mut() {
                line.wrapped = true;
            }
            assert!(sb.get(sb.len() - 1).is_some_and(|l| l.wrapped));
        }
    }

    #[test]
    fn pop_newest_straightens_a_wrapped_ring() {
        let mut sb = Scrollback::new(4);
        for n in 0..6 {
            sb.push(line(n));
        }
        assert_ne!(sb.head, 0, "ring has wrapped");
        assert_eq!(sb.pop_newest().map(|l| l.text()), Some("5".into()));
        assert_eq!(sb.pop_newest().map(|l| l.text()), Some("4".into()));
        assert_eq!(texts(&sb), "23");
        sb.push(line(7));
        sb.push(line(8));
        sb.push(line(9));
        assert_eq!(texts(&sb), "3789", "a refilled ring evicts in age order");
        let mut empty = Scrollback::new(0);
        assert_eq!(empty.pop_newest(), None);
    }

    #[test]
    fn take_and_set_lines_keep_age_order_and_the_cap() {
        let mut sb = Scrollback::new(3);
        for n in 0..5 {
            sb.push(line(n));
        }
        let lines = sb.take_lines();
        assert_eq!(lines.iter().map(Line::text).collect::<String>(), "234");
        assert!(sb.is_empty());
        assert_eq!(sb.cap(), 3);
        sb.set_lines((0..10).map(line).collect());
        assert_eq!(texts(&sb), "789", "only the newest `cap` lines are kept");
        assert!(sb.buf.capacity() <= 3, "storage {}", sb.buf.capacity());
        sb.push(line(1));
        assert_eq!(texts(&sb), "891");
        let mut none = Scrollback::new(0);
        none.set_lines(vec![line(1)]);
        assert!(none.is_empty());
        assert_eq!(none.buf.capacity(), 0);
    }

    #[test]
    fn debug_prints_counts_not_slots() {
        let mut sb = Scrollback::new(4);
        for n in 0..6 {
            sb.push(line(n));
        }
        assert_eq!(format!("{sb:?}"), "Scrollback { len: 4, cap: 4, .. }");
    }

    #[test]
    fn footprint() {
        // The module docs' memory table depends on `Line::blank` allocating
        // exactly `cols` cells and no marks.
        let line = Line::blank(80, Style::DEFAULT);
        assert_eq!(line.cells.capacity(), 80);
        assert_eq!(line.marks.capacity(), 0);
        let heap = line.cells.capacity() * size_of::<Cell>();
        let total = size_of::<Line>() + heap;
        eprintln!(
            "Cell {} B, Line {} B, 80-col cells heap {heap} B, total {total} B/line; \
             10k lines {:.1} MiB, 1M lines {:.1} MiB",
            size_of::<Cell>(),
            size_of::<Line>(),
            mib(total * 10_000),
            mib(total * 1_000_000),
        );
    }

    #[allow(clippy::cast_precision_loss)] // display only
    fn mib(bytes: usize) -> f64 {
        bytes as f64 / (1024.0 * 1024.0)
    }
}
