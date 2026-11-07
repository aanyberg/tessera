import AppKit
import TesseraKit

/// One pane of a tab: a session and the terminal surface that shows it
/// (ADR 0035). Panes compare by identity.
@MainActor
final class Pane: Equatable {
    let session: TesseraSession
    let view: TerminalSurfaceView
    /// The session's title as the core last reported it (already sanitised
    /// there, docs/security.md S3); empty until the session sets one.
    var title = ""

    init(session: TesseraSession) {
        self.session = session
        view = TerminalSurfaceView(session: session)
    }

    nonisolated static func == (lhs: Pane, rhs: Pane) -> Bool {
        lhs === rhs
    }
}

/// A tab's content view: lays out the tab's ``PaneTree`` with nested split
/// views, one per split, whose leaves are the panes' terminal surfaces
/// (ADR 0035).
///
/// After every change to the tree the split views are built again from it:
/// each pane's view moves to its new split view within the same window, so
/// its surface stays attached. Each split's ratio lives in the tree, which
/// only a divider drag changes; a split view that resizes lays its
/// children out from it, so the dividers do not drift. They keep every pane
/// at least ``minimumColumns`` × ``minimumRows`` cells; each pane's grid is
/// the whole cells its frame holds, like a window's, the rest going to the
/// padding band.
@MainActor
final class PaneContainerView: NSView, NSSplitViewDelegate {
    /// The smallest pane, in cells of its font (D1).
    static let minimumColumns = 10
    static let minimumRows = 3
    /// A cell's size in points while a pane's cell size is unknown (before
    /// its surface attaches): about Menlo 13 pt, the default font.
    static let fallbackCellSize = CGSize(width: 8, height: TerminalSurfaceView.fallbackLineHeight)

    private(set) var tree: PaneTree<Pane>

    init(pane: Pane, frame: NSRect) {
        tree = PaneTree(pane)
        super.init(frame: frame)
        rebuild()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// Top-left coordinates, as ``PaneTree`` uses (and `NSSplitView` does).
    override var isFlipped: Bool { true }

    /// The panes in reading order.
    var panes: [Pane] { tree.leaves }

    /// Each pane's frame in this view's (top-left) coordinates.
    var paneFrames: [(leaf: Pane, frame: CGRect)] {
        panes.map { ($0, convert($0.view.bounds, from: $0.view)) }
    }

    // MARK: Changing the tree

    /// Whether `pane` can be split toward `direction`: the tab holds fewer
    /// than ``PaneTree/maxLeaves`` panes and the tree with the new pane
    /// beside `pane` fits this view, every pane at its smallest size, the
    /// new one's from the font it opens with (``newPaneMinimumSize(splitting:)``).
    func canSplit(_ pane: Pane, _ direction: PaneSplitDirection) -> Bool {
        guard tree.contains(pane), panes.count < PaneTree<Pane>.maxLeaves else { return false }
        let added = newPaneMinimumSize(splitting: pane)
        let minimum = tree.minimumSize(divider: PaneSplitView.thickness) { leaf in
            let size = Self.minimumSize(of: leaf.view)
            return leaf == pane ? PaneTree<Pane>.combine(size, added, direction, divider: PaneSplitView.thickness) : size
        }
        return minimum.width <= bounds.width && minimum.height <= bounds.height
    }

    /// The smallest size of a pane about to open beside `pane`: it starts
    /// at the configured font, so that of a pane here at it (not zoomed).
    /// Without one it is estimated: the first pane's cells scaled back from
    /// its zoom to its actual size (a pixel more per cell, for the
    /// rounding), or the fallback cell size's, but never less than `pane`'s
    /// own smallest size. The actual size is not always the configured
    /// font (a `font_size` set to the zoom leaves the view zoomed, #100),
    /// so a split can be refused that would have fitted. A split can still
    /// be let through that does not fit after such an edit (ADR 0035, #315).
    private func newPaneMinimumSize(splitting pane: Pane) -> CGSize {
        let views = panes.map(\.view)
        if let view = views.first(where: { $0.metrics != nil && $0.zoomedFontSizePt == nil }) {
            return Self.minimumSize(of: view)
        }
        let estimate: CGSize
        if let view = views.first(where: { $0.metrics != nil }), let metrics = view.metrics,
            let configured = view.attachedFontSizePt, metrics.fontSizePt > 0
        {
            let factor = Double(configured / metrics.fontSizePt)
            var scaled = metrics
            scaled.cellWidthPx = UInt32((Double(metrics.cellWidthPx) * factor).rounded(.up)) + 1
            scaled.cellHeightPx = UInt32((Double(metrics.cellHeightPx) * factor).rounded(.up)) + 1
            estimate = Self.minimumSize(of: scaled, bar: Self.scrollBarWidth(of: view))
        } else {
            estimate = Self.minimumSize(of: nil, bar: 0)
        }
        let own = Self.minimumSize(of: pane.view)
        return CGSize(width: max(estimate.width, own.width), height: max(estimate.height, own.height))
    }

    /// Splits `pane` in two halves, `new` taking the right or the bottom
    /// one. Returns `false`, changing nothing, when ``canSplit(_:_:)`` says
    /// no or `new` is already a pane here.
    @discardableResult
    func split(_ pane: Pane, _ direction: PaneSplitDirection, adding new: Pane) -> Bool {
        guard canSplit(pane, direction), !tree.contains(new) else { return false }
        tree.split(pane, direction, adding: new)
        rebuild()
        return true
    }

    /// Takes `pane` out: its sibling takes the space of their split, and its
    /// view leaves the window. The last pane is never removed (`false`).
    @discardableResult
    func remove(_ pane: Pane) -> Bool {
        guard tree.remove(pane) else { return false }
        rebuild()
        return true
    }

    /// The pane next to `pane` toward `direction`, as laid out now, or `nil`
    /// at the edge of the tab. A pane with no visible size is passed over.
    func neighbour(of pane: Pane, toward direction: PaneDirection) -> Pane? {
        let frames = paneFrames
        guard let focused = frames.first(where: { $0.leaf == pane })?.frame else { return nil }
        let candidates = frames.filter { $0.leaf != pane && $0.frame.width >= 1 && $0.frame.height >= 1 }
        return PaneTree.neighbour(of: focused, toward: direction, among: candidates)
    }

    /// A pane's smallest size may have changed (its cells did: a zoom, a
    /// font or scale change; or its scroll bar's style): the window's
    /// smallest size follows, and the split views lay their panes out
    /// again so each keeps its smallest size where there is room.
    func paneMinimumSizeDidChange() {
        if let root = subviews.last, subviews.count == 1 {
            for splitView in Self.splitViews(from: root) {
                splitView.resizeSubviews(withOldSize: splitView.bounds.size)
            }
        }
        window?.contentMinSize = minimumSize
    }

    /// The smallest content size that keeps every pane at its smallest size.
    var minimumSize: CGSize {
        tree.minimumSize(divider: PaneSplitView.thickness) { Self.minimumSize(of: $0.view) }
    }

    /// The smallest size of a pane's view: ``minimumColumns`` ×
    /// ``minimumRows`` cells of its font, its padding on both sides, and a
    /// legacy scroll bar's width.
    static func minimumSize(of view: TerminalSurfaceView) -> CGSize {
        minimumSize(of: view.metrics, bar: scrollBarWidth(of: view))
    }

    /// The width a pane's scroll bar takes from its grid: a legacy one's.
    private static func scrollBarWidth(of view: TerminalSurfaceView) -> CGFloat {
        view.scroller.effectiveStyle == .legacy ? TerminalScroller.width(for: .legacy) : 0
    }

    /// The smallest size of a pane with `metrics`' cells (the fallback cell
    /// size without them) and a scroll bar `bar` wide.
    private static func minimumSize(of metrics: TesseraSurface.Metrics?, bar: CGFloat) -> CGSize {
        guard let metrics, metrics.scale > 0 else {
            return CGSize(
                width: CGFloat(minimumColumns) * fallbackCellSize.width + bar,
                height: CGFloat(minimumRows) * fallbackCellSize.height)
        }
        let scale = CGFloat(metrics.scale)
        let padding = 2 * CGFloat(metrics.paddingPx)
        let width = (padding + CGFloat(minimumColumns) * CGFloat(metrics.cellWidthPx)) / scale
        let height = (padding + CGFloat(minimumRows) * CGFloat(metrics.cellHeightPx)) / scale
        return CGSize(width: width.rounded(.up) + bar, height: height.rounded(.up))
    }

    // MARK: Layout

    /// The index in ``PaneTree/Node/splits`` of the split `splitView` shows:
    /// the split views mirror the tree's splits in pre-order. `nil` for a
    /// split view not (or no longer) in this view's tree.
    private func splitIndex(of splitView: NSSplitView) -> Int? {
        guard let root = subviews.last else { return nil }
        let splitViews = Self.splitViews(from: root)
        guard splitViews.count == tree.root.splits.count else { return nil }
        return splitViews.firstIndex { $0 === splitView }
    }

    /// The split views under `view` in pre-order.
    private static func splitViews(from view: NSView) -> [NSSplitView] {
        guard let splitView = view as? NSSplitView else { return [] }
        return [splitView] + splitView.subviews.flatMap { splitViews(from: $0) }
    }

    /// Builds the split views for the tree, moving each pane's view into
    /// its place while both are in the window (so no surface detaches), and
    /// then drops the old split views, with the view of a pane no longer in
    /// the tree.
    private func rebuild() {
        let old = subviews
        let root = install(tree.root, frame: bounds, into: self)
        root.autoresizingMask = [.width, .height]
        for view in old where view !== root && view.superview === self {
            view.removeFromSuperview()
        }
        // The ratios alone can leave a pane below its smallest size (its
        // font is larger than its sibling's): lay out again with the clamps.
        paneMinimumSizeDidChange()
    }

    @discardableResult
    private func install(_ node: PaneTree<Pane>.Node, frame: NSRect, into parent: NSView) -> NSView {
        switch node {
        case .leaf(let pane):
            let view = pane.view
            if view.superview !== parent {
                parent.addSubview(view)
            }
            view.autoresizingMask = []
            view.frame = frame
            return view
        case .split(let split):
            let splitView = PaneSplitView(frame: frame)
            splitView.isVertical = split.direction == .right
            splitView.dividerStyle = .thin
            splitView.delegate = self
            parent.addSubview(splitView)
            let (first, second) = PaneTree<Pane>.divide(
                NSRect(origin: .zero, size: frame.size), split.direction, ratio: split.ratio,
                divider: splitView.dividerThickness)
            install(split.first, frame: first, into: splitView)
            install(split.second, frame: second, into: splitView)
            return splitView
        }
    }

    /// The smallest size of `view`: a pane's, or a split's of its two
    /// children.
    private func minimumSize(of view: NSView) -> CGSize {
        if let terminal = view as? TerminalSurfaceView {
            return Self.minimumSize(of: terminal)
        }
        guard let splitView = view as? NSSplitView, splitView.subviews.count == 2 else { return .zero }
        return PaneTree<Pane>.combine(
            minimumSize(of: splitView.subviews[0]), minimumSize(of: splitView.subviews[1]),
            splitView.isVertical ? .right : .below, divider: splitView.dividerThickness)
    }

    // MARK: NSSplitViewDelegate

    /// A divider stops where the subtree before it would get smaller than
    /// its panes' smallest size.
    func splitView(
        _ splitView: NSSplitView, constrainMinCoordinate proposedMinimumPosition: CGFloat,
        ofSubviewAt dividerIndex: Int
    ) -> CGFloat {
        guard splitView.subviews.indices.contains(dividerIndex) else { return proposedMinimumPosition }
        let before = splitView.subviews[dividerIndex]
        let minimum = minimumSize(of: before)
        let limit = splitView.isVertical ? before.frame.minX + minimum.width : before.frame.minY + minimum.height
        return max(proposedMinimumPosition, limit)
    }

    /// And where the subtree after it would.
    func splitView(
        _ splitView: NSSplitView, constrainMaxCoordinate proposedMaximumPosition: CGFloat,
        ofSubviewAt dividerIndex: Int
    ) -> CGFloat {
        guard splitView.subviews.indices.contains(dividerIndex + 1) else { return proposedMaximumPosition }
        let after = splitView.subviews[dividerIndex + 1]
        let minimum = minimumSize(of: after)
        let limit =
            splitView.isVertical
            ? after.frame.maxX - minimum.width - splitView.dividerThickness
            : after.frame.maxY - minimum.height - splitView.dividerThickness
        return min(proposedMaximumPosition, limit)
    }

    /// A split view's new size (the window resized, or an outer divider
    /// moved) goes to its two children by ``firstLength(in:ratio:)``, from
    /// the split's ratio in the tree. AppKit's proportional default would
    /// shrink a child already at its smallest size, and a share read back
    /// from rounded frames would drift.
    func splitView(_ splitView: NSSplitView, resizeSubviewsWithOldSize oldSize: NSSize) {
        let views = splitView.subviews
        guard views.count == 2 else {
            splitView.adjustSubviews()
            return
        }
        let vertical = splitView.isVertical
        let ratio: CGFloat
        if let index = splitIndex(of: splitView) {
            ratio = tree.root.splits[index].ratio
        } else {
            // Not in the tree (yet): keep the share the frames have.
            let oldSpace = (vertical ? oldSize.width : oldSize.height) - splitView.dividerThickness
            let oldFirst = vertical ? views[0].frame.width : views[0].frame.height
            ratio = oldSpace > 0 ? min(max(oldFirst / oldSpace, 0), 1) : 0.5
        }
        let bounds = splitView.bounds
        let divider = splitView.dividerThickness
        let space = max(0, (vertical ? bounds.width : bounds.height) - divider)
        let first = firstLength(in: splitView, ratio: ratio)
        if vertical {
            views[0].frame = NSRect(x: 0, y: 0, width: first, height: bounds.height)
            views[1].frame = NSRect(x: first + divider, y: 0, width: space - first, height: bounds.height)
        } else {
            views[0].frame = NSRect(x: 0, y: 0, width: bounds.width, height: first)
            views[1].frame = NSRect(x: 0, y: first + divider, width: bounds.width, height: space - first)
        }
    }

    /// A divider drag sets its split's ratio in the tree to the share the
    /// drag left, so later resizes and rebuilds keep it. AppKit names a
    /// divider on a resize too, so a drag is told apart by the first
    /// child's length: a resize leaves the one ``firstLength(in:ratio:)``
    /// gives for the stored ratio, and keeps that ratio.
    func splitViewDidResizeSubviews(_ notification: Notification) {
        guard let splitView = notification.object as? NSSplitView, splitView.subviews.count == 2,
            let index = splitIndex(of: splitView)
        else { return }
        var ratios = tree.root.splits.map(\.ratio)
        let vertical = splitView.isVertical
        let first = vertical ? splitView.subviews[0].frame.width : splitView.subviews[0].frame.height
        let space = (vertical ? splitView.bounds.width : splitView.bounds.height) - splitView.dividerThickness
        guard space > 0, abs(first - firstLength(in: splitView, ratio: ratios[index])) >= 0.5 else { return }
        ratios[index] = first / space
        tree.setRatios(ratios)
    }

    /// The length of a split's first child at `ratio` of the space the
    /// divider leaves, rounded to the nearest point (as
    /// ``PaneTree/divide(_:_:ratio:divider:)`` does), except that neither
    /// child gets smaller than its panes' smallest size: the other gives way.
    private func firstLength(in splitView: NSSplitView, ratio: CGFloat) -> CGFloat {
        let vertical = splitView.isVertical
        let space = max(0, (vertical ? splitView.bounds.width : splitView.bounds.height) - splitView.dividerThickness)
        let firstMinimum = minimumSize(of: splitView.subviews[0])
        let secondMinimum = minimumSize(of: splitView.subviews[1])
        var first = (space * ratio).rounded()
        first = min(first, space - (vertical ? secondMinimum.width : secondMinimum.height))
        first = max(first, vertical ? firstMinimum.width : firstMinimum.height)
        return min(max(first, 0), space)
    }

    // MARK: Actions

    /// ⌘W (Close Tab's action, sent up the responder chain from the focused
    /// pane): closes the focused pane, or the tab when it is the last one.
    /// The tab's close button and AppKit's tab-bar closes reach the window
    /// directly and close the whole tab.
    @objc func performClose(_ sender: Any?) {
        if let controller = window?.windowController as? TerminalWindowController {
            controller.closeFocusedPane(sender)
        } else {
            window?.performClose(sender)
        }
    }
}

/// A split of a tab's panes, with a thin divider of a fixed thickness, so
/// the tree's geometry knows it.
@MainActor
final class PaneSplitView: NSSplitView {
    static let thickness: CGFloat = 1

    override var dividerThickness: CGFloat { Self.thickness }
}
