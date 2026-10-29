//! Which visible rows changed: the grid's damage bookkeeping (ADR 0016).
//!
//! Every [`crate::Grid`] primitive that changes a visible cell, a line's
//! wrap flag or the cursor position records the rows it touched here, so a
//! renderer can re-shape and re-upload only those rows instead of diffing
//! the whole screen. Two kinds of reader are served:
//!
//! - an owner with `&mut Grid` calls [`crate::Grid::take_damage`] once per
//!   frame: the rows changed since the previous call;
//! - a reader holding only `&Grid` (the app's view copies the screen under
//!   the terminal lock) remembers [`Damage::generation`] and asks
//!   [`crate::Grid::damage_since`] next time.
//!
//! The answer is conservative: a reported row may not have changed (false
//! positives are allowed), but a row that changed is always reported (no
//! false negatives). A property test in `grid.rs` holds the grid to that.

use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU64, Ordering};

/// The visible rows a [`Damage`] covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum DamageRows {
    /// Nothing changed.
    None,
    /// Rows in this inclusive range may have changed; rows outside it did
    /// not. One range is a superset: rows 2 and 40 changing give `2..=40`.
    Rows(RangeInclusive<u16>),
    /// Anything may have changed: the whole screen, a resize, an
    /// alternate-screen switch or a full-screen scroll.
    #[default]
    All,
}

impl DamageRows {
    /// The damage covering both `self` and `other`.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::None, x) | (x, Self::None) => x.clone(),
            (Self::Rows(a), Self::Rows(b)) => {
                Self::Rows(*a.start().min(b.start())..=*a.end().max(b.end()))
            }
        }
    }

    /// The damage with `row` added.
    #[must_use]
    pub fn with_row(&self, row: u16) -> Self {
        self.union(&Self::Rows(row..=row))
    }

    /// True when `row` is covered.
    #[must_use]
    pub fn contains(&self, row: u16) -> bool {
        match self {
            Self::None => false,
            Self::Rows(rows) => rows.contains(&row),
            Self::All => true,
        }
    }

    /// True for [`DamageRows::None`].
    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// What changed on a grid's screen, and the generation to ask from next
/// time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Damage {
    /// The rows that may have changed.
    pub rows: DamageRows,
    /// The grid's damage generation when this was computed. Pass it to
    /// [`crate::Grid::damage_since`] to learn what changed afterwards.
    pub generation: u64,
}

/// Every grid starts its generations in its own range of this width, so a
/// generation remembered from one grid never looks recent to another.
const GENERATIONS_PER_GRID: u64 = 1 << 40;

/// The next grid's first generation, in units of [`GENERATIONS_PER_GRID`].
static NEXT_BASE: AtomicU64 = AtomicU64::new(1);

/// Per-row change stamps (crate-private; the grid drives it).
///
/// Each tracker's generations start at its own multiple of
/// [`GENERATIONS_PER_GRID`] (2^40). `generation` counts stamps with
/// saturating adds, so it never wraps, but a tracker that makes more than
/// 2^40 stamps runs into the next tracker's range, and the multiplier
/// wraps after 2^24 trackers (each new grid and each clone takes one;
/// `Grid::reset` keeps its tracker and takes none). Either would only
/// matter to a reader that moves
/// between grids, and none does today: a view reads one session's grid
/// for its whole life.
///
/// A row's stamp is the generation of the last change to it; `all` is the
/// generation of the last change to the whole screen. Writes at the cursor
/// (the print path) only set `cursor_dirty`, one store per character; the
/// flag is folded into the cursor row's stamp whenever the cursor leaves
/// its row and when the damage is taken.
pub(crate) struct DamageTracker {
    /// This grid's first generation; anything below it is another grid's.
    base: u64,
    generation: u64,
    all: u64,
    rows: Vec<u64>,
    /// The cursor row changed since its stamp (a write or a cursor move).
    cursor_dirty: bool,
    /// Generation at the last [`DamageTracker::take`].
    taken: u64,
}

/// A clone is a new tracker: a fresh range of generations, everything
/// damaged. Sharing the original's range would let a generation one of
/// them handed out look recent to the other after both diverged.
impl Clone for DamageTracker {
    fn clone(&self) -> Self {
        Self::new(u16::try_from(self.rows.len()).unwrap_or(u16::MAX))
    }
}

/// Opaque: generations differ between grids holding the same screen, so
/// they stay out of `Grid`'s `Debug` output, which tests compare.
impl std::fmt::Debug for DamageTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DamageTracker").finish_non_exhaustive()
    }
}

impl DamageTracker {
    /// A tracker for `rows` rows that reports everything as damaged.
    pub(crate) fn new(rows: u16) -> Self {
        let base = NEXT_BASE
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(GENERATIONS_PER_GRID);
        let generation = base + 1;
        Self {
            base,
            generation,
            all: generation,
            rows: vec![generation; usize::from(rows)],
            cursor_dirty: false,
            taken: base,
        }
    }

    /// The cursor row changed. The print path's only bookkeeping.
    #[inline]
    pub(crate) fn touch_cursor_row(&mut self) {
        self.cursor_dirty = true;
    }

    /// `row` changed.
    #[inline]
    pub(crate) fn extend_row(&mut self, row: u16) {
        self.generation = self.generation.saturating_add(1);
        if let Some(stamp) = self.rows.get_mut(usize::from(row)) {
            *stamp = self.generation;
        }
    }

    /// Rows `top..=bottom` changed.
    pub(crate) fn extend_rows(&mut self, top: u16, bottom: u16) {
        self.generation = self.generation.saturating_add(1);
        let end = (usize::from(bottom) + 1).min(self.rows.len());
        if let Some(stamps) = self.rows.get_mut(usize::from(top)..end) {
            stamps.fill(self.generation);
        }
    }

    /// The whole screen changed.
    pub(crate) fn extend_all(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.all = self.generation;
    }

    /// The screen now has `rows` rows and all of it changed.
    pub(crate) fn resize(&mut self, rows: u16) {
        self.extend_all();
        self.rows.clear();
        self.rows.resize(usize::from(rows), self.generation);
    }

    /// The cursor leaves row `from`: folds a pending change of it into its
    /// stamp. The new row is dirty, since the cursor is drawn there.
    #[inline]
    pub(crate) fn cursor_left_row(&mut self, from: u16) {
        self.extend_row(from);
        self.cursor_dirty = true;
    }

    /// The rows changed after generation `since`, with the cursor at
    /// `cursor_row`.
    pub(crate) fn since(&self, since: u64, cursor_row: u16) -> DamageRows {
        if since < self.base || since > self.generation || self.all > since {
            return DamageRows::All;
        }
        let mut lo = usize::MAX;
        let mut hi = 0;
        for (row, &stamp) in self.rows.iter().enumerate() {
            if stamp > since {
                lo = lo.min(row);
                hi = row;
            }
        }
        let mut damage = if lo == usize::MAX {
            DamageRows::None
        } else {
            DamageRows::Rows(clamp_u16(lo)..=clamp_u16(hi))
        };
        if self.cursor_dirty {
            damage = damage.with_row(cursor_row);
        }
        damage
    }

    /// The damage since the last [`Self::take`].
    pub(crate) fn current(&self, cursor_row: u16) -> Damage {
        Damage {
            rows: self.since(self.taken, cursor_row),
            generation: self.generation,
        }
    }

    /// The damage since the last call, which then starts over.
    pub(crate) fn take(&mut self, cursor_row: u16) -> Damage {
        let damage = self.current(cursor_row);
        if self.cursor_dirty {
            // Fold the flag so `since` readers still see the row.
            self.extend_row(cursor_row);
            self.cursor_dirty = false;
        }
        self.taken = self.generation;
        Damage {
            rows: damage.rows,
            generation: self.generation,
        }
    }

    /// The current generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

fn clamp_u16(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_and_contains() {
        let a = DamageRows::Rows(2..=4);
        assert_eq!(a.union(&DamageRows::None), a);
        assert_eq!(DamageRows::None.union(&a), a);
        assert_eq!(a.union(&DamageRows::Rows(9..=9)), DamageRows::Rows(2..=9));
        assert_eq!(a.union(&DamageRows::All), DamageRows::All);
        assert_eq!(DamageRows::None.with_row(3), DamageRows::Rows(3..=3));
        assert!(a.contains(2) && a.contains(4) && !a.contains(5));
        assert!(DamageRows::All.contains(u16::MAX));
        assert!(!DamageRows::None.contains(0));
        assert_eq!(DamageRows::default(), DamageRows::All);
    }

    #[test]
    fn a_new_tracker_is_all_damaged_then_clean() {
        let mut t = DamageTracker::new(4);
        assert_eq!(t.current(0).rows, DamageRows::All);
        assert_eq!(t.take(0).rows, DamageRows::All);
        assert_eq!(t.current(0).rows, DamageRows::None);
        t.extend_row(2);
        assert_eq!(t.take(0).rows, DamageRows::Rows(2..=2));
        t.touch_cursor_row();
        assert_eq!(t.take(3).rows, DamageRows::Rows(3..=3));
        assert_eq!(t.current(3).rows, DamageRows::None);
    }

    #[test]
    fn since_readers_see_folded_cursor_rows() {
        let mut t = DamageTracker::new(4);
        let g = t.generation();
        t.touch_cursor_row();
        // A `&Grid` reader sees the dirty cursor row...
        assert_eq!(t.since(g, 1), DamageRows::Rows(1..=1));
        // ...and still sees it after an owner took the damage.
        t.take(1);
        assert_eq!(t.since(g, 1), DamageRows::Rows(1..=1));
        let g = t.generation();
        assert_eq!(t.since(g, 1), DamageRows::None);
    }

    #[test]
    fn a_clone_starts_its_own_generations() {
        let mut a = DamageTracker::new(3);
        a.take(0);
        let since = a.generation();
        a.extend_row(1);
        let b = a.clone();
        assert!(b.base > a.generation, "a fresh range");
        assert_eq!(
            b.since(since, 0),
            DamageRows::All,
            "another grid's generation"
        );
        assert_eq!(b.current(0).rows, DamageRows::All);
        assert_eq!(a.since(b.generation(), 0), DamageRows::All);
    }

    #[test]
    fn debug_output_does_not_depend_on_the_grid() {
        let mut a = DamageTracker::new(2);
        a.extend_row(1);
        let b = DamageTracker::new(2);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn generations_of_another_grid_mean_everything() {
        let a = DamageTracker::new(2);
        let b = DamageTracker::new(2);
        assert!(b.base > a.generation);
        assert_eq!(b.since(a.generation(), 0), DamageRows::All);
        assert_eq!(a.since(b.generation(), 0), DamageRows::All);
        assert_eq!(a.since(0, 0), DamageRows::All);
    }
}
