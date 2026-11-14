import AppKit
import Metal
import QuartzCore
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
        for file in files {
            try? FileManager.default.removeItem(at: file)
        }
        files = []
        appDelegate = nil
        view = nil
        window = nil
        file = nil
    }

    /// The recording files of every terminal a test opened (removed in
    /// tearDown).
    private var files: [URL] = []

    /// The metrics ``open(modes:)`` gives the view.
    private static let metrics = TesseraSurface.Metrics(
        widthPx: 480, heightPx: 200, scale: 1, cellWidthPx: 10, cellHeightPx: 20, baselinePx: 15, paddingPx: 0,
        rows: 10, cols: 48, fontSizePt: 13, cursorRow: 0, cursorCol: 0, cursorOnScreen: true, cursorWide: false)

    /// Makes the next window or tab run a child that sets `modes` (printf
    /// escapes) and records its input to a new file, which it returns.
    private func spawnRecorder(modes: String) -> URL {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(
            "tessera-mouse-\(UUID().uuidString).bin")
        files.append(file)
        appDelegate.spawnOptions = .init(
            program: "/bin/sh",
            arguments: ["-c", #"printf "$1"; seq 1 "$2"; stty raw -echo; exec cat > "$0""#, file.path, modes, "\(Self.lines)"])
        return file
    }

    /// Opens a terminal whose child set `modes` (printf escapes) and records
    /// its input, and waits until both are in effect.
    private func open(modes: String = anyMotionSGR) async throws {
        file = spawnRecorder(modes: modes)
        let controller = try appDelegate.newWindow()
        view = controller.focusedPane.view
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        window.isKeyForTest = true
        view.windowKeyChanged(isKey: true)
        // Hidden: no frame is drawn, so no render replaces the metrics below.
        view.occlusionOverride = true
        view.metrics = Self.metrics
        try await waitForRecorder()
    }

    /// Waits until ``view``'s child has set its modes, printed its lines and
    /// started recording to ``file``, and checks that nothing reached it.
    private func waitForRecorder() async throws {
        checkedLength = 0
        // 30 s: the child's first output can stall that long on a loaded host.
        try await waitUntil("the modes and lines are parsed and the recorder runs", timeout: .seconds(30)) {
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
                windowNumber: view.window?.windowNumber ?? 0, context: nil, eventNumber: 0, clickCount: 1,
                pressure: 1))
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

    func testARightOrOtherClickInAWindowThatIsNotKeyIsNotReported() async throws {
        try await open()
        // A background window, or any window of an inactive app: AppKit
        // delivers these presses, unlike the left button's first mouse.
        window.isKeyForTest = false
        let sent = view.sentMouseCount
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 2, row: 2))
        view.rightMouseDragged(with: try mouse(.rightMouseDragged, col: 3, row: 2))
        view.rightMouseUp(with: try mouse(.rightMouseUp, col: 3, row: 2))
        view.otherMouseDown(with: try other(.otherMouseDown, button: 2, col: 2, row: 2))
        view.otherMouseDragged(with: try other(.otherMouseDragged, button: 2, col: 3, row: 2))
        view.otherMouseUp(with: try other(.otherMouseUp, button: 2, col: 3, row: 2))
        XCTAssertEqual(view.sentMouseCount, sent, "neither the presses nor their drags and releases reach the core")
        XCTAssertTrue(view.pressOutcomes.isEmpty)
        try await assertBytes([])
        // Once the window is key, they are the program's.
        window.isKeyForTest = true
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 2, row: 2))
        view.rightMouseUp(with: try mouse(.rightMouseUp, col: 2, row: 2))
        view.otherMouseDown(with: try other(.otherMouseDown, button: 2, col: 2, row: 2))
        view.otherMouseUp(with: try other(.otherMouseUp, button: 2, col: 2, row: 2))
        try await assertBytes(Self.sgr("2;3;3M") + Self.sgr("2;3;3m") + Self.sgr("1;3;3M") + Self.sgr("1;3;3m"))
    }

    // MARK: Pointer events with no cell (#197, #210)

    func testWithoutMetricsNoPressIsReportedAndAHeldButtonStaysAtItsLastCell() async throws {
        try await open()
        view.metrics = nil
        let sent = view.sentMouseCount
        try moved(3, 2)
        try down(3, 2)
        XCTAssertNil(view.pressOutcomes[.left], "a press not sent leaves no record")
        try dragged(4, 2)
        try up(4, 2)
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 3, row: 2))
        view.rightMouseUp(with: try mouse(.rightMouseUp, col: 3, row: 2))
        XCTAssertEqual(view.sentMouseCount, sent, "no cell is known: nothing goes out at 1;1")
        try await assertBytes([])

        // A press sent before the cell size is lost: its drags and release
        // go to the last cell the core took, not to 1;1.
        view.metrics = Self.metrics
        try down(3, 2)
        view.metrics = nil
        try dragged(7, 5)
        try up(7, 5)
        try await assertBytes(Self.sgr("0;4;3M") + Self.sgr("0;4;3m"), "a drag within the last cell is not reported")
        XCTAssertNil(view.pressOutcomes[.left], "released")
    }

    /// While no cell can be named, a wheel event is not sent: a program
    /// tracking the mouse would get it at cell 1;1 (#289).
    func testWithoutMetricsAWheelEventIsNotReported() async throws {
        try await open()
        let event = try XCTUnwrap(
            CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 1, wheel1: 3, wheel2: 0, wheel3: 0))
        event.setIntegerValueField(.scrollWheelEventIsContinuous, value: 0)
        let wheel = try XCTUnwrap(NSEvent(cgEvent: event))
        XCTAssertNotEqual(wheel.scrollingDeltaY, 0, "the event system kept the delta")
        view.metrics = nil
        let sent = view.sentScrollCount
        view.scrollWheel(with: wheel)
        XCTAssertEqual(view.sentScrollCount, sent, "not sent")
        try await assertBytes([], "nothing at 1;1")
        view.metrics = Self.metrics
        view.scrollWheel(with: wheel)
        XCTAssertEqual(view.sentScrollCount, sent + 1, "with a cell size it is")
    }

    func testAReleaseTheCoreRefusesKeepsThePressRecord() async throws {
        try await open()
        try down(1, 1)
        // A location that is not a number: the core refuses the release.
        let nan = try XCTUnwrap(
            NSEvent.mouseEvent(
                with: .leftMouseUp, location: NSPoint(x: CGFloat.nan, y: CGFloat.nan), modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        view.mouseUp(with: nan)
        XCTAssertEqual(view.pressOutcomes[.left], .consumed, "the program still holds the button")
        try up(1, 1)
        try await assertBytes(Self.sgr("0;2;2M") + Self.sgr("0;2;2m"), "the next release reaches the program")
        XCTAssertNil(view.pressOutcomes[.left])
    }

    func testAHeldButtonTheCoreRefusesToReleaseKeepsItsRecord() async throws {
        try await open()
        try down(1, 1)
        try await assertBytes(Self.sgr("0;2;2M"))
        // A closed session refuses every event, the release on leaving the
        // window too.
        try view.session.close()
        window.contentView = nil
        XCTAssertEqual(view.pressOutcomes[.left], .consumed, "not released to the core: still held")
    }

    /// The render view's edges as AppKit's hit test draws them (#210.7): in
    /// the unflipped view the top pixel row is y == maxY, inside; y == 0,
    /// the bottom edge, is outside, as is x == maxX. With either scroller
    /// style, the expected cells taken from the test's metrics.
    func testThePointerOnTheEdgesOfTheTextIsReportedAsAppKitHitTestsIt() async throws {
        try await open()
        window.orderFrontRegardless()
        func at(_ type: NSEvent.EventType, _ point: NSPoint) throws -> NSEvent {
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: view.convert(point, to: nil), modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        }
        // The SGR cell (1-based column;row) of a view point, from the test's
        // metrics (no padding, scale 1), clamped to the grid as the core does.
        let metrics = Self.metrics
        func cell(_ point: NSPoint) -> String {
            let col = min(Int(point.x) / Int(metrics.cellWidthPx), Int(metrics.cols) - 1)
            let row = min(Int(view.bounds.height - point.y) / Int(metrics.cellHeightPx), Int(metrics.rows) - 1)
            return "\(col + 1);\(row + 1)"
        }
        // Both scroller styles: an always-shown (legacy) scroller narrows the
        // render view (the CI runner's default), an overlay one does not.
        for style in [NSScroller.Style.overlay, .legacy] {
            view.scroller.preferredStyleOverride = style
            view.metrics = metrics
            let frame = view.renderView.frame
            XCTAssertEqual(frame.minX, 0, "\(style.rawValue)")
            XCTAssertEqual(frame.maxY, view.bounds.maxY, "\(style.rawValue)")
            XCTAssertGreaterThan(frame.width, 20, "\(style.rawValue)")
            XCTAssertGreaterThan(frame.height, 40, "\(style.rawValue)")
            // The top pixel row, through the window as AppKit delivers it.
            let top = NSPoint(x: 15, y: frame.maxY)
            window.sendEvent(try at(.leftMouseDown, top))
            window.sendEvent(try at(.leftMouseUp, top))
            try await assertBytes(
                Self.sgr("0;\(cell(top))M") + Self.sgr("0;\(cell(top))m"),
                "\(style.rawValue): a click on the top pixel row")
            // The top-left corner and the pixels just inside the bottom and
            // right edges.
            let inside = [
                NSPoint(x: frame.minX, y: frame.maxY), NSPoint(x: 15, y: frame.minY + 0.5),
                NSPoint(x: frame.maxX - 0.5, y: frame.minY + 0.5),
            ]
            for point in inside {
                view.mouseMoved(with: try at(.mouseMoved, point))
            }
            try await assertBytes(
                inside.flatMap { Self.sgr("35;\(cell($0))M") }, "\(style.rawValue): the top-left corner, inside the bottom and right edges")
            // On the bottom and right edges: outside.
            view.mouseMoved(with: try at(.mouseMoved, NSPoint(x: 15, y: frame.minY)))
            view.mouseMoved(with: try at(.mouseMoved, NSPoint(x: frame.maxX, y: 30)))
            view.rightMouseDown(with: try at(.rightMouseDown, NSPoint(x: 15, y: frame.minY)))
            try await assertBytes([], "\(style.rawValue): the bottom and right edges are outside")
        }
    }

    func testAClickOnTheLegacyScrollerStripIsNotReported() async throws {
        try await open()
        view.scroller.preferredStyleOverride = .legacy
        XCTAssertLessThan(view.renderView.frame.maxX, view.bounds.maxX, "the legacy scroller takes a strip")
        // The restyle resized the surface and read its metrics: back to the
        // test's (48 columns of 10 px, the last ones under the strip).
        view.metrics = Self.metrics
        // The strip's middle: right of the render view, over the scroller.
        let strip = NSPoint(
            x: (view.renderView.frame.maxX + view.bounds.maxX) / 2, y: view.bounds.height - 30)
        func event(_ type: NSEvent.EventType) throws -> NSEvent {
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: view.convert(strip, to: nil), modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        }
        let sent = view.sentMouseCount
        // The scroller takes left clicks; right and other ones fall through.
        view.rightMouseDown(with: try event(.rightMouseDown))
        view.rightMouseUp(with: try event(.rightMouseUp))
        let middle = try XCTUnwrap(try event(.otherMouseDown).cgEvent)
        middle.setIntegerValueField(.mouseEventButtonNumber, value: 2)
        view.otherMouseDown(with: try XCTUnwrap(NSEvent(cgEvent: middle)))
        view.mouseMoved(with: try event(.mouseMoved))
        XCTAssertEqual(view.sentMouseCount, sent, "no press or motion over the strip")
        XCTAssertTrue(view.pressOutcomes.isEmpty)
        try await assertBytes([])

        // A press over the grid keeps its drag and release over the strip
        // (the last column), or the program's button would stay down.
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 45, row: 1))
        view.rightMouseDragged(with: try event(.rightMouseDragged))
        view.rightMouseUp(with: try event(.rightMouseUp))
        try await assertBytes(Self.sgr("2;46;2M") + Self.sgr("34;48;2M") + Self.sgr("2;48;2m"))
    }

    /// At backing scale 3, which no display has, so a view that took the
    /// window's scale instead of the metrics' fails on 1x and 2x hosts alike.
    func testSGRPixelsTakeThePaddingOffAtBackingScale3() async throws {
        try await open(modes: #"\033[?1003h\033[?1016h"#)
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device on this machine: the surface test needs a GPU")
        }
        // The core takes its own padding off, so the view and the core must
        // agree: a surface at 3x, and its metrics.
        if view.surface.isAttached {
            try view.surface.detach()
        }
        let size = view.bounds.size
        do {
            try view.surface.attach(
                layer: CAMetalLayer(), widthPx: UInt32(size.width * 3), heightPx: UInt32(size.height * 3), scale: 3)
        } catch {
            throw XCTSkip("the core could not attach a renderer (no GPU can present, or no font): \(error)")
        }
        let metrics = try view.surface.metrics()
        view.metrics = metrics
        XCTAssertEqual(metrics.scale, 3)
        let padding = CGFloat(metrics.paddingPx)
        XCTAssertGreaterThan(padding, 0, "the theme's padding, in pixels")
        let (width, height) = (CGFloat(metrics.cellWidthPx), CGFloat(metrics.cellHeightPx))
        /// An event at `x`, `y` pixels from the view's top-left corner.
        func event(_ type: NSEvent.EventType, _ x: CGFloat, _ y: CGFloat) throws -> NSEvent {
            let point = NSPoint(x: x / 3, y: view.bounds.height - y / 3)
            return try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: view.convert(point, to: nil), modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        }
        // Inside cell 3, 2 (3.5 cells and a quarter pixel right, 2.5 cells
        // and a quarter pixel down of the text area's corner: off any pixel
        // edge, so the division by 3 cannot round across one), then into the
        // top-left padding band, which clamps to the text area's first pixel.
        let x = padding + 3.5 * width + 0.25
        let y = padding + 2.5 * height + 0.25
        view.mouseDown(with: try event(.leftMouseDown, x, y))
        view.mouseDragged(with: try event(.leftMouseDragged, 1, 1))
        view.mouseUp(with: try event(.leftMouseUp, 1, 1))
        let pixel = { (value: CGFloat) in Int(value.rounded(.down)) + 1 }
        try await assertBytes(
            Self.sgr("0;\(pixel(3.5 * width + 0.25));\(pixel(2.5 * height + 0.25))M") + Self.sgr("32;1;1M") + Self.sgr("0;1;1m"),
            "pixels from the text area's corner, one-based (padding \(padding) px, cells \(width) x \(height) px)")
    }

    // MARK: A button held while the view goes away (#197)

    func testATabSwitchedAwayFromWithAButtonHeldReleasesIt() async throws {
        let firstFile = spawnRecorder(modes: Self.anyMotionSGR)
        let firstWindow = try XCTUnwrap(appDelegate.newWindow().window)
        let secondFile = spawnRecorder(modes: Self.anyMotionSGR)
        let secondWindow = try XCTUnwrap(appDelegate.newTab(in: firstWindow).window)
        let group = try XCTUnwrap(firstWindow.tabGroup)
        XCTAssertEqual(group.windows.count, 2)
        // The second tab joined the group after its view moved into its
        // window; the first was in it from the start.
        for (tab, other, recording) in [(secondWindow, firstWindow, secondFile), (firstWindow, secondWindow, firstFile)] {
            view = try XCTUnwrap((tab.windowController as? TerminalWindowController)?.focusedPane.view)
            file = recording
            view.occlusionOverride = true
            view.metrics = Self.metrics
            group.selectedWindow = tab
            try await waitForRecorder()
            let event = try mouse(.leftMouseDown, col: 1, row: 1)
            view.mouseDown(with: event)
            view.mouseDragged(with: try mouse(.leftMouseDragged, col: 2, row: 1))
            XCTAssertEqual(view.pressOutcomes[.left], .consumed)
            // AppKit sends the release, if at all, to the tab now selected.
            group.selectedWindow = other
            XCTAssertTrue(view.pressOutcomes.isEmpty, "released to the core")
            try await assertBytes(
                Self.sgr("0;2;2M") + Self.sgr("32;3;2M") + Self.sgr("0;3;2m"), "at the last cell the core took")
            view.mouseUp(with: try mouse(.leftMouseUp, col: 2, row: 1))
            try await assertBytes([], "and the late release is not sent again")
        }
    }

    func testAViewThatLeavesItsWindowWithAButtonHeldReleasesIt() async throws {
        try await open()
        view.rightMouseDown(with: try mouse(.rightMouseDown, col: 1, row: 1))
        view.otherMouseDown(with: try other(.otherMouseDown, button: 2, col: 1, row: 1))
        window.contentView = nil
        XCTAssertTrue(view.pressOutcomes.isEmpty)
        try await assertBytes(
            Self.sgr("2;2;2M") + Self.sgr("1;2;2M") + Self.sgr("1;2;2m") + Self.sgr("2;2;2m"),
            "both buttons released, middle (2) before right (3) in button order")
    }

    /// Closing a tab with a button held (the release path and the tab
    /// selection observation run) and its input context active (a key
    /// window's) frees its view, controller and session.
    func testClosingATabWithAButtonHeldReleasesItsViewControllerAndSession() async throws {
        try await closeATabWithAButtonHeld(holdingTheWindow: false)
    }

    /// The same while something else keeps the closed window alive, as
    /// AppKit does on the macOS 15 CI runner: the view and session are
    /// still freed, as the view leaves the closed window.
    func testClosingATabWhoseWindowOutlivesItReleasesItsViewAndSession() async throws {
        try await closeATabWithAButtonHeld(holdingTheWindow: true)
    }

    private func closeATabWithAButtonHeld(holdingTheWindow: Bool) async throws {
        weak var weakView: TerminalSurfaceView?
        weak var weakController: TerminalWindowController?
        weak var weakSession: TesseraSession?
        var heldWindow: NSWindow?
        try autoreleasepool {
            file = spawnRecorder(modes: Self.anyMotionSGR)
            let controller = try appDelegate.newWindow()
            let surface = controller.focusedPane.view
            weakController = controller
            weakView = surface
            weakSession = surface.session
            if holdingTheWindow { heldWindow = controller.window }
            surface.occlusionOverride = true
            surface.metrics = Self.metrics
            // As AppKit does for the key window's first responder (the
            // window is key on the CI runner, never in a locked session).
            surface.inputContext?.activate()
        }
        view = weakView
        try await waitForRecorder()
        try down(1, 1)
        XCTAssertEqual(view.pressOutcomes[.left], .consumed)
        view = nil
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the closed tab's objects are freed") {
            weakView == nil && weakController == nil && weakSession == nil
        }
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakSession, "session")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        if holdingTheWindow {
            XCTAssertNotNil(heldWindow, "the window outlives the tab")
            XCTAssertNil(heldWindow?.contentView, "the view left the closed window")
        }
        heldWindow = nil
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
