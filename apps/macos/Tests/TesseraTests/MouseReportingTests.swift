import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Mouse reporting through `TerminalSurfaceView` (#12): the view sends every
/// pointer event to the core, and the bytes the core writes for a program
/// that tracks the mouse reach the PTY.
///
/// The child turns on any-motion tracking with SGR coordinates (`CSI ? 1003
/// h CSI ? 1006 h`, unless a test asks for other modes), prints 200 lines
/// and then records its input to a file in raw mode, as in
/// ``TerminalSurfaceViewTests``. The modes are output before the lines, so
/// once the session holds the lines the core has parsed the modes. The view
/// lives in a ``KeyControlledWindow`` with known metrics (10 x 20 px cells,
/// no padding, scale 1), and the tests call the view's event methods with
/// synthetic events, as AppKit would.
@MainActor
final class MouseReportingTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var file: URL!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!

    private static let anyMotionSGR = #"\033[?1003h\033[?1006h"#
    private static let lines = 200

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
        if let file {
            try? FileManager.default.removeItem(at: file)
        }
        appDelegate = nil
        view = nil
        window = nil
        file = nil
    }

    /// Opens a terminal whose child set `modes` (printf escapes) and records
    /// its input, and waits until both are in effect.
    private func open(modes: String = anyMotionSGR) async throws {
        file = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-mouse-\(UUID().uuidString).bin")
        appDelegate.spawnOptions = .init(
            program: "/bin/sh",
            arguments: ["-c", #"printf "$1"; seq 1 "$2"; stty raw -echo; exec cat > "$0""#, file.path, modes, "\(Self.lines)"])
        let controller = try appDelegate.newWindow()
        view = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        window.isKeyForTest = true
        view.windowKeyChanged(isKey: true)
        // Hidden: no frame is drawn, so no render replaces the metrics below.
        view.occlusionOverride = true
        view.metrics = .init(
            widthPx: 480, heightPx: 200, scale: 1, cellWidthPx: 10, cellHeightPx: 20, baselinePx: 15, paddingPx: 0,
            rows: 10, cols: 48, fontSizePt: 13, cursorRow: 0, cursorCol: 0, cursorOnScreen: true, cursorWide: false)
        try await waitUntil("the modes and lines are parsed and the recorder runs") {
            guard let position = try? self.view.session.scrollPosition else { return false }
            return position.history + UInt64(position.rows) > UInt64(Self.lines)
                && FileManager.default.fileExists(atPath: self.file.path)
        }
        // The core may still have the focus report (DECSET 1004 is off, so
        // none) or anything else queued; start the byte checks clean.
        try await assertBytes([], "nothing before the first event")
    }

    // MARK: Events

    /// A mouse event at the centre of cell `col`, `row` (zero-based).
    private func mouse(
        _ type: NSEvent.EventType, col: Int, row: Int, _ flags: NSEvent.ModifierFlags = []
    ) throws -> NSEvent {
        let fromTop = CGFloat(row) * 20 + 10
        let location = NSPoint(x: CGFloat(col) * 10 + 5, y: view.bounds.height - fromTop)
        return try XCTUnwrap(
            NSEvent.mouseEvent(
                with: type, location: view.convert(location, to: nil), modifierFlags: flags, timestamp: 0,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
    }

    private func moved(_ col: Int, _ row: Int, _ flags: NSEvent.ModifierFlags = []) throws {
        view.mouseMoved(with: try mouse(.mouseMoved, col: col, row: row, flags))
    }

    private func down(_ col: Int, _ row: Int, _ flags: NSEvent.ModifierFlags = []) throws {
        view.mouseDown(with: try mouse(.leftMouseDown, col: col, row: row, flags))
    }

    private func dragged(_ col: Int, _ row: Int, _ flags: NSEvent.ModifierFlags = []) throws {
        view.mouseDragged(with: try mouse(.leftMouseDragged, col: col, row: row, flags))
    }

    private func up(_ col: Int, _ row: Int, _ flags: NSEvent.ModifierFlags = []) throws {
        view.mouseUp(with: try mouse(.leftMouseUp, col: col, row: row, flags))
    }

    /// The bytes of an SGR report (`ESC [ <` ... `M` or `m`).
    private static func sgr(_ report: String) -> [UInt8] { Array("\u{1b}[<\(report)".utf8) }

    // MARK: Tests

    func testMotionPressDragAndReleaseReachTheProgramInSGR() async throws {
        try await open()
        try moved(3, 2)
        try await assertBytes(Self.sgr("35;4;3M"), "plain motion: button 3 + motion 32, one-based cell")
        try moved(3, 2)
        try await assertBytes([], "motion within the same cell is not reported again")
        try down(3, 2)
        XCTAssertEqual(view.pressOutcomes[.left], .consumed, "the program has the press: no selection")
        try dragged(5, 2)
        try up(5, 2)
        try await assertBytes(Self.sgr("0;4;3M") + Self.sgr("32;6;3M") + Self.sgr("0;6;3m"))
        XCTAssertNil(view.pressOutcomes[.left], "released")
    }

    func testRightButtonPressDragAndRelease() async throws {
        try await open()
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 0, row: 0))
        view.rightMouseDragged(with: try mouse(.rightMouseDragged, col: 1, row: 0))
        view.rightMouseUp(with: try mouse(.rightMouseUp, col: 1, row: 0))
        try await assertBytes(Self.sgr("2;1;1M") + Self.sgr("34;2;1M") + Self.sgr("2;2;1m"))
    }

    /// An `otherMouse` event of button `number` at cell `col`, `row`:
    /// `NSEvent.mouseEvent` cannot set the button number, so the event goes
    /// through its `CGEvent`.
    private func other(_ type: NSEvent.EventType, button number: Int64, col: Int, row: Int) throws -> NSEvent {
        let cgEvent = try XCTUnwrap(try mouse(type, col: col, row: row).cgEvent)
        cgEvent.setIntegerValueField(.mouseEventButtonNumber, value: number)
        let event = try XCTUnwrap(NSEvent(cgEvent: cgEvent))
        XCTAssertEqual(event.buttonNumber, Int(number))
        return event
    }

    func testMiddleBackAndForwardButtonsPressDragAndRelease() async throws {
        try await open()
        // Each button's press, drag and release; the release names the
        // button (SGR), so a release of the wrong button would show.
        for (number, code) in [(2, 1), (3, 128), (4, 129)] as [(Int64, Int)] {
            view.otherMouseDown(with: try other(.otherMouseDown, button: number, col: 0, row: 0))
            view.otherMouseDragged(with: try other(.otherMouseDragged, button: number, col: 1, row: 0))
            view.otherMouseUp(with: try other(.otherMouseUp, button: number, col: 1, row: 0))
            try await assertBytes(
                Self.sgr("\(code);1;1M") + Self.sgr("\(code + 32);2;1M") + Self.sgr("\(code);2;1m"),
                "button \(number)")
            XCTAssertTrue(view.pressOutcomes.isEmpty, "button \(number) released")
        }
    }

    func testShiftBypassesReportingForTheView() async throws {
        try await open()
        try down(1, 1, .shift)
        XCTAssertEqual(view.pressOutcomes[.left], .ignored, "Shift: the press is the view's (selection)")
        try dragged(2, 1, .shift)
        try up(2, 1)
        try await assertBytes([], "a bypassed press reports nothing, not even its release without Shift")
        try moved(4, 1, .shift)
        try await assertBytes([], "nor Shift-motion")
        // Shift-Option is block selection (30-app): the view's too.
        try down(1, 1, [.shift, .option])
        XCTAssertEqual(view.pressOutcomes[.left], .ignored)
        try up(1, 1, [.shift, .option])
        try await assertBytes([])

        // A reported press keeps its release, Shift or not: the program
        // must not be left with a held button.
        try down(1, 1)
        try up(1, 1, .shift)
        try await assertBytes(Self.sgr("0;2;2M") + Self.sgr("0;2;2m"))
    }

    func testCommandClickNeverReachesTheProgram() async throws {
        try await open()
        try down(1, 1, .command)
        XCTAssertEqual(view.pressOutcomes[.left], .ignored, "Command: a link click, the view's")
        try dragged(2, 1, .command)
        try up(2, 1, .command)
        try await assertBytes([])
    }

    func testOptionIsMetaAndControlIsControl() async throws {
        try await open()
        try down(0, 0, .option)
        try up(0, 0, .option)
        try down(0, 0, .control)
        try up(0, 0, .control)
        try down(0, 0, [.option, .control])
        try up(0, 0, [.option, .control])
        try await assertBytes(
            Self.sgr("8;1;1M") + Self.sgr("8;1;1m") + Self.sgr("16;1;1M") + Self.sgr("16;1;1m") + Self.sgr("24;1;1M")
                + Self.sgr("24;1;1m"),
            "Option is reported as Meta (8), never bypassed (D2)")
    }

    func testNormalTrackingReportsClicksButNotMotion() async throws {
        try await open(modes: #"\033[?1000h\033[?1006h"#)
        try moved(3, 2)
        try down(3, 2)
        try dragged(4, 2)
        try up(4, 2)
        try await assertBytes(Self.sgr("0;4;3M") + Self.sgr("0;5;3m"))
        XCTAssertNotNil(view.lastSentMouse, "motion was still sent to the core, which reports none in 1000")
    }

    func testTrackingOffReportsNothingAndLeavesEveryPressToTheView() async throws {
        try await open(modes: "")
        try moved(3, 2)
        XCTAssertEqual(view.lastSentMouse?.kind, .motion, "plain motion still goes to the core")
        XCTAssertEqual(view.lastSentMouse?.button, TesseraSession.MouseButton.none)
        XCTAssertEqual(view.lastSentMouse?.col, 3)
        XCTAssertEqual(view.lastSentMouse?.row, 2)
        try down(3, 2)
        XCTAssertEqual(view.pressOutcomes[.left], .ignored)
        try dragged(4, 2)
        try up(4, 2)
        try await assertBytes([])
    }

    // MARK: The activating click (D13)

    func testTheClickThatActivatesTheWindowIsNotSent() async throws {
        try await open()
        let click = try mouse(.leftMouseDown, col: 2, row: 2)
        XCTAssertFalse(view.acceptsFirstMouse(for: click), "AppKit keeps the activating press from the view")
        // Its drags and release, should AppKit deliver them, find no press.
        let sent = view.sentMouseCount
        try dragged(3, 2)
        try up(3, 2)
        view.rightMouseUp(with: try mouse(.rightMouseUp, col: 3, row: 2))
        XCTAssertEqual(view.sentMouseCount, sent, "no drag or release without the view's own press")
        try await assertBytes([])
        // The next click is the program's.
        try down(3, 2)
        try up(3, 2)
        try await assertBytes(Self.sgr("0;4;3M") + Self.sgr("0;4;3m"))
    }

    // MARK: Pointer plumbing (no session needed)

    func testOtherButtonsMapToMiddleBackAndForwardOnly() throws {
        func button(_ number: Int64) throws -> TesseraSession.MouseButton? {
            let cgEvent = try XCTUnwrap(
                CGEvent(mouseEventSource: nil, mouseType: .otherMouseDown, mouseCursorPosition: .zero, mouseButton: .center))
            cgEvent.setIntegerValueField(.mouseEventButtonNumber, value: number)
            let event = try XCTUnwrap(NSEvent(cgEvent: cgEvent))
            XCTAssertEqual(event.buttonNumber, Int(number))
            return TerminalSurfaceView.otherButton(event)
        }
        XCTAssertEqual(try button(2), .middle)
        XCTAssertEqual(try button(3), .back)
        XCTAssertEqual(try button(4), .forward)
        XCTAssertNil(try button(5), "no report exists for it: not sent as another button")
        XCTAssertNil(try button(15))
    }

    func testATrackingAreaDeliversMotionWhileTheWindowIsKey() async throws {
        try await open()
        let areas = view.trackingAreas.filter { $0.owner as AnyObject? === view }
        XCTAssertEqual(areas.count, 1)
        let options = try XCTUnwrap(areas.first).options
        XCTAssertTrue(options.isSuperset(of: [.mouseMoved, .activeInKeyWindow, .inVisibleRect]))
        // Moving the view between windows keeps one area.
        let other = KeyControlledWindow(contentRect: view.frame)
        defer { other.close() }
        other.contentView = view
        XCTAssertEqual(view.trackingAreas.filter { $0.owner as AnyObject? === view }.count, 1)
    }

    // MARK: Flood

    /// 100,000 motion events inside one cell report once; 20,000 across
    /// two cells report every change without blocking the main thread
    /// (time recorded, no bar). The view keeps no per-event history.
    func testAMotionFloodIsBoundedAndDoesNotBlock() async throws {
        try await open()
        let base = view.bounds.height
        var events: [NSEvent] = []
        for index in 0..<100 {
            // Points inside cell 3,2 (x 30..39, y 40..59 from the top).
            let point = NSPoint(x: 30 + CGFloat(index % 10), y: base - 40 - CGFloat(index % 20))
            events.append(
                try XCTUnwrap(
                    NSEvent.mouseEvent(
                        with: .mouseMoved, location: view.convert(point, to: nil), modifierFlags: [], timestamp: 0,
                        windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 0, pressure: 0)))
        }
        let clock = ContinuousClock()
        let oneCell = clock.measure {
            for index in 0..<100_000 {
                view.mouseMoved(with: events[index % events.count])
            }
        }
        try await assertBytes(Self.sgr("35;4;3M"), "one report for 100,000 events in one cell")

        let cells = [try mouse(.mouseMoved, col: 0, row: 0), try mouse(.mouseMoved, col: 1, row: 0)]
        let twoCells = clock.measure {
            for index in 0..<20_000 {
                view.mouseMoved(with: cells[index % 2])
            }
        }
        // Every report reached the program, or the core dropped some when
        // its input queue was full (never blocking); either way the
        // sentinel after them arrives.
        let received = try await receivedBytes()
        let report = Self.sgr("35;1;1M") + Self.sgr("35;2;1M")
        XCTAssertEqual(received.count % (report.count / 2), 0, "whole reports only")
        XCTAssertLessThanOrEqual(received.count, 10_000 * report.count)
        print(
            "mouse flood: 100,000 same-cell moves \(oneCell), 20,000 cell changes \(twoCells),",
            "\(received.count / (report.count / 2)) reports written")
    }

    // MARK: Helpers

    /// Length of the recorder's file, up to and including the last sentinel
    /// ``receivedBytes()`` saw.
    private var checkedLength = 0

    /// Asserts that exactly `expected` reached the PTY since the last check.
    private func assertBytes(
        _ expected: [UInt8], _ message: String = "", file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        let received = try await receivedBytes()
        let hex = { (bytes: [UInt8]) in bytes.map { String(format: "%02x", $0) }.joined(separator: " ") }
        XCTAssertEqual(hex(received), hex(expected), message, file: file, line: line)
    }

    /// What reached the PTY since the last check: sends a sentinel key and
    /// returns everything before it. Input reaches the recorder in order,
    /// so once the new sentinel is in the file, so is everything sent before
    /// it; only a `~` past ``checkedLength`` is the new one.
    private func receivedBytes() async throws -> [UInt8] {
        try view.session.send(key: .init(key: .char, codepoint: "~", text: "~"))
        var received: [UInt8] = []
        let previous = checkedLength
        try await waitUntil("the sentinel arrives", timeout: .seconds(20)) {
            received = (try? Array(Data(contentsOf: self.file))) ?? []
            return received.count > previous && received.last == UInt8(ascii: "~")
        }
        checkedLength = received.count
        return Array(received[min(previous, received.count)...].dropLast())
    }

    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(10), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out after \(timeout): \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
