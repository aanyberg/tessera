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
    /// `lines`) and waits until all of them are in the session: with the
    /// cursor's line, `lines + 1` lines fill the screen and the history.
    private func open(lines: Int = 1000) async throws {
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "seq 1 \(lines); stty raw -echo; exec cat"])
        let controller = try appDelegate.newWindow()
        view = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        view.frameStatusOverride = .presented
        view.occlusionOverride = false
        // The scroll bar's visibility follows the user's setting otherwise.
        view.scroller.preferredStyleOverride = .overlay
        try await waitUntil("the child's \(lines) lines are in the session") {
            guard let position = try? self.view.session.scrollPosition else { return false }
            return position.history + UInt64(position.rows) == UInt64(lines + 1)
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

    /// Asserts that the scroll bar shows `expected`: the knob's proportion
    /// rows / (history + rows), and its place along the track for the
    /// offset, to the pixel.
    private func assertScroller(
        shows expected: TesseraSession.ScrollPosition, _ message: String, file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let scroller = view.scroller
        XCTAssertEqual(scroller.position, expected, message, file: file, line: line)
        XCTAssertFalse(scroller.isHidden, message, file: file, line: line)
        let history = Double(expected.history)
        let proportion = Double(expected.rows) / (history + Double(expected.rows))
        XCTAssertEqual(Double(scroller.knobProportion), proportion, accuracy: 1e-9, message, file: file, line: line)
        let pixel = 1 / (window.backingScaleFactor)
        let slot = scroller.rect(for: .knobSlot)
        let knob = scroller.rect(for: .knob)
        XCTAssertGreaterThan(knob.height, 0, message, file: file, line: line)
        // AppKit keeps a knob long enough to grab.
        XCTAssertGreaterThanOrEqual(knob.height + pixel, slot.height * proportion, message, file: file, line: line)
        let fromTop = (history - Double(expected.offset)) / history
        let travel = slot.height - knob.height
        let top = scroller.isFlipped ? slot.minY + travel * fromTop : slot.maxY - knob.height - travel * fromTop
        XCTAssertEqual(knob.minY, top, accuracy: pixel, "\(message): knob position", file: file, line: line)
    }

    func testTheScrollerFollowsOutputAndScrolling() async throws {
        try await open()
        try requireSurface()
        tick()
        assertScroller(shows: try position, "after the first frame")

        // Output at the live screen: more history, a smaller knob at the
        // bottom.
        let before = try position.history
        try view.session.write(Array(String(repeating: "x\n", count: 50).utf8))
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
        try view.session.write(Array(String(repeating: "y\n", count: 30).utf8))
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
        assertScroller(shows: shown, "a short history")
        let proportional = view.scroller.rect(for: .knobSlot).height * view.scroller.knobProportion
        XCTAssertEqual(
            view.scroller.rect(for: .knob).height, proportional, accuracy: 1 / window.backingScaleFactor,
            "the knob's length is the screen's share of the lines")
        view.scroll(to: .top)
        tick()
        assertScroller(shows: try position, "at the top")
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
        // Scrolling back during the composition is the user's choice; the
        // next update of the same composition does not undo it.
        try press(commandUp())
        view.setMarkedText("にほ", selectedRange: NSRange(location: 2, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertGreaterThan(try position.offset, 0)
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
