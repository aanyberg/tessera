import AppKit
import TesseraKit

/// The exit-status gutter's hover (#45b, ADR 0039): the core draws a mark in
/// the left padding beside each finished command's prompt row; resting the
/// pointer on it shows the command's exit status and how long it ran.
///
/// Each visible row's slice of the left padding is one AppKit tool-tip rect,
/// owned by the view, so AppKit times the tip, hides it when the pointer
/// moves to another row and asks again there. The text is built when AppKit
/// asks, from ``TesseraSession/commandMark(atRow:)``: the core answers from
/// the view's current scroll position, so a mark that scrolled away has no
/// tip, and a row without a mark gets none either. Nothing here is logged
/// (S8): a failed call shows no tip.
extension TerminalSurfaceView: NSViewToolTipOwner {
    /// The tool-tip rects of the gutter and what they were laid out for.
    struct GutterToolTips {
        /// The padding, cell height, rows and scale the rects were laid out
        /// for, and the view's height (they are placed from its bottom).
        struct Layout: Equatable {
            var paddingPx: UInt32
            var cellHeightPx: UInt32
            var rows: UInt16
            var scale: Float
            var height: CGFloat
        }

        var layout: Layout?
        /// One tag per visible row, top row first.
        var tags: [NSView.ToolTipTag] = []
        /// The rect each tag was added with, in view coordinates.
        var rects: [NSRect] = []
    }

    /// Lays out one tool-tip rect per visible row over the left padding,
    /// again only when the padding, cell height, row count, scale or the
    /// view's height changed. Without a surface, a cell size or a padding
    /// there are none.
    func updateGutterToolTips() {
        let layout = metrics.map {
            GutterToolTips.Layout(
                paddingPx: $0.paddingPx, cellHeightPx: $0.cellHeightPx, rows: $0.rows, scale: $0.scale,
                height: bounds.height)
        }
        guard layout != gutterToolTips.layout else { return }
        for tag in gutterToolTips.tags {
            removeToolTip(tag)
        }
        gutterToolTips = GutterToolTips(layout: layout)
        guard let layout, layout.scale > 0, layout.paddingPx > 0, layout.cellHeightPx > 0 else { return }
        let scale = CGFloat(layout.scale)
        let padding = CGFloat(layout.paddingPx) / scale
        let cellHeight = CGFloat(layout.cellHeightPx) / scale
        for row in 0..<layout.rows {
            let fromTop = padding + CGFloat(row) * cellHeight
            let y = isFlipped ? fromTop : bounds.height - fromTop - cellHeight
            let rect = NSRect(x: 0, y: y, width: padding, height: cellHeight)
            gutterToolTips.tags.append(addToolTip(rect, owner: self, userData: nil))
            gutterToolTips.rects.append(rect)
        }
    }

    /// AppKit asks for the tip of the gutter rect under the pointer: an
    /// empty string shows none.
    func view(
        _ view: NSView, stringForToolTip tag: NSView.ToolTipTag, point: NSPoint, userData data: UnsafeMutableRawPointer?
    ) -> String {
        gutterToolTip(at: point) ?? ""
    }

    /// The tip for the mark at `point` (view coordinates), or `nil` when the
    /// point is not in the left padding of a visible row or that row has no
    /// mark.
    func gutterToolTip(at point: NSPoint) -> String? {
        guard let metrics, metrics.cellHeightPx > 0 else { return nil }
        let (cell, pixels) = cell(at: point)
        let fromTop = pixels.y - CGFloat(metrics.paddingPx)
        guard pixels.x >= 0, pixels.x < CGFloat(metrics.paddingPx), fromTop >= 0,
            fromTop < CGFloat(metrics.rows) * CGFloat(metrics.cellHeightPx)
        else { return nil }
        guard let mark = try? session.commandMark(atRow: cell.row) else { return nil }
        return Self.gutterToolTip(for: mark)
    }

    /// The tip's text: the exit status on the first line, how long the
    /// command ran on the second.
    static func gutterToolTip(for mark: TesseraSession.CommandMark) -> String {
        let status = mark.exitCode.map { "Exit status \($0)" } ?? "Exit status unknown"
        let duration = mark.duration.map { "Ran for \(formatted($0))" } ?? "Duration unknown"
        return "\(status)\n\(duration)"
    }

    /// `duration` in the largest units that keep it short, the same in every
    /// locale: `850 ms`, `4.2 s`, `3 min 12 s`, `2 h 5 min`.
    static func formatted(_ duration: Duration) -> String {
        let (seconds, attoseconds) = duration.components
        guard seconds >= 0, attoseconds >= 0 else { return "0 ms" }
        let millis = seconds.multipliedReportingOverflow(by: 1000)
        guard !millis.overflow else { return "\(seconds / 3600) h" }
        let sum = millis.partialValue.addingReportingOverflow(attoseconds / 1_000_000_000_000_000)
        guard !sum.overflow else { return "\(seconds / 3600) h" }
        let total = sum.partialValue
        switch total {
        case ..<1000: return "\(total) ms"
        case ..<60_000:
            let tenths = total / 100
            return "\(tenths / 10).\(tenths % 10) s"
        case ..<3_600_000: return "\(total / 60_000) min \(total / 1000 % 60) s"
        default: return "\(total / 3_600_000) h \(total / 60_000 % 60) min"
        }
    }
}
