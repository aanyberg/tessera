//! What the renderer keeps between frames so a damaged frame re-shapes and
//! re-uploads only its damaged rows (ADR 0016).
//!
//! Every visible row keeps its shaped glyphs, its selection and overlay
//! coverage, and the instances of its background, overlay, glyph and
//! decoration quads. On the GPU the instance buffer is laid out as four
//! sections (one per pass, in draw order) of one slot per row, then a slot
//! for the cursor:
//!
//! ```text
//! [ bg row 0 | ... ][ overlay row 0 | ... ][ glyph row 0 | ... ][ deco row 0 | ... ][ cursor ]
//! ```
//!
//! A slot holds its row's instances followed by empty ones (zero-sized
//! quads that rasterise nothing), so each pass is still one draw over its
//! section and the passes keep their order. A damaged row rewrites its
//! slots and only those bytes are uploaded; a row whose selection or
//! overlay coverage changed rewrites only its overlay slot; a row that
//! outgrows a slot makes the renderer lay the buffer out again and upload
//! all of it.

use std::ops::Range;

use crate::passes::Instance;
use crate::passes::overlay::Span;
use crate::shaping::ShapedGlyph;

/// The four per-row passes, in draw order: background, overlay, glyph,
/// decoration.
pub(crate) const ROW_PASSES: usize = 4;

/// Index of the overlay pass in [`Row::passes`].
pub(crate) const OVERLAY_PASS: usize = 1;

/// An instance that draws nothing: a zero-sized solid quad.
pub(crate) const EMPTY: Instance = Instance {
    rect: [0.0; 4],
    clip: [0.0; 4],
    color: [0.0; 4],
    texel: [0.0; 2],
    kind: 0,
    extra: 0.0,
};

/// One row's cached work.
#[derive(Debug, Default)]
pub(crate) struct Row {
    /// The shaped glyphs; the vector is reused from frame to frame.
    pub shaped: Vec<ShapedGlyph>,
    /// Instances of the background, overlay, glyph and decoration passes.
    pub passes: [Vec<Instance>; ROW_PASSES],
    /// The selection and overlay coverage the overlay instances were built
    /// for, compared with the next frame's to find rows whose highlights
    /// changed without damage.
    pub spans: Vec<Span>,
    /// Cells in the row when it was shaped.
    pub len: usize,
    /// The row drew a `.notdef` glyph while the fallback index was still
    /// building (ADR 0007), so it is shaped again once the index is ready.
    pub pending_fallback: bool,
}

/// Where every row's instances live in the instance buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Slots {
    /// `rows[pass][row]`: the slot of `row` in `pass`'s section.
    rows: [Vec<Range<u32>>; ROW_PASSES],
    /// The cursor's slot, after the sections.
    cursor: Range<u32>,
}

impl Slots {
    /// A layout for rows whose passes need `counts[row][pass]` instances
    /// and a cursor of `cursor` instances. With `headroom` each slot is
    /// rounded up so a row can grow without a new layout; without it every
    /// slot is exactly its count.
    pub fn new(counts: &[[usize; ROW_PASSES]], cursor: usize, headroom: bool) -> Self {
        let cap = |count: usize, min: usize| {
            if headroom {
                count.next_power_of_two().max(min)
            } else {
                count
            }
        };
        let mut next = 0u32;
        let mut take = |len: usize| {
            let start = next;
            next = next.saturating_add(u32::try_from(len).unwrap_or(u32::MAX));
            start..next
        };
        let mins = [4, 4, 32, 4];
        let rows = std::array::from_fn(|pass| {
            counts
                .iter()
                .map(|c| take(cap(c[pass], mins[pass])))
                .collect()
        });
        let cursor = take(cap(cursor, 16));
        Self { rows, cursor }
    }

    /// Instances in the whole layout.
    pub fn total(&self) -> u32 {
        self.cursor.end
    }

    /// Number of rows laid out.
    pub fn rows(&self) -> usize {
        self.rows[0].len()
    }

    /// The slot of `row` in `pass`.
    pub fn row(&self, pass: usize, row: usize) -> Range<u32> {
        self.rows[pass][row].clone()
    }

    /// The cursor's slot.
    pub fn cursor(&self) -> Range<u32> {
        self.cursor.clone()
    }

    /// `pass`'s whole section, what one draw covers.
    pub fn section(&self, pass: usize) -> Range<u32> {
        let rows = &self.rows[pass];
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            return first.start..last.end;
        }
        // No rows: an empty range where the section would start.
        let at = if pass == 0 {
            0
        } else {
            self.section(pass - 1).end
        };
        at..at
    }

    /// True when every row's instances fit its slots and the cursor fits
    /// its slot.
    pub fn fits(&self, rows: &[Row], cursor: usize) -> bool {
        rows.len() == self.rows()
            && rows.iter().enumerate().all(|(r, row)| {
                (0..ROW_PASSES).all(|p| row.passes[p].len() <= len(&self.rows[p][r]))
            })
            && cursor <= len(&self.cursor)
    }
}

/// Length of `range`.
fn len(range: &Range<u32>) -> usize {
    usize::try_from(range.end - range.start).unwrap_or(usize::MAX)
}

/// Copies `instances` into `slot` of `mirror`, filling the rest of the slot
/// with [`EMPTY`]. `instances` must fit.
pub(crate) fn fill_slot(mirror: &mut [Instance], slot: Range<u32>, instances: &[Instance]) {
    let start = usize::try_from(slot.start).unwrap_or(usize::MAX);
    let end = usize::try_from(slot.end).unwrap_or(usize::MAX);
    let Some(target) = mirror.get_mut(start..end) else {
        return;
    };
    let (used, rest) = target.split_at_mut(instances.len().min(target.len()));
    used.copy_from_slice(&instances[..used.len()]);
    rest.fill(EMPTY);
}

/// True when `slot` of `mirror` already holds exactly `instances` followed
/// by [`EMPTY`] ones, so writing them again would change nothing.
pub(crate) fn slot_holds(mirror: &[Instance], slot: Range<u32>, instances: &[Instance]) -> bool {
    let start = usize::try_from(slot.start).unwrap_or(usize::MAX);
    let end = usize::try_from(slot.end).unwrap_or(usize::MAX);
    mirror.get(start..end).is_some_and(|target| {
        target.len() >= instances.len()
            && target[..instances.len()] == *instances
            && target[instances.len()..].iter().all(|i| *i == EMPTY)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(bg: usize, overlay: usize, glyph: usize, deco: usize) -> Row {
        Row {
            passes: [
                vec![EMPTY; bg],
                vec![EMPTY; overlay],
                vec![EMPTY; glyph],
                vec![EMPTY; deco],
            ],
            ..Row::default()
        }
    }

    #[test]
    fn sections_are_consecutive_and_in_pass_order() {
        let slots = Slots::new(&[[1, 0, 40, 0], [0, 6, 3, 5]], 2, true);
        assert_eq!(slots.row(0, 0), 0..4);
        assert_eq!(slots.row(0, 1), 4..8);
        assert_eq!(slots.section(0), 0..8);
        assert_eq!(slots.row(1, 0), 8..12, "at least 4 overlay slots");
        assert_eq!(slots.row(1, 1), 12..20, "6 overlays round up to 8");
        assert_eq!(slots.section(1), 8..20);
        assert_eq!(slots.row(2, 0), 20..84, "40 glyphs round up to 64");
        assert_eq!(slots.row(2, 1), 84..116, "at least 32 glyph slots");
        assert_eq!(slots.section(3), 116..128);
        assert_eq!(slots.cursor(), 128..144);
        assert_eq!(slots.total(), 144);
    }

    #[test]
    fn exact_slots_have_no_headroom() {
        let slots = Slots::new(&[[1, 0, 40, 0], [0, 2, 3, 5]], 2, false);
        assert_eq!(slots.total(), 1 + 40 + 2 + 3 + 5 + 2);
        assert!(slots.fits(&[row(1, 0, 40, 0), row(0, 2, 3, 5)], 2));
        assert!(!slots.fits(&[row(1, 0, 41, 0), row(0, 2, 3, 5)], 2));
        assert!(!slots.fits(&[row(1, 1, 40, 0), row(0, 2, 3, 5)], 2));
        assert!(!slots.fits(&[row(1, 0, 40, 0), row(0, 2, 3, 5)], 3));
        assert!(!slots.fits(&[row(1, 0, 40, 0)], 2), "row count changed");
    }

    #[test]
    fn empty_layouts_have_empty_sections() {
        let slots = Slots::new(&[], 0, true);
        for pass in 0..ROW_PASSES {
            assert!(slots.section(pass).is_empty());
        }
        assert_eq!(slots.cursor(), 0..16);
    }

    #[test]
    fn a_slot_is_padded_with_empty_instances() {
        let mut mirror = vec![Instance { kind: 7, ..EMPTY }; 6];
        let one = Instance { kind: 1, ..EMPTY };
        fill_slot(&mut mirror, 1..4, &[one]);
        let kinds: Vec<u32> = mirror.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [7, 1, 0, 0, 7, 7]);
        // Out of range is ignored rather than panicking.
        fill_slot(&mut mirror, 5..9, &[one]);
        assert_eq!(mirror[5].kind, 7);
        assert!(slot_holds(&mirror, 1..4, &[one]));
        assert!(!slot_holds(&mirror, 1..4, &[]), "a stale instance is left");
        assert!(!slot_holds(&mirror, 0..3, &[one]));
        assert!(!slot_holds(&mirror, 5..9, &[]), "out of range");
    }
}
