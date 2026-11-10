import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The exit-status gutter's hover in `TerminalSurfaceView` (#45b, ADR
/// 0039): one tool-tip rect per row over the left padding, whose text is the
/// core's record of the command on that row.
///
/// The child prints a failing command that runs for 300 ms and a succeeding
/// one with their OSC 133 marks: row 0 is the first prompt, row 1 its
/// output, row 2 the second prompt (the shell reports the first command's
/// end there, as zsh does) and row 3 the live prompt. As in ``PromptNavigationTests`` the view
/// lives in a ``KeyControlledWindow`` that is never on screen.
@MainActor
final class GutterToolTipTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        window?.close()
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
        appDelegate = nil
        view = nil
        window = nil
    }

    private func openWithCommands() async throws {
        let mark = { (payload: String) in "\\033]133;\(payload)\\007" }
        let failing = "\(mark("A"))$ \(mark("B"))false\\n\(mark("C"))no\\n"
        let succeeding = "\(mark("A"))$ \(mark("B"))true\\n\(mark("C"))\(mark("D;0"))\(mark("A"))$ "
        // Each step waits for a line (unechoed) from the test. The script
        // shows `ready` once echo is off and erases it before the first
        // prompt: a line written before `stty -echo` ran would be echoed
        // and push every row down one. The commands start after the window
        // sized the grid: a resize before a command finished would make its
        // duration unknown. The failing command ends in a read of its own:
        // had its end come in one read with the next command's, only the
        // last would get a record (D-COALESCE, ADR 0039).
        let script =
            "stty -echo; printf ready; read go; printf '\\r\\033[K\(failing)'; sleep 0.3; "
            + "printf '\(mark("D;1"))'; read go; printf '\(succeeding)'; exec cat"
        try await open(script)
        try view.session.write(Array("go\n".utf8))
        try await waitUntil("the failing command's mark") {
            (try? self.view.session.commandMark(atRow: 0)) != nil
        }
        try view.session.write(Array("go\n".utf8))
        try await waitUntil("both commands' marks") {
            self.view.metrics != nil && (try? self.view.session.commandMark(atRow: 2)) != nil
        }
    }

    /// Opens the view running `script`, which prints `ready` once echo is
    /// off and then reads a line before it goes on.
    private func open(_ script: String) async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", script])
        let controller = try appDelegate.newWindow()
        view = controller.focusedPane.view
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        view.frameStatusOverride = .presented
        view.occlusionOverride = false
        try await waitUntil("the surface") {
            guard let metrics = self.view.metrics, let position = try? self.view.session.scrollPosition else {
                return false
            }
            return position.rows == metrics.rows
        }
        try await waitUntil("echo off") {
            (try? self.view.session.accessibleText().string.hasPrefix("ready")) == true
        }
    }

    /// The middle of `row`'s slice of the left padding, or of its first
    /// cell, in view coordinates.
    private func point(row: UInt16, inCell: Bool = false) throws -> NSPoint {
        let metrics = try XCTUnwrap(view.metrics)
        let scale = CGFloat(metrics.scale)
        let padding = CGFloat(metrics.paddingPx) / scale
        let x = inCell ? padding + CGFloat(metrics.cellWidthPx) / scale / 2 : padding / 2
        let fromTop = padding + (CGFloat(row) + 0.5) * CGFloat(metrics.cellHeightPx) / scale
        return NSPoint(x: x, y: view.isFlipped ? fromTop : view.bounds.height - fromTop)
    }

    private func tip(row: UInt16, inCell: Bool = false) throws -> String {
        view.view(view, stringForToolTip: 0, point: try point(row: row, inCell: inCell), userData: nil)
    }

    func testTheTipShowsTheCoresExitStatusAndDuration() async throws {
        try await openWithCommands()
        let failed = try XCTUnwrap(try view.session.commandMark(atRow: 0))
        XCTAssertEqual(failed.exitCode, 1)
        let duration = try XCTUnwrap(failed.duration, "the core recorded how long it ran")
        // The core times C and D when it processes them, not when the
        // child wrote them: allow some of the 300 ms sleep to be lost.
        XCTAssertGreaterThanOrEqual(duration, .milliseconds(250))
        XCTAssertEqual(try tip(row: 0), "Exit status 1\nRan for \(TerminalSurfaceView.formatted(duration))")
        XCTAssertTrue(try tip(row: 0).hasPrefix("Exit status 1\nRan for "))
        let succeeded = try XCTUnwrap(try view.session.commandMark(atRow: 2))
        XCTAssertEqual(succeeded.exitCode, 0)
        // Its marks came in one read: whether its record kept a duration
        // depends on that read's marks, so only the status is pinned.
        XCTAssertEqual(try tip(row: 2), TerminalSurfaceView.gutterToolTip(for: succeeded))
        XCTAssertTrue(try tip(row: 2).hasPrefix("Exit status 0\n"))
    }

    func testRowsWithoutAMarkAndTheCellsHaveNoTip() async throws {
        try await openWithCommands()
        XCTAssertEqual(try tip(row: 1), "", "the failing command's output row")
        XCTAssertEqual(try tip(row: 3), "", "the live prompt has not finished")
        XCTAssertEqual(try tip(row: 0, inCell: true), "", "only the padding hovers")
        XCTAssertNil(view.gutterToolTip(at: NSPoint(x: -1, y: try point(row: 0).y)))
    }

    func testOneTipRectPerRow() async throws {
        try await openWithCommands()
        let metrics = try XCTUnwrap(view.metrics)
        XCTAssertEqual(view.gutterToolTips.tags.count, Int(metrics.rows))
        let tags = view.gutterToolTips.tags
        view.updateGutterToolTips()
        XCTAssertEqual(view.gutterToolTips.tags, tags, "nothing changed: the rects stay")
    }

    /// The rect AppKit should have for `row`'s slice of the left padding,
    /// from the view's current metrics.
    private func expectedRect(row: Int) throws -> NSRect {
        let metrics = try XCTUnwrap(view.metrics)
        let scale = CGFloat(metrics.scale)
        let padding = CGFloat(metrics.paddingPx) / scale
        let cellHeight = CGFloat(metrics.cellHeightPx) / scale
        let fromTop = padding + CGFloat(row) * cellHeight
        let y = view.isFlipped ? fromTop : view.bounds.height - fromTop - cellHeight
        return NSRect(x: 0, y: y, width: padding, height: cellHeight)
    }

    /// Each row's rect lies in the left padding and its centre maps to that
    /// row: pins the flip (`let y = fromTop`) and the origin (`x: padding`).
    func testEachTipRectIsItsRowsSliceOfThePadding() async throws {
        try await openWithCommands()
        let metrics = try XCTUnwrap(view.metrics)
        let rects = view.gutterToolTips.rects
        XCTAssertEqual(rects.count, Int(metrics.rows))
        XCTAssertEqual(rects.count, view.gutterToolTips.tags.count)
        for (row, rect) in rects.enumerated() {
            XCTAssertEqual(rect, try expectedRect(row: row), "row \(row)")
            let (cell, pixels) = view.cell(at: NSPoint(x: rect.midX, y: rect.midY))
            XCTAssertEqual(Int(cell.row), row, "the centre of row \(row)'s rect")
            XCTAssertGreaterThanOrEqual(pixels.x, 0)
            XCTAssertLessThan(pixels.x, CGFloat(metrics.paddingPx), "row \(row)'s rect is in the padding")
        }
        XCTAssertEqual(
            view.gutterToolTip(at: NSPoint(x: rects[0].midX, y: rects[0].midY)), try tip(row: 0),
            "the top rect hovers the top row's mark")
        XCTAssertFalse(try tip(row: 0).isEmpty)
    }

    /// The top-left corner (above row 0) has no tip although row 0 has a
    /// mark: `cell(at:)` clamps it to row 0, so only the `fromTop >= 0`
    /// guard keeps it out.
    func testTheTopLeftPaddingHasNoTip() async throws {
        try await openWithCommands()
        let metrics = try XCTUnwrap(view.metrics)
        let padding = CGFloat(metrics.paddingPx) / CGFloat(metrics.scale)
        XCTAssertFalse(try tip(row: 0).isEmpty, "row 0 has a mark")
        let fromTop = padding / 2
        let point = NSPoint(x: padding / 2, y: view.isFlipped ? fromTop : view.bounds.height - fromTop)
        XCTAssertNil(view.gutterToolTip(at: point))
    }

    /// The bottom-left corner (below the last row) has no tip although the
    /// last row has a mark: `cell(at:)` clamps it to that row, so only the
    /// `fromTop < rows * cellHeight` guard keeps it out.
    func testTheBottomLeftPaddingHasNoTip() async throws {
        let mark = { (payload: String) in "\\033]133;\(payload)\\007" }
        let command = "\(mark("A"))$ \(mark("B"))false\\n\(mark("C"))no\\n\(mark("D;1"))"
        // 100 lines, the command (its prompt is line 100), 200 lines.
        try await open(
            "stty -echo; printf ready; read go; printf '\\r\\033[K'; seq 1 100; printf '\(command)'; "
                + "seq 1 200; exec cat")
        try view.session.write(Array("go\n".utf8))
        try await waitUntil("all the lines") {
            (try? self.view.session.accessibleText().string.contains("\n200")) == true
        }
        let metrics = try XCTUnwrap(view.metrics)
        let rows = UInt64(metrics.rows)
        let history = try view.session.scrollPosition.history
        // The top row shows line `history - offset`: put line 100 last.
        try view.session.scroll(to: .offset(history - (100 - (rows - 1))))
        let last = metrics.rows - 1
        XCTAssertNotNil(try view.session.commandMark(atRow: last), "the last row has the command's mark")
        XCTAssertFalse(try tip(row: last).isEmpty)
        let scale = CGFloat(metrics.scale)
        let padding = CGFloat(metrics.paddingPx) / scale
        let gridBottom = padding + CGFloat(metrics.rows) * CGFloat(metrics.cellHeightPx) / scale
        let below = view.bounds.height - gridBottom
        XCTAssertGreaterThan(below, 0, "there is padding below the last row")
        let fromTop = gridBottom + below / 2
        let point = NSPoint(x: padding / 2, y: view.isFlipped ? fromTop : view.bounds.height - fromTop)
        XCTAssertEqual(view.cell(at: point).cell.row, last, "the point clamps to the last row")
        XCTAssertNil(view.gutterToolTip(at: point))
    }

    /// After a resize the core forgets the command's lines, so its duration
    /// is unknown: the tip says so instead of "Ran for 0 ms"
    /// (`duration_known == false`).
    func testAResizeBeforeTheHoverLeavesTheDurationUnknown() async throws {
        try await openWithCommands()
        let before = try XCTUnwrap(view.metrics)
        XCTAssertNotNil(try view.session.commandMark(atRow: 0)?.duration, "known before the resize")
        let size = view.frame.size
        window.setContentSize(NSSize(width: size.width - 80, height: size.height))
        try await waitUntil("the grid re-sized") {
            guard let metrics = self.view.metrics, let position = try? self.view.session.scrollPosition else {
                return false
            }
            return metrics.cols != before.cols && position.rows == metrics.rows
        }
        let mark = try XCTUnwrap(try view.session.commandMark(atRow: 0), "the mark stays")
        XCTAssertEqual(mark.exitCode, 1)
        XCTAssertNil(mark.duration)
        XCTAssertEqual(try tip(row: 0), "Exit status 1\nDuration unknown")
    }

    /// A font-size change re-lays the rects at once, without waiting for
    /// a layout pass: pins `defer { updateGutterToolTips() }` in
    /// `refreshMetrics()`.
    func testAFontSizeChangeMovesTheTipRects() async throws {
        try await openWithCommands()
        let before = try XCTUnwrap(view.metrics)
        let rects = view.gutterToolTips.rects
        view.increaseFontSize(nil)
        let after = try XCTUnwrap(view.metrics)
        XCTAssertNotEqual(after.cellHeightPx, before.cellHeightPx, "the cells grew")
        XCTAssertNotEqual(view.gutterToolTips.rects, rects)
        XCTAssertEqual(view.gutterToolTips.rects.count, Int(after.rows))
        XCTAssertEqual(view.gutterToolTips.tags.count, Int(after.rows))
        for (row, rect) in view.gutterToolTips.rects.enumerated() {
            XCTAssertEqual(rect, try expectedRect(row: row), "row \(row)")
        }
    }

    func testTheTipFormatsDurationsTheSameInEveryLocale() {
        let tip = { (code: Int32?, duration: Duration?) in
            TerminalSurfaceView.gutterToolTip(for: .init(exitCode: code, duration: duration))
        }
        XCTAssertEqual(tip(1, .milliseconds(850)), "Exit status 1\nRan for 850 ms")
        XCTAssertEqual(tip(0, .milliseconds(4_250)), "Exit status 0\nRan for 4.2 s")
        XCTAssertEqual(tip(127, .seconds(192)), "Exit status 127\nRan for 3 min 12 s")
        XCTAssertEqual(tip(0, .seconds(7_500)), "Exit status 0\nRan for 2 h 5 min")
        XCTAssertEqual(tip(nil, .zero), "Exit status unknown\nRan for 0 ms")
        XCTAssertEqual(tip(2, nil), "Exit status 2\nDuration unknown")
        XCTAssertEqual(tip(0, .seconds(Int64.max)), "Exit status 0\nRan for \(Int64.max / 3600) h")
        // The seconds fit in milliseconds but adding the fraction does
        // not: the hours, not a trap.
        let seconds = Int64.max / 1000
        XCTAssertEqual(
            tip(0, Duration(secondsComponent: seconds, attosecondsComponent: 999_000_000_000_000_000)),
            "Exit status 0\nRan for \(seconds / 3600) h")
    }

    /// Polls `condition` on the main actor until it holds or ten seconds
    /// pass.
    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
