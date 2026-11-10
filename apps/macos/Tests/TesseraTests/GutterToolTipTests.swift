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
        try view.session.write(Array("go\n".utf8))
        try await waitUntil("the failing command's mark") {
            (try? self.view.session.commandMark(atRow: 0)) != nil
        }
        try view.session.write(Array("go\n".utf8))
        try await waitUntil("both commands' marks") {
            self.view.metrics != nil && (try? self.view.session.commandMark(atRow: 2)) != nil
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
        XCTAssertGreaterThanOrEqual(duration, .milliseconds(300))
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
