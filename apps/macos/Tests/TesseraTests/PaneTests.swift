import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The pane tree's rules without AppKit (ADR 0035, D1): splitting, closing,
/// sizes and spatial neighbours.
@MainActor
final class PaneTreeTests: XCTestCase {
    private typealias Tree = PaneTree<Int>

    func testSplitsAddTheNewPaneAfterTheSplitOneAndRefuseDuplicatesAndAFullTree() {
        var tree = Tree(0)
        XCTAssertTrue(tree.split(0, .right, adding: 1))
        XCTAssertTrue(tree.split(0, .below, adding: 2))
        XCTAssertEqual(tree.leaves, [0, 2, 1], "reading order: the left column first, top to bottom")
        XCTAssertFalse(tree.split(7, .right, adding: 3), "no such pane")
        XCTAssertFalse(tree.split(1, .right, adding: 2), "2 is a pane already")
        for leaf in 3..<Tree.maxLeaves {
            XCTAssertTrue(tree.split(leaf - 1, .right, adding: leaf), "pane \(leaf + 1)")
        }
        XCTAssertEqual(tree.leaves.count, 16)
        XCTAssertFalse(tree.split(0, .right, adding: 99), "the 17th pane is refused")
        XCTAssertEqual(tree.leaves.count, 16)
    }

    func testRemovingAPaneGivesItsSiblingTheSplitsPlaceAndNeverRemovesTheLast() {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(1, .below, adding: 2)
        tree.setRatios([0.25, 0.75])
        XCTAssertTrue(tree.remove(1))
        XCTAssertEqual(tree.leaves, [0, 2])
        guard case .split(let split) = tree.root else { return XCTFail("a split stays") }
        XCTAssertEqual(split.direction, .right, "2 took 1's split's place, beside 0")
        XCTAssertEqual(split.ratio, 0.25, "with the outer split's ratio")
        XCTAssertFalse(tree.remove(5), "no such pane")
        XCTAssertTrue(tree.remove(0))
        XCTAssertEqual(tree.leaves, [2])
        XCTAssertFalse(tree.remove(2), "the last pane stays")
        XCTAssertEqual(tree.leaves, [2])
    }

    func testRatiosAreSetInPreOrderAndClamped() {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(0, .below, adding: 2)
        tree.split(1, .below, adding: 3)
        tree.setRatios([2, 0.3, -1])
        XCTAssertEqual(tree.root.splits.map(\.ratio), [1, 0.3, 0])
        tree.setRatios([0.5])
        XCTAssertEqual(tree.root.splits.map(\.ratio), [0.5, 0.3, 0], "splits past the list keep theirs")
    }

    func testFramesShareTheSpaceTheDividersLeave() {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(0, .below, adding: 2)
        let frames = Dictionary(
            uniqueKeysWithValues: tree.frames(in: CGRect(x: 0, y: 0, width: 201, height: 101), divider: 1).map {
                ($0.leaf, $0.frame)
            })
        XCTAssertEqual(frames[0], CGRect(x: 0, y: 0, width: 100, height: 50))
        XCTAssertEqual(frames[2], CGRect(x: 0, y: 51, width: 100, height: 50))
        XCTAssertEqual(frames[1], CGRect(x: 101, y: 0, width: 100, height: 101))
    }

    func testTheMinimumSizeAddsUpAlongEachSplitAndTakesTheLargestAcrossIt() {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(1, .below, adding: 2)
        let size = tree.minimumSize(divider: 1) { $0 == 0 ? CGSize(width: 30, height: 10) : CGSize(width: 20, height: 12) }
        XCTAssertEqual(size, CGSize(width: 30 + 1 + 20, height: 12 + 1 + 12))
    }

    /// 0 | 1
    /// --+--
    /// 2 | 3
    func testNeighboursInATwoByTwoLayoutAreTheSpatialOnes() {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(0, .below, adding: 2)
        tree.split(1, .below, adding: 3)
        let frames = tree.frames(in: CGRect(x: 0, y: 0, width: 201, height: 101), divider: 1)
        func neighbour(_ leaf: Int, _ direction: PaneDirection) -> Int? {
            let focused = frames.first { $0.leaf == leaf }!.frame
            return Tree.neighbour(of: focused, toward: direction, among: frames.filter { $0.leaf != leaf })
        }
        XCTAssertEqual(neighbour(0, .right), 1)
        XCTAssertEqual(neighbour(0, .down), 2)
        XCTAssertNil(neighbour(0, .left))
        XCTAssertNil(neighbour(0, .up))
        XCTAssertEqual(neighbour(3, .left), 2)
        XCTAssertEqual(neighbour(3, .up), 1)
        XCTAssertNil(neighbour(3, .right))
        XCTAssertNil(neighbour(3, .down))
        XCTAssertEqual(neighbour(1, .left), 0)
        XCTAssertEqual(neighbour(1, .down), 3)
        XCTAssertEqual(neighbour(2, .up), 0)
        XCTAssertEqual(neighbour(2, .right), 3)
    }

    /// A column on the left beside three stacked panes: moving left from
    /// any of them reaches the column; moving right from the column, the
    /// pane it overlaps most.
    func testANeighbourIsTheNearestThenTheOneThatOverlapsMost() {
        let frames: [(leaf: Int, frame: CGRect)] = [
            (0, CGRect(x: 0, y: 0, width: 100, height: 300)),
            (1, CGRect(x: 101, y: 0, width: 100, height: 49)),
            (2, CGRect(x: 101, y: 50, width: 100, height: 200)),
            (3, CGRect(x: 101, y: 251, width: 100, height: 49)),
            (4, CGRect(x: 202, y: 0, width: 100, height: 300)),
        ]
        func neighbour(_ leaf: Int, _ direction: PaneDirection) -> Int? {
            let focused = frames.first { $0.leaf == leaf }!.frame
            return Tree.neighbour(of: focused, toward: direction, among: frames.filter { $0.leaf != leaf })
        }
        XCTAssertEqual(neighbour(0, .right), 2, "the nearest column's pane that overlaps most, not 4 further on")
        XCTAssertEqual(neighbour(4, .left), 2)
        for leaf in 1...3 {
            XCTAssertEqual(neighbour(leaf, .left), 0)
            XCTAssertEqual(neighbour(leaf, .right), 4)
        }
        XCTAssertEqual(neighbour(1, .down), 2)
        XCTAssertEqual(neighbour(3, .up), 2)
        XCTAssertNil(neighbour(1, .up))
        let diagonal: [(leaf: Int, frame: CGRect)] = [(1, CGRect(x: 101, y: 51, width: 100, height: 50))]
        XCTAssertNil(
            Tree.neighbour(of: CGRect(x: 0, y: 0, width: 100, height: 50), toward: .right, among: diagonal),
            "a pane only diagonally across is no neighbour")
    }
}

/// Split panes in a tab (ADR 0035, D1, D2): splitting spawns a session per
/// pane, the focus moves spatially, ⌘W closes the focused pane, and closing
/// the tab asks once for all its busy panes. Sessions run an interactive
/// `/bin/sh`.
@MainActor
final class PaneTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var presenter: RecordingCloseConfirmationPresenter!
    private var previousPresenter: (any CloseConfirmationPresenter)?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        presenter = RecordingCloseConfirmationPresenter()
        previousPresenter = appDelegate.closeConfirmationPresenter
        appDelegate.closeConfirmationPresenter = presenter
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        appDelegate.closeConfirmationPresenter = previousPresenter
        appDelegate = nil
        presenter = nil
    }

    /// A tab whose content area is `size`.
    private func openTab(_ size: NSSize = NSSize(width: 1200, height: 800)) throws -> TerminalWindowController {
        let controller = try appDelegate.newWindow()
        controller.window?.setContentSize(size)
        controller.paneContainer.layoutSubtreeIfNeeded()
        return controller
    }

    private func split(_ controller: TerminalWindowController, _ direction: PaneSplitDirection) throws -> Pane {
        let pane = try XCTUnwrap(controller.splitFocusedPane(direction), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        return pane
    }

    /// 0 | 1
    /// --+--
    /// 2 | 3
    private func openTwoByTwo() throws -> (TerminalWindowController, [Pane]) {
        let controller = try openTab()
        let top = controller.focusedPane
        let topRight = try split(controller, .right)
        controller.focus(top)
        let bottomLeft = try split(controller, .below)
        controller.focus(topRight)
        let bottomRight = try split(controller, .below)
        return (controller, [top, topRight, bottomLeft, bottomRight])
    }

    private func frame(of pane: Pane, in controller: TerminalWindowController) -> CGRect {
        controller.paneContainer.convert(pane.view.bounds, from: pane.view)
    }

    // MARK: Splitting

    func testSplittingRightAndBelowEachStartASessionAndAView() throws {
        let controller = try openTab()
        let first = controller.focusedPane
        let attaches = first.view.attachAttemptCount
        let right = try split(controller, .right)
        XCTAssertEqual(controller.panes, [first, right])
        XCTAssertNotEqual(right.session.id, first.session.id, "a session of its own")
        XCTAssertTrue(right.view.window === controller.window, "its view is in the tab")
        XCTAssertTrue(controller.focusedPane === right, "the new pane has the focus")
        XCTAssertTrue(controller.window?.firstResponder === right.view)
        let left = frame(of: first, in: controller)
        let rightFrame = frame(of: right, in: controller)
        XCTAssertEqual(rightFrame.minX, left.maxX + PaneSplitView.thickness, "beside it, behind a divider")
        XCTAssertEqual(rightFrame.minY, left.minY)
        XCTAssertEqual(rightFrame.height, left.height)
        let halves = PaneTree<Pane>.divide(
            controller.paneContainer.bounds, .right, ratio: 0.5, divider: PaneSplitView.thickness)
        XCTAssertEqual(left, halves.first, "the split pane keeps the left half")
        XCTAssertEqual(rightFrame, halves.second, "the new one takes the right half")

        let below = try split(controller, .below)
        XCTAssertEqual(controller.panes, [first, right, below])
        XCTAssertEqual(Set(controller.sessions.map(\.id)).count, 3)
        let belowFrame = frame(of: below, in: controller)
        let upper = frame(of: right, in: controller)
        XCTAssertEqual(belowFrame.minY, upper.maxY + PaneSplitView.thickness, "under the split pane")
        XCTAssertEqual(belowFrame.minX, upper.minX)
        XCTAssertNoThrow(try below.session.resize(rows: 24, cols: 80), "the new session is live")
        XCTAssertEqual(first.view.attachAttemptCount, attaches, "moving between split views kept the surface")
    }

    func testADraggedDividerKeepsItsPlaceWhenAnotherPaneSplits() throws {
        let controller = try openTab()
        let left = controller.focusedPane
        _ = try split(controller, .right)
        let splitView = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        splitView.setPosition(300, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertEqual(frame(of: left, in: controller).width, 300, "the drag moved the divider")
        // The right pane splits: the split views are built again.
        _ = try split(controller, .below)
        XCTAssertEqual(frame(of: left, in: controller).width, 300, "the dragged divider stays")
        controller.focus(left)
        _ = try split(controller, .below)
        XCTAssertEqual(frame(of: left, in: controller).width, 300, "and stays after a split on its other side")
    }

    func testEachPanesGridIsTheWholeCellsOfItsFrame() async throws {
        let (controller, panes) = try openTwoByTwo()
        guard panes.allSatisfy(\.view.surface.isAttached) else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        controller.window?.setContentSize(NSSize(width: 1003, height: 717))
        controller.paneContainer.layoutSubtreeIfNeeded()
        // The panes tile the tab, the dividers between them.
        let bounds = controller.paneContainer.bounds
        let frames = panes.map { frame(of: $0, in: controller) }
        XCTAssertEqual(frames[0].width + PaneSplitView.thickness + frames[1].width, bounds.width)
        XCTAssertEqual(frames[2].width + PaneSplitView.thickness + frames[3].width, bounds.width)
        XCTAssertEqual(frames[0].height + PaneSplitView.thickness + frames[2].height, bounds.height)
        XCTAssertEqual(frames[1].height + PaneSplitView.thickness + frames[3].height, bounds.height)
        for pane in panes {
            let view = pane.view
            let metrics = try XCTUnwrap(view.metrics)
            let scale = CGFloat(metrics.scale)
            let bar = view.scroller.effectiveStyle == .legacy ? TerminalScroller.width(for: .legacy) : 0
            XCTAssertEqual(view.renderView.frame.width, view.bounds.width - bar)
            XCTAssertEqual(metrics.widthPx, UInt32((view.renderView.bounds.width * scale).rounded()), "the frame's width")
            XCTAssertEqual(metrics.heightPx, UInt32((view.bounds.height * scale).rounded()), "the frame's height")
            let padding = 2 * metrics.paddingPx
            XCTAssertEqual(UInt32(metrics.cols), (metrics.widthPx - padding) / metrics.cellWidthPx, "whole columns")
            XCTAssertEqual(UInt32(metrics.rows), (metrics.heightPx - padding) / metrics.cellHeightPx, "whole rows")
        }
    }

    func testTheSeventeenthSplitIsRefused() throws {
        let controller = try openTab(NSSize(width: 1600, height: 1000))
        for direction in [PaneSplitDirection.right, .right, .below, .below] {
            for pane in controller.panes {
                controller.focus(pane)
                _ = try split(controller, direction)
            }
        }
        XCTAssertEqual(controller.panes.count, PaneTree<Pane>.maxLeaves)
        let sessions = Set(controller.sessions.map(\.id))
        XCTAssertFalse(controller.paneContainer.canSplit(controller.focusedPane, .right))
        XCTAssertNil(try controller.splitFocusedPane(.right))
        XCTAssertNil(try controller.splitFocusedPane(.below))
        XCTAssertEqual(controller.panes.count, 16)
        XCTAssertEqual(Set(controller.sessions.map(\.id)), sessions)
    }

    /// Splitting again and again: a split that would leave a half smaller
    /// than 10 × 3 cells is refused, and no pane ever is.
    func testASplitBelowTheSmallestPaneIsRefused() throws {
        for direction in [PaneSplitDirection.right, .below] {
            let controller = try openTab(NSSize(width: 420, height: 260))
            var refused = false
            for _ in 0..<8 {
                let pane = controller.focusedPane
                let before = controller.panes.count
                if try controller.splitFocusedPane(direction) == nil {
                    refused = true
                    XCTAssertEqual(controller.panes.count, before)
                    let size = pane.view.frame.size
                    let minimum = PaneContainerView.minimumSize(of: pane.view)
                    let half = ((direction == .right ? size.width : size.height) - PaneSplitView.thickness) / 2
                    XCTAssertLessThan(half.rounded(.down), direction == .right ? minimum.width : minimum.height)
                    break
                }
                controller.paneContainer.layoutSubtreeIfNeeded()
            }
            XCTAssertTrue(refused, "\(direction): a split is refused before eight")
            if controller.panes.allSatisfy(\.view.surface.isAttached) {
                try assertEveryPaneHoldsTheSmallestGrid(controller, "\(direction)")
            }
            for pane in controller.panes {
                let minimum = PaneContainerView.minimumSize(of: pane.view)
                XCTAssertGreaterThanOrEqual(pane.view.frame.width, minimum.width, "\(direction)")
                XCTAssertGreaterThanOrEqual(pane.view.frame.height, minimum.height, "\(direction)")
            }
            let minimum = controller.paneContainer.minimumSize
            XCTAssertEqual(controller.window?.contentMinSize, minimum, "the window cannot shrink the panes below it")
            controller.window?.close()
        }
    }

    /// L | (R1 | R2): R1 at its smallest, then the outer divider dragged
    /// as far right as it goes. R1 keeps its size; R2 gives way.
    func testANestedPaneAtItsSmallestSizeKeepsItWhenTheOuterDividerMoves() throws {
        let controller = try openTab()
        _ = try split(controller, .right)
        _ = try split(controller, .right)
        try skipUnlessAttached(controller)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        let inner = try XCTUnwrap(outer.subviews.last as? NSSplitView)
        inner.setPosition(PaneContainerView.minimumSize(of: controller.panes[1].view).width, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "R1 at its smallest")
        let limit = controller.paneContainer.splitView(
            outer, constrainMaxCoordinate: outer.bounds.width, ofSubviewAt: 0)
        outer.setPosition(limit, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "the outer divider at its limit")
    }

    /// L | (T over B): T at its smallest height, then the window shrunk to
    /// its smallest size.
    func testAPaneAtItsSmallestSizeKeepsItWhenTheWindowShrinks() throws {
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        _ = try split(controller, .right)
        _ = try split(controller, .below)
        try skipUnlessAttached(controller)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        let inner = try XCTUnwrap(outer.subviews.last as? NSSplitView)
        inner.setPosition(PaneContainerView.minimumSize(of: controller.panes[1].view).height, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "T at its smallest")
        window.setContentSize(window.contentMinSize)
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "the window at its smallest")
    }

    /// The 2x2 tab with its right column at its smallest, and another tab
    /// of the window, with one pane, shrunk to its own smallest size:
    /// switching back gives the 2x2 tab its smallest size, and every pane
    /// keeps its own.
    func testSwitchingBackFromASmallerTabKeepsEveryPanesSmallestSize() throws {
        let (controller, _) = try openTwoByTwo()
        let window = try XCTUnwrap(controller.window)
        try skipUnlessAttached(controller)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        let limit = controller.paneContainer.splitView(
            outer, constrainMaxCoordinate: outer.bounds.width, ofSubviewAt: 0)
        outer.setPosition(limit, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "the right column at its smallest")
        let other = try XCTUnwrap(appDelegate.newTab(in: window).window)
        other.tabGroup?.selectedWindow = other
        other.setContentSize(other.contentMinSize)
        window.tabGroup?.selectedWindow = window
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertLessThan(window.frame.width, 1200, "the tab took the smaller tab's frame, down to its smallest size")
        try assertEveryPaneHoldsTheSmallestGrid(controller, "back in the 2x2 tab")
    }

    /// Two panes side by side, the window narrowed and widened again: the
    /// divider comes back to where it was, each cycle, in large steps and
    /// in single points (no drift from rounding the shares).
    func testTheDividerComesBackAfterTheWindowNarrowsAndWidens() throws {
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        let left = controller.focusedPane
        _ = try split(controller, .right)
        func cycle(from wide: CGFloat, to narrow: CGFloat, by step: CGFloat) {
            for width in stride(from: wide, through: narrow, by: -step) {
                window.setContentSize(NSSize(width: width, height: 800))
            }
            for width in stride(from: narrow, through: wide, by: step) {
                window.setContentSize(NSSize(width: width, height: 800))
            }
            controller.paneContainer.layoutSubtreeIfNeeded()
        }
        let start = frame(of: left, in: controller).width
        cycle(from: 1200, to: 900, by: 4)
        XCTAssertEqual(frame(of: left, in: controller).width, start, accuracy: 1, "after a cycle in 4 pt steps")
        window.setContentSize(NSSize(width: 1000, height: 800))
        let halves = PaneTree<Pane>.divide(
            controller.paneContainer.bounds, .right, ratio: 0.5, divider: PaneSplitView.thickness)
        XCTAssertEqual(
            frame(of: left, in: controller).width, halves.first.width,
            "the stored ratio, rounded to the nearest point as a split lays it out")
        window.setContentSize(NSSize(width: 1201, height: 800))
        controller.paneContainer.layoutSubtreeIfNeeded()
        let wide = frame(of: left, in: controller).width
        for round in 1...2 {
            cycle(from: 1201, to: 801, by: 1)
            XCTAssertEqual(
                frame(of: left, in: controller).width, wide, accuracy: 1, "after cycle \(round) in 1 pt steps")
        }
    }

    /// L | (R1 | R2), the outer divider dragged left and back five times:
    /// the inner divider comes back each time.
    func testANestedDividerComesBackAfterTheOuterDividerMovesAndReturns() throws {
        let controller = try openTab()
        _ = try split(controller, .right)
        let rightOne = controller.focusedPane
        _ = try split(controller, .right)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        outer.setPosition(600, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        let start = frame(of: rightOne, in: controller).width
        for drag in 1...5 {
            for position in stride(from: 600, through: 400, by: -3) {
                outer.setPosition(CGFloat(position), ofDividerAt: 0)
            }
            for position in stride(from: 400, through: 600, by: 3) {
                outer.setPosition(CGFloat(position), ofDividerAt: 0)
            }
            outer.setPosition(600, ofDividerAt: 0)
            controller.paneContainer.layoutSubtreeIfNeeded()
            XCTAssertEqual(frame(of: rightOne, in: controller).width, start, accuracy: 1, "after drag \(drag)")
        }
    }

    /// A narrow tab whose pane is zoomed out, split twice from that pane:
    /// the new panes open at the configured font, so the split is judged by
    /// it, and no pane ends up below its smallest size.
    func testSplitsOfAZoomedOutPaneJudgeTheNewPaneByTheConfiguredFont() throws {
        for direction in [PaneSplitDirection.right, .below] {
            let size = direction == .right ? NSSize(width: 200, height: 260) : NSSize(width: 500, height: 120)
            let controller = try openTab(size)
            let zoomed = controller.focusedPane
            try skipUnlessAttached(controller)
            for _ in 0..<9 { zoomed.view.decreaseFontSize(nil) }
            for _ in 0..<2 {
                controller.focus(zoomed)
                guard try controller.splitFocusedPane(direction) != nil else { break }
                controller.paneContainer.layoutSubtreeIfNeeded()
            }
            try skipUnlessAttached(controller)
            controller.paneContainer.layoutSubtreeIfNeeded()
            assertEveryPaneHoldsItsSmallestSize(controller, "\(direction)")
            try assertEveryPaneHoldsTheSmallestGrid(controller, "\(direction)")
            let window = try XCTUnwrap(controller.window)
            XCTAssertLessThanOrEqual(
                window.contentMinSize.width, controller.paneContainer.bounds.width, "\(direction): the panes fit")
            XCTAssertLessThanOrEqual(
                window.contentMinSize.height, controller.paneContainer.bounds.height, "\(direction): the panes fit")
            window.close()
        }
    }

    /// A single pane zoomed in to 25 pt in a tab a little narrower (or
    /// lower) than two such panes: the new pane may open at that size too
    /// (config.toml's `font_size` set to the zoom leaves the view zoomed,
    /// its actual size the old one, #100), so the split is judged by the
    /// larger of the zoomed pane's own size and the estimate scaled back
    /// to its actual size, and refused.
    func testASplitOfAZoomedInPaneIsJudgedByItsOwnSizeToo() throws {
        for direction in [PaneSplitDirection.right, .below] {
            let controller = try openTab()
            let view = controller.focusedPane.view
            try skipUnlessAttached(controller)
            for _ in 0..<12 { view.increaseFontSize(nil) }
            XCTAssertNotNil(view.zoomedFontSizePt, "\(direction): zoomed")
            let one = PaneContainerView.minimumSize(of: view)
            let size =
                direction == .right
                ? NSSize(width: one.width * 2 + PaneSplitView.thickness - 4, height: one.height + 20)
                : NSSize(width: one.width + 20, height: one.height * 2 + PaneSplitView.thickness - 4)
            let window = try XCTUnwrap(controller.window)
            window.setContentSize(size)
            controller.paneContainer.layoutSubtreeIfNeeded()
            XCTAssertNil(try controller.splitFocusedPane(direction), "\(direction): refused")
            XCTAssertEqual(controller.panes.count, 1, "\(direction)")
            window.close()
        }
    }

    /// A single pane zoomed out, in a tab with room for two zoomed-out
    /// panes but not for one beside a pane at the configured font: the
    /// split pane's own size alone would let it through, the estimate
    /// refuses it.
    func testASplitOfALoneZoomedOutPaneIsJudgedByTheConfiguredFont() throws {
        for direction in [PaneSplitDirection.right, .below] {
            let controller = try openTab()
            let view = controller.focusedPane.view
            try skipUnlessAttached(controller)
            let configured = PaneContainerView.minimumSize(of: view)
            for _ in 0..<9 { view.decreaseFontSize(nil) }
            XCTAssertNotNil(view.zoomedFontSizePt, "\(direction): zoomed")
            let one = PaneContainerView.minimumSize(of: view)
            let thickness = PaneSplitView.thickness
            let size =
                direction == .right
                ? NSSize(width: one.width + configured.width + thickness - 4, height: configured.height + 20)
                : NSSize(width: configured.width + 20, height: one.height + configured.height + thickness - 4)
            let along = direction == .right ? (size.width, one.width) : (size.height, one.height)
            XCTAssertGreaterThanOrEqual(along.0, along.1 * 2 + thickness, "\(direction): two zoomed-out panes fit")
            let window = try XCTUnwrap(controller.window)
            window.setContentSize(size)
            controller.paneContainer.layoutSubtreeIfNeeded()
            XCTAssertNil(try controller.splitFocusedPane(direction), "\(direction): refused")
            XCTAssertEqual(controller.panes.count, 1, "\(direction)")
            window.close()
        }
    }

    /// Two panes, one zoomed in a little or a lot: the window's smallest
    /// size follows, so at that size every pane keeps its smallest size.
    func testZoomingInAPaneRaisesTheWindowsSmallestSize() throws {
        for steps in [7, 27] {
            let controller = try openTab(NSSize(width: 1200, height: 600))
            let left = controller.focusedPane
            _ = try split(controller, .right)
            try skipUnlessAttached(controller)
            let window = try XCTUnwrap(controller.window)
            controller.focus(left)
            for _ in 0..<steps { left.view.increaseFontSize(nil) }
            XCTAssertEqual(
                window.contentMinSize, controller.paneContainer.minimumSize, "\(steps) steps: the window follows")
            window.setContentSize(window.contentMinSize)
            controller.paneContainer.layoutSubtreeIfNeeded()
            assertEveryPaneHoldsItsSmallestSize(controller, "\(steps) steps")
            try assertEveryPaneHoldsTheSmallestGrid(controller, "\(steps) steps")
            left.view.resetFontSize(nil)
            XCTAssertEqual(
                window.contentMinSize, controller.paneContainer.minimumSize, "\(steps) steps: and Actual Size")
            window.close()
        }
    }

    /// L | R with L dragged to its smallest size, then zoomed in while the
    /// window has room: L takes what its larger cells need at once, R
    /// giving way, before any resize.
    func testAPaneZoomedInAtItsSmallestSizeGrowsIntoItsSibling() throws {
        let controller = try openTab(NSSize(width: 1200, height: 600))
        let left = controller.focusedPane
        _ = try split(controller, .right)
        try skipUnlessAttached(controller)
        let splitView = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        splitView.setPosition(0, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertEqual(left.view.frame.width, PaneContainerView.minimumSize(of: left.view).width, "at its smallest")
        for _ in 0..<7 { left.view.increaseFontSize(nil) }
        controller.paneContainer.layoutSubtreeIfNeeded()
        assertEveryPaneHoldsItsSmallestSize(controller, "after the zoom")
        try assertEveryPaneHoldsTheSmallestGrid(controller, "after the zoom")
    }

    /// Three panes at the window's smallest width with overlay scroll bars;
    /// the system switches to legacy ones (a mouse plugged in): the window's
    /// smallest size takes the bars, and the panes keep their smallest size.
    func testAScrollerStyleSwitchRaisesTheWindowsSmallestSize() throws {
        let controller = try openTab(NSSize(width: 1200, height: 500))
        controller.focusedPane.view.scroller.preferredStyleOverride = .overlay
        try split(controller, .right).view.scroller.preferredStyleOverride = .overlay
        try split(controller, .right).view.scroller.preferredStyleOverride = .overlay
        try skipUnlessAttached(controller)
        let window = try XCTUnwrap(controller.window)
        XCTAssertEqual(window.contentMinSize, controller.paneContainer.minimumSize, "overlay")
        window.setContentSize(NSSize(width: window.contentMinSize.width, height: 500))
        controller.paneContainer.layoutSubtreeIfNeeded()
        try assertEveryPaneHoldsTheSmallestGrid(controller, "overlay, at the smallest width")
        for pane in controller.panes { pane.view.scroller.preferredStyleOverride = .legacy }
        XCTAssertEqual(window.contentMinSize, controller.paneContainer.minimumSize, "legacy")
        window.setContentSize(NSSize(width: window.contentMinSize.width, height: 500))
        controller.paneContainer.layoutSubtreeIfNeeded()
        assertEveryPaneHoldsItsSmallestSize(controller, "legacy, at the smallest width")
        try assertEveryPaneHoldsTheSmallestGrid(controller, "legacy, at the smallest width")
    }

    // MARK: Focus

    func testCommandOptionArrowsMoveTheFocusToTheSpatialNeighbour() throws {
        let (controller, panes) = try openTwoByTwo()
        let window = try XCTUnwrap(controller.window)
        func move(_ action: (TerminalWindowController) -> Void, from: Int, to: Int) {
            controller.focus(panes[from])
            action(controller)
            XCTAssertTrue(controller.focusedPane === panes[to], "\(from) → \(to)")
            XCTAssertTrue(window.firstResponder === panes[to].view, "\(from) → \(to) has the keyboard")
        }
        move({ $0.selectPaneRight(nil) }, from: 0, to: 1)
        move({ $0.selectPaneBelow(nil) }, from: 0, to: 2)
        move({ $0.selectPaneLeft(nil) }, from: 0, to: 0)
        move({ $0.selectPaneAbove(nil) }, from: 0, to: 0)
        move({ $0.selectPaneLeft(nil) }, from: 3, to: 2)
        move({ $0.selectPaneAbove(nil) }, from: 3, to: 1)
        move({ $0.selectPaneRight(nil) }, from: 3, to: 3)
        move({ $0.selectPaneBelow(nil) }, from: 1, to: 3)
        move({ $0.selectPaneLeft(nil) }, from: 1, to: 0)
        move({ $0.selectPaneAbove(nil) }, from: 2, to: 0)
        move({ $0.selectPaneRight(nil) }, from: 2, to: 3)
        // A click focuses the pane it lands in.
        let click = try XCTUnwrap(
            NSEvent.mouseEvent(
                with: .leftMouseDown, location: panes[2].view.convert(NSPoint(x: 20, y: 20), to: nil),
                modifierFlags: [], timestamp: 0, windowNumber: window.windowNumber, context: nil, eventNumber: 0,
                clickCount: 1, pressure: 1))
        panes[2].view.mouseDown(with: click)
        XCTAssertTrue(controller.focusedPane === panes[2], "a click")
    }

    /// A pane with no visible size (squeezed to nothing) is not a place the
    /// focus moves to: ⌘⌥→ goes past it, or stays.
    func testTheFocusSkipsAPaneWithNoVisibleSize() throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let middle = try split(controller, .right)
        let right = try split(controller, .right)
        controller.focus(left)
        middle.view.setFrameSize(NSSize(width: 0, height: middle.view.frame.height))
        controller.selectPaneRight(nil)
        XCTAssertTrue(controller.focusedPane === right, "past the squeezed pane")
        right.view.setFrameSize(NSSize(width: 0, height: right.view.frame.height))
        controller.focus(left)
        controller.selectPaneRight(nil)
        XCTAssertTrue(controller.focusedPane === left, "no visible pane to the right")
    }

    func testTheSelectPaneMenuHasCommandOptionArrows() throws {
        let shell = try XCTUnwrap(NSApp.mainMenu?.items.first { $0.submenu?.title == "Shell" }?.submenu)
        let select = try XCTUnwrap(shell.items.first { $0.title == "Select Pane" }?.submenu)
        let expected: [(String, Selector, Int)] = [
            ("Left", #selector(TerminalWindowController.selectPaneLeft(_:)), NSLeftArrowFunctionKey),
            ("Right", #selector(TerminalWindowController.selectPaneRight(_:)), NSRightArrowFunctionKey),
            ("Above", #selector(TerminalWindowController.selectPaneAbove(_:)), NSUpArrowFunctionKey),
            ("Below", #selector(TerminalWindowController.selectPaneBelow(_:)), NSDownArrowFunctionKey),
        ]
        XCTAssertEqual(select.items.map(\.title), expected.map(\.0))
        for (item, (title, action, key)) in zip(select.items, expected) {
            XCTAssertEqual(item.action, action, title)
            XCTAssertEqual(item.keyEquivalent, MainMenu.functionKey(key), title)
            XCTAssertEqual(item.keyEquivalentModifierMask, [.command, .option], title)
            XCTAssertNil(item.target, "\(title) goes up the responder chain")
        }
        // Enabled only with a pane to move to.
        let controller = try openTab()
        let left = try XCTUnwrap(select.items.first)
        XCTAssertFalse(controller.validateMenuItem(left), "one pane")
        _ = try split(controller, .right)
        XCTAssertTrue(controller.validateMenuItem(left), "two panes")
    }

    #if DEBUG
        func testKeysReachOnlyTheFocusedPane() throws {
            let controller = try openTab()
            let window = try XCTUnwrap(controller.window)
            let left = controller.focusedPane
            let right = try split(controller, .right)
            func type(_ character: String) throws {
                window.sendEvent(
                    try XCTUnwrap(
                        NSEvent.keyEvent(
                            with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                            windowNumber: window.windowNumber, context: nil, characters: character,
                            charactersIgnoringModifiers: character, isARepeat: false, keyCode: 0)))
            }
            try type("a")
            XCTAssertEqual(right.view.sentKeys.map(\.codepoint), ["a"], "the focused pane")
            XCTAssertEqual(left.view.sentKeys.count, 0, "not the other")
            controller.moveFocus(.left)
            try type("b")
            XCTAssertEqual(left.view.sentKeys.map(\.codepoint), ["b"])
            XCTAssertEqual(right.view.sentKeys.count, 1)
        }
    #endif

    func testTheTabTitleIsTheFocusedPanesTitle() async throws {
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        let left = controller.focusedPane
        let right = try split(controller, .right)
        controller.session(left.session, didSetTitle: "vim")
        XCTAssertEqual(window.title, "Terminal", "the right pane has the focus and no title")
        // Through the router, from the new pane's own session.
        try right.session.write(Array(#"printf '\033]0;make\007'"#.utf8) + [0x0a])
        try await waitUntil("the new pane's title arrives") { window.title == "make" }
        controller.focus(left)
        XCTAssertEqual(window.title, "vim")
        controller.session(right.session, didSetTitle: "top")
        XCTAssertEqual(window.title, "vim", "an unfocused pane's title stays its own")
        controller.moveFocus(.right)
        XCTAssertEqual(window.title, "top")
    }

    // MARK: Closing

    func testCommandWClosesTheFocusedPaneAndTheLastPaneClosesTheTab() throws {
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        let left = controller.focusedPane
        let right = try split(controller, .right)
        XCTAssertTrue(appDelegate.router.controller(for: right.session) === controller, "the new session is routed")
        // ⌘W, as the menu sends it: up the responder chain from the pane.
        XCTAssertTrue(right.view.tryToPerform(#selector(NSWindow.performClose(_:)), with: nil))
        XCTAssertEqual(controller.panes, [left], "the focused pane closed")
        XCTAssertNil(appDelegate.router.controller(for: right.session), "its session is no longer routed")
        XCTAssertTrue(appDelegate.router.controller(for: left.session) === controller, "the other one still is")
        XCTAssertThrowsError(try right.session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound, "with its session")
        }
        XCTAssertTrue(controller.focusedPane === left)
        XCTAssertTrue(window.firstResponder === left.view, "the remaining pane has the keyboard")
        XCTAssertEqual(left.view.frame.size, controller.paneContainer.bounds.size, "and the whole tab")
        XCTAssertTrue(window.isVisible, "the tab stays")
        XCTAssertTrue(presenter.sheets.isEmpty, "nothing ran: no question")

        XCTAssertTrue(left.view.tryToPerform(#selector(NSWindow.performClose(_:)), with: nil))
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "the last pane's ⌘W closed the tab")
    }

    func testCommandWOnTheLastPaneAsksAsTheTabDoes() async throws {
        let controller = try openTab()
        try await start("sleep 100", in: controller.focusedPane)
        XCTAssertTrue(controller.focusedPane.view.tryToPerform(#selector(NSWindow.performClose(_:)), with: nil))
        XCTAssertEqual(presenter.sheets.map(\.confirmation), [CloseConfirmation(scope: .tab, processNames: ["sleep"])])
        XCTAssertEqual(controller.pendingClose, .tab)
        presenter.sheets[0].answer(true)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    func testCommandWOnABusyPaneAsksAboutThatPaneOnly() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller, .right)
        try await start("sleep 100", in: right)
        try await start("sleep 200", in: left)
        controller.focus(right)

        controller.closeFocusedPane(nil)
        XCTAssertEqual(presenter.sheets.map(\.confirmation), [CloseConfirmation(scope: .pane, processNames: ["sleep"])])
        XCTAssertEqual(presenter.sheets[0].confirmation.messageText, "Close this pane?")
        XCTAssertEqual(controller.pendingClose, .pane)
        controller.closeFocusedPane(nil)
        controller.window?.performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1, "nothing else asks meanwhile")

        presenter.sheets[0].answer(false)
        XCTAssertEqual(controller.panes, [left, right], "Cancel keeps the pane")
        XCTAssertNil(controller.pendingClose)
        controller.closeFocusedPane(nil)
        presenter.sheets[1].answer(true)
        XCTAssertEqual(controller.panes, [left], "Close closes it")
        XCTAssertThrowsError(try right.session.foregroundProcess())
        XCTAssertEqual(try left.session.foregroundProcess()?.name, "sleep", "the other pane's job runs on")
    }

    /// D2: three panes, the middle one busy: closing the tab asks once and
    /// names only that pane's job; Close ends every pane's session.
    func testATabWithOneBusyPaneOfThreeAsksOnceNamingThatPane() async throws {
        let controller = try openTab()
        let first = controller.focusedPane
        let second = try split(controller, .right)
        let third = try split(controller, .right)
        try await start("sleep 100", in: second)

        controller.window?.performClose(nil)
        XCTAssertEqual(presenter.sheets.map(\.confirmation), [CloseConfirmation(scope: .tab, processNames: ["sleep"])])
        controller.window?.performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1, "asks once")
        presenter.sheets[0].answer(true)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        for pane in [first, second, third] {
            XCTAssertThrowsError(try pane.session.foregroundProcess()) {
                XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
            }
        }
    }

    func testATabWithNoBusyPaneClosesWithoutAsking() throws {
        let controller = try openTab()
        _ = try split(controller, .right)
        _ = try split(controller, .below)
        let sessions = controller.sessions
        controller.window?.performClose(nil)
        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        for session in sessions {
            XCTAssertThrowsError(try session.foregroundProcess())
        }
    }

    /// ⇧⌘W and quitting count every pane of every tab, the unfocused ones
    /// too.
    func testTheWindowAndQuitConfirmationsNameEveryBusyPane() async throws {
        let first = try openTab()
        let second = try appDelegate.newTab(in: try XCTUnwrap(first.window))
        let hidden = first.focusedPane
        _ = try split(first, .right)
        try await start("sleep 100", in: hidden)
        try await start("sleep 200", in: try split(second, .below))
        _ = try split(second, .right)

        presenter.quitAnswer = false
        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateCancel)
        XCTAssertEqual(presenter.quits, [CloseConfirmation(scope: .quit, processNames: ["sleep", "sleep"])])
        second.closeTabGroup(nil)
        XCTAssertEqual(
            presenter.sheets.map(\.confirmation), [CloseConfirmation(scope: .window, processNames: ["sleep", "sleep"])])
        presenter.sheets[0].answer(true)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    /// A pane's shell exiting cleanly closes that pane; a busy pane's sheet
    /// (a real alert) ends as Cancel when that pane closes underneath it.
    func testACleanExitClosesOnlyItsPaneAndEndsItsSheet() async throws {
        appDelegate.closeConfirmationPresenter = AlertCloseConfirmationPresenter()
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        let left = controller.focusedPane
        let right = try split(controller, .right)
        try await start("sleep 100", in: right)
        autoreleasepool { controller.closeFocusedPane(nil) }
        XCTAssertNotNil(window.attachedSheet, "the busy pane asks on a sheet")

        autoreleasepool { controller.session(right.session, didExitWithCode: 0, signal: 0) }
        XCTAssertEqual(window.sheets.count, 0, "its sheet ended")
        XCTAssertNil(controller.pendingClose)
        XCTAssertEqual(controller.panes, [left], "only that pane closed")
        XCTAssertTrue(controller.focusedPane === left)

        // Through the router, from the shell itself.
        let third = try split(controller, .below)
        try third.session.write(Array("exit\n".utf8))
        try await waitUntil("the exiting pane closes") { controller.panes == [left] }
        XCTAssertTrue(appDelegate.windowControllers.contains { $0 === controller }, "the tab stays")
        controller.session(left.session, didExitWithCode: 1, signal: 0)
        XCTAssertEqual(controller.panes, [left], "a failed exit keeps the pane, showing it")
    }

    // MARK: Releasing

    func testClosingAPaneReleasesItsPaneViewAndSession() async throws {
        weak var weakPane: Pane?
        weak var weakView: TerminalSurfaceView?
        weak var weakSession: TesseraSession?
        weak var weakController: TerminalWindowController?
        try autoreleasepool {
            let controller = try openTab()
            weakController = controller
            let pane = try split(controller, .right)
            weakPane = pane
            weakView = pane.view
            weakSession = pane.session
            // As AppKit does for the key window's first responder.
            pane.view.inputContext?.activate()
        }
        autoreleasepool { weakController?.closeFocusedPane(nil) }
        try? await waitUntil("the closed pane's objects are freed") {
            weakPane == nil && weakView == nil && weakSession == nil
        }
        XCTAssertNil(weakPane, "pane")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakSession, "session")
        XCTAssertEqual(weakController?.panes.count, 1, "the tab stays")
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the tab's controller is freed") { weakController == nil }
        XCTAssertNil(weakController, "controller")
    }

    func testClosingATabReleasesEveryPane() async throws {
        struct WeakRef {
            weak var object: AnyObject?
        }
        var weakPanes: [WeakRef] = []
        weak var weakController: TerminalWindowController?
        weak var weakWindow: NSWindow?
        try autoreleasepool {
            let controller = try openTab()
            weakController = controller
            weakWindow = controller.window
            _ = try split(controller, .right)
            _ = try split(controller, .below)
            for pane in controller.panes {
                weakPanes += [WeakRef(object: pane), WeakRef(object: pane.view), WeakRef(object: pane.session)]
            }
            controller.focusedPane.view.inputContext?.activate()
        }
        // AppKit may keep the closed window (the macOS 15 runner does).
        let heldWindow = weakWindow
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the closed tab's objects are freed") {
            weakController == nil && weakPanes.allSatisfy { $0.object == nil }
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertEqual(weakPanes.compactMap(\.object).count, 0, "panes, views and sessions")
        XCTAssertNil(heldWindow?.contentView, "the panes left the closed window")
    }

    // MARK: Gestures that end (#289.2, #299.2)

    /// A child printing a link at the top left, then waiting.
    private static let linkMetrics = TesseraSurface.Metrics(
        widthPx: 480, heightPx: 200, scale: 1, cellWidthPx: 10, cellHeightPx: 20, baselinePx: 15, paddingPx: 0,
        rows: 10, cols: 48, fontSizePt: 13, cursorRow: 0, cursorCol: 0, cursorOnScreen: true, cursorWide: false)

    private func openLinkTabs() async throws -> (view: TerminalSurfaceView, tab: NSWindow, other: NSWindow) {
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", #"printf 'https://a.bc/x'; exec cat > /dev/null"#])
        let controller = try appDelegate.newWindow()
        let tab = try XCTUnwrap(controller.window)
        let other = try XCTUnwrap(appDelegate.newTab(in: tab).window)
        let view = controller.focusedPane.view
        view.occlusionOverride = true
        view.metrics = Self.linkMetrics
        view.scroller.preferredStyleOverride = .overlay
        try await waitUntil("the link is printed") { try view.session.link(col: 0, row: 0) != nil }
        tab.tabGroup?.selectedWindow = tab
        return (view, tab, other)
    }

    private func mouse(
        _ type: NSEvent.EventType, _ view: NSView, col: Int, row: Int, _ flags: NSEvent.ModifierFlags = []
    ) throws -> NSEvent {
        let point = NSPoint(x: CGFloat(col) * 10 + 5, y: view.bounds.height - (CGFloat(row) * 20 + 10))
        return try XCTUnwrap(
            NSEvent.mouseEvent(
                with: type, location: view.convert(point, to: nil), modifierFlags: flags, timestamp: 0,
                windowNumber: view.window?.windowNumber ?? 0, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
    }

    /// A Command-click pressed on a link, then the tab switched, the window
    /// lost key status or another pane took the keyboard: a release later
    /// in the same cell opens nothing (#299.2).
    func testACommandClickInterruptedBeforeItsReleaseOpensNothing() async throws {
        let (view, tab, other) = try await openLinkTabs()
        let opener = RecordingLinkOpener()
        view.linkOpener = opener
        func click(interruptedBy interrupt: () -> Void) throws {
            view.mouseDown(with: try mouse(.leftMouseDown, view, col: 3, row: 0, .command))
            interrupt()
            view.mouseUp(with: try mouse(.leftMouseUp, view, col: 3, row: 0, .command))
        }
        try click {}
        XCTAssertEqual(opener.opened.map(\.absoluteString), ["https://a.bc/x"], "an uninterrupted click opens it")
        try click {
            tab.tabGroup?.selectedWindow = other
            tab.tabGroup?.selectedWindow = tab
        }
        XCTAssertEqual(opener.opened.count, 1, "a tab switch")
        try click { view.windowKeyChanged(isKey: false) }
        XCTAssertEqual(opener.opened.count, 1, "the window lost key status")
        try click { _ = tab.makeFirstResponder(nil) }
        XCTAssertEqual(opener.opened.count, 1, "the keyboard went elsewhere")
    }

    /// A selection drag above the view autoscrolls; switching tabs ends the
    /// gesture with its autoscroll, so a drag delivered later does not
    /// restart it in the hidden tab (#289.2).
    func testATabSwitchEndsASelectionDragAndItsAutoscroll() async throws {
        let (view, tab, other) = try await openLinkTabs()
        view.mouseDown(with: try mouse(.leftMouseDown, view, col: 3, row: 2))
        view.mouseDragged(with: try mouse(.leftMouseDragged, view, col: 3, row: -2))
        XCTAssertEqual(view.autoscroller.direction, .up, "it autoscrolls")
        tab.tabGroup?.selectedWindow = other
        XCTAssertNil(view.autoscroller.direction, "the switch stopped it")
        view.mouseDragged(with: try mouse(.leftMouseDragged, view, col: 3, row: -3))
        XCTAssertNil(view.autoscroller.direction, "and ended the gesture: a later drag does not restart it")
        view.mouseUp(with: try mouse(.leftMouseUp, view, col: 3, row: -3))
    }

    /// Another pane taking the keyboard ends a drag begun here, with its
    /// autoscroll.
    func testAPaneLosingTheKeyboardEndsADragAndItsAutoscroll() async throws {
        let (view, tab, _) = try await openLinkTabs()
        let controller = try XCTUnwrap(tab.windowController as? TerminalWindowController)
        let pane = controller.focusedPane
        let other = try split(controller, .right)
        controller.focus(pane)
        XCTAssertTrue(tab.firstResponder === view)
        view.mouseDown(with: try mouse(.leftMouseDown, view, col: 3, row: 2))
        view.mouseDragged(with: try mouse(.leftMouseDragged, view, col: 3, row: -2))
        XCTAssertEqual(view.autoscroller.direction, .up, "it autoscrolls")
        XCTAssertTrue(tab.makeFirstResponder(other.view))
        XCTAssertNil(view.autoscroller.direction, "the other pane taking the keyboard stopped it")
        view.mouseDragged(with: try mouse(.leftMouseDragged, view, col: 3, row: -3))
        XCTAssertNil(view.autoscroller.direction, "and ended the gesture: a later drag does not restart it")
        view.mouseUp(with: try mouse(.leftMouseUp, view, col: 3, row: -3))
    }

    // MARK: Flood

    /// The standing flood check: `yes` in two panes while the window and the
    /// divider between them keep resizing. Memory stays bounded, the main
    /// actor keeps up, every pane's grid follows its frame, and closing the
    /// tab is quick.
    func testAFloodWhileResizingPanesStaysBoundedAndClosesQuickly() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller, .right)
        let window = try XCTUnwrap(controller.window)
        let splitView = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        for pane in [left, right] {
            try await start("yes", in: pane)
        }
        var step = 0
        // One step of window and divider resizing.
        func resize() {
            step += 1
            let width: CGFloat = step % 2 == 0 ? 1200 : 900
            window.setContentSize(NSSize(width: width, height: step % 3 == 0 ? 800 : 600))
            splitView.setPosition(CGFloat(200 + (step * 37) % 400), ofDividerAt: 0)
            controller.paneContainer.layoutSubtreeIfNeeded()
        }
        // A second of it first: the panes' drawables and glyph atlases
        // reach every size, a one-off step of up to about 150 MB that then
        // swings up and down with the sizes, so the budget below measures
        // growth past the warm-up's highest footprint.
        var baseline = Self.physFootprint()
        let warmUp = ContinuousClock.now + .seconds(1)
        while ContinuousClock.now < warmUp {
            resize()
            try await Task.sleep(for: .milliseconds(20))
            baseline = max(baseline, Self.physFootprint())
        }
        var peak = baseline
        var worstLag: Duration = .zero
        let end = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < end {
            resize()
            let start = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(20))
            worstLag = max(worstLag, ContinuousClock.now - start - .milliseconds(20))
            peak = max(peak, Self.physFootprint())
        }
        // Each grid is the whole cells its pane's frame holds once the
        // resizing stops.
        try await Task.sleep(for: .milliseconds(200))
        for pane in [left, right] where pane.view.surface.isAttached {
            let view = pane.view
            let metrics = try XCTUnwrap(view.metrics)
            let scale = CGFloat(metrics.scale)
            XCTAssertEqual(metrics.widthPx, UInt32((view.renderView.bounds.width * scale).rounded()), "the frame's width")
            XCTAssertEqual(metrics.heightPx, UInt32((view.bounds.height * scale).rounded()), "the frame's height")
            let padding = 2 * metrics.paddingPx
            XCTAssertEqual(UInt32(metrics.cols), (metrics.widthPx - padding) / metrics.cellWidthPx, "whole columns")
            XCTAssertEqual(UInt32(metrics.rows), (metrics.heightPx - padding) / metrics.cellHeightPx, "whole rows")
        }

        let closeStart = ContinuousClock.now
        window.close()
        try await waitUntil("the tab closed") { appDelegate.windowControllers.isEmpty }
        let closeTime = ContinuousClock.now - closeStart

        let growthMB = Double(peak - baseline) / 1_048_576
        print("pane flood: \(step) resize steps, growth \(Int(growthMB)) MB, worst lag \(worstLag), close \(closeTime)")
        XCTAssertGreaterThan(step, 30, "the resizing kept up (79 steps under TSan locally)")
        XCTAssertLessThan(growthMB, 100, "footprint grew \(growthMB) MB during the flood")
        XCTAssertLessThan(worstLag, .milliseconds(500), "main actor lagged \(worstLag)")
        XCTAssertLessThan(closeTime, .seconds(3), "closing took \(closeTime)")
    }

    /// The process's physical footprint in bytes.
    private static func physFootprint() -> UInt64 {
        var usage = rusage_info_v4()
        let status = withUnsafeMutablePointer(to: &usage) {
            $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
                proc_pid_rusage(getpid(), RUSAGE_INFO_V4, $0)
            }
        }
        return status == 0 ? usage.ri_phys_footprint : 0
    }

    // MARK: Helpers

    /// Every pane's grid, as the core got it, holds at least 10 × 3 cells
    /// (D1): the numbers are written out, not taken from the container.
    private func assertEveryPaneHoldsTheSmallestGrid(
        _ controller: TerminalWindowController, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        for (index, pane) in controller.panes.enumerated() {
            let metrics = try XCTUnwrap(pane.view.metrics, "\(message): pane \(index) has a grid", file: file, line: line)
            XCTAssertGreaterThanOrEqual(metrics.cols, 10, "\(message): pane \(index)'s columns", file: file, line: line)
            XCTAssertGreaterThanOrEqual(metrics.rows, 3, "\(message): pane \(index)'s rows", file: file, line: line)
        }
    }

    private func assertEveryPaneHoldsItsSmallestSize(
        _ controller: TerminalWindowController, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        for (index, pane) in controller.panes.enumerated() {
            let minimum = PaneContainerView.minimumSize(of: pane.view)
            let size = pane.view.frame.size
            XCTAssertGreaterThanOrEqual(size.width, minimum.width, "\(message): pane \(index)", file: file, line: line)
            XCTAssertGreaterThanOrEqual(size.height, minimum.height, "\(message): pane \(index)", file: file, line: line)
        }
    }

    private func skipUnlessAttached(_ controller: TerminalWindowController) throws {
        guard controller.panes.allSatisfy(\.view.surface.isAttached) else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
    }

    /// Types `command` into the pane's shell and waits until it runs in the
    /// foreground.
    private func start(_ command: String, in pane: Pane) async throws {
        let name = String(command.prefix { $0 != " " })
        try pane.session.write(Array("\(command)\n".utf8))
        try await waitUntil("\(name) runs in the foreground") {
            try pane.session.foregroundProcess()?.name == name
        }
    }

    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(10), _ condition: () throws -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while try !condition() {
            guard ContinuousClock.now < deadline else {
                throw PollTimedOut(description: "timed out after \(timeout): \(description)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
    }
}
