import AppKit
import Metal
import TesseraKit
import XCTest

@testable import Tessera

/// Scrollback navigation in `TerminalSurfaceView` (#31): the View menu's
/// ⌘↑/⌘↓/⌘Home/⌘End through AppKit's key equivalent matching, momentum
/// after a key, the overlay scroller and a composition started while
/// scrolled back (#134).
///
/// The child prints its history, then copies its input to its output in
/// raw mode, so a test makes it print by writing to the session. As in
/// ``TerminalSurfaceViewTests`` the view lives in a ``KeyControlledWindow``
/// that is never on screen, and the tests drive the display link
/// themselves; nothing waits on a clock except for the child's output.
@MainActor
final class TerminalScrollTests: XCTestCase {
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

    /// Opens a terminal whose child printed `lines` lines (the numbers 1 to
    /// `lines`), and waits until the child is ready: it prints one more,
    /// empty, line only once raw mode is set, so with the cursor's line
    /// `lines + 2` lines fill the screen and the history then. Writing
    /// before that would race `stty`: the cooked terminal echoes input
    /// itself, as well as `cat`, and holds an escape sequence until a
    /// newline.
    ///
    /// `modes` (printf escapes) is printed before the lines, so the core
    /// has parsed it once the lines are in the session.
    private func open(lines: Int = 1000, modes: String = "") async throws {
        appDelegate.spawnOptions = .init(
            program: "/bin/sh",
            arguments: ["-c", "printf '\(modes)'; seq 1 \(lines); stty raw -echo; printf '\\n'; exec cat"])
        let controller = try appDelegate.newWindow()
        view = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        view.frameStatusOverride = .presented
        view.occlusionOverride = false
        // The scroll bar's visibility follows the user's setting otherwise.
        view.scroller.preferredStyleOverride = .overlay
        try await waitUntil("the child's \(lines) lines and its raw-mode line are in the session") {
            guard let position = try? self.view.session.scrollPosition else { return false }
            return position.history + UInt64(position.rows) == UInt64(lines + 2)
        }
    }

    private var position: TesseraSession.ScrollPosition {
        get throws { try view.session.scrollPosition }
    }

    private func requireSurface() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device on this machine: the scroll bar follows rendered frames")
        }
        guard view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the view (no GPU can present, or no font)")
        }
    }

    /// One display refresh, after telling the view about output that
    /// arrived (the router's call may still be queued).
    private func tick() {
        view.sessionDidOutput()
        view.displayLinkDidFire()
    }

    // MARK: Keys

    /// A key-down event as the keyboard delivers it: the arrows carry the
    /// numeric pad and function flags, Home and End the function flag.
    private func key(_ functionKey: Int, code: UInt16, _ flags: NSEvent.ModifierFlags) throws -> NSEvent {
        let characters = MainMenu.functionKey(functionKey)
        return try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                windowNumber: window.windowNumber, context: nil, characters: characters,
                charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code))
    }

    private func commandUp() throws -> NSEvent { try key(NSUpArrowFunctionKey, code: 126, [.command, .numericPad, .function]) }
    private func commandDown() throws -> NSEvent {
        try key(NSDownArrowFunctionKey, code: 125, [.command, .numericPad, .function])
    }
    private func commandHome() throws -> NSEvent { try key(NSHomeFunctionKey, code: 115, [.command, .function]) }
    private func commandEnd() throws -> NSEvent { try key(NSEndFunctionKey, code: 119, [.command, .function]) }

    /// Sends `event` through the installed menu bar with the View menu's
    /// items aimed at the view: AppKit matches the key equivalent, and only
    /// the target is the test's (no window is key in the test host, so the
    /// responder chain would end in the app). Returns whether it matched.
    @discardableResult
    private func press(_ event: NSEvent) throws -> Bool {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let menu = try XCTUnwrap(bar.items.first { $0.submenu?.title == "View" }?.submenu)
        for item in menu.items {
            item.target = view
        }
        defer {
            for item in menu.items {
                item.target = nil
            }
        }
        return bar.performKeyEquivalent(with: event)
    }

    func testCommandArrowsPageAndCommandHomeEndJump() async throws {
        try await open()
        let start = try position
        let page = UInt64(start.rows - 1)
        XCTAssertEqual(start.offset, 0)
        XCTAssertTrue(try press(commandUp()), "⌘↑ is a key equivalent")
        XCTAssertEqual(try position.offset, page, "⌘↑ pages up: the screen less one line")
        try press(commandUp())
        XCTAssertEqual(try position.offset, 2 * page)
        XCTAssertTrue(try press(commandDown()), "⌘↓ is a key equivalent")
        XCTAssertEqual(try position.offset, page, "⌘↓ pages back down")
        XCTAssertTrue(try press(commandHome()), "⌘Home is a key equivalent")
        XCTAssertEqual(try position.offset, start.history, "⌘Home goes to the oldest line")
        XCTAssertTrue(try press(commandEnd()), "⌘End is a key equivalent")
        XCTAssertEqual(try position.offset, 0, "⌘End goes to the live screen")
        // Without ⌘ the keys are the program's.
        XCTAssertFalse(try press(key(NSUpArrowFunctionKey, code: 126, [.numericPad, .function])))
        XCTAssertFalse(try press(key(NSHomeFunctionKey, code: 115, [.function])))
        XCTAssertEqual(try position.offset, 0)
    }

    // MARK: Momentum

    /// A trackpad scroll event: `deltaY` points in `phase` (a gesture with
    /// the fingers down) or `momentum` (the scroll that follows the lift).
    private func trackpad(
        _ deltaY: Int32, phase: CGScrollPhase? = nil, momentum: CGMomentumScrollPhase? = nil
    ) throws -> NSEvent {
        let event = try XCTUnwrap(
            CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1, wheel1: deltaY, wheel2: 0, wheel3: 0))
        event.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        event.setIntegerValueField(.scrollWheelEventScrollPhase, value: Int64(phase?.rawValue ?? 0))
        event.setIntegerValueField(.scrollWheelEventMomentumPhase, value: Int64(momentum?.rawValue ?? 0))
        let nsEvent = try XCTUnwrap(NSEvent(cgEvent: event))
        XCTAssertTrue(nsEvent.hasPreciseScrollingDeltas)
        assertConverted(nsEvent, deltaY)
        return nsEvent
    }

    func testMomentumAfterAKeyDoesNotMoveTheViewUntilTheNextGesture() async throws {
        try await open()
        view.scroll(to: .offset(500))
        // Which sign scrolls back depends on how the event system reads a
        // synthetic event; the first gesture finds out.
        view.scrollWheel(with: try trackpad(100, phase: .began))
        let afterGesture = try position.offset
        XCTAssertNotEqual(afterGesture, 500, "a gesture moves the view")
        let back: Int32 = afterGesture > 500 ? 100 : -100
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        view.scrollWheel(with: try trackpad(back, momentum: .begin))
        let afterMomentum = try position.offset
        XCTAssertGreaterThan(afterMomentum, max(afterGesture, 500), "momentum moves the view while nothing interrupted it")

        // A key returns the view to the live screen; the momentum still
        // arriving from the gesture before must not carry it away.
        view.processKeyDown(
            try XCTUnwrap(
                NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, characters: "a", charactersIgnoringModifiers: "a",
                    isARepeat: false, keyCode: 0))
        ) { view.insertText("a", replacementRange: NSRange(location: NSNotFound, length: 0)) }
        XCTAssertEqual(try position.offset, 0, "typing returns to the live screen")
        for _ in 0..<5 {
            view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        }
        view.scrollWheel(with: try trackpad(0, momentum: .end))
        XCTAssertEqual(try position.offset, 0, "momentum after the key is dropped")

        // The next gesture scrolls, and its own momentum does too.
        view.scrollWheel(with: try trackpad(back, phase: .began))
        let nextGesture = try position.offset
        XCTAssertGreaterThan(nextGesture, 0, "a new gesture scrolls")
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        view.scrollWheel(with: try trackpad(back, momentum: .begin))
        XCTAssertGreaterThan(try position.offset, nextGesture, "the new gesture's momentum scrolls")
    }

    func testMomentumAfterAKeyboardScrollIsDropped() async throws {
        try await open()
        view.scrollWheel(with: try trackpad(100, phase: .began))
        let back: Int32 = try position.offset > 0 ? 100 : -100
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        try press(commandEnd())
        XCTAssertEqual(try position.offset, 0)
        view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        XCTAssertEqual(try position.offset, 0, "⌘End's live screen stays")
    }

    // MARK: Typing while scrolled back

    func testAKeyWhileScrolledBackRedrawsTheLiveScreen() async throws {
        try await open()
        try requireSurface()
        try press(commandUp())
        tick()
        let renders = view.renderCount
        view.processKeyDown(
            try XCTUnwrap(
                NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, characters: "a", charactersIgnoringModifiers: "a",
                    isARepeat: false, keyCode: 0))
        ) { view.insertText("a", replacementRange: NSRange(location: NSNotFound, length: 0)) }
        XCTAssertEqual(try position.offset, 0)
        // Before the child's echo can arrive: the key alone asks for the
        // frame that shows the live screen.
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders + 1, "the live screen is drawn even if the child echoes nothing")
    }

    func testAPasteWhileScrolledBackRedrawsTheLiveScreenAndDropsMomentum() async throws {
        try await open()
        try requireSurface()
        // Never the user's clipboard.
        view.pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        defer { view.pasteboard.releaseGlobally() }
        view.pasteboard.clearContents()
        view.pasteboard.setString("p", forType: .string)
        try press(commandUp())
        view.scrollWheel(with: try trackpad(0, phase: .began))
        XCTAssertFalse(view.ignoresMomentum, "a gesture began")
        tick()
        let renders = view.renderCount
        view.paste(nil)
        XCTAssertEqual(try position.offset, 0, "pasting returns to the live screen")
        XCTAssertTrue(view.ignoresMomentum, "the gesture's momentum is dropped")
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders + 1, "the live screen is drawn")
    }

    // MARK: Scroll bar

    /// Asserts that `scroller` shows `expected`: the knob's proportion
    /// rows / (history + rows) and its value (0 at the oldest line, 1 at the
    /// live screen) exactly, and the knob AppKit draws for them to the
    /// device pixel.
    ///
    /// The geometry is measured against AppKit's own track, found by
    /// moving the knob to either end, not against `rect(for: .knobSlot)`:
    /// on macOS 15 (the CI runner) the overlay slot is the scroller's full
    /// height while the knob travels about a point less, and AppKit floors
    /// the knob's origin and length to device pixels (whole points at 1x).
    /// Both ends and the knob are floored the same way, so the knob's
    /// origin is within one device pixel of the interpolation between the
    /// ends. Its length depends on AppKit's internal track: it is held to
    /// the screen's share of the measured track within a pixel plus 1 % of
    /// the track (1.4 pt of 200 observed on macOS 15 at 1x), and to at
    /// least that share when AppKit's minimum knob length applies.
    static func assertKnob(
        of scroller: TerminalScroller, shows expected: TesseraSession.ScrollPosition, scale: CGFloat,
        proportional: Bool = false, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertEqual(scroller.position, expected, message, file: file, line: line)
        XCTAssertFalse(scroller.isHidden, message, file: file, line: line)
        let history = Double(expected.history)
        let proportion = Double(expected.rows) / (history + Double(expected.rows))
        let fromTop = (history - Double(expected.offset)) / history
        XCTAssertEqual(Double(scroller.knobProportion), proportion, accuracy: 1e-9, message, file: file, line: line)
        XCTAssertEqual(scroller.doubleValue, fromTop, accuracy: 1e-9, "\(message): value", file: file, line: line)

        let knob = scroller.rect(for: .knob)
        let value = scroller.doubleValue
        scroller.doubleValue = 0
        let atTop = scroller.rect(for: .knob)
        scroller.doubleValue = 1
        let atBottom = scroller.rect(for: .knob)
        scroller.doubleValue = value
        let pixel = 1 / scale
        // The same length, computed by AppKit at each place: equal up to
        // floating-point error (at 3x one ulp apart), well under a pixel.
        let sameLength = pixel / 1000
        XCTAssertGreaterThan(knob.height, 0, message, file: file, line: line)
        XCTAssertEqual(atTop.height, knob.height, accuracy: sameLength, message, file: file, line: line)
        XCTAssertEqual(atBottom.height, knob.height, accuracy: sameLength, message, file: file, line: line)

        let origin = atTop.minY + (atBottom.minY - atTop.minY) * CGFloat(fromTop)
        XCTAssertEqual(
            knob.minY, origin, accuracy: pixel + 1e-9, "\(message): knob position", file: file, line: line)
        let track = abs(atBottom.minY - atTop.minY) + knob.height
        let share = track * CGFloat(proportion)
        let tolerance = pixel + track / 100
        XCTAssertGreaterThanOrEqual(
            knob.height + tolerance, share, "\(message): knob length", file: file, line: line)
        if proportional {
            XCTAssertEqual(knob.height, share, accuracy: tolerance, "\(message): knob length", file: file, line: line)
        }
    }

    private func assertScroller(
        shows expected: TesseraSession.ScrollPosition, proportional: Bool = false, _ message: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        Self.assertKnob(
            of: view.scroller, shows: expected, scale: window.backingScaleFactor, proportional: proportional,
            message, file: file, line: line)
    }

    /// A window at a chosen backing scale: AppKit rounds the knob to its
    /// device pixels. Never on screen.
    private final class ScaledWindow: NSWindow {
        var scale: CGFloat = 1
        override var backingScaleFactor: CGFloat { scale }
    }

    func testTheKnobMatchesThePositionAtEveryBackingScale() {
        // No session: the scroller alone, as a frame would update it.
        let positions: [TesseraSession.ScrollPosition] = [
            .init(offset: 0, history: 1000, rows: 12), .init(offset: 11, history: 1039, rows: 12),
            .init(offset: 400, history: 1039, rows: 12), .init(offset: 1039, history: 1039, rows: 12),
            .init(offset: 0, history: 19, rows: 12), .init(offset: 7, history: 19, rows: 12),
            .init(offset: 19, history: 19, rows: 12),
        ]
        for scale: CGFloat in [1, 2, 3] {
            let scaled = ScaledWindow(
                contentRect: NSRect(x: 0, y: 0, width: 480, height: 200), styleMask: [.titled], backing: .buffered,
                defer: false)
            scaled.isReleasedWhenClosed = false
            scaled.scale = scale
            let host = NSView(frame: NSRect(x: 0, y: 0, width: 480, height: 200))
            host.wantsLayer = true
            scaled.contentView = host
            let scroller = TerminalScroller()
            scroller.frame = NSRect(
                x: 480 - TerminalScroller.width, y: 0, width: TerminalScroller.width, height: 200)
            host.addSubview(scroller)
            for position in positions {
                scroller.update(position)
                Self.assertKnob(
                    of: scroller, shows: position, scale: scale, proportional: position.history < 100,
                    "\(position) at \(scale)x")
            }
            scaled.close()
        }
    }

    func testTheScrollerFollowsOutputAndScrolling() async throws {
        try await open()
        try requireSurface()
        tick()
        assertScroller(shows: try position, "after the first frame")

        // Output at the live screen: more history, a smaller knob at the
        // bottom.
        let before = try position.history
        // CR LF: each line starts at the left, so none wraps.
        try view.session.write(Array(String(repeating: "x\r\n", count: 50).utf8))
        try await waitUntil("the echo arrives") { (try? self.position.history) == before + 50 }
        tick()
        assertScroller(shows: try position, "after output")

        // Scrolling moves it.
        try press(commandUp())
        tick()
        XCTAssertGreaterThan(try position.offset, 0)
        assertScroller(shows: try position, "after paging up")
        view.scroll(to: .offset(400))
        tick()
        assertScroller(shows: try position, "after scrolling to 400 lines back")

        // Output under a scrolled-back view keeps it on its lines: the
        // offset and history both grow.
        try view.session.write(Array(String(repeating: "y\r\n", count: 30).utf8))
        try await waitUntil("the echo arrives") { (try? self.position.history) == before + 80 }
        tick()
        XCTAssertEqual(try position.offset, 430)
        assertScroller(shows: try position, "after output while scrolled back")
    }

    func testTheKnobIsAsTallAsTheScreenIsOfTheLines() async throws {
        // A short history: the knob is far longer than AppKit's minimum.
        try await open(lines: 30)
        try requireSurface()
        tick()
        let shown = try position
        XCTAssertGreaterThan(shown.history, 0)
        assertScroller(shows: shown, proportional: true, "a short history")
        view.scroll(to: .top)
        tick()
        assertScroller(shows: try position, proportional: true, "at the top")
    }

    func testDraggingTheKnobOrClickingTheTrackMovesTheView() async throws {
        try await open()
        try requireSurface()
        tick()
        let scroller = view.scroller
        let history = try position.history
        let page = UInt64(try position.rows - 1)
        // AppKit sets the value as the knob is dragged, then sends the
        // action with the part tracked.
        scroller.doubleValue = 0
        scroller.userMoved(part: .knob)
        XCTAssertEqual(try position.offset, history, "the knob at the top shows the oldest line")
        scroller.doubleValue = 1
        scroller.userMoved(part: .knob)
        XCTAssertEqual(try position.offset, 0, "the knob at the bottom shows the live screen")
        scroller.doubleValue = 0.25
        scroller.userMoved(part: .knobSlot)
        XCTAssertEqual(try position.offset, history - UInt64((0.25 * Double(history)).rounded()), "a jump to a spot")
        tick()
        let before = try position.offset
        scroller.userMoved(part: .decrementPage)
        XCTAssertEqual(try position.offset, before + page, "a click above the knob pages up")
        scroller.userMoved(part: .incrementPage)
        XCTAssertEqual(try position.offset, before, "a click below the knob pages down")
    }

    // The line parts are deprecated (macOS draws no scroller arrows), but
    // VoiceOver's increment and decrement buttons still reach them.
    @available(macOS, deprecated: 10.14)
    func testOnlyADraggedKnobIgnoresNewPositions() {
        // A held click in the track pages until the knob reaches the
        // pointer: the knob must follow each page there.
        let parts: [(NSScroller.Part, Bool)] = [
            (.knob, true), (.knobSlot, false), (.decrementPage, false), (.incrementPage, false),
            (.decrementLine, false), (.incrementLine, false), (.noPart, false),
        ]
        for (part, frozen) in parts {
            XCTAssertEqual(TerminalScroller.freezesKnob(tracking: true, part: part), frozen, "tracking \(part)")
            XCTAssertFalse(TerminalScroller.freezesKnob(tracking: false, part: part), "not tracking, \(part)")
        }
    }

    func testAKeyboardScrollAsksForAFrameWithoutOutput() async throws {
        try await open()
        try requireSurface()
        tick()
        let renders = view.renderCount
        // No output in between: only the scroll can ask for this frame.
        view.scroll(to: .pageUp)
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders + 1, "the scrolled view is drawn")
        XCTAssertEqual(view.scroller.position, try position, "and the scroll bar shows it")
    }

    func testAWheelScrollShowsTheScroller() async throws {
        try await open()
        try requireSurface()
        // Halfway back, so a wheel event moves the view either way; set
        // through the session so nothing shows the scroller yet.
        try view.session.scroll(to: .offset(500))
        tick()
        XCTAssertFalse(view.scroller.isRevealed)
        view.scrollWheel(with: try trackpad(100, phase: .began))
        XCTAssertNotEqual(try position.offset, 500, "the wheel moved the view")
        XCTAssertTrue(view.scroller.isRevealed, "a wheel scroll shows the scroll bar")
    }

    func testTheScrollerIsHiddenOnTheAlternateScreen() async throws {
        try await open()
        try requireSurface()
        tick()
        XCTAssertFalse(view.scroller.isHidden, "there is history")
        try view.session.write(Array("\u{1b}[?1049h".utf8))
        try await waitUntil("the alternate screen is shown") { (try? self.position.history) == 0 }
        tick()
        XCTAssertTrue(view.scroller.isHidden, "the alternate screen has no history")
        try view.session.write(Array("\u{1b}[?1049l".utf8))
        try await waitUntil("the primary screen is back") { (try? self.position.history) ?? 0 > 0 }
        tick()
        XCTAssertFalse(view.scroller.isHidden)
    }

    func testTheScrollerShowsWhenScrolledAndStaysShownWhenTheSettingSaysAlways() async throws {
        try await open()
        try requireSurface()
        tick()
        let scroller = view.scroller
        let inScroller = view.convert(NSPoint(x: scroller.frame.midX, y: scroller.frame.midY), to: view.superview)
        XCTAssertFalse(scroller.isRevealed, "an overlay scroller waits for a scroll")
        XCTAssertTrue(view.hitTest(inScroller) === view, "a click there is the terminal's while it is not shown")
        try press(commandUp())
        XCTAssertTrue(scroller.isRevealed, "a scroll shows it")
        XCTAssertTrue(view.hitTest(inScroller) === scroller, "and it takes clicks while shown")
        scroller.preferredStyleOverride = .legacy
        XCTAssertTrue(scroller.isRevealed)
        XCTAssertEqual(scroller.alphaValue, 1, "Show scroll bars: Always")
        scroller.preferredStyleOverride = .overlay
        XCTAssertEqual(scroller.alphaValue, 0, "between scrolls when it does not stay shown")
    }

    // MARK: Input method (#134)

    func testACompositionWhileScrolledBackReturnsToTheLiveCursor() async throws {
        try await open()
        try requireSurface()
        try press(commandUp())
        tick()
        XCTAssertGreaterThan(try position.offset, 0)
        XCTAssertEqual(view.metrics?.cursorOnScreen, false, "scrolled back: the cursor is below the view")
        view.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(try position.offset, 0, "the composition starts at the live screen")
        let metrics = try XCTUnwrap(view.metrics)
        XCTAssertTrue(metrics.cursorOnScreen, "the composition's metrics are the live screen's")
        XCTAssertEqual(metrics.cursorRow, try view.surface.metrics().cursorRow)
        // The overlay and the candidate window sit on the cursor's row.
        let scale = CGFloat(metrics.scale)
        let fromTop = (CGFloat(metrics.paddingPx) + CGFloat(metrics.cursorRow) * CGFloat(metrics.cellHeightPx)) / scale
        let cell = view.compositionCellRect()
        XCTAssertEqual(cell.maxY, view.bounds.height - fromTop, accuracy: 0.01)
        let overlay = try XCTUnwrap(view.markedTextOverlay)
        XCTAssertEqual(overlay.frame.minY, cell.minY, accuracy: 0.01)
        // During the composition ⌘↑ does not scroll (ADR 0032), and the
        // next update of the composition stays put.
        try press(commandUp())
        view.setMarkedText("にほ", selectedRange: NSRange(location: 2, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(try position.offset, 0)
        view.unmarkText()
    }

    func testACompositionAtTheLiveScreenDoesNotScroll() async throws {
        try await open()
        XCTAssertEqual(try position.offset, 0)
        view.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(try position.offset, 0)
        XCTAssertFalse(view.ignoresMomentum, "nothing moved, so a gesture's momentum carries on")
        view.unmarkText()
    }

    // MARK: Momentum after a key (ADR 0032)

    /// A key that returns the view to the live screen, dropping momentum.
    private func typeA() throws {
        view.processKeyDown(
            try XCTUnwrap(
                NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, characters: "a", charactersIgnoringModifiers: "a",
                    isARepeat: false, keyCode: 0))
        ) { view.insertText("a", replacementRange: NSRange(location: NSNotFound, length: 0)) }
        XCTAssertEqual(try position.offset, 0, "typing returns to the live screen")
        XCTAssertTrue(view.ignoresMomentum)
    }

    /// A notched mouse wheel's event: whole lines, no phases, not precise.
    private func notched(_ lines: Int32) throws -> NSEvent {
        let event = try XCTUnwrap(
            CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 1, wheel1: lines, wheel2: 0, wheel3: 0))
        event.setIntegerValueField(.scrollWheelEventIsContinuous, value: 0)
        let nsEvent = try XCTUnwrap(NSEvent(cgEvent: event))
        XCTAssertFalse(nsEvent.hasPreciseScrollingDeltas)
        XCTAssertTrue(nsEvent.phase.isEmpty && nsEvent.momentumPhase.isEmpty)
        assertConverted(nsEvent, lines)
        return nsEvent
    }

    /// The event system kept a synthetic scroll's delta: non-zero with the
    /// sign it was made with (zero stays zero). A device utility on the
    /// host (BetterTouchTool, Logi Options) can rewrite synthetic scroll
    /// events; without this, such a host fails the scroll tests later and
    /// without saying why (#210).
    private func assertConverted(
        _ event: NSEvent, _ delta: Int32, file: StaticString = #filePath, line: UInt = #line
    ) {
        guard delta != 0 else {
            XCTAssertEqual(event.scrollingDeltaY, 0, "a zero delta stays zero", file: file, line: line)
            return
        }
        let message = "the host converted a synthetic scroll of \(delta) to scrollingDeltaY \(event.scrollingDeltaY): "
            + "a device utility may be rewriting scroll events"
        XCTAssertNotEqual(event.scrollingDeltaY, 0, message, file: file, line: line)
        XCTAssertEqual(event.scrollingDeltaY < 0, delta < 0, message, file: file, line: line)
    }

    /// The trackpad delta that scrolls back, found with a gesture that is
    /// then ended (the sign depends on how the system reads a synthetic
    /// event).
    private func backDelta() throws -> Int32 {
        view.scrollWheel(with: try trackpad(100, phase: .began))
        let back: Int32 = try position.offset > 0 ? 100 : -100
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        return back
    }

    func testAPreciseEventWithoutPhasesStartsANewGestureAfterAKey() async throws {
        try await open()
        let back = try backDelta()
        try typeA()
        // A smooth-scrolling utility or a device that sends no phases.
        view.scrollWheel(with: try trackpad(0))
        XCTAssertFalse(view.ignoresMomentum, "a precise event with no phase is a new gesture")
        view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        XCTAssertGreaterThan(try position.offset, 0, "the momentum after it scrolls")
    }

    func testANotchedWheelScrollsAfterAKeyAndDoesNotEndTheMomentumDrop() async throws {
        try await open()
        _ = try backDelta()
        try typeA()
        // At the live screen only one direction moves the view.
        view.scrollWheel(with: try notched(3))
        if try position.offset == 0 {
            view.scrollWheel(with: try notched(-3))
        }
        XCTAssertGreaterThan(try position.offset, 0, "a notched wheel scrolls after a key")
        XCTAssertTrue(view.ignoresMomentum, "a notched wheel is not a trackpad gesture")
    }

    func testMomentumWithoutAGesturePhaseScrollsAfterAKey() async throws {
        try await open()
        let back = try backDelta()
        try typeA()
        // A device whose events all carry a momentum phase and no gesture
        // phase: its next flick is a new gesture.
        view.scrollWheel(with: try trackpad(back, momentum: .begin))
        view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        XCTAssertFalse(view.ignoresMomentum)
        XCTAssertGreaterThan(try position.offset, 0, "the flick after the key scrolls")
    }

    func testMomentumOfAGestureInterruptedByAKeyIsDropped() async throws {
        try await open()
        let back = try backDelta()
        // The fingers are down when the key is pressed; the gesture's
        // remaining events mark its momentum as its own.
        view.scrollWheel(with: try trackpad(back, phase: .began))
        try typeA()
        view.scrollWheel(with: try trackpad(0, phase: .changed))
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        view.scrollWheel(with: try trackpad(back, momentum: .begin))
        view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        XCTAssertTrue(view.ignoresMomentum)
        XCTAssertEqual(try position.offset, 0, "the interrupted gesture's momentum is dropped")
    }

    func testMomentumAfterTwoKeysOfAnInterruptedGestureIsDropped() async throws {
        try await open()
        let back = try backDelta()
        view.scrollWheel(with: try trackpad(back, phase: .began))
        try typeA()
        view.scrollWheel(with: try trackpad(0, phase: .changed))
        view.scrollWheel(with: try trackpad(0, phase: .ended))
        // A second key between the lift and the momentum: the gesture's
        // events since the first key still mark the momentum as its own.
        try typeA()
        view.scrollWheel(with: try trackpad(back, momentum: .begin))
        view.scrollWheel(with: try trackpad(back, momentum: .continuous))
        XCTAssertTrue(view.ignoresMomentum)
        XCTAssertEqual(try position.offset, 0, "the interrupted gesture's momentum is dropped")
    }

    // MARK: Composition (ADR 0032)

    private static let scrollActions: [Selector] = [
        #selector(TerminalSurfaceView.scrollToTop(_:)), #selector(TerminalSurfaceView.scrollToBottom(_:)),
        #selector(TerminalSurfaceView.scrollPageUpInHistory(_:)),
        #selector(TerminalSurfaceView.scrollPageDownInHistory(_:)),
    ]

    private func composeNi() {
        view.setMarkedText(
            "に", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
    }

    func testTheViewMenuScrollItemsAreDisabledWhileComposing() async throws {
        try await open()
        composeNi()
        let overlay = try XCTUnwrap(view.markedTextOverlay)
        for action in Self.scrollActions {
            let item = NSMenuItem(title: "", action: action, keyEquivalent: "")
            XCTAssertFalse(view.validateMenuItem(item), "\(action) while composing")
        }
        // AppKit reports the key equivalent of a disabled item as handled
        // (`performKeyEquivalent` returns true) without sending its action.
        try press(commandUp())
        try press(commandHome())
        XCTAssertEqual(try position.offset, 0, "⌘↑ and ⌘Home do not scroll")
        XCTAssertEqual(view.markedTextOverlay?.text, overlay.text)
        XCTAssertEqual(view.markedTextOverlay?.frame, overlay.frame)
        view.unmarkText()
        for action in Self.scrollActions {
            XCTAssertTrue(view.validateMenuItem(NSMenuItem(title: "", action: action, keyEquivalent: "")), "\(action)")
        }
        XCTAssertTrue(try press(commandUp()))
        XCTAssertGreaterThan(try position.offset, 0, "⌘↑ pages again")
    }

    /// Wheel events in both directions, gesture, momentum and notched.
    private func wheelBothWays() throws {
        for delta: Int32 in [100, -100] {
            view.scrollWheel(with: try trackpad(delta, phase: .began))
            view.scrollWheel(with: try trackpad(delta, phase: .changed))
            view.scrollWheel(with: try trackpad(delta, momentum: .continuous))
            view.scrollWheel(with: try notched(delta / 30))
        }
    }

    func testTheWheelSendsNothingWhileComposing() async throws {
        try await open()
        view.scroll(to: .offset(500))
        // Starting a composition returns to the live screen, which drops
        // momentum.
        composeNi()
        XCTAssertEqual(try position.offset, 0)
        XCTAssertTrue(view.ignoresMomentum)
        let sent = view.sentScrollCount
        view.scrollWheel(with: try trackpad(0, phase: .began))
        XCTAssertFalse(view.ignoresMomentum, "the phases are still followed")
        try wheelBothWays()
        XCTAssertEqual(view.sentScrollCount, sent, "nothing reaches the core")
        XCTAssertEqual(try position.offset, 0)
        view.unmarkText()

        // The alternate screen: no cursor keys either.
        try view.session.write(Array("\u{1b}[?1049h".utf8))
        try await waitUntil("the alternate screen is shown") { (try? self.position.history) == 0 }
        composeNi()
        let sentAlternate = view.sentScrollCount
        try wheelBothWays()
        XCTAssertEqual(view.sentScrollCount, sentAlternate, "no wheel reaches the core on the alternate screen")
        view.unmarkText()
        view.scrollWheel(with: try notched(1))
        XCTAssertEqual(view.sentScrollCount, sentAlternate + 1, "after the composition the wheel is sent")
    }

    func testTheScrollerMovesNothingWhileComposing() async throws {
        try await open()
        let scroller = view.scroller
        scroller.update(try position)
        composeNi()
        scroller.doubleValue = 0
        scroller.userMoved(part: .knob)
        XCTAssertEqual(try position.offset, 0, "a knob drag while composing")
        scroller.userMoved(part: .decrementPage)
        XCTAssertEqual(try position.offset, 0, "a track click while composing")
        view.unmarkText()
        scroller.userMoved(part: .decrementPage)
        XCTAssertGreaterThan(try position.offset, 0, "a track click after it")
    }

    // MARK: Flash only when the view moved (ADR 0032)

    func testAWheelReportedToAProgramDoesNotShowTheScroller() async throws {
        try await open(modes: "\\033[?1000h\\033[?1006h")
        view.scroller.update(try position)
        XCTAssertFalse(view.scroller.isHidden)
        XCTAssertFalse(view.scroller.isRevealed)
        let sent = view.sentScrollCount
        view.scrollWheel(with: try trackpad(100, phase: .began))
        view.scrollWheel(with: try trackpad(-100, phase: .changed))
        XCTAssertEqual(view.sentScrollCount, sent + 2, "the wheel went to the core")
        XCTAssertEqual(try position.offset, 0, "the program has the wheel: the view stays")
        XCTAssertFalse(view.scroller.isRevealed, "nothing scrolled, so the scroll bar does not show")
    }

    // MARK: Cursor (ADR 0032)

    func testTheCursorIsAnArrowOverTheScrollerAndAnIBeamElsewhere() async throws {
        try await open()
        let scroller = view.scroller
        scroller.update(try position)
        XCTAssertFalse(scroller.isHidden)
        for style: NSScroller.Style in [.overlay, .legacy] {
            scroller.preferredStyleOverride = style
            let rects = view.cursorRects()
            XCTAssertEqual(rects.count, 2, "\(style)")
            let text = view.renderView.frame
            XCTAssertEqual(
                rects.first?.0, NSRect(x: 0, y: 0, width: min(text.maxX, scroller.frame.minX), height: text.height),
                "\(style): the I-beam over the text, up to the scroll bar")
            XCTAssertTrue(rects.first?.1 === NSCursor.iBeam)
            XCTAssertEqual(rects.last?.0, scroller.frame, "\(style): the arrow over the scroll bar")
            XCTAssertTrue(rects.last?.1 === NSCursor.arrow)
        }
        XCTAssertEqual(view.cursorRects().first?.0, view.renderView.frame, "legacy: the I-beam covers the grid")
        scroller.preferredStyleOverride = .overlay
        scroller.update(.init(offset: 0, history: 0, rows: try position.rows))
        XCTAssertTrue(scroller.isHidden)
        let hidden = view.cursorRects()
        XCTAssertEqual(hidden.count, 1)
        XCTAssertEqual(hidden.first?.0, view.bounds, "no scroll bar: the I-beam everywhere")
        XCTAssertTrue(hidden.first?.1 === NSCursor.iBeam)
    }

    // MARK: Show scroll bars: Always (ADR 0032)

    func testALegacyScrollerNarrowsTheGrid() async throws {
        try await open()
        try requireSurface()
        tick()
        let scale = window.backingScaleFactor
        XCTAssertEqual(view.renderView.frame, view.bounds, "overlay: the grid is the whole view")
        XCTAssertEqual(view.surface.size, view.pixelSize)
        XCTAssertEqual(view.pixelSize.widthPx, UInt32((view.bounds.width * scale).rounded()))
        let overlaySize = view.surface.size
        let overlayCols = try XCTUnwrap(view.metrics).cols
        let resizes = view.coreResizeCount

        view.scroller.preferredStyleOverride = .legacy
        XCTAssertEqual(view.coreResizeCount, resizes + 1, "one resize")
        let legacyWidth = NSScroller.scrollerWidth(for: .regular, scrollerStyle: .legacy)
        XCTAssertEqual(view.renderView.frame.width, view.bounds.width - legacyWidth)
        XCTAssertFalse(view.scroller.frame.intersects(view.renderView.frame), "the scroll bar is beside the grid")
        XCTAssertEqual(view.scroller.frame.maxX, view.bounds.maxX)
        XCTAssertEqual(view.surface.size?.widthPx, UInt32((view.renderView.frame.width * scale).rounded()))
        XCTAssertEqual(view.surface.size, view.pixelSize)
        let metrics = try XCTUnwrap(view.metrics)
        XCTAssertEqual(metrics.widthPx, view.pixelSize.widthPx)
        XCTAssertEqual(
            UInt32(metrics.cols), max(1, (metrics.widthPx - 2 * metrics.paddingPx) / metrics.cellWidthPx))
        XCTAssertLessThan(metrics.cols, overlayCols, "fewer columns at the same frame")

        view.scroller.preferredStyleOverride = .overlay
        XCTAssertEqual(view.coreResizeCount, resizes + 2, "one resize back")
        XCTAssertEqual(view.surface.size, overlaySize)
        XCTAssertEqual(view.metrics?.cols, overlayCols)
    }

    func testTheSystemSettingChangeResizesTheGrid() async throws {
        try await open()
        try requireSurface()
        let system = TerminalScroller.systemStyle
        defer { TerminalScroller.systemStyle = system }
        TerminalScroller.systemStyle = { .overlay }
        view.scroller.preferredStyleOverride = nil
        let size = view.surface.size
        let resizes = view.coreResizeCount
        // The setting changes; AppKit then posts its notification.
        TerminalScroller.systemStyle = { .legacy }
        XCTAssertEqual(view.coreResizeCount, resizes, "nothing until the notification")
        NotificationCenter.default.post(name: NSScroller.preferredScrollerStyleDidChangeNotification, object: nil)
        XCTAssertEqual(view.coreResizeCount, resizes + 1)
        XCTAssertEqual(view.scroller.scrollerStyle, .legacy)
        XCTAssertLessThan(try XCTUnwrap(view.surface.size).widthPx, try XCTUnwrap(size).widthPx)
        XCTAssertTrue(view.scroller.isRevealed, "Always: shown")
        TerminalScroller.systemStyle = { .overlay }
        NotificationCenter.default.post(name: NSScroller.preferredScrollerStyleDidChangeNotification, object: nil)
        XCTAssertEqual(view.coreResizeCount, resizes + 2)
        XCTAssertEqual(view.surface.size, size)
    }

    func testTheAlternateScreenDoesNotResizeALegacyGrid() async throws {
        try await open()
        try requireSurface()
        view.scroller.preferredStyleOverride = .legacy
        tick()
        let size = view.surface.size
        let resizes = view.coreResizeCount
        let scroller = view.scroller
        try view.session.write(Array("\u{1b}[?1049h".utf8))
        try await waitUntil("the alternate screen is shown") { (try? self.position.history) == 0 }
        tick()
        XCTAssertEqual(view.coreResizeCount, resizes, "entering the alternate screen does not resize")
        XCTAssertEqual(view.surface.size, size)
        XCTAssertFalse(scroller.isHidden, "Always: shown with no history")
        XCTAssertFalse(scroller.isEnabled, "an empty track")
        XCTAssertEqual(scroller.usableParts, .noScrollerParts, "no knob")
        XCTAssertNil(scroller.target(for: .knob))
        XCTAssertNil(scroller.target(for: .decrementPage))
        try view.session.write(Array("\u{1b}[?1049l".utf8))
        try await waitUntil("the primary screen is back") { (try? self.position.history) ?? 0 > 0 }
        tick()
        XCTAssertEqual(view.coreResizeCount, resizes, "leaving it does not resize")
        XCTAssertEqual(view.surface.size, size)
        XCTAssertTrue(scroller.isEnabled)
        XCTAssertNotNil(scroller.target(for: .decrementPage))
    }

    func testClicksOnTheGridReachTheTerminalViewInEitherStyle() async throws {
        try await open()
        for style: NSScroller.Style in [.overlay, .legacy] {
            view.scroller.preferredStyleOverride = style
            let inGrid = NSPoint(x: 20, y: 20)
            XCTAssertNil(view.renderView.hitTest(inGrid), "\(style)")
            XCTAssertTrue(view.hitTest(view.convert(inGrid, to: view.superview)) === view, "\(style)")
            XCTAssertTrue(view.surface.layer === view.metalLayer, "\(style)")
        }
    }

    // MARK: The scroller alone

    /// A scroller in a window that is never on screen, at the right edge
    /// of a 480 x 200 host, showing `position`.
    private func standaloneScroller(
        _ position: TesseraSession.ScrollPosition
    ) -> (scroller: TerminalScroller, window: NSWindow) {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200), styleMask: [.titled], backing: .buffered,
            defer: false)
        window.isReleasedWhenClosed = false
        let host = NSView(frame: NSRect(x: 0, y: 0, width: 480, height: 200))
        host.wantsLayer = true
        window.contentView = host
        let scroller = TerminalScroller()
        scroller.preferredStyleOverride = .overlay
        scroller.frame = NSRect(x: 480 - TerminalScroller.width, y: 0, width: TerminalScroller.width, height: 200)
        host.addSubview(scroller)
        scroller.update(position)
        return (scroller, window)
    }

    @available(macOS, deprecated: 10.14)
    func testTheLineTargetsMoveOneLine() {
        let (scroller, window) = standaloneScroller(.init(offset: 5, history: 10, rows: 12))
        defer { window.close() }
        XCTAssertEqual(scroller.target(for: .decrementLine), .offset(6))
        XCTAssertEqual(scroller.target(for: .incrementLine), .offset(4))
        scroller.update(.init(offset: 10, history: 10, rows: 12))
        XCTAssertEqual(scroller.target(for: .decrementLine), .offset(10), "at the oldest line")
        scroller.update(.init(offset: 0, history: 10, rows: 12))
        XCTAssertEqual(scroller.target(for: .incrementLine), .offset(0), "at the live screen")
    }

    func testAFadedScrollerLetsClicksThroughWithoutACommit() {
        let (scroller, window) = standaloneScroller(.init(offset: 200, history: 500, rows: 12))
        defer { window.close() }
        var clock: CFTimeInterval = 1000
        scroller.now = { clock }
        let point = NSPoint(x: scroller.frame.midX, y: scroller.frame.midY)
        XCTAssertNil(scroller.hitTest(point), "not shown yet")
        scroller.flash()
        XCTAssertTrue(scroller.isRevealed)
        XCTAssertTrue(scroller.hitTest(point) === scroller)
        clock += TerminalScroller.holdDuration + TerminalScroller.fadeDuration - 0.01
        XCTAssertTrue(scroller.isRevealed, "still fading")
        clock += 0.02
        // No Core Animation commit in between: the finished animation is
        // still on the layer.
        XCTAssertFalse(scroller.isRevealed, "faded out")
        XCTAssertNil(scroller.hitTest(point), "a click goes through")
    }

    func testAPositionThatKeepsTheHistoryAndStyleSetsNoScrollerProperty() {
        for style: NSScroller.Style in [.overlay, .legacy] {
            let (scroller, window) = standaloneScroller(.init(offset: 0, history: 0, rows: 12))
            defer { window.close() }
            let name = style == .legacy ? "legacy" : "overlay"
            scroller.preferredStyleOverride = style
            let sets = PropertySetCounter(scroller)
            // Output streaming into the scrollback while the view is
            // scrolled back: one new position per frame.
            for frame in 0..<100 {
                scroller.update(.init(offset: UInt64(50 + frame), history: UInt64(500 + frame), rows: 12))
                if frame == 0 {
                    XCTAssertGreaterThan(sets.count, 0, "\(name): history appeared, so the first one sets")
                    sets.count = 0
                }
            }
            XCTAssertEqual(sets.count, 0, "\(name): 99 positions with history in one style set nothing")
            scroller.update(.init(offset: 0, history: 0, rows: 12))
            XCTAssertGreaterThan(sets.count, 0, "\(name): history gone, so it sets again")
            sets.count = 0
            scroller.preferredStyleOverride = style == .legacy ? .overlay : .legacy
            XCTAssertGreaterThan(sets.count, 0, "\(name): a new style sets again")
            XCTAssertEqual(scroller.isHidden, style == .legacy, "and applies it: overlay without history is hidden")
        }
    }

    /// The scroller as drawn: the lowest alpha over every pixel of its
    /// bounds, and the colour of its left edge at mid height (outside the
    /// track AppKit draws).
    private func strip(of scroller: NSScroller) throws -> (minAlpha: CGFloat, edge: NSColor) {
        let bitmap = try XCTUnwrap(scroller.bitmapImageRepForCachingDisplay(in: scroller.bounds))
        scroller.cacheDisplay(in: scroller.bounds, to: bitmap)
        var minAlpha: CGFloat = 1
        for y in 0..<bitmap.pixelsHigh {
            for x in 0..<bitmap.pixelsWide {
                minAlpha = min(minAlpha, bitmap.colorAt(x: x, y: y)?.alphaComponent ?? 0)
            }
        }
        let edge = try XCTUnwrap(bitmap.colorAt(x: 0, y: bitmap.pixelsHigh / 2)?.usingColorSpace(.sRGB))
        return (minAlpha, edge)
    }

    func testALegacyStripIsOpaqueAndFollowsTheAppearance() throws {
        // Created while the system asks for legacy scrollers: filled from
        // the start, with no style change to apply it.
        let system = TerminalScroller.systemStyle
        defer { TerminalScroller.systemStyle = system }
        TerminalScroller.systemStyle = { .legacy }
        XCTAssertNotNil(TerminalScroller().layer?.backgroundColor, "Show scroll bars: Always, a new scroller is filled")
        TerminalScroller.systemStyle = system
        let (scroller, window) = standaloneScroller(.init(offset: 100, history: 0, rows: 12))
        defer { window.close() }
        scroller.preferredStyleOverride = .legacy
        let width = TerminalScroller.width(for: .legacy)
        scroller.frame = NSRect(x: 480 - width, y: 0, width: width, height: 200)
        // One scroller through both appearances and back: the strip's fill
        // follows a change of appearance, not only the first one.
        for history: UInt64 in [0, 500] {
            scroller.update(.init(offset: history / 5, history: history, rows: 12))
            var brightness: [CGFloat] = []
            for name in [NSAppearance.Name.aqua, .darkAqua, .aqua] {
                scroller.appearance = NSAppearance(named: name)
                let (minAlpha, edge) = try strip(of: scroller)
                XCTAssertEqual(minAlpha, 1, "\(name.rawValue), history \(history): every pixel of the strip is opaque")
                brightness.append(edge.brightnessComponent)
            }
            XCTAssertGreaterThan(brightness[0], 0.5, "history \(history): light appearance, a light strip")
            XCTAssertLessThan(brightness[1], 0.5, "history \(history): dark appearance, a dark strip")
            XCTAssertEqual(brightness[2], brightness[0], accuracy: 0.01, "history \(history): light again")
        }
        // An overlay scroller is drawn over the text: AppKit's track only,
        // nothing filled behind it.
        scroller.appearance = NSAppearance(named: .darkAqua)
        scroller.preferredStyleOverride = .overlay
        scroller.frame = NSRect(x: 480 - TerminalScroller.width, y: 0, width: TerminalScroller.width, height: 200)
        XCTAssertNil(scroller.layer?.backgroundColor, "overlay: no strip fill")
        XCTAssertLessThan(try strip(of: scroller).minAlpha, 1, "overlay: the text shows around and through the track")
        // Back to legacy in the appearance the app is not in: the fill is
        // the scroller's appearance's, not the app's.
        let appIsDark = NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        scroller.appearance = NSAppearance(named: appIsDark ? .aqua : .darkAqua)
        scroller.preferredStyleOverride = .legacy
        scroller.frame = NSRect(x: 480 - width, y: 0, width: width, height: 200)
        let (minAlpha, edge) = try strip(of: scroller)
        XCTAssertEqual(minAlpha, 1, "legacy again: opaque again")
        XCTAssertEqual(
            edge.brightnessComponent < 0.5, !appIsDark, "legacy again: the strip follows the scroller's appearance")
    }

    func testAKnobClickedAndHeldStillDuringOutputDoesNotMoveTheView() async throws {
        try await open()
        try requireSurface()
        view.scroller.preferredStyleOverride = .legacy
        view.scroll(to: .offset(500))
        tick()
        let scroller = view.scroller
        let knob = scroller.rect(for: .knob)
        XCTAssertFalse(knob.isEmpty)
        let point = scroller.convert(NSPoint(x: knob.midX, y: knob.midY), to: nil)
        var targets: [TesseraSession.ScrollTarget] = []
        let original = scroller.onScroll
        scroller.onScroll = {
            targets.append($0)
            original?($0)
        }
        let start = try position
        // AppKit's tracking loop runs until the release, which the holder
        // posts after output has grown the history; the pointer never moves.
        let holder = KnobHolder(view: view, window: window, point: point)
        let timer = Timer(
            timeInterval: 0.05, target: holder, selector: #selector(KnobHolder.fire(_:)), userInfo: nil, repeats: true)
        RunLoop.main.add(timer, forMode: .common)
        RunLoop.main.add(timer, forMode: .eventTracking)
        scroller.mouseDown(with: try holder.event(.leftMouseDown))
        timer.invalidate()
        tick()
        let end = try position
        let growth = end.history - start.history
        XCTAssertGreaterThan(growth, 0, "output arrived while the knob was held")
        XCTAssertEqual(end.offset, start.offset + growth, "the view stays on its lines: targets \(targets)")
        XCTAssertEqual(targets, [], "a knob clicked and released in place sends nothing")
    }

    func testAKnobActionAtTheValueAlreadySentIsNotSentAgain() {
        let (scroller, window) = standaloneScroller(.init(offset: 200, history: 500, rows: 12))
        defer { window.close() }
        var targets: [TesseraSession.ScrollTarget] = []
        scroller.onScroll = { targets.append($0) }
        scroller.beginTracking()
        scroller.doubleValue = 0.5
        scroller.userMoved(part: .knob)
        XCTAssertEqual(targets, [.offset(250)])
        // Output grows the history while the knob is held still; AppKit
        // sends the frozen value again on release.
        scroller.update(.init(offset: 250 + 300, history: 800, rows: 12))
        scroller.doubleValue = 0.5
        scroller.userMoved(part: .knob)
        XCTAssertEqual(targets.count, 1, "the same value is not sent again")
        scroller.doubleValue = 0.6
        scroller.userMoved(part: .knob)
        XCTAssertEqual(targets.last, .offset(800 - 480), "a new value is")
        scroller.userMoved(part: .decrementPage)
        scroller.userMoved(part: .decrementPage)
        XCTAssertEqual(targets.suffix(2), [.pageUp, .pageUp], "track clicks are not deduplicated")
        scroller.endTracking()
        scroller.beginTracking()
        scroller.doubleValue = 0.6
        scroller.userMoved(part: .knob)
        XCTAssertEqual(targets.count, 5, "the first action of a new press always goes")
        scroller.endTracking()
    }

    func testAScrollerHiddenUnderThePointerWaitsForItAgain() throws {
        let (scroller, window) = standaloneScroller(.init(offset: 200, history: 500, rows: 12))
        defer { window.close() }
        let enter = NSEvent.enterExitEvent(
            with: .mouseEntered, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: window.windowNumber,
            context: nil, eventNumber: 0, trackingNumber: 0, userData: nil)
        scroller.mouseEntered(with: try XCTUnwrap(enter))
        XCTAssertTrue(scroller.isRevealed, "under the pointer")
        scroller.update(.init(offset: 0, history: 0, rows: 12))
        XCTAssertTrue(scroller.isHidden, "no history")
        scroller.update(.init(offset: 0, history: 30, rows: 12))
        XCTAssertFalse(scroller.isHidden)
        XCTAssertFalse(scroller.isRevealed, "not assumed to be under the pointer")
        scroller.flash()
        XCTAssertTrue(scroller.isRevealed, "a scroll shows it")
    }

    /// Polls `condition` on the main actor until it holds or a timeout
    /// passes.
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

/// Counts the sets of a scroller's `alphaValue`, `isEnabled` and `isHidden`
/// (key-value observation reports every set, even to the same value).
@MainActor
private final class PropertySetCounter {
    var count = 0
    private var observations: [NSKeyValueObservation] = []

    init(_ scroller: NSScroller) {
        observations = [
            scroller.observe(\.alphaValue) { [weak self] _, _ in MainActor.assumeIsolated { self?.count += 1 } },
            scroller.observe(\.isEnabled) { [weak self] _, _ in MainActor.assumeIsolated { self?.count += 1 } },
            scroller.observe(\.isHidden) { [weak self] _, _ in MainActor.assumeIsolated { self?.count += 1 } },
        ]
    }
}

/// Holds a knob pressed for ``TerminalScrollTests``: every 50 ms it has the
/// child print 40 lines (ten times) and draws a frame, then posts the
/// release at the press's point.
@MainActor
private final class KnobHolder: NSObject {
    let view: TerminalSurfaceView
    let window: NSWindow
    let point: NSPoint
    private var step = 0

    init(view: TerminalSurfaceView, window: NSWindow, point: NSPoint) {
        self.view = view
        self.window = window
        self.point = point
    }

    func event(_ type: NSEvent.EventType) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.mouseEvent(
                with: type, location: point, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1,
                pressure: type == .leftMouseUp ? 0 : 1))
    }

    @objc func fire(_ timer: Timer) {
        step += 1
        if step <= 10 {
            try? view.session.write(Array(String(repeating: "x\r\n", count: 40).utf8))
        }
        view.sessionDidOutput()
        view.displayLinkDidFire()
        // A second release in case AppKit missed the first.
        if step == 20 || step == 60, let up = try? event(.leftMouseUp) {
            NSApp.postEvent(up, atStart: false)
        }
        if step > 70 {
            timer.invalidate()
        }
    }
}
