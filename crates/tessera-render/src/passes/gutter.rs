//! After the cursor: the exit-status marks in the left padding (#45b).
//!
//! One solid bar per [`crate::GutterMark`]: as tall as its row, half the
//! left padding wide (at least a pixel) and centred in it, in
//! [`Theme::mark_color`]. Clipped to the padding column and the target, so
//! a mark never covers a cell. A mark on a row the frame does not have or
//! that starts below the target draws nothing, and so does every mark when
//! the padding is zero. The bars are rebuilt every frame (at most one per
//! visible row), so they take no part in the per-row damage.

use super::{Instance, Layout, f32_from};
use crate::frame::FrameInput;
use crate::theme::Theme;

/// Appends the gutter marks of `frame` to `out`: at most one per row that
/// starts inside the target, whatever `frame.gutter` holds.
pub(crate) fn push(
    out: &mut Vec<Instance>,
    layout: &Layout,
    theme: &Theme,
    frame: &FrameInput<'_>,
) {
    let pad = layout.origin[0];
    if frame.gutter.is_empty() || pad == 0 {
        return;
    }
    let rows = frame.rows.len().min(layout.visible_rows());
    let width = (pad / 2).max(1);
    let x = f32_from((pad - width) / 2);
    let clip = [0.0, 0.0, f32_from(pad), f32_from(layout.target.height)];
    // At most one bar per row: a producer that repeats a row (or floods
    // the slice) draws no more than the screen holds.
    let mut last = None;
    for mark in frame.gutter {
        let row = usize::from(mark.row);
        if row >= rows || last.is_some_and(|last| row <= last) {
            continue;
        }
        last = Some(row);
        let [_, y] = layout.cell_origin(row, 0);
        let rect = [x, y, f32_from(width), f32_from(layout.cell[1])];
        out.push(Instance::solid(rect, theme.mark_color(mark.status)).clipped(clip));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CommandStatus, GutterMark};
    use crate::shaping::CellMetrics;
    use crate::size::PhysicalSize;

    fn layout(padding: f32, rows: u32) -> Layout {
        let metrics = CellMetrics {
            width_px: 8,
            height_px: 16,
            baseline_px: 12.0,
        };
        let pad = super::super::px_from(padding * 2.0);
        let size = PhysicalSize::new(80 + 2 * pad, rows * 16 + 2 * pad);
        Layout::new(metrics, 2.0, padding, size)
    }

    fn push_all(layout: &Layout, rows: usize, marks: &[GutterMark]) -> Vec<Instance> {
        let cells: Vec<&[tessera_grid::Cell]> = vec![&[]; rows];
        let frame = FrameInput::new(&cells).with_gutter(marks);
        let mut out = Vec::new();
        push(&mut out, layout, &Theme::default(), &frame);
        out
    }

    #[test]
    fn a_mark_is_a_bar_centred_in_the_padding() {
        let layout = layout(4.0, 3);
        let out = push_all(&layout, 3, &[GutterMark::new(1, CommandStatus::Failure)]);
        assert_eq!(out.len(), 1);
        // Padding 8 px: a 4 px bar from x = 2, on row 1 (y = 8 + 16).
        assert_eq!(out[0].rect, [2.0, 24.0, 4.0, 16.0]);
        assert_eq!(out[0].clip[2], 8.0, "clipped to the padding");
        let red = Theme::default().ansi[1].premultiplied();
        assert_eq!(out[0].color, red);
    }

    #[test]
    fn marks_outside_the_rows_or_target_and_repeats_draw_nothing() {
        let layout = layout(4.0, 2);
        let marks = [
            GutterMark::new(0, CommandStatus::Success),
            GutterMark::new(0, CommandStatus::Failure),
            GutterMark::new(1, CommandStatus::Unknown),
            GutterMark::new(3, CommandStatus::Success),
            GutterMark::new(u16::MAX, CommandStatus::Success),
        ];
        // Two rows and the bottom padding: row 2 would start in the
        // padding, row 3 below the target.
        let out = push_all(&layout, 5, &marks);
        assert_eq!(
            out.len(),
            2,
            "rows 0 and 1 once each; 3 is below the target"
        );
        assert_eq!(out.len(), push_all(&layout, 2, &marks).len());
        assert_eq!(
            push_all(&layout, 1, &marks).len(),
            1,
            "the frame has one row"
        );
        let flood = vec![GutterMark::new(1, CommandStatus::Success); 100_000];
        assert_eq!(push_all(&layout, 2, &flood).len(), 1);
    }

    #[test]
    fn no_padding_draws_no_marks() {
        let layout = layout(0.0, 2);
        assert_eq!(
            push_all(&layout, 2, &[GutterMark::new(0, CommandStatus::Success)]).len(),
            0
        );
        let thin = self::layout(0.5, 2);
        let out = push_all(&thin, 2, &[GutterMark::new(0, CommandStatus::Success)]);
        assert_eq!(
            out[0].rect[..3],
            [0.0, 1.0, 1.0],
            "one pixel of padding: a one pixel bar"
        );
    }
}
