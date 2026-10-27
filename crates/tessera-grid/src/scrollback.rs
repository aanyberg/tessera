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
//! Measured on `aarch64-apple-darwin` with rustc 1.99 when this module was
//! written (the `footprint` unit test prints the current values with
//! `--nocapture`):
//!
//! | item | bytes |
//! |---|---|
//! | `size_of::<Cell>()` | 16 |
//! | `size_of::<Line>()` (the ring slot: two `Vec` headers and a flag) | 56 |
//! | heap for `cells` of an 80-column line (`80 × 16`, allocated exactly) | 1 280 |
//! | **one 80-column line, total** | **1 336** |
//!
//! That is 13 360 000 bytes (≈ 12.7 MiB) for the default 10 000 lines and
//! 1 336 000 000 bytes (≈ 1 274 MiB) for 1 000 000 lines. Lines without
//! marks carry no `marks` heap allocation. Allocator rounding adds nothing
//! here: on macOS 26.6 `malloc_size` reports exactly 1 280 bytes for a
//! 1 280-byte request (and for an 80-cell `Vec<Cell>`). Wider lines scale
//! linearly with the column count; a larger [`Cell`] scales the heap part.
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
