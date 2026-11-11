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

    /// Starts the core, as the first window does: its configuration sets
    /// `paneDimming`, which would otherwise reset a test's amount when the
    /// test, run alone, opens its first window.
    private func startCore() throws {
        try appDelegate.newWindow().window?.close()
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
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
        let arrows = Array(select.items.prefix(expected.count))
        XCTAssertEqual(arrows.map(\.title), expected.map(\.0))
        for (item, (title, action, key)) in zip(arrows, expected) {
            XCTAssertEqual(item.action, action, title)
            XCTAssertEqual(item.keyEquivalent, MainMenu.functionKey(key), title)
            XCTAssertEqual(item.keyEquivalentModifierMask, [.command, .option], title)
            XCTAssertNil(item.target, "\(title) goes up the responder chain")
        }
        // Then ⌘[ and ⌘] through the panes in reading order (#38b).
        let cycle = select.items.dropFirst(expected.count + 1).map { ($0.title, authoredKeyEquivalent(of: $0).key, $0.action) }
        XCTAssertEqual(cycle.map(\.0), ["Previous", "Next"])
        XCTAssertEqual(cycle.map(\.1), ["[", "]"])
        XCTAssertEqual(
            cycle.map(\.2),
            [#selector(TerminalWindowController.selectPreviousPane(_:)), #selector(TerminalWindowController.selectNextPane(_:))])
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
            // The window's content, which observes the window (#363).
            weakPanes.append(WeakRef(object: controller.window?.contentView))
            controller.focusedPane.view.inputContext?.activate()
        }
        // AppKit may keep the closed window (the macOS 15 runner does).
        let heldWindow = weakWindow
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the closed tab's objects are freed") {
            weakController == nil && weakPanes.allSatisfy { $0.object == nil }
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertEqual(weakPanes.compactMap(\.object).count, 0, "panes, views, sessions and the window's content")
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

    /// The standing flood check: `yes` in two panes. At a fixed size, memory
    /// stays bounded; while the window and the divider between them keep
    /// resizing, the main actor keeps up, every pane's grid follows its
    /// frame, and closing the tab is quick.
    func testAFloodWhileResizingPanesStaysBoundedAndClosesQuickly() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller, .right)
        let window = try XCTUnwrap(controller.window)
        let splitView = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        for pane in [left, right] {
            try await start("yes", in: pane)
        }
        // Phase 1, the flood at a fixed size: memory. As in the maximized
        // flood test, a fixed 8 s warm-up first, for both panes' scrollback
        // to fill at CI speed; the baseline is its highest footprint, and the
        // next 5 s must not grow past it by the budget.
        var baseline: UInt64 = 0
        let warmUpEnd = ContinuousClock.now + .seconds(8)
        while ContinuousClock.now < warmUpEnd {
            try await Task.sleep(for: .milliseconds(20))
            baseline = max(baseline, Self.physFootprint())
        }
        var peak = baseline
        let floodEnd = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < floodEnd {
            try await Task.sleep(for: .milliseconds(20))
            peak = max(peak, Self.physFootprint())
        }
        let growthMB = Double(peak - baseline) / 1_048_576
        XCTAssertLessThan(growthMB, 100, "footprint grew \(growthMB) MB during the flood")

        // Phase 2, the flood while the window and the divider resize: lag,
        // grids and close, not memory. Resizing a rendering window
        // saw-tooths the footprint on CI: macOS keeps the old-size
        // drawables and frees them in batches every 40 to 100 steps (#326,
        // "Live resize footprint" in docs/debugging.md;
        // ResizeFootprintTests bounds it), so a budget here measured where
        // a batch fell (0 to 108 MB of "growth" on CI).
        // The sizes the resizing visits: every (window width, window
        // height, divider) combination once per cycle, the periods being
        // coprime.
        let widths: [CGFloat] = [900, 1200]
        let heights: [CGFloat] = [600, 600, 800]
        let dividers: [CGFloat] = [200, 300, 400, 500, 600]
        let cycle = widths.count * heights.count * dividers.count
        var step = 0
        // One step of window and divider resizing.
        func resize() {
            step += 1
            window.setContentSize(NSSize(width: widths[step % widths.count], height: heights[step % heights.count]))
            splitView.setPosition(dividers[step % dividers.count], ofDividerAt: 0)
            controller.paneContainer.layoutSubtreeIfNeeded()
        }
        // A full cycle first, not timed: the panes' drawables, glyph atlases
        // and re-wrapped scrollback reach every size once.
        while step < cycle {
            resize()
            try await Task.sleep(for: .milliseconds(20))
        }
        var worstLag: Duration = .zero
        let floodStart = step
        let end = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < end {
            resize()
            let start = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(20))
            worstLag = max(worstLag, ContinuousClock.now - start - .milliseconds(20))
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

        let floodSteps = step - floodStart
        print("pane flood: growth \(Int(growthMB)) MB at a fixed size, \(floodSteps) resize steps, worst lag \(worstLag), close \(closeTime)")
        // 65 steps under TSan locally, 32 with the test host in the
        // background policy beside CPU burners, 27 with it starved.
        XCTAssertGreaterThanOrEqual(floodSteps, 10, "the resizing kept up")
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

// MARK: - Pane polish (#38b)

/// Equalising and the state file's layouts, without AppKit.
extension PaneTreeTests {
    /// L | (R1 / R2) gets halves; L | M | R thirds; a double-clicked
    /// divider equalises only its split.
    func testEqualizingGivesEveryPaneAnEqualShareAlongEachSplit() {
        var stacked = Tree(0)
        stacked.split(0, .right, adding: 1)
        stacked.split(1, .below, adding: 2)
        stacked.setRatios([0.2, 0.9])
        stacked.equalize()
        XCTAssertEqual(stacked.root.splits.map(\.ratio), [0.5, 0.5])

        var row = Tree(0)
        row.split(0, .right, adding: 1)
        row.split(1, .right, adding: 2)
        row.setRatios([0.8, 0.1])
        row.equalize()
        let space = CGRect(x: 0, y: 0, width: 902, height: 300)
        let widths = row.frames(in: space, divider: 1).map(\.frame.width)
        XCTAssertEqual(widths.count, 3)
        XCTAssertLessThanOrEqual(widths.max()! - widths.min()!, 1, "thirds: \(widths)")

        var one = Tree(0)
        one.split(0, .right, adding: 1)
        one.split(1, .right, adding: 2)
        one.setRatios([0.8, 0.1])
        one.equalize(splitAt: 1)
        XCTAssertEqual(one.root.splits.map(\.ratio), [0.8, 0.5], "only the inner split")
        one.equalize(splitAt: 7)
        XCTAssertEqual(one.root.splits.map(\.ratio), [0.8, 0.5], "no such split")
    }

    func testALayoutKeepsTheTreesShapeAndRatios() throws {
        var tree = Tree(0)
        tree.split(0, .right, adding: 1)
        tree.split(1, .below, adding: 2)
        tree.setRatios([0.3, 0.6])
        XCTAssertEqual(tree.depth, 2)
        let layout = PaneLayout(tree)
        XCTAssertEqual(layout.leafCount, 3)
        XCTAssertEqual(layout.depth, 2)
        XCTAssertTrue(layout.isRestorable)
        let back = try XCTUnwrap(layout.tree(leaves: ["a", "b", "c"]))
        XCTAssertEqual(back.leaves, ["a", "b", "c"])
        XCTAssertEqual(back.root.splits.map(\.direction), [.right, .below])
        XCTAssertEqual(back.root.splits.map(\.ratio), [0.3, 0.6])
        XCTAssertNil(layout.tree(leaves: ["a", "b"]), "a pane short")
        let json = String(decoding: try JSONEncoder().encode(layout), as: UTF8.self)
        XCTAssertTrue(json.contains(#""direction":"right""#), json)
        XCTAssertEqual(try JSONDecoder().decode(PaneLayout.self, from: Data(json.utf8)), layout)
    }
}

extension PaneTests {
    // MARK: Maximizing

    /// ⌘⇧↩ on a 2x2 tab: the focused pane fills the tab; the others stay
    /// open, their sessions still read output, and they are not resized,
    /// even while the window is; ⌘⇧↩ again puts every pane back at the
    /// exact frame it had.
    func testMaximizingAPaneKeepsTheOthersAliveAndUnresizedAndBringsThemBackExactly() async throws {
        let (controller, panes) = try openTwoByTwo()
        let window = try XCTUnwrap(controller.window)
        let container = controller.paneContainer
        controller.focus(panes[3])
        let frames = panes.map { frame(of: $0, in: controller) }
        let resizes = panes.map(\.view.coreResizeCount)

        controller.toggleZoomPane(nil)
        container.layoutSubtreeIfNeeded()
        XCTAssertTrue(controller.isZoomed)
        XCTAssertEqual(frame(of: panes[3], in: controller), container.bounds, "the focused pane fills the tab")
        XCTAssertFalse(panes[3].view.isHiddenOrHasHiddenAncestor)
        XCTAssertTrue(window.firstResponder === panes[3].view, "and keeps the keyboard")
        for index in 0..<3 {
            XCTAssertTrue(panes[index].view.isHiddenOrHasHiddenAncestor, "pane \(index) is hidden")
            XCTAssertTrue(panes[index].view.window === window, "pane \(index) stays in the tab")
        }

        // The window resizes while zoomed: the hidden panes keep their size.
        window.setContentSize(NSSize(width: 1000, height: 700))
        container.layoutSubtreeIfNeeded()
        XCTAssertEqual(frame(of: panes[3], in: controller), container.bounds, "the maximized pane follows the tab")
        for index in 0..<3 {
            XCTAssertEqual(panes[index].view.frame.size, frames[index].size, "pane \(index) is not resized")
            XCTAssertEqual(panes[index].view.coreResizeCount, resizes[index], "pane \(index)'s grid stays")
        }
        // A hidden pane's shell still runs and its output is read.
        try panes[0].session.write(Array(#"printf '\033]0;hidden\007'"#.utf8) + [0x0a])
        try await waitUntil("the hidden pane's title arrives") { panes[0].title == "hidden" }
        XCTAssertNoThrow(try panes[1].session.foregroundProcess(), "a hidden pane's session is open")

        window.setContentSize(NSSize(width: 1200, height: 800))
        container.layoutSubtreeIfNeeded()
        controller.toggleZoomPane(nil)
        container.layoutSubtreeIfNeeded()
        XCTAssertFalse(controller.isZoomed)
        XCTAssertEqual(panes.map { frame(of: $0, in: controller) }, frames, "every pane is back at its frame")
        XCTAssertTrue(panes.allSatisfy { !$0.view.isHiddenOrHasHiddenAncestor })
        XCTAssertTrue(window.firstResponder === panes[3].view)
        for index in 0..<3 {
            XCTAssertEqual(panes[index].view.coreResizeCount, resizes[index], "pane \(index) came back at its grid")
        }
    }

    /// A layout held by a clamp (the left pane's larger font keeps it above
    /// its ratio's share): maximizing either pane leaves the other's frame
    /// and grid alone, a font change of the maximized pane too, and showing
    /// every pane again brings back the exact frames.
    func testMaximizingAPaneOfAClampedLayoutLeavesTheOthersAlone() throws {
        let controller = try openTab(NSSize(width: 400, height: 400))
        let left = controller.focusedPane
        let right = try split(controller, .right)
        try skipUnlessAttached(controller)
        let container = controller.paneContainer
        controller.focus(left)
        for _ in 0..<20 { left.view.increaseFontSize(nil) }
        container.layoutSubtreeIfNeeded()
        let frames = [left, right].map { frame(of: $0, in: controller) }
        XCTAssertGreaterThan(frames[0].width, frames[1].width + 1, "the clamp holds the left pane above half")

        for (shown, hidden) in [(left, right), (right, left)] {
            let resizes = [left, right].map(\.view.coreResizeCount)
            let hiddenIndex = hidden === left ? 0 : 1
            controller.focus(shown)
            XCTAssertTrue(controller.toggleZoom())
            container.layoutSubtreeIfNeeded()
            XCTAssertEqual(hidden.view.frame.size, frames[hiddenIndex].size, "the hidden pane keeps its frame")
            XCTAssertEqual(hidden.view.coreResizeCount, resizes[hiddenIndex], "and its grid")
            if shown === left {
                // The clamp comes from the maximized pane: a font change of
                // it lays nothing hidden out.
                left.view.decreaseFontSize(nil)
                container.layoutSubtreeIfNeeded()
                XCTAssertEqual(right.view.frame.size, frames[1].size, "a font change of the maximized pane")
                XCTAssertEqual(right.view.coreResizeCount, resizes[1])
                left.view.increaseFontSize(nil)
            }
            XCTAssertTrue(controller.toggleZoom())
            container.layoutSubtreeIfNeeded()
            XCTAssertEqual([left, right].map { frame(of: $0, in: controller) }, frames, "every pane is back at its frame")
            XCTAssertEqual(hidden.view.coreResizeCount, resizes[hiddenIndex], "the hidden pane was never resized")
        }
    }

    /// A split, a close, a focus move, equalising or a new tab show every
    /// pane first.
    func testSplitCloseFocusMovesAndNewTabsShowEveryPaneFirst() throws {
        let (controller, panes) = try openTwoByTwo()
        let readingOrder = controller.panes
        let container = controller.paneContainer
        func zoom(_ pane: Pane) {
            controller.focus(pane)
            XCTAssertTrue(controller.toggleZoom())
            XCTAssertTrue(container.zoomedPane === pane)
        }
        zoom(panes[0])
        controller.selectPaneRight(nil)
        XCTAssertFalse(controller.isZoomed, "⌘⌥→")
        XCTAssertTrue(controller.focusedPane === panes[1])

        zoom(panes[0])
        controller.selectNextPane(nil)
        XCTAssertFalse(controller.isZoomed, "⌘]")
        XCTAssertTrue(controller.focusedPane === panes[2], "the next in reading order")
        controller.selectPreviousPane(nil)
        XCTAssertTrue(controller.focusedPane === panes[0])
        controller.selectPreviousPane(nil)
        XCTAssertTrue(controller.focusedPane === panes[3], "wrapping around")

        zoom(panes[0])
        controller.focus(panes[0])
        XCTAssertTrue(controller.isZoomed, "focusing the maximized pane keeps it")
        controller.focus(panes[2])
        XCTAssertFalse(controller.isZoomed, "focusing another")

        zoom(panes[3])
        let added = try split(controller, .right)
        XCTAssertFalse(controller.isZoomed, "a split")
        XCTAssertTrue(controller.focusedPane === added)
        XCTAssertTrue(controller.panes.allSatisfy { !$0.view.isHiddenOrHasHiddenAncestor })

        zoom(added)
        controller.closeFocusedPane(nil)
        XCTAssertFalse(controller.isZoomed, "closing the maximized pane")
        XCTAssertEqual(controller.panes, readingOrder)

        zoom(panes[1])
        controller.session(panes[2].session, didExitWithCode: 0, signal: 0)
        XCTAssertFalse(controller.isZoomed, "a hidden pane closing")
        XCTAssertEqual(controller.panes, [panes[0], panes[1], panes[3]])
        XCTAssertTrue(controller.panes.allSatisfy { !$0.view.isHiddenOrHasHiddenAncestor })

        zoom(panes[1])
        controller.equalizePanes(nil)
        XCTAssertFalse(controller.isZoomed, "equalising")

        zoom(panes[1])
        let tab = try appDelegate.newTab(in: try XCTUnwrap(controller.window))
        XCTAssertFalse(controller.isZoomed, "a new tab")
        XCTAssertFalse(tab.isZoomed)
    }

    func testMaximizePaneIsAShellMenuItemCheckedWhileAPaneIs() throws {
        let item = try XCTUnwrap(MainMenu.item(forCommand: "zoom_pane", in: NSApp.mainMenu))
        XCTAssertEqual(item.title, "Maximize Pane")
        XCTAssertEqual(item.action, #selector(TerminalWindowController.toggleZoomPane(_:)))
        XCTAssertEqual(authoredKeyEquivalent(of: item).key, "\r")
        XCTAssertEqual(item.keyEquivalentModifierMask, [.command, .shift])
        let equalize = try XCTUnwrap(MainMenu.item(forCommand: "equalize_panes", in: NSApp.mainMenu))
        XCTAssertEqual(equalize.title, "Equalize Panes")
        let controller = try openTab()
        XCTAssertFalse(controller.validateMenuItem(item), "one pane")
        XCTAssertFalse(controller.validateMenuItem(equalize), "one pane")
        XCTAssertFalse(controller.toggleZoom(), "a lone pane is not maximized")
        _ = try split(controller, .right)
        XCTAssertTrue(controller.validateMenuItem(item))
        XCTAssertEqual(item.state, .off)
        XCTAssertTrue(controller.validateMenuItem(equalize))
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.validateMenuItem(item))
        XCTAssertEqual(item.state, .on, "checked while a pane is maximized")
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.validateMenuItem(item))
        XCTAssertEqual(item.state, .off)
    }

    // MARK: Dimming

    /// The dim views over a container's panes, by pane.
    private func shades(_ controller: TerminalWindowController) -> [PaneDimView?] {
        controller.panes.map { pane in pane.view.subviews.lazy.compactMap { $0 as? PaneDimView }.first }
    }

    func testOnlyUnfocusedPanesOfASplitTabAreDimmed() throws {
        try startCore()
        defer { appDelegate.paneDimming = 0 }
        appDelegate.paneDimming = 0.4
        let controller = try openTab()
        XCTAssertEqual(controller.paneContainer.inactivePaneDimming, 0.4, "a new tab takes the amount")
        XCTAssertEqual(shades(controller).compactMap { $0 }.count, 0, "a lone pane is never dimmed")
        let left = controller.focusedPane
        let right = try split(controller, .right)
        var current = shades(controller)
        XCTAssertNotNil(current[0], "the unfocused pane")
        XCTAssertNil(current[1], "not the focused one")
        XCTAssertEqual(current[0]?.alphaValue ?? 0, 0.4, accuracy: 0.001)
        XCTAssertTrue(left.view.subviews.last is PaneDimView, "above the pane's own views")
        XCTAssertEqual(current[0]?.frame, left.view.bounds)

        // Clicks go through the shade to the pane.
        let window = try XCTUnwrap(controller.window)
        let point = left.view.convert(NSPoint(x: 30, y: 30), to: nil)
        let hit = window.contentView?.hitTest(window.contentView!.convert(point, from: nil))
        XCTAssertFalse(hit is PaneDimView, "\(String(describing: hit))")
        XCTAssertTrue(hit === left.view || hit?.isDescendant(of: left.view) == true, "\(String(describing: hit))")

        controller.focus(left)
        current = shades(controller)
        XCTAssertNil(current[0], "the shade follows the focus")
        XCTAssertNotNil(current[1])

        controller.toggleZoomPane(nil)
        XCTAssertNil(shades(controller)[0], "the maximized pane is focused")

        controller.toggleZoomPane(nil)
        appDelegate.paneDimming = 0
        XCTAssertEqual(shades(controller).compactMap { $0 }.count, 0, "0 draws nothing")
        appDelegate.paneDimming = 5
        XCTAssertEqual(shades(controller)[1]?.alphaValue ?? 0, 0.8, accuracy: 0.001, "at most 0.8")
        appDelegate.paneDimming = .nan
        XCTAssertEqual(shades(controller).compactMap { $0 }.count, 0, "not a number draws nothing")
        appDelegate.paneDimming = 0.3
        XCTAssertNotNil(shades(controller)[1])

        controller.closeFocusedPane(nil)
        XCTAssertEqual(controller.panes, [right])
        XCTAssertEqual(shades(controller).compactMap { $0 }.count, 0, "back to one pane: none")
    }

    /// A configuration the core applies is read again (live reload): the
    /// app delegate's path, on the app's own core.
    func testAnAppliedConfigurationReadsTheDimmingAgain() throws {
        defer { appDelegate.paneDimming = 0 }
        _ = try openTab()
        appDelegate.paneDimming = 0.123
        let reload = try XCTUnwrap(appDelegate.router.onConfigurationApplied)
        reload()
        XCTAssertNotEqual(appDelegate.paneDimming, 0.123, "read from the core again")
        XCTAssertTrue((0...0.8).contains(appDelegate.paneDimming))
    }

    // MARK: Equalising and dividers

    func testDoubleClickingADividerEqualisesItsSplit() throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller, .right)
        let below = try split(controller, .below)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? PaneSplitView)
        let inner = try XCTUnwrap(outer.subviews.last as? PaneSplitView)
        outer.setPosition(300, ofDividerAt: 0)
        inner.setPosition(200, ofDividerAt: 0)
        controller.paneContainer.layoutSubtreeIfNeeded()
        let innerHeights = [frame(of: right, in: controller).height, frame(of: below, in: controller).height]

        let window = try XCTUnwrap(controller.window)
        let divider = outer.convert(NSPoint(x: 300.5, y: 100), to: nil)
        let doubleClick = try XCTUnwrap(
            NSEvent.mouseEvent(
                with: .leftMouseDown, location: divider, modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 2, pressure: 1))
        // The release, in case the click were tracked as a drag (it should
        // not be): AppKit's tracking then ends at once instead of hanging.
        NSApp.postEvent(
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: .leftMouseUp, location: divider, modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 2, pressure: 0)),
            atStart: false)
        outer.mouseDown(with: doubleClick)
        controller.paneContainer.layoutSubtreeIfNeeded()
        let cell = PaneContainerView.fallbackCellSize
        XCTAssertEqual(
            frame(of: left, in: controller).width, frame(of: right, in: controller).width, accuracy: max(1, cell.width))
        XCTAssertEqual(
            [frame(of: right, in: controller).height, frame(of: below, in: controller).height], innerHeights,
            "the other split keeps its ratio")
        XCTAssertEqual(controller.paneContainer.tree.root.splits[0].ratio, 0.5)

        controller.equalizePanes(nil)
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertEqual(controller.paneContainer.tree.root.splits.map(\.ratio), [0.5, 0.5])
        XCTAssertEqual(
            frame(of: right, in: controller).height, frame(of: below, in: controller).height, accuracy: 1,
            "Equalize Panes sets every split")
    }

    /// #315.3: a 100-step divider drag through AppKit's own tracking
    /// (`PaneSplitView.mouseDown` follows the posted drags until the posted
    /// release) passes each pane's size to the core at most once a display
    /// refresh, and its final size at the end, as a live window resize does.
    func testADividerDragThroughMouseDownCoalescesTheResizes() throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller, .right)
        try skipUnlessAttached(controller)
        let window = try XCTUnwrap(controller.window)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? PaneSplitView)
        let start = frame(of: left, in: controller).maxX + 0.5
        func event(_ type: NSEvent.EventType, x: CGFloat) throws -> NSEvent {
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: outer.convert(NSPoint(x: x, y: 200), to: nil), modifierFlags: [],
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber, context: nil,
                    eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1))
        }
        for step in 1...100 {
            NSApp.postEvent(try event(.leftMouseDragged, x: start - CGFloat(2 * step)), atStart: false)
        }
        NSApp.postEvent(try event(.leftMouseUp, x: start - 200), atStart: false)
        let before = [left, right].map(\.view.coreResizeCount)
        let ticks = [left, right].map(\.view.tickCount)
        outer.mouseDown(with: try event(.leftMouseDown, x: start))
        controller.paneContainer.layoutSubtreeIfNeeded()
        for (index, pane) in [left, right].enumerated() {
            let resizes = pane.view.coreResizeCount - before[index]
            let refreshes = pane.view.tickCount - ticks[index]
            XCTAssertGreaterThan(resizes, 0, "pane \(index) was resized")
            XCTAssertLessThanOrEqual(resizes, refreshes + 1, "pane \(index): \(resizes) resizes, \(refreshes) refreshes")
            XCTAssertEqual(pane.view.metrics?.widthPx, pane.view.pixelSize.widthPx, "pane \(index) ends at its size")
        }
        XCTAssertLessThan(frame(of: left, in: controller).maxX, start - 50, "the drag moved the divider")
    }

    // MARK: ⌘W's title (#315.2)

    func testCommandWSaysClosePaneWhileTheTabHasSeveralPanes() throws {
        let item = try XCTUnwrap(MainMenu.item(forCommand: "close_tab", in: NSApp.mainMenu))
        defer { item.title = PaneContainerView.closeTabTitle }
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        /// Validates `item` as AppKit does: the first object up the
        /// responder chain from the pane that answers its action.
        func validate() -> Bool {
            var responder: NSResponder? = window.firstResponder
            while let current = responder, !current.responds(to: item.action) {
                responder = current.nextResponder
            }
            return (responder as? any NSMenuItemValidation)?.validateMenuItem(item) ?? false
        }
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Tab", "one pane")
        _ = try split(controller, .right)
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Pane")
        controller.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window))
        XCTAssertEqual(item.title, "Close Tab", "another window's menu says Close Tab")
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Pane")
        controller.closeFocusedPane(nil)
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Tab", "back to one pane")
        _ = try split(controller, .below)
        XCTAssertTrue(validate())
        window.close()
        XCTAssertEqual(item.title, "Close Tab", "the tab closed")
    }

    // MARK: Releasing and flooding a maximized, dimmed tab

    func testClosingAMaximizedDimmedTabReleasesEveryPane() async throws {
        struct WeakRef {
            weak var object: AnyObject?
        }
        defer { appDelegate.paneDimming = 0 }
        appDelegate.paneDimming = 0.5
        var weakPanes: [WeakRef] = []
        weak var weakController: TerminalWindowController?
        weak var weakContainer: PaneContainerView?
        weak var weakClosed: Pane?
        try autoreleasepool {
            let (controller, panes) = try openTwoByTwo()
            weakController = controller
            weakContainer = controller.paneContainer
            for pane in panes {
                weakPanes += [WeakRef(object: pane), WeakRef(object: pane.view), WeakRef(object: pane.session)]
            }
            // A maximized pane closed: released too, before anything else
            // (a focus move would clear what still held it).
            controller.focus(panes[3])
            controller.toggleZoomPane(nil)
            weakClosed = panes[3]
            controller.closeFocusedPane(nil)
        }
        try? await waitUntil("the closed maximized pane is freed") { weakClosed == nil }
        XCTAssertNil(weakClosed, "the maximized pane that closed")
        try autoreleasepool {
            let controller = try XCTUnwrap(weakController)
            controller.focus(controller.panes[0])
            controller.toggleZoomPane(nil)
            XCTAssertTrue(controller.isZoomed)
            controller.focusedPane.view.inputContext?.activate()
        }
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the closed tab's objects are freed") {
            weakController == nil && weakContainer == nil && weakPanes.allSatisfy { $0.object == nil }
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakContainer, "container")
        XCTAssertEqual(weakPanes.compactMap(\.object).count, 0, "panes, views, sessions and the window's content")
    }

    /// The standing flood check on a maximized tab: `yes` in the maximized
    /// pane and in a hidden one. Without resizing, memory stays bounded;
    /// while the window resizes, the main actor keeps up, the hidden pane is
    /// never resized, and closing the tab is quick.
    func testAFloodInAMaximizedTabStaysBoundedAndClosesQuickly() async throws {
        defer { appDelegate.paneDimming = 0 }
        appDelegate.paneDimming = 0.5
        let controller = try openTab()
        let shown = controller.focusedPane
        let hidden = try split(controller, .right)
        controller.focus(shown)
        controller.toggleZoomPane(nil)
        let window = try XCTUnwrap(controller.window)
        for pane in [shown, hidden] {
            try await start("yes", in: pane)
        }
        let hiddenSize = hidden.view.frame.size
        let hiddenResizes = hidden.view.coreResizeCount

        // Phase 1, the flood at a fixed size: memory. A fixed 8 s warm-up
        // first, long enough for both panes' scrollback to fill at CI speed
        // (where real rendering slows the flood, the footprint still rises
        // 3-4 MB/s after 15 s, so no plateau check can pass there). The
        // baseline is the warm-up's highest footprint, and the next 5 s
        // must not grow past it by the budget.
        var baseline: UInt64 = 0
        let warmUpEnd = ContinuousClock.now + .seconds(8)
        while ContinuousClock.now < warmUpEnd {
            try await Task.sleep(for: .milliseconds(20))
            baseline = max(baseline, Self.physFootprint())
        }
        var peak = baseline
        let floodEnd = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < floodEnd {
            try await Task.sleep(for: .milliseconds(20))
            peak = max(peak, Self.physFootprint())
        }
        let growthMB = Double(peak - baseline) / 1_048_576
        XCTAssertLessThan(growthMB, 100, "footprint grew \(growthMB) MB during the flood")

        // Phase 2, the flood while the window resizes: what a maximized tab
        // adds, not memory. Resizing a rendering window saw-tooths the
        // footprint on CI whether or not a tab is maximized: macOS keeps the
        // old-size drawables for a while (#326, "Live resize footprint" in
        // docs/debugging.md; ResizeFootprintTests bounds it), so no
        // footprint budget here.
        let sizes = [NSSize(width: 900, height: 600), NSSize(width: 1200, height: 800), NSSize(width: 1000, height: 700)]
        var step = 0
        var worstLag: Duration = .zero
        let resizeEnd = ContinuousClock.now + .seconds(2)
        while ContinuousClock.now < resizeEnd {
            step += 1
            window.setContentSize(sizes[step % sizes.count])
            controller.paneContainer.layoutSubtreeIfNeeded()
            let start = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(20))
            worstLag = max(worstLag, ContinuousClock.now - start - .milliseconds(20))
        }
        XCTAssertGreaterThan(step, sizes.count, "the window resized")
        XCTAssertEqual(hidden.view.frame.size, hiddenSize, "the hidden pane is not resized")
        XCTAssertEqual(hidden.view.coreResizeCount, hiddenResizes)
        XCTAssertTrue(hidden.view.isHiddenOrHasHiddenAncestor, "the hidden pane stays hidden")
        XCTAssertTrue(controller.isZoomed)

        let closeStart = ContinuousClock.now
        window.close()
        try await waitUntil("the tab closed") { appDelegate.windowControllers.isEmpty }
        let closeTime = ContinuousClock.now - closeStart
        print("maximized flood: growth \(Int(growthMB)) MB, \(step) resizes, worst lag \(worstLag), close \(closeTime)")
        XCTAssertLessThan(worstLag, .milliseconds(500), "main actor lagged \(worstLag)")
        XCTAssertLessThan(closeTime, .seconds(3), "closing took \(closeTime)")
    }
}

/// `[appearance] dim_inactive_panes` through the wrapper, on a core of the
/// test's own: read at start and again after an edit is applied, clamped.
@MainActor
final class PaneDimmingConfigTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-pane-dim-\(UUID().uuidString)")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    func testTheDimmingFollowsTheConfigurationLive() async throws {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let file = dir.appendingPathComponent("config.toml")
        try "[appearance]\ndim_inactive_panes = 0.25\n".write(to: file, atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        self.core = core
        let router = SessionRouter()
        core.delegate = router
        var applied = 0
        router.onConfigurationApplied = { applied += 1 }
        XCTAssertEqual(try core.dimInactivePanes(), 0.25)
        try "[appearance]\ndim_inactive_panes = 3.0\n".write(to: file, atomically: true, encoding: .utf8)
        let deadline = ContinuousClock.now + .seconds(10)
        while applied == 0, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertGreaterThan(applied, 0, "on_config_applied reached the router")
        XCTAssertEqual(try core.dimInactivePanes(), 0.8, "clamped")
        core.shutdown()
        XCTAssertThrowsError(try core.dimInactivePanes()) {
            XCTAssertEqual($0 as? TesseraCoreError, .nullPointer)
        }
    }
}

// MARK: - Pane follow-ups (#323)

/// Runs a test's steps from a Timer, which can fire inside AppKit's
/// tracking loops (in the event-tracking mode), until a step sets
/// ``step`` to 3.
@MainActor
private final class DragDriver: NSObject {
    var step = 0
    var failure: String?
    var onTick: (() -> Void)?

    @objc func fire(_ timer: Timer) {
        onTick?()
        if step == 3 {
            timer.invalidate()
            onTick = nil
        }
    }
}

/// Stands between a tab and the app delegate: counts the layout changes
/// (each asks for a window-state save) and the panes closed, and can make
/// the spawns past a number fail; everything else goes to the app.
@MainActor
private final class PaneHostProxy: TerminalWindowControllerDelegate {
    struct SpawnRefused: Error {}

    let app: AppDelegate
    private(set) var layoutChanges = 0
    private(set) var spawned: [TesseraSession] = []
    private(set) var closed: [TesseraSession] = []
    /// How many more spawns succeed; `nil` for no limit.
    var spawnsLeft: Int?

    init(app: AppDelegate) {
        self.app = app
    }

    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController) {
        app.terminalWindowControllerWantsNewTab(controller)
    }

    func terminalWindowControllerSpawnSession(_ controller: TerminalWindowController) throws -> TesseraSession {
        if let left = spawnsLeft {
            guard left > 0 else { throw SpawnRefused() }
            spawnsLeft = left - 1
        }
        let session = try app.terminalWindowControllerSpawnSession(controller)
        spawned.append(session)
        return session
    }

    func terminalWindowController(_ controller: TerminalWindowController, didClosePaneWith session: TesseraSession) {
        closed.append(session)
        app.terminalWindowController(controller, didClosePaneWith: session)
    }

    func terminalWindowControllerDidClose(_ controller: TerminalWindowController) {
        app.terminalWindowControllerDidClose(controller)
    }

    func terminalWindowControllerDidChangeLayout(_ controller: TerminalWindowController) {
        layoutChanges += 1
        app.terminalWindowControllerDidChangeLayout(controller)
    }

    func presentOpenError(_ error: any Error) {
        app.presentOpenError(error)
    }

    var closeConfirmationPresenter: (any CloseConfirmationPresenter)? { app.closeConfirmationPresenter }
}

extension PaneTests {
    // MARK: Maximizing keeps the keyboard where it is

    /// #323 M1 (the #38b adversarial run's T9): maximizing and showing every
    /// pane again move the focused pane's view without taking the keyboard
    /// from it, so a child with focus reports on (DECSET 1004) receives no
    /// `ESC[O ESC[I`, and a marked-text composition stays open.
    func testT9MaximizeSendsNoFocusReports() async throws {
        let controller = try openTab()
        let pane = controller.focusedPane
        _ = try split(controller, .right)
        controller.focus(pane)
        let window = try XCTUnwrap(controller.window)
        XCTAssertTrue(window.firstResponder === pane.view)
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-focus-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: url) }
        // Every byte the child reads goes to the file.
        try pane.session.write(Array("stty raw -echo; printf '\\033[?1004h'; cat > '\(url.path)'\n".utf8))
        try await waitUntil("cat runs") { try pane.session.foregroundProcess()?.name == "cat" }
        func received() -> [UInt8] { (try? [UInt8](Data(contentsOf: url))) ?? [] }
        func contains(_ bytes: [UInt8], _ part: [UInt8]) -> Bool {
            bytes.count >= part.count && (0...(bytes.count - part.count)).contains { Array(bytes[$0..<$0 + part.count]) == part }
        }
        let focusIn: [UInt8] = [0x1b, 0x5b, 0x49]
        let focusOut: [UInt8] = [0x1b, 0x5b, 0x4f]
        // The session focused as in a key window (a locked session has
        // none), and the reports reach the child.
        try await waitUntil("the child gets focus reports") {
            pane.view.windowKeyChanged(isKey: false)
            pane.view.windowKeyChanged(isKey: true)
            return contains(received(), focusIn)
        }
        // A marker, so every report sent so far has arrived: pasted, so it
        // queues behind the reports (a write goes straight to the pty and
        // could overtake one).
        try pane.session.paste("a")
        try await waitUntil("the first marker arrives") { received().last == 0x61 }
        let before = received().count

        pane.view.setMarkedText(
            "に", selectedRange: NSRange(location: 1, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertTrue(pane.view.hasMarkedText())
        for cycle in 0..<3 {
            for zoomed in [true, false] {
                XCTAssertTrue(controller.toggleZoom())
                XCTAssertEqual(controller.isZoomed, zoomed)
                let state = "\(zoomed ? "maximized" : "every pane shown"), cycle \(cycle)"
                XCTAssertTrue(window.firstResponder === pane.view, "the pane keeps the keyboard, \(state)")
                XCTAssertTrue(pane.view.hasMarkedText(), "the composition stays, \(state)")
            }
        }
        try pane.session.paste("x")
        try await waitUntil("the last marker arrives") { received().last == 0x78 }
        let after = Array(received()[before...])
        XCTAssertFalse(contains(after, focusOut), "a focus-out report: \(after)")
        XCTAssertFalse(contains(after, focusIn), "a focus-in report: \(after)")
        XCTAssertEqual(after, [0x78])
    }

    /// #331 M3: ⌘⇧↩ with the find field focused leaves the keyboard in the
    /// field, maximized and back.
    func testMaximizingWithTheFindFieldFocusedKeepsTheKeyboardInIt() throws {
        let controller = try openTab()
        let pane = controller.focusedPane
        _ = try split(controller, .right)
        controller.focus(pane)
        pane.view.showFindBar(nil)
        let bar = try XCTUnwrap(pane.view.findBar)
        XCTAssertTrue(bar.fieldHasFocus, "the keyboard is in the field")
        for zoomed in [true, false] {
            controller.toggleZoomPane(nil)
            XCTAssertEqual(controller.isZoomed, zoomed)
            XCTAssertTrue(bar.fieldHasFocus, zoomed ? "maximized" : "every pane shown")
            XCTAssertTrue(controller.focusedPane === pane)
        }
    }

    /// While the window itself holds the keyboard (nothing in the tab has
    /// it), maximizing and showing every pane again give it to the focused
    /// pane.
    func testMaximizingGivesTheKeyboardToTheFocusedPaneWhenTheWindowHoldsIt() throws {
        let controller = try openTab()
        let pane = controller.focusedPane
        _ = try split(controller, .right)
        controller.focus(pane)
        let window = try XCTUnwrap(controller.window)
        for zoomed in [true, false] {
            XCTAssertTrue(window.makeFirstResponder(nil))
            XCTAssertTrue(window.firstResponder === window)
            XCTAssertTrue(controller.toggleZoom())
            XCTAssertEqual(controller.isZoomed, zoomed)
            XCTAssertTrue(window.firstResponder === pane.view, zoomed ? "maximized" : "every pane shown")
        }
    }

    // MARK: The shade

    /// A dimmed pane's shade covers it after the window resizes, and goes
    /// back above a view the pane adds later when the dimming changes.
    func testTheShadeCoversItsPaneAfterAResizeAndStaysAboveLaterViews() throws {
        try startCore()
        defer { appDelegate.paneDimming = 0 }
        appDelegate.paneDimming = 0.4
        let controller = try openTab()
        let left = controller.focusedPane
        _ = try split(controller, .right)
        let shade = try XCTUnwrap(shades(controller)[0], "the unfocused pane is dimmed")
        let window = try XCTUnwrap(controller.window)
        let start = left.view.bounds
        for size in [NSSize(width: 1000, height: 700), NSSize(width: 700, height: 500), NSSize(width: 1300, height: 900)] {
            window.setContentSize(size)
            controller.paneContainer.layoutSubtreeIfNeeded()
            XCTAssertEqual(shade.frame, left.view.bounds, "\(size)")
        }
        XCTAssertNotEqual(left.view.bounds, start, "the pane was resized")

        let later = NSView(frame: NSRect(x: 0, y: 0, width: 40, height: 40))
        left.view.addSubview(later)
        appDelegate.paneDimming = 0.5
        XCTAssertTrue(shades(controller)[0] === shade, "the same shade")
        XCTAssertTrue(left.view.subviews.last === shade, "above the later view")
        XCTAssertEqual(shade.alphaValue, 0.5, accuracy: 0.001)
        later.removeFromSuperview()
    }

    // MARK: Saving

    /// A 100-step divider drag through AppKit's tracking asks for one
    /// window-state save, at its end, and the saved ratio is the drag's.
    func testADividerDragSavesTheWindowStateOnceAtItsEnd() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        _ = try split(controller, .right)
        let proxy = PaneHostProxy(app: appDelegate)
        controller.delegate = proxy
        let defaults = try XCTUnwrap(CountingDefaults(suiteName: "com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store
        defer {
            appDelegate.windowStateStore = nil
            controller.delegate = appDelegate
        }
        let window = try XCTUnwrap(controller.window)
        let outer = try XCTUnwrap(controller.paneContainer.subviews.first as? PaneSplitView)
        let start = frame(of: left, in: controller).maxX + 0.5
        func event(_ type: NSEvent.EventType, x: CGFloat) throws -> NSEvent {
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: outer.convert(NSPoint(x: x, y: 200), to: nil), modifierFlags: [],
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber, context: nil,
                    eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1))
        }
        for step in 1...100 {
            NSApp.postEvent(try event(.leftMouseDragged, x: start - CGFloat(2 * step)), atStart: false)
        }
        NSApp.postEvent(try event(.leftMouseUp, x: start - 200), atStart: false)
        outer.mouseDown(with: try event(.leftMouseDown, x: start))
        XCTAssertLessThan(frame(of: left, in: controller).maxX, start - 50, "the drag moved the divider")
        XCTAssertEqual(proxy.layoutChanges, 1, "one save asked for, at the end of the drag")

        let ratio = Double(controller.paneContainer.tree.root.splits[0].ratio)
        try await waitUntil("the drag's ratio is saved") {
            guard case .split(_, let saved, _, _) = store.load()?.windows.first?.layout(ofTab: 0) else { return false }
            return abs(saved - ratio) < 1e-9
        }
        // A click on the divider that moves nothing asks for none.
        NSApp.postEvent(try event(.leftMouseUp, x: frame(of: left, in: controller).maxX + 0.5), atStart: false)
        outer.mouseDown(with: try event(.leftMouseDown, x: frame(of: left, in: controller).maxX + 0.5))
        XCTAssertEqual(proxy.layoutChanges, 1, "a click without a move")
    }

    /// A pane's shell exiting during a divider drag (#371), with the events
    /// posted to NSApp's queue and the press dispatched from the run loop,
    /// so that AppKit's tracking loop takes the drag's events from the queue
    /// as for a real drag, and the shell's exit arrives inside it; a Timer
    /// in the event-tracking mode drives the drag. The tree changes at once, the split views only when
    /// the drag ends: AppKit raised (`objectAtIndex:` beyond bounds) on the
    /// next step after a rebuild, which ended the drag without
    /// ``PaneContainerView/dividerDragDidEnd(_:)``. The outer split survives
    /// the close, so the drag's last position is its ratio, and saved.
    func testAPaneClosingDuringADividerDragLetsTheDragEndAndKeepsItsRatio() async throws {
        // Weak, assigned (not initialised) so that Swift 6.1 does not ask
        // for a `weak let`, which it does not have. The test's own pool
        // would keep the split views, autoreleased by the layouts, and with
        // them the closed pane's view.
        weak var closing: Pane?
        weak var closingView: TerminalSurfaceView?
        let (controller, left, topRight) = try autoreleasepool {
            let controller = try openTab()
            let left = controller.focusedPane
            let topRight = try split(controller, .right)
            let bottomRight = try split(controller, .below)
            closing = bottomRight
            closingView = bottomRight.view
            XCTAssertTrue(controller.paneContainer.subviews.first is PaneSplitView)
            return (controller, left, topRight)
        }
        try skipUnlessAttached(controller)
        let proxy = PaneHostProxy(app: appDelegate)
        controller.delegate = proxy
        let defaults = try XCTUnwrap(CountingDefaults(suiteName: "com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store
        defer {
            appDelegate.windowStateStore = nil
            controller.delegate = appDelegate
        }
        let container = controller.paneContainer
        let window = try XCTUnwrap(controller.window)
        let start = autoreleasepool { frame(of: left, in: controller).maxX + 0.5 }
        let windowNumber = window.windowNumber
        // Not through the split view, which the drag's end drops.
        let toWindow = { (x: CGFloat) in container.convert(NSPoint(x: x, y: 200), to: nil) }
        @MainActor func post(_ type: NSEvent.EventType, x: CGFloat) {
            guard
                let event = NSEvent.mouseEvent(
                    with: type, location: toWindow(x), modifierFlags: [],
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: windowNumber, context: nil,
                    eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1)
            else { return XCTFail("no \(type) event") }
            NSApp.postEvent(event, atStart: false)
        }

        /// 0: wait for the drag to track, then drag 100 points; 1: once the
        /// divider moved, exit the bottom right shell; 2: once its pane is
        /// gone from the tree, drag 100 more and let go; 3: done.
        let driver = DragDriver()
        let deadline = ContinuousClock.now + .seconds(10)
        driver.onTick = { [unowned driver] in
            guard ContinuousClock.now < deadline else {
                driver.failure = "timed out at step \(driver.step)"
                post(.leftMouseUp, x: start - 200)
                return driver.step = 3
            }
            switch driver.step {
            case 0 where container.isTrackingDividerDrag:
                for step in 1...10 { post(.leftMouseDragged, x: start - CGFloat(10 * step)) }
                driver.step = 1
            case 1 where self.frame(of: left, in: controller).maxX < start - 50:
                do {
                    try closing?.session.write(Array("exit\n".utf8))
                } catch {
                    driver.failure = "write: \(error)"
                }
                driver.step = 2
            case 2 where controller.panes.count == 2:
                XCTAssertTrue(container.isTrackingDividerDrag, "the pane closed while the drag tracked")
                for step in 11...20 { post(.leftMouseDragged, x: start - CGFloat(10 * step)) }
                post(.leftMouseUp, x: start - 200)
                driver.step = 3
            default:
                break
            }
        }
        let timer = Timer(
            timeInterval: 0.01, target: driver, selector: #selector(DragDriver.fire(_:)), userInfo: nil, repeats: true)
        RunLoop.main.add(timer, forMode: .eventTracking)
        defer { timer.invalidate() }
        // The press is posted and dispatched as NSApp's loop does, from a
        // Timer (not the main queue, whose blocks would wait for it, nor
        // the test's task), in a pool of its own: the test's would keep
        // what the drag autoreleases, the closed pane's view with it.
        let downDriver = DragDriver()
        downDriver.onTick = { [unowned downDriver] in
            autoreleasepool {
                post(.leftMouseDown, x: start)
                if let event = NSApp.nextEvent(
                    matching: .leftMouseDown, until: .distantPast, inMode: .default, dequeue: true)
                {
                    NSApp.sendEvent(event)
                } else {
                    downDriver.failure = "the press was not queued"
                }
            }
            downDriver.step = 3
        }
        RunLoop.main.add(
            Timer(
                timeInterval: 0, target: downDriver, selector: #selector(DragDriver.fire(_:)), userInfo: nil,
                repeats: true),
            forMode: .default)

        try await waitUntil("the drag ends") {
            driver.failure != nil || (driver.step == 3 && !container.isTrackingDividerDrag)
        }
        XCTAssertNil(downDriver.failure)
        XCTAssertNil(driver.failure)
        XCTAssertEqual(controller.panes, [left, topRight])
        XCTAssertNil(closingView?.window, "the closed pane's view left the tab")
        let width = frame(of: left, in: controller).maxX
        XCTAssertEqual(width, start - 200.5, accuracy: 1, "the divider is where the drag let go")
        let ratio = container.tree.root.splits[0].ratio
        let space = container.bounds.width - PaneSplitView.thickness
        XCTAssertEqual(ratio * space, width, accuracy: 1, "the tree keeps the drag's ratio")
        try await waitUntil("the drag's ratio is saved") {
            guard case .split(_, let saved, _, _) = store.load()?.windows.first?.layout(ofTab: 0) else { return false }
            return abs(saved - Double(ratio)) < 1e-9
        }
        try? await waitUntil("the closed pane is freed", timeout: .seconds(5)) { closing == nil && closingView == nil }
        XCTAssertNil(closing, "pane")
        XCTAssertNil(closingView, "view")
    }

    /// Presses on the divider at `x` (in `container`'s coordinates), as
    /// ``testAPaneClosingDuringADividerDragLetsTheDragEndAndKeepsItsRatio()``
    /// does, and runs `steps` in turn from an event-tracking Timer: each is
    /// retried every 10 ms until it returns `true`, and gets a function that
    /// posts a mouse event at an x. Returns once every step ran and the
    /// drag stopped tracking (or the container is gone).
    private func trackDividerDrag(
        at x: CGFloat, in container: PaneContainerView, window: NSWindow,
        steps: [(_ post: (NSEvent.EventType, CGFloat) -> Void) -> Bool]
    ) async throws {
        weak var weakContainer: PaneContainerView?
        weakContainer = container
        let windowNumber = window.windowNumber
        let toWindow = { (x: CGFloat) in weakContainer?.convert(NSPoint(x: x, y: 200), to: nil) ?? .zero }
        let post = { (type: NSEvent.EventType, x: CGFloat) in
            guard
                let event = NSEvent.mouseEvent(
                    with: type, location: toWindow(x), modifierFlags: [],
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: windowNumber, context: nil,
                    eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1)
            else { return XCTFail("no \(type) event") }
            NSApp.postEvent(event, atStart: false)
        }
        let driver = DragDriver()
        let deadline = ContinuousClock.now + .seconds(10)
        var next = 0
        driver.onTick = { [unowned driver] in
            guard ContinuousClock.now < deadline else {
                driver.failure = "timed out at step \(next)"
                post(.leftMouseUp, x)
                return driver.step = 3
            }
            guard weakContainer?.isTrackingDividerDrag == true, steps[next](post) else { return }
            next += 1
            if next == steps.count { driver.step = 3 }
        }
        let timer = Timer(
            timeInterval: 0.01, target: driver, selector: #selector(DragDriver.fire(_:)), userInfo: nil, repeats: true)
        RunLoop.main.add(timer, forMode: .eventTracking)
        defer { timer.invalidate() }
        let downDriver = DragDriver()
        downDriver.onTick = { [unowned downDriver] in
            autoreleasepool {
                post(.leftMouseDown, x)
                if let event = NSApp.nextEvent(
                    matching: .leftMouseDown, until: .distantPast, inMode: .default, dequeue: true)
                {
                    NSApp.sendEvent(event)
                } else {
                    downDriver.failure = "the press was not queued"
                }
            }
            downDriver.step = 3
        }
        RunLoop.main.add(
            Timer(
                timeInterval: 0, target: downDriver, selector: #selector(DragDriver.fire(_:)), userInfo: nil,
                repeats: true),
            forMode: .default)
        try await waitUntil("the drag ends") {
            driver.failure != nil || (driver.step == 3 && weakContainer?.isTrackingDividerDrag != true)
        }
        XCTAssertNil(downDriver.failure)
        XCTAssertNil(driver.failure)
    }

    /// Every pane's shell exiting during a divider drag (#371): the last
    /// one closes the window, whose views leave it only once the drag has
    /// ended. Taking them out inside AppKit's tracking loop raised as a
    /// rebuild did, and skipped ``PaneContainerView/dividerDragDidEnd(_:)``.
    func testEveryPaneClosingDuringADividerDragLetsTheDragEndAndFreesTheTab() async throws {
        struct WeakRef {
            weak var object: AnyObject?
        }
        weak var weakController: TerminalWindowController?
        weak var weakContainer: PaneContainerView?
        var weakViews: [WeakRef] = []
        let (initialSessions, start) = try autoreleasepool {
            let controller = try openTab()
            _ = try split(controller, .right)
            try skipUnlessAttached(controller)
            weakController = controller
            weakContainer = controller.paneContainer
            weakViews = controller.panes.map { WeakRef(object: $0.view) }
            return (controller.panes.map(\.session), frame(of: controller.panes[0], in: controller).maxX + 0.5)
        }
        var sessions = initialSessions
        try await trackDividerDrag(
            at: start, in: try XCTUnwrap(weakContainer), window: try XCTUnwrap(weakController?.window),
            steps: [
                { post in
                    for step in 1...5 { post(.leftMouseDragged, start - CGFloat(10 * step)) }
                    return true
                },
                { _ in
                    for session in sessions { try? session.write(Array("exit\n".utf8)) }
                    sessions = []
                    return true
                },
                { post in
                    guard !self.appDelegate.windowControllers.contains(where: { $0 === weakController }) else {
                        return false
                    }
                    post(.leftMouseDragged, start - 100)
                    post(.leftMouseUp, start - 100)
                    return true
                },
            ])
        try? await waitUntil("the closed tab is freed") {
            weakController == nil && weakContainer == nil && weakViews.allSatisfy { $0.object == nil }
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakContainer, "container")
        XCTAssertEqual(weakViews.compactMap(\.object).count, 0, "views")
    }

    /// A pane's shell exiting before the drag moves the divider (#371): the
    /// drag's moves after it reach the tree only at its end, which saves
    /// them.
    func testAPaneClosingBeforeADividerDragMovesKeepsAndSavesTheDragsRatio() async throws {
        weak var closing: Pane?
        let (controller, left, topRight) = try autoreleasepool {
            let controller = try openTab()
            let left = controller.focusedPane
            let topRight = try split(controller, .right)
            closing = try split(controller, .below)
            return (controller, left, topRight)
        }
        try skipUnlessAttached(controller)
        let proxy = PaneHostProxy(app: appDelegate)
        controller.delegate = proxy
        let defaults = try XCTUnwrap(CountingDefaults(suiteName: "com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store
        defer {
            appDelegate.windowStateStore = nil
            controller.delegate = appDelegate
        }
        let container = controller.paneContainer
        let start = autoreleasepool { frame(of: left, in: controller).maxX + 0.5 }
        try await trackDividerDrag(
            at: start, in: container, window: try XCTUnwrap(controller.window),
            steps: [
                { _ in
                    try? closing?.session.write(Array("exit\n".utf8))
                    return true
                },
                { post in
                    guard controller.panes.count == 2 else { return false }
                    for step in 1...20 { post(.leftMouseDragged, start - CGFloat(10 * step)) }
                    post(.leftMouseUp, start - 200)
                    return true
                },
            ])
        XCTAssertEqual(controller.panes, [left, topRight])
        let width = frame(of: left, in: controller).maxX
        XCTAssertEqual(width, start - 200.5, accuracy: 1, "the divider is where the drag let go")
        let ratio = container.tree.root.splits[0].ratio
        XCTAssertEqual(ratio * (container.bounds.width - PaneSplitView.thickness), width, accuracy: 1)
        try await waitUntil("the drag's ratio is saved") {
            guard case .split(_, let saved, _, _) = store.load()?.windows.first?.layout(ofTab: 0) else { return false }
            return abs(saved - Double(ratio)) < 1e-9
        }
    }

    /// A spawn failing partway through a restored layout: the tab keeps its
    /// one pane, and the session started before the failure is closed and
    /// handed back to the app.
    func testASpawnFailingPartwayThroughARestoreClosesTheSessionsItStarted() throws {
        let controller = try openTab()
        let proxy = PaneHostProxy(app: appDelegate)
        controller.delegate = proxy
        defer { controller.delegate = appDelegate }
        proxy.spawnsLeft = 1
        let layout = PaneLayout.split(
            .right, ratio: 0.5, first: .leaf, second: .split(.below, ratio: 0.5, first: .leaf, second: .leaf))
        XCTAssertThrowsError(try controller.restoreLayout(layout)) { error in
            XCTAssertTrue(error is PaneHostProxy.SpawnRefused, "\(error)")
        }
        XCTAssertEqual(controller.panes.count, 1)
        XCTAssertEqual(proxy.spawned.count, 1, "one session started before the failure")
        XCTAssertEqual(proxy.closed.map(\.id), proxy.spawned.map(\.id), "and it is closed")
        XCTAssertThrowsError(try proxy.spawned[0].close(), "already closed") { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertEqual(proxy.layoutChanges, 0, "nothing changed to save")
    }

    // MARK: ⌘W's title outside the panes

    /// With the keyboard outside the panes (the window holds it), ⌘W
    /// closes the tab and its title says so, however many panes it has.
    func testCommandWSaysCloseTabWhileTheKeyboardIsOutsideThePanes() throws {
        let item = try XCTUnwrap(MainMenu.item(forCommand: "close_tab", in: NSApp.mainMenu))
        defer { item.title = PaneContainerView.closeTabTitle }
        let controller = try openTab()
        let window = try XCTUnwrap(controller.window)
        _ = try split(controller, .right)
        func validate() -> Bool {
            var responder: NSResponder? = window.firstResponder
            while let current = responder, !current.responds(to: item.action) {
                responder = current.nextResponder
            }
            return (responder as? any NSMenuItemValidation)?.validateMenuItem(item) ?? false
        }
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Pane", "the focused pane holds the keyboard")
        XCTAssertTrue(window.makeFirstResponder(nil))
        XCTAssertTrue(window.firstResponder === window)
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Tab", "the window holds the keyboard")
        controller.focus(controller.focusedPane)
        XCTAssertTrue(validate())
        XCTAssertEqual(item.title, "Close Pane", "back in the pane")
    }
}
