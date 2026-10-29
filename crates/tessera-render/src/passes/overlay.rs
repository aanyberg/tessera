//! Pass 2: the selection and the overlays.
//!
//! Drawn between the cell backgrounds and the glyphs, so a highlight is
//! alpha-blended over the cell's background and the text stays on top in
//! its own colors (no inverse video, ADR 0010). Each row's coverage is a
//! list of [`Span`]s, computed by [`row_spans`] from the frame's
//! [`Selection`] and [`Overlay`]s.
//!
//! Spans of the same [`Mark`] are merged into their union: a cell covered by
//! three overlapping matches is highlighted once, exactly like a cell
//! covered by one, so a highlight's opacity never depends on how many
//! matches overlap (and a flood of overlays draws no more than the screen
//! holds). Different marks stack in a fixed order, from bottom to top:
//! selection, matches, the current match, link underlines. So the current
//! match is always on top of the matches and the selection under both.
//!
//! - The selection is one quad of [`Theme::selection`] per row;
//! - [`OverlayKind::Match`] and [`OverlayKind::CurrentMatch`] are quads of
//!   [`Theme::search_match`] and [`Theme::search_match_current`];
//! - [`OverlayKind::Underline`] is a bar one point high (one pixel per unit
//!   of backing scale) at the underline position, in each covered cell's
//!   foreground color.
//!
//! A span that starts on the right half of a wide character, or ends on
//! its left half, is widened to the whole character, and spans are clipped
//! to the visible columns. Shaping never sees any of this, so a selection
//! that ends inside a ligature leaves the ligature whole. After the union
//! every mark has at most one span per two visible columns, so a row's
//! overlay instances are bounded by its width ([`push_row`] also stops at
//! [`INSTANCES_PER_COLUMN`] per visible column, as a guard).

use tessera_grid::Cell;

use super::{CellColors, Instance, Layout, f32_from};
use crate::frame::{FrameInput, Overlay, OverlayKind, Selection};
use crate::palette::Palette;
use crate::theme::{Rgba, Theme};

/// Most overlay instances one row emits per visible column. The union of
/// each mark's spans already stays below 2 per column (one quad per
/// disjoint span of three filled marks, one underline bar per cell); this
/// is a guard so no input can make a row's slot grow with the input.
pub(crate) const INSTANCES_PER_COLUMN: usize = 4;

/// What a [`Span`] draws. The order of the variants is the drawing order,
/// bottom to top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Mark {
    /// The selection highlight.
    Selection,
    /// A search match.
    Match,
    /// The current search match.
    CurrentMatch,
    /// A link underline.
    Underline,
}

/// One highlight on one row: `first..=last` columns, already widened over
/// wide characters and clipped to the visible columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Span {
    /// What to draw.
    pub mark: Mark,
    /// First column covered.
    pub first: u16,
    /// Last column covered (inclusive).
    pub last: u16,
}

impl Mark {
    /// The mark an overlay of `kind` draws.
    fn of(kind: OverlayKind) -> Self {
        match kind {
            OverlayKind::Match => Self::Match,
            OverlayKind::CurrentMatch => Self::CurrentMatch,
            OverlayKind::Underline => Self::Underline,
        }
    }
}

/// `first..=last` widened over the wide characters of `cells` at either
/// end and clipped to `cols` visible columns, or `None` when nothing of it
/// is visible.
fn widened(cells: &[Cell], first: u16, last: u16, cols: usize) -> Option<(u16, u16)> {
    let max = u16::try_from(cols.checked_sub(1)?).unwrap_or(u16::MAX);
    let at = |c: u16| cells.get(usize::from(c));
    let first = if at(first).is_some_and(Cell::is_spacer) {
        first.saturating_sub(1)
    } else {
        first
    };
    let last = if at(last).is_some_and(Cell::is_wide) {
        last.saturating_add(1)
    } else {
        last
    };
    let last = last.min(max);
    (first <= last).then_some((first, last))
}

/// Replaces `out` with row `r`'s coverage (its `cells`, of which `cols`
/// are visible): every span widened and clipped, then each mark's spans
/// merged into their union (overlapping or adjacent spans become one),
/// sorted by mark in drawing order and then by column.
pub(crate) fn row_spans<'o>(
    out: &mut Vec<Span>,
    selection: Option<&Selection>,
    overlays: impl IntoIterator<Item = &'o Overlay>,
    (r, cells): (u16, &[Cell]),
    cols: usize,
) {
    out.clear();
    let last_col = u16::try_from(cols.saturating_sub(1)).unwrap_or(u16::MAX);
    if let Some((first, last)) = selection.and_then(|s| s.cols_on(r, last_col))
        && let Some((first, last)) = widened(cells, first, last, cols)
    {
        out.push(Span {
            mark: Mark::Selection,
            first,
            last,
        });
    }
    for overlay in overlays {
        let Some(end) = overlay.cols.end.checked_sub(1) else {
            continue;
        };
        if overlay.cols.start > end {
            continue;
        }
        if let Some((first, last)) = widened(cells, overlay.cols.start, end, cols) {
            out.push(Span {
                mark: Mark::of(overlay.kind),
                first,
                last,
            });
        }
    }
    union_by_mark(out);
}

/// Sorts `spans` by mark and column and merges each mark's overlapping or
/// adjacent spans, in place.
fn union_by_mark(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    spans.sort_unstable_by_key(|s| (s.mark, s.first, s.last));
    let mut kept = 0;
    for i in 1..spans.len() {
        let next = spans[i];
        let open = &mut spans[kept];
        if open.mark == next.mark && u32::from(next.first) <= u32::from(open.last) + 1 {
            open.last = open.last.max(next.last);
        } else {
            kept += 1;
            spans[kept] = next;
        }
    }
    spans.truncate(kept + 1);
}

/// The overlays of `frame` grouped by row: `buckets[r]` lists the indices
/// of the overlays on row `r`. Overlays on rows past
/// `rows` are dropped.
pub(crate) fn bucket(buckets: &mut Vec<Vec<usize>>, frame: &FrameInput<'_>, rows: usize) {
    buckets.iter_mut().for_each(Vec::clear);
    buckets.resize_with(rows, Vec::new);
    for (i, overlay) in frame.overlays.iter().enumerate() {
        if let Some(bucket) = buckets.get_mut(usize::from(overlay.row)) {
            bucket.push(i);
        }
    }
}

/// Appends the quads of row `r`'s `spans` (its cells are `row`) to `out`,
/// at most [`INSTANCES_PER_COLUMN`] per visible column.
pub(crate) fn push_row(
    out: &mut Vec<Instance>,
    layout: &Layout,
    (palette, theme): (&Palette, &Theme),
    r: usize,
    row: &[Cell],
    spans: &[Span],
) {
    let limit = out.len() + layout.visible_cols().saturating_mul(INSTANCES_PER_COLUMN);
    for span in spans {
        if out.len() >= limit {
            break;
        }
        let (first, last) = (usize::from(span.first), usize::from(span.last));
        let fill =
            |color: Rgba| Instance::solid(layout.cells_rect(r, first, last - first + 1), color);
        match span.mark {
            Mark::Selection => out.push(fill(theme.selection)),
            Mark::Match => out.push(fill(theme.search_match)),
            Mark::CurrentMatch => out.push(fill(theme.search_match_current)),
            Mark::Underline => push_underline(out, layout, palette, r, row, (first, last)),
        }
    }
    out.truncate(limit);
}

/// The link underline's bar under `cols` cells from `(r, col)`: one point
/// high at the underline position, inside the row.
pub(crate) fn underline_rect(layout: &Layout, r: usize, col: usize, cols: usize) -> [f32; 4] {
    let height = layout.stroke.min(layout.cell[1]).max(1);
    let top = layout
        .decoration
        .underline
        .min(layout.cell[1].saturating_sub(height));
    let [x, y, w, _] = layout.cells_rect(r, col, cols);
    [x, y + f32_from(top), w, f32_from(height)]
}

/// A link underline over `first..=last` of row `r`: one bar per run of
/// cells with the same foreground.
fn push_underline(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    r: usize,
    row: &[Cell],
    (first, last): (usize, usize),
) {
    let fg = |c: usize| {
        let style = row
            .get(c)
            .map_or(tessera_grid::Style::DEFAULT, |cell| cell.style);
        CellColors::foreground(palette, style)
    };
    let mut start = first;
    let mut color = fg(first);
    for c in first + 1..=last + 1 {
        let next = (c <= last).then(|| fg(c));
        if next == Some(color) {
            continue;
        }
        out.push(Instance::solid(
            underline_rect(layout, r, start, c - start),
            color,
        ));
        if let Some(next) = next {
            (start, color) = (c, next);
        }
    }
}

#[cfg(test)]
mod tests {
    use tessera_grid::{CellFlags, Color, Point, Style};

    use super::*;
    use crate::shaping::CellMetrics;
    use crate::size::PhysicalSize;

    fn wide_row() -> Vec<Cell> {
        // `a 中 b c`: the wide character at columns 1-2.
        let mut row = vec![Cell::new('a', Style::DEFAULT); 5];
        row[1] = Cell {
            flags: CellFlags::WIDE,
            ..Cell::new('中', Style::DEFAULT)
        };
        row[2] = Cell {
            flags: CellFlags::SPACER,
            ..Cell::BLANK
        };
        row
    }

    fn spans(
        selection: Option<Selection>,
        overlays: &[Overlay],
        cells: &[Cell],
        r: u16,
    ) -> Vec<Span> {
        let mut out = Vec::new();
        let on_row = overlays.iter().filter(|o| o.row == r);
        row_spans(
            &mut out,
            selection.as_ref(),
            on_row,
            (r, cells),
            cells.len(),
        );
        out
    }

    fn sel(first: u16, last: u16) -> Span {
        Span {
            mark: Mark::Selection,
            first,
            last,
        }
    }

    #[test]
    fn endpoints_on_either_half_take_the_whole_wide_character() {
        let row = wide_row();
        let linear = |a: u16, b: u16| Selection::linear(Point::new(0, a), Point::new(0, b));
        assert_eq!(
            spans(Some(linear(2, 4)), &[], &row, 0),
            [sel(1, 4)],
            "start on the spacer"
        );
        assert_eq!(
            spans(Some(linear(0, 1)), &[], &row, 0),
            [sel(0, 2)],
            "end on the wide half"
        );
        assert_eq!(
            spans(Some(linear(0, 2)), &[], &row, 0),
            [sel(0, 2)],
            "end on the spacer"
        );
        assert_eq!(spans(Some(linear(2, 2)), &[], &row, 0), [sel(1, 2)]);
        let block = Selection::block(Point::new(0, 3), Point::new(0, 2));
        assert_eq!(spans(Some(block), &[], &row, 0), [sel(1, 3)]);
    }

    #[test]
    fn selections_past_the_frame_are_clipped_without_panicking() {
        let row = wide_row();
        let far = Selection::linear(Point::new(0, 3), Point::new(u16::MAX, u16::MAX));
        assert_eq!(spans(Some(far), &[], &row, 0), [sel(3, 4)]);
        assert_eq!(
            spans(Some(far), &[], &row, 9),
            [sel(0, 4)],
            "inner rows whole"
        );
        let off = Selection::linear(Point::new(0, 40), Point::new(0, 50));
        assert_eq!(spans(Some(off), &[], &row, 0), []);
        let mut out = Vec::new();
        row_spans(&mut out, Some(&far), [], (0, &row), 0);
        assert_eq!(out, [], "no visible columns");
    }

    #[test]
    fn marks_are_ordered_selection_match_current_underline() {
        let row = wide_row();
        let overlays = [
            Overlay::new(OverlayKind::CurrentMatch, 0, 3..5),
            Overlay::new(OverlayKind::Match, 0, 2..3),
            Overlay::new(OverlayKind::Underline, 1, 0..2),
            Overlay::new(OverlayKind::Match, 0, 4..4),
            Overlay::new(OverlayKind::Match, 0, 9..20),
        ];
        let selection = Some(Selection::linear(Point::new(0, 0), Point::new(0, 0)));
        assert_eq!(
            spans(selection, &overlays, &row, 0),
            [
                sel(0, 0),
                Span {
                    mark: Mark::Match,
                    first: 1,
                    last: 2
                },
                Span {
                    mark: Mark::CurrentMatch,
                    first: 3,
                    last: 4
                },
            ],
            "empty and invisible overlays draw nothing; the current match is drawn last"
        );
        assert_eq!(
            spans(None, &overlays, &row, 1),
            [Span {
                mark: Mark::Underline,
                first: 0,
                last: 2
            }]
        );
    }

    /// Overlapping and adjacent spans of one mark become their union, so a
    /// cell is highlighted once however many matches cover it; different
    /// marks stay apart.
    #[test]
    fn each_marks_spans_are_merged_into_their_union() {
        let row = vec![Cell::new('a', Style::DEFAULT); 12];
        let m = |a, b| Overlay::new(OverlayKind::Match, 0, a..b);
        let overlays = [
            m(6, 8),
            m(0, 3),
            m(2, 4),
            m(4, 5),
            m(0, 1),
            Overlay::new(OverlayKind::CurrentMatch, 0, 2..4),
            m(10, 11),
        ];
        let span = |mark, first, last| Span { mark, first, last };
        assert_eq!(
            spans(None, &overlays, &row, 0),
            [
                span(Mark::Match, 0, 4),
                span(Mark::Match, 6, 7),
                span(Mark::Match, 10, 10),
                span(Mark::CurrentMatch, 2, 3),
            ]
        );
    }

    /// A flood of full-row overlays of every kind leaves one span per mark.
    #[test]
    fn a_flood_of_overlays_collapses_to_one_span_per_mark() {
        let row = wide_row();
        let kinds = [
            OverlayKind::Match,
            OverlayKind::CurrentMatch,
            OverlayKind::Underline,
        ];
        let flood: Vec<Overlay> = (0..10_000)
            .map(|i| Overlay::new(kinds[i % 3], 0, 0..u16::MAX))
            .collect();
        let selection = Some(Selection::linear(Point::new(0, 0), Point::new(0, 0)));
        let got = spans(selection, &flood, &row, 0);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[0], sel(0, 0), "the selection is drawn first");
    }

    /// The instances one row emits are bounded by its width even for a
    /// worst case: link underlines over cells whose foreground alternates
    /// (one bar per cell) on top of every other mark.
    #[test]
    fn a_rows_instances_are_bounded_by_its_width() {
        let palette = Palette::new(&Theme::default());
        let metrics = CellMetrics {
            width_px: 8,
            height_px: 16,
            baseline_px: 12.0,
        };
        let cols = 200;
        let layout = Layout::new(metrics, 1.0, 0.0, PhysicalSize::new(8 * cols, 16));
        let row: Vec<Cell> = (0..cols)
            .map(|c| {
                let fg = Color::Indexed(u8::try_from(c % 2).unwrap_or(0));
                Cell::new(
                    'a',
                    Style {
                        fg,
                        ..Style::DEFAULT
                    },
                )
            })
            .collect();
        let kinds = [
            OverlayKind::Match,
            OverlayKind::CurrentMatch,
            OverlayKind::Underline,
        ];
        let flood: Vec<Overlay> = (0..2400)
            .map(|i| {
                let start = u16::try_from(i % 7).unwrap_or(0);
                Overlay::new(kinds[i % 3], 0, start..u16::MAX)
            })
            .collect();
        let selection = Some(Selection::linear(Point::new(0, 0), Point::new(0, 199)));
        let spans = spans(selection, &flood, &row, 0);
        let mut out = Vec::new();
        push_row(
            &mut out,
            &layout,
            (&palette, &Theme::default()),
            0,
            &row,
            &spans,
        );
        let cols = cols as usize;
        assert!(
            out.len() <= 2 * cols + 3,
            "{} instances for {cols} columns",
            out.len()
        );
        assert!(out.len() >= cols, "every cell still gets its underline bar");
        // The guard: whatever the spans, a row never emits more than
        // INSTANCES_PER_COLUMN per column, also when the span that crosses
        // the limit is cut in the middle (150 bars each: the sixth ends
        // at 900).
        let many = vec![
            Span {
                mark: Mark::Underline,
                first: 0,
                last: 149,
            };
            50
        ];
        let mut out = Vec::new();
        push_row(
            &mut out,
            &layout,
            (&palette, &Theme::default()),
            0,
            &row,
            &many,
        );
        assert_eq!(out.len(), INSTANCES_PER_COLUMN * cols);
    }

    #[test]
    fn link_underlines_take_each_cells_foreground() {
        let palette = Palette::new(&Theme::default());
        let metrics = CellMetrics {
            width_px: 16,
            height_px: 30,
            baseline_px: 24.0,
        };
        let layout = Layout::new(metrics, 2.0, 0.0, PhysicalSize::new(160, 30));
        let red = Style {
            fg: Color::Indexed(1),
            ..Style::DEFAULT
        };
        let row = [
            Cell::new('a', Style::DEFAULT),
            Cell::new('b', Style::DEFAULT),
            Cell::new('c', red),
        ];
        let span = Span {
            mark: Mark::Underline,
            first: 0,
            last: 3,
        };
        let mut out = Vec::new();
        push_row(
            &mut out,
            &layout,
            (&palette, &Theme::default()),
            0,
            &row,
            &[span],
        );
        let bars: Vec<_> = out.iter().map(|i| (i.rect, i.color)).collect();
        let fg = |s| CellColors::foreground(&palette, s).premultiplied();
        assert_eq!(
            bars,
            [
                ([0.0, 26.0, 32.0, 2.0], fg(Style::DEFAULT)),
                ([32.0, 26.0, 16.0, 2.0], fg(red)),
                ([48.0, 26.0, 16.0, 2.0], fg(Style::DEFAULT)),
            ],
            "a cell past the row's end takes the default foreground"
        );
    }
}
