import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Forwards a private core's output to the views on its sessions, as the
/// app's ``SessionRouter`` does, and counts the deliveries.
@MainActor
private final class FindOutputRouter: TesseraAppDelegate {
    private struct WeakView { weak var view: TerminalSurfaceView? }
    private var views: [UInt64: WeakView] = [:]
    private(set) var deliveries = 0

    func add(_ view: TerminalSurfaceView) { views[view.session.id] = WeakView(view: view) }

    func sessionDidOutput(_ session: TesseraSession) {
        deliveries += 1
        views[session.id]?.view?.sessionDidOutput()
    }
    func session(_ session: TesseraSession, didSetTitle title: String) {}
    func sessionDidRingBell(_ session: TesseraSession) {}
    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {}
    func appearanceDidChange(opaque: Bool) {}
}

private struct TimedOut: Error, CustomStringConvertible {
    var description: String
}

/// Drives `view`'s display link until the child's output shows as `lines`
/// on the terminal: as many lines in the scrollback and on the screen
/// (trailing blanks dropped), the last of them complete. Output arrives in
/// order, so the earlier lines are there too. A query typed before then
/// searches only what has arrived: the search reports done with too few
/// matches, or makes an older match current, on a slow runner (CI). Output
/// that fits on the screen never moves the scroll position, so it is read
/// as text, through a select-all that is dropped again.
@MainActor
private func waitForLines(
    _ lines: [String], on view: TerminalSurfaceView, timeout: Duration = .seconds(60)
) async throws {
    let deadline = ContinuousClock.now + timeout
    while true {
        try view.session.selectAll()
        let text = try view.session.selectionText() ?? ""
        try view.session.selectClear()
        var shown = text.split(separator: "\n", omittingEmptySubsequences: false).map {
            String($0.reversed().drop(while: \.isWhitespace).reversed())
        }
        while shown.last?.isEmpty == true { shown.removeLast() }
        if shown.count == lines.count && shown.last == lines.last { return }
        guard ContinuousClock.now < deadline else {
            XCTFail("timed out after \(timeout) waiting for \(lines); shown: \(shown)")
            throw TimedOut(description: "the printed lines")
        }
        view.displayLinkDidFire()
        try await Task.sleep(for: .milliseconds(5))
    }
}

/// The find bar on one view (#46b, ADR 0038): the count, moves, Escape,
/// refusals, smart case, live output, Copy All Matches, the per-frame
/// budget and the controller's states. On a core of the test's own (its
/// configuration in a temporary directory, never the user's), with views
/// on its sessions; the display link is driven by hand
/// (``TerminalSurfaceView/displayLinkDidFire()``).
@MainActor
final class FindBarTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?
    private var router: FindOutputRouter!
    private var windows: [KeyControlledWindow] = []

    override func setUp() async throws {
        FindController.debounce = .zero
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-find-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute configuration directory")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        for window in windows {
            (window.contentView as? TerminalSurfaceView)?.inputContext?.deactivate()
            window.close()
        }
        windows = []
        core?.shutdown()
        core = nil
        router = nil
        FindController.debounce = .milliseconds(120)
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    /// Starts the test's core with `config` as its `config.toml`.
    @discardableResult
    private func startCore(_ config: String = "") throws -> TesseraApp {
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        let core = try TesseraApp()
        router = FindOutputRouter()
        core.delegate = router
        self.core = core
        return core
    }

    /// A view on a session running `script` (`sh -c`), in a window of its
    /// own unless `windowed` is false (then the session keeps `rows` ×
    /// `cols`).
    private func makeView(
        _ script: String, windowed: Bool = true, rows: UInt16 = 10, cols: UInt16 = 48
    ) throws -> TerminalSurfaceView {
        let core = try core ?? startCore()
        let session = try core.spawn(.init(program: "/bin/sh", arguments: ["-c", script], rows: rows, cols: cols))
        let view = TerminalSurfaceView(session: session)
        router.add(view)
        if windowed {
            let window = KeyControlledWindow(contentRect: view.frame)
            window.contentView = view
            _ = window.makeFirstResponder(view)
            windows.append(window)
        }
        return view
    }

    /// `lines` printed, then the child waits; returned once they are all on
    /// the terminal.
    private func makeView(showing lines: [String], windowed: Bool = true) async throws -> TerminalSurfaceView {
        let text = lines.joined(separator: #"\r\n"#)
        let view = try makeView("printf '\(text)'; exec cat > /dev/null", windowed: windowed)
        try await waitForLines(lines, on: view)
        return view
    }

    /// Drives the display link until `condition` holds.
    private func tick(
        _ view: TerminalSurfaceView, until description: String, timeout: Duration = .seconds(20),
        _ condition: () throws -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while try !condition() {
            guard ContinuousClock.now < deadline else {
                let status = (try? view.session.searchStatus).map { "\($0)" } ?? "no session"
                XCTFail("timed out after \(timeout): \(description); the search: \(status)")
                throw TimedOut(description: description)
            }
            view.displayLinkDidFire()
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Drives the display link until the child's output fills `rows` rows
    /// (history and screen, the cursor's row included). A query typed
    /// before then makes the first match found current, and output after
    /// it adds newer matches without moving it: a slow runner (CI) types
    /// before the last lines have arrived.
    private func waitForOutput(_ view: TerminalSurfaceView, rows: UInt64) async throws {
        try await tick(view, until: "\(rows) rows of output", timeout: .seconds(60)) {
            let position = try view.session.scrollPosition
            return position.history + UInt64(position.rows) >= rows
        }
    }

    /// Opens the bar and types `query` into its field, as the user would.
    private func type(_ query: String, into view: TerminalSurfaceView) throws {
        if !view.isFindBarOpen { view.showFindBar(nil) }
        let bar = try XCTUnwrap(view.findBar)
        bar.field.stringValue = query
        bar.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: bar.field))
    }

    private func status(_ view: TerminalSurfaceView) throws -> TesseraSession.SearchStatus {
        try view.session.searchStatus
    }

    // MARK: Count and moves

    func testTypingAQueryShowsTheCountFromTheCore() async throws {
        let view = try await makeView(showing: ["hit one", "miss", "hit two", "hit three"])
        try type("hit", into: view)
        try await tick(view, until: "three matches") { try status(view).matches == 3 && status(view).done }
        let bar = try XCTUnwrap(view.findBar)
        XCTAssertEqual(try status(view).current, 2, "the newest match is current once found")
        XCTAssertEqual(bar.countLabel.stringValue, "3 of 3")
        XCTAssertEqual(view.find.countText, "3 of 3")
        try type("miss", into: view)
        try await tick(view, until: "one match") { try status(view).matches == 1 && status(view).done }
        XCTAssertEqual(bar.countLabel.stringValue, "1 of 1")
        try type("nothing", into: view)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(bar.countLabel.stringValue, "No matches")
        XCTAssertFalse(bar.olderButton.isEnabled)
        try type("", into: view)
        XCTAssertEqual(bar.countLabel.stringValue, "", "an empty query shows nothing")
        XCTAssertFalse(try status(view).active, "and clears the core's search")
    }

    func testFindNextMovesOlderAndScrollsAndFindPreviousNewerBothWrap() async throws {
        let view = try makeView("echo needle 0; seq 1 60; echo needle 1; seq 61 120; echo needle 2; exec cat > /dev/null")
        // 123 lines and the cursor's row.
        try await waitForOutput(view, rows: 124)
        try type("needle", into: view)
        try await tick(view, until: "three matches") { try status(view).matches == 3 && status(view).done }
        XCTAssertEqual(try status(view).current, 2)
        XCTAssertEqual(try view.session.scrollPosition.offset, 0, "the newest is on the live screen")

        view.findNextMatch(nil)
        XCTAssertEqual(try status(view).current, 1, "⌘G: older")
        let first = try view.session.scrollPosition.offset
        XCTAssertGreaterThan(first, 0, "scrolled back to it")
        view.findNextMatch(nil)
        XCTAssertEqual(try status(view).current, 0)
        let second = try view.session.scrollPosition.offset
        XCTAssertGreaterThan(second, first, "further back")
        XCTAssertEqual(view.find.countText, "1 of 3")
        view.findNextMatch(nil)
        XCTAssertEqual(try status(view).current, 2, "older wraps to the newest")
        XCTAssertLessThan(try view.session.scrollPosition.offset, second)

        view.findPreviousMatch(nil)
        XCTAssertEqual(try status(view).current, 0, "⇧⌘G: newer wraps to the oldest")
        view.findPreviousMatch(nil)
        XCTAssertEqual(try status(view).current, 1, "newer")
        XCTAssertGreaterThan(try view.session.scrollPosition.offset, 0)
    }

    func testANewQueryScrollsToItsNewestMatch() async throws {
        let view = try makeView("echo needle 0; echo needle 1; seq 1 60; exec cat > /dev/null")
        // 62 lines and the cursor's row.
        try await waitForOutput(view, rows: 63)
        try type("needle", into: view)
        // The tick that finds the first match scrolls to it with one call
        // after its step (#331: not a move away, a scroll back and a move
        // back).
        let deadline = ContinuousClock.now + .seconds(20)
        var revealCalls: Int?
        while revealCalls == nil {
            guard ContinuousClock.now < deadline else { return XCTFail("nothing found") }
            let before = view.find.searchCallCount
            view.displayLinkDidFire()
            if view.find.pendingMove == nil, try status(view).matches > 0 {
                revealCalls = view.find.searchCallCount - before
            }
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertEqual(revealCalls, 2, "one step and one reveal")
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        XCTAssertEqual(try status(view).current, 1, "the newest")
        let position = try view.session.scrollPosition
        XCTAssertGreaterThan(position.offset, 50, "scrolled back to it")
        // `needle 1` has 60 lines and the cursor's row below it.
        let rowsBelow: UInt64 = 61
        XCTAssertTrue(
            position.offset <= rowsBelow && rowsBelow < position.offset + UInt64(position.rows),
            "the newest match is in view: \(position)")
        // The same query again keeps the match and the view.
        try view.session.write(Array("\n".utf8))
        for _ in 0..<5 { view.displayLinkDidFire() }
        XCTAssertEqual(try status(view).current, 1)
    }

    func testANewQueryWhoseNewestMatchIsInViewLeavesTheViewAlone() async throws {
        // 12 rows: `needle 1` sits 9 rows up, in view but above the middle,
        // so centring it would move the view.
        let view = try makeView(
            "echo needle 0; seq 1 60; echo needle 1; seq 1 8; exec cat > /dev/null", windowed: false, rows: 12)
        // 70 lines and the cursor's row.
        try await waitForOutput(view, rows: 71)
        XCTAssertEqual(try view.session.scrollPosition.offset, 0, "at the bottom")
        try type("needle", into: view)
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        XCTAssertEqual(try status(view).current, 1, "the newest")
        XCTAssertEqual(try view.session.scrollPosition.offset, 0, "the view stays at the bottom")
    }

    func testReturnAndShiftReturnInTheFieldMoveOlderAndNewer() async throws {
        let view = try await makeView(showing: ["hit one", "hit two", "hit three"])
        try type("hit", into: view)
        try await tick(view, until: "three matches") { try status(view).matches == 3 && status(view).done }
        let bar = try XCTUnwrap(view.findBar)
        let editor = NSTextView()
        XCTAssertTrue(bar.control(bar.field, textView: editor, doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(try status(view).current, 1, "Return: older")
        bar.onNewer?()
        XCTAssertEqual(try status(view).current, 2, "the newer button")
        bar.olderButton.performClick(nil)
        XCTAssertEqual(try status(view).current, 1, "the older button")
    }

    func testAMoveBeforeTheFirstMatchWaitsForIt() async throws {
        let view = try await makeView(showing: ["hit one", "hit two"])
        try type("hit", into: view)
        // Nothing stepped yet: ⌘G waits for the first match.
        view.findNextMatch(nil)
        XCTAssertEqual(view.find.pendingMove, .older)
        try await tick(view, until: "found") { try status(view).current != nil }
        XCTAssertEqual(try status(view).current, 1, "the newest, once found")
    }

    // MARK: Escape and focus

    func testCommandFAndEscapeGoThroughTheMenuAndReturnFocusToTheTerminal() async throws {
        let view = try await makeView(showing: ["hit one", "hit two"])
        let window = try XCTUnwrap(view.window)
        XCTAssertTrue(window.firstResponder === view)
        // ⌘F is the menu's (the key map's `find`), not the terminal's.
        let commandF = try key("f", code: 3, [.command], in: window)
        XCTAssertFalse(view.performKeyEquivalent(with: commandF), "the menu runs Find")
        XCTAssertTrue(MainMenu.keyEquivalent(of: "find", in: NSApp.mainMenu, matches: commandF))
        let find = try XCTUnwrap(MainMenu.item(forCommand: "find", in: NSApp.mainMenu)?.action)
        XCTAssertTrue(window.firstResponder?.tryToPerform(find, with: nil) ?? false, "up the responder chain")
        let bar = try XCTUnwrap(view.findBar)
        XCTAssertTrue(bar.fieldHasFocus, "the keyboard is in the field")
        XCTAssertTrue(bar.isDescendant(of: view), "the bar floats over the pane (E2)")

        // ⌘G from the field reaches the pane through the responder chain.
        bar.field.currentEditor()?.insertText("hit")
        bar.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: bar.field))
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        let next = try XCTUnwrap(MainMenu.item(forCommand: "find_next", in: NSApp.mainMenu)?.action)
        XCTAssertTrue(window.firstResponder?.tryToPerform(next, with: nil) ?? false)
        XCTAssertEqual(try status(view).current, 0, "older from the field")

        let editor = try XCTUnwrap(bar.field.currentEditor() as? NSTextView)
        editor.doCommand(by: #selector(NSResponder.cancelOperation(_:)))
        XCTAssertNil(view.findBar, "Escape closes the bar")
        XCTAssertFalse(try status(view).active, "and clears the search")
        XCTAssertTrue(window.firstResponder === view, "the keyboard is back in the terminal")
        XCTAssertEqual(view.find.lastQuery, "hit")

        // ⌘G with the bar closed opens it with the last query.
        view.findNextMatch(nil)
        XCTAssertNotNil(view.findBar)
        XCTAssertEqual(view.findBar?.field.stringValue, "hit")
        XCTAssertTrue(window.firstResponder === view, "the keyboard stays in the terminal")
        try await tick(view, until: "found again") { try status(view).current != nil }
        XCTAssertEqual(try status(view).current, 1, "its newest match current")
        view.findBar?.closeButton.performClick(nil)
        XCTAssertNil(view.findBar, "the close button")
    }

    func testTheBarFloatsInTheTopTrailingCornerAndNeverResizesTheGrid() async throws {
        let view = try await makeView(showing: ["hit"])
        let resizes = view.coreResizeCount
        let render = view.renderView.frame
        view.showFindBar(nil)
        let bar = try XCTUnwrap(view.findBar)
        let inset = FindBarView.margin
        let scrollBar = TerminalScroller.width(for: view.scroller.effectiveStyle)
        XCTAssertEqual(bar.frame.maxX, view.bounds.width - inset - scrollBar, accuracy: 1, "trailing, clear of the scroll bar")
        let top = view.isFlipped ? bar.frame.minY : view.bounds.height - bar.frame.maxY
        XCTAssertEqual(top, inset, accuracy: 1, "at the top")
        XCTAssertLessThanOrEqual(bar.frame.width, FindBarView.preferredWidth)
        XCTAssertEqual(view.renderView.frame, render, "no layout slot")
        XCTAssertEqual(view.coreResizeCount, resizes, "the grid keeps its size")
        // A narrow pane gets a narrower bar.
        view.window?.setContentSize(NSSize(width: 200, height: 200))
        view.layoutSubtreeIfNeeded()
        XCTAssertLessThanOrEqual(view.findBar?.frame.maxX ?? .infinity, view.bounds.width - inset)
        XCTAssertGreaterThanOrEqual(view.findBar?.frame.minX ?? -1, 0)
    }

    func testClicksOnTheBarOutsideItsControlsNeverReachTheTerminal() async throws {
        let view = try makeView(#"printf '\033[?1000h\033[?1006hready'; exec cat > /dev/null"#)
        // `sentMouseCount` counts every press handed to the session, whether
        // or not the modes have arrived yet.
        let position = try view.session.scrollPosition
        let cols = UInt16(view.bounds.width / 10)
        view.metrics = TesseraSurface.Metrics(
            widthPx: UInt32(view.bounds.width), heightPx: UInt32(view.bounds.height), scale: 1, cellWidthPx: 10,
            cellHeightPx: 20, baselinePx: 15, paddingPx: 0, rows: position.rows, cols: cols, fontSizePt: 13,
            cursorRow: 0, cursorCol: 0, cursorOnScreen: true, cursorWide: false)
        view.showFindBar(nil)
        view.layoutSubtreeIfNeeded()
        let bar = try XCTUnwrap(view.findBar)
        let window = try XCTUnwrap(view.window as? KeyControlledWindow)
        // Key, as a window clicked in is: the terminal takes other buttons
        // only then.
        window.isKeyForTest = true
        XCTAssertTrue(bar.fieldHasFocus)
        let editor = window.firstResponder
        let padding = NSPoint(x: 2, y: bar.bounds.midY)
        let count = bar.countLabel.convert(
            NSPoint(x: bar.countLabel.bounds.midX, y: bar.countLabel.bounds.midY), to: bar)
        for (name, point) in [("padding", padding), ("count", count)] {
            let inWindow = bar.convert(point, to: nil)
            let hit = try XCTUnwrap(view.superview?.hitTest(view.superview!.convert(inWindow, from: nil)), name)
            XCTAssertTrue(hit.isDescendant(of: bar), "\(name) is the bar's: \(hit)")
            for type: NSEvent.EventType in [
                .leftMouseDown, .leftMouseDragged, .leftMouseUp, .rightMouseDown, .rightMouseUp, .otherMouseDown,
                .otherMouseUp,
            ] {
                let event = try XCTUnwrap(
                    NSEvent.mouseEvent(
                        with: type, location: inWindow, modifierFlags: [], timestamp: 0,
                        windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
                switch type {
                case .leftMouseDown: hit.mouseDown(with: event)
                case .leftMouseDragged: hit.mouseDragged(with: event)
                case .leftMouseUp: hit.mouseUp(with: event)
                case .rightMouseDown: hit.rightMouseDown(with: event)
                case .rightMouseUp: hit.rightMouseUp(with: event)
                case .otherMouseDown: hit.otherMouseDown(with: event)
                default: hit.otherMouseUp(with: event)
                }
            }
            XCTAssertEqual(view.sentMouseCount, 0, "no press from the \(name) reached the program")
            XCTAssertTrue(window.firstResponder === editor, "the field keeps the keyboard after the \(name)")
        }
        // The same press on the terminal is the program's.
        let terminal = try XCTUnwrap(
            NSEvent.mouseEvent(
                with: .leftMouseDown, location: view.convert(NSPoint(x: 5, y: 5), to: nil), modifierFlags: [],
                timestamp: 0, windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1,
                pressure: 1))
        view.mouseDown(with: terminal)
        XCTAssertEqual(view.sentMouseCount, 1, "a press on the terminal is sent")
    }

    // MARK: Refusals and case

    func testAnInvalidRegexShowsAnInlineErrorAndFixingItSearchesAgain() async throws {
        let view = try await makeView(showing: ["hit one", "hit two"])
        view.showFindBar(nil)
        let bar = try XCTUnwrap(view.findBar)
        bar.regexButton.performClick(nil)
        XCTAssertTrue(view.find.regex, "the toggle")
        try type("(hit", into: view)
        XCTAssertEqual(view.find.errorText, "Invalid regex: unclosed group")
        XCTAssertEqual(bar.countLabel.stringValue, "Invalid regex: unclosed group")
        XCTAssertEqual(bar.countLabel.textColor, .systemRed)
        XCTAssertNotNil(bar.countLabel.toolTip, "the full message")
        XCTAssertEqual(view.find.countText, "")
        XCTAssertEqual(try status(view).matches, 0)
        XCTAssertFalse(try status(view).active)
        view.displayLinkDidFire()
        XCTAssertEqual(view.find.stepCount, 0, "nothing to step")

        try type("(hit)", into: view)
        XCTAssertNil(view.find.errorText)
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        XCTAssertEqual(bar.countLabel.stringValue, "2 of 2")
        XCTAssertEqual(bar.countLabel.textColor, .secondaryLabelColor)
    }

    func testSmartCaseAndTheCaseToggles() async throws {
        let view = try await makeView(showing: ["one FOO", "two foo"])
        try type("foo", into: view)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 2, "'foo' finds 'FOO' in smart case")
        try type("Foo", into: view)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 0, "'Foo' does not find 'foo' or 'FOO'")
        XCTAssertEqual(view.find.countText, "No matches")

        let bar = try XCTUnwrap(view.findBar)
        let menu = try XCTUnwrap(bar.caseButton.menu)
        let ignore = try XCTUnwrap(menu.items.firstIndex { $0.title == "Ignore Case" })
        menu.performActionForItem(at: ignore)
        XCTAssertEqual(view.find.caseMode, .insensitive)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 2, "Ignore Case overrides")
        XCTAssertEqual(menu.items[ignore].state, .on)

        let match = try XCTUnwrap(menu.items.firstIndex { $0.title == "Match Case" })
        menu.performActionForItem(at: match)
        try type("foo", into: view)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 1, "Match Case overrides")
    }

    // MARK: Output and screens

    func testOutputWhileTheBarIsOpenKeepsTheCountLive() async throws {
        // `cat` with the terminal's echo: each line written shows twice.
        let view = try makeView("printf 'hit\\r\\n'; exec cat")
        try type("hit", into: view)
        try await tick(view, until: "one match") { try status(view).matches == 1 && status(view).done }
        // Output still in flight takes one more step; then nothing to do.
        try await tick(view, until: "done: the display link may idle", timeout: .seconds(2)) { !view.find.needsStep }
        try view.session.write(Array("hit again\n".utf8))
        try await tick(view, until: "the new lines are searched") { try status(view).matches == 3 && status(view).done }
        XCTAssertEqual(view.findBar?.countLabel.stringValue, "1 of 3", "the current match stays the one shown")
    }

    func testTheAlternateScreenShowsOnlyItsMatches() async throws {
        let view = try makeView(
            #"printf 'hit a\r\nhit b\r\n'; read x; printf '\033[?1049h\033[Halt hit'; exec cat > /dev/null"#)
        try type("hit", into: view)
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        try view.session.write(Array("\n".utf8))
        try await tick(view, until: "the alternate screen's") { try status(view).matches == 1 && status(view).done }
        XCTAssertEqual(view.find.countText, "1 of 1")
    }

    func testEvictedScrollbackDropsItsMatches() async throws {
        try startCore("[terminal]\nscrollback_lines = 100\n")
        let view = try makeView("echo hit; read x; seq 1 300; exec cat > /dev/null")
        try type("hit", into: view)
        try await tick(view, until: "one match") { try status(view).matches == 1 && status(view).done }
        try view.session.write(Array("\n".utf8))
        try await tick(view, until: "evicted") { try status(view).matches == 0 && status(view).done }
        XCTAssertEqual(view.find.countText, "No matches")
        XCTAssertNil(try status(view).current)
    }

    // MARK: Panes

    func testASecondViewsSearchIsIndependentAndClosingItsSessionFreesIt() async throws {
        let first = try await makeView(showing: ["alpha", "beta"])
        let second = try await makeView(showing: ["alpha", "beta", "beta"])
        try type("alpha", into: first)
        try type("beta", into: second)
        try await tick(first, until: "alpha") { try status(first).matches == 1 && status(first).done }
        try await tick(second, until: "beta") { try status(second).matches == 2 && status(second).done }
        second.closeFindBar()
        XCTAssertFalse(try status(second).active)
        XCTAssertEqual(try status(first).matches, 1, "the first keeps its search")
        XCTAssertEqual(first.find.query, "alpha")
        try second.session.close()
        XCTAssertThrowsError(try status(second), "no search for a closed session") { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertTrue(try status(first).active)
    }

    // MARK: Use Selection and Copy All Matches

    func testUseSelectionForFindTakesTheFirstLineSafeAndEscaped() async throws {
        let view = try await makeView(showing: ["a.b*c x\u{202E}y", "a.b*c", "aXbbc"])
        try await tick(view, until: "the text is shown") {
            try view.session.selectAll()
            return try view.session.selectionText()?.hasSuffix("aXbbc") == true
        }
        view.showFindBar(nil)
        view.findBar?.regexButton.performClick(nil)
        view.useSelectionForFind(nil)
        XCTAssertEqual(view.find.query, #"a\.b\*c xy"#, "the first line, its bidi override dropped, escaped")
        XCTAssertEqual(view.findBar?.field.stringValue, view.find.query)
        try view.session.selectBegin(col: 0, row: 1, granularity: .line, block: false)
        view.useSelectionForFind(nil)
        try await tick(view, until: "found") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 2, "the escaped pattern finds itself, not aXbbc")

        // With the bar closed it sets the query ⌘G searches for.
        view.closeFindBar()
        try view.session.selectBegin(col: 0, row: 2, granularity: .line, block: false)
        view.useSelectionForFind(nil)
        XCTAssertNil(view.findBar, "the bar stays closed")
        view.findNextMatch(nil)
        XCTAssertEqual(view.findBar?.field.stringValue, "aXbbc")
    }

    func testRegexEscapingIsPerUnicodeScalar() async throws {
        // `.` with a combining acute is one Swift character, but the regex
        // reads the `.` alone (#331).
        let text = "a.\u{301}b"
        XCTAssertEqual(FindController.escapedForRegex(text).unicodeScalars.map(\.value), [0x61, 0x5C, 0x2E, 0x301, 0x62])
        XCTAssertEqual(
            FindController.query(fromSelection: text, regex: true)?.unicodeScalars.map(\.value),
            [0x61, 0x5C, 0x2E, 0x301, 0x62])
        let view = try await makeView(showing: ["a.\u{301}b ax\u{301}b"])
        view.find.regex = true
        try type(try XCTUnwrap(FindController.query(fromSelection: text, regex: true)), into: view)
        try await tick(view, until: "done") { try status(view).done }
        XCTAssertEqual(try status(view).matches, 1, "the escaped dot finds itself, not the x")
    }

    func testTheSelectionQueryIsCutAtTheQueryLimit() {
        let long = String(repeating: "é", count: 1_000)
        let query = FindController.query(fromSelection: long, regex: false)
        XCTAssertEqual(query?.utf8.count, 1_024)
        let escaped = FindController.query(fromSelection: String(repeating: ".", count: 600), regex: true)
        XCTAssertEqual(escaped?.utf8.count, 1_024)
        XCTAssertTrue(escaped?.hasSuffix(#"\."#) ?? false, "no escape cut in half")
        XCTAssertNil(FindController.query(fromSelection: "\u{1B}\u{7}\n", regex: false), "nothing left")
        XCTAssertEqual(
            FindController.displayText("a\u{1B}[31mb\u{202E}c\u{2028}d\u{85}e\u{7F}f\u{2067}g\u{200F}h\u{61C}i"),
            "a[31mbcdefghi")
        // Other format characters are text: a ZWJ emoji sequence, ZWNJ and
        // a soft hyphen still find their own source.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
        XCTAssertEqual(FindController.displayText("a\(family)b\u{200C}c\u{AD}d\u{200B}e"), "a\(family)b\u{200C}c\u{AD}d\u{200B}e")
        XCTAssertEqual(FindController.query(fromSelection: family, regex: false), family)
        XCTAssertEqual(
            FindController.syntaxSummary("regex parse error:\n    x\u{202E}(\n     ^\nerror: unclosed group"),
            "Invalid regex: unclosed group")
    }

    func testCopyAllMatchesPutsThemOnThePasteboardAndFlagsACut() async throws {
        let pasteboard = NSPasteboard(name: .init("tessera-find-\(UUID().uuidString)"))
        defer { pasteboard.releaseGlobally() }
        pasteboard.clearContents()
        pasteboard.setString("before", forType: .string)
        // 2000 lines of 600 digits: 1.2 MB of matches (26 000 rows at 48
        // columns, all kept).
        try startCore("[terminal]\nscrollback_lines = 40000\n")
        let view = try makeView("echo hit one; echo hit two; read x; printf '%0600d\\n' $(seq 1 2000); exec cat > /dev/null")
        view.pasteboard = pasteboard
        view.copyAllFindMatches(nil)
        XCTAssertEqual(pasteboard.string(forType: .string), "before", "no search: nothing copied")
        try type("hit", into: view)
        try await tick(view, until: "two matches") { try status(view).matches == 2 && status(view).done }
        view.copyAllFindMatches(nil)
        XCTAssertEqual(pasteboard.string(forType: .string), "hit\nhit")
        XCTAssertEqual(view.findBar?.countLabel.stringValue, "Copied all matches")

        view.find.regex = true
        try type("[0-9]+", into: view)
        try view.session.write(Array("\n".utf8))
        try await tick(view, until: "2000 matches") { try status(view).matches == 2_000 && status(view).done }
        view.copyAllFindMatches(nil)
        let copied = try XCTUnwrap(pasteboard.string(forType: .string))
        XCTAssertLessThanOrEqual(copied.utf8.count, TesseraSession.searchMaxMatchesTextBytes)
        XCTAssertGreaterThan(copied.utf8.count, TesseraSession.searchMaxMatchesTextBytes - 601)
        XCTAssertEqual(view.findBar?.countLabel.stringValue, "Copied the matches that fit in 1 MB")
        try type("[0-9]+ ", into: view)
        XCTAssertNotEqual(view.findBar?.countLabel.stringValue, "Copied the matches that fit in 1 MB", "a new query drops it")
    }

    /// At the cap the notice counts the matches copied: lines evicted from
    /// the history since the last step take their matches out of the copy
    /// but not out of the status the bar holds (#375).
    func testCopyAllMatchesPastTheCapSaysHowManyItCopied() async throws {
        let pasteboard = NSPasteboard(name: .init("tessera-find-\(UUID().uuidString)"))
        defer { pasteboard.releaseGlobally() }
        try startCore("[terminal]\nscrollback_lines = 110000\n")
        // 110 000 lines of digits: more matches than the cap, under 1 MB
        // of text for the newest 100 000. Then, on a line from the test,
        // 20 000 more, which evict the oldest.
        let view = try makeView(
            "stty -echo; seq 1 110000; read x; seq 200001 220000; exec cat > /dev/null", windowed: false, rows: 4,
            cols: 8)
        view.pasteboard = pasteboard
        try await tick(view, until: "110 000 lines of history", timeout: .seconds(120)) {
            try view.session.scrollPosition.history >= 109_990
        }
        view.find.regex = true
        try type("[0-9]+", into: view)
        try await tick(view, until: "done", timeout: .seconds(120)) { try status(view).done }
        XCTAssertTrue(try status(view).truncated)
        view.copyAllFindMatches(nil)
        let copied = try XCTUnwrap(pasteboard.string(forType: .string))
        XCTAssertEqual(copied.split(separator: "\n").count, TesseraSession.searchMaxMatches)
        XCTAssertTrue(copied.hasSuffix("110000"), "the newest kept")
        let max = FindController.format(TesseraSession.searchMaxMatches)
        XCTAssertEqual(view.find.notice, "Copied \(max) of more than \(max)", "not 'all matches'")

        // No step meanwhile (the display link is not driven): the bar's
        // status still counts the evicted lines' matches.
        try view.session.write(Array("go\n".utf8))
        let deadline = ContinuousClock.now + .seconds(120)
        while try !view.session.accessibleText().string.contains("220000") {
            guard ContinuousClock.now < deadline else { return XCTFail("the second batch did not arrive") }
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(view.find.status.matches, TesseraSession.searchMaxMatches)
        view.copyAllFindMatches(nil)
        let evicted = try XCTUnwrap(pasteboard.string(forType: .string))
        let count = evicted.split(separator: "\n").count
        XCTAssertLessThan(count, TesseraSession.searchMaxMatches, "the evicted lines' matches are not copied")
        XCTAssertTrue(evicted.hasSuffix("110000"), "the output after the step is not searched yet")
        XCTAssertEqual(view.find.notice, "Copied \(FindController.format(count)) of more than \(max)")
    }

    /// #331 round 1 (swift B2): the copy notice says "all" only when the
    /// copy is whole, nothing gave way at the cap and no line was skipped;
    /// each cut names itself, the 1 MB one first, and skipped lines are
    /// named whatever else it says (#375). At the cap it counts the matches
    /// copied, not the status's.
    func testTheCopyNoticeSaysAllOnlyWhenNothingIsMissing() {
        typealias Status = TesseraSession.SearchStatus
        let cap = TesseraSession.searchMaxMatches
        let max = FindController.format(cap)
        let cases: [(copyTruncated: Bool, copied: Int, status: Status, notice: String)] = [
            (false, 12, Status(matches: 12, active: true, done: true), "Copied all matches"),
            (true, 9, Status(matches: 12, active: true, done: true), "Copied the matches that fit in 1 MB"),
            (
                false, cap, Status(matches: cap, active: true, done: true, truncated: true),
                "Copied \(max) of more than \(max)"
            ),
            (
                false, 12, Status(matches: 12, active: true, done: true, linesSkipped: true),
                "Copied the matches found; some lines were skipped"
            ),
            (
                true, 9, Status(matches: 12, active: true, done: true, linesSkipped: true),
                "Copied the matches that fit in 1 MB; some lines were skipped"
            ),
            (
                true, 9, Status(matches: 12, active: true, done: true, truncated: true, linesSkipped: true),
                "Copied the matches that fit in 1 MB; some lines were skipped"
            ),
            (
                false, cap, Status(matches: cap, active: true, done: true, truncated: true, linesSkipped: true),
                "Copied \(max) of more than \(max); some lines were skipped"
            ),
            (
                false, 90_000, Status(matches: cap, active: true, done: true, truncated: true),
                "Copied \(FindController.format(90_000)) of more than \(max)"
            ),
        ]
        for (copyTruncated, copied, status, notice) in cases {
            XCTAssertEqual(
                FindController.copyNotice(copyTruncated: copyTruncated, copied: copied, status: status), notice,
                "copy cut \(copyTruncated), \(copied) copied, \(status)")
        }
    }

    /// The count as a function of the status alone: pins the ", some
    /// lines skipped" suffix on every form of the count (#318).
    func testTheCountTextFollowsTheStatus() {
        typealias Status = TesseraSession.SearchStatus
        let max = FindController.format(TesseraSession.searchMaxMatches)
        let cases: [(status: Status, text: String)] = [
            (Status(active: true), "Searching…"),
            (Status(active: true, done: true), "No matches"),
            (Status(active: true, done: true, linesSkipped: true), "No matches, some lines skipped"),
            (Status(matches: 1, active: true, done: true), "1 match"),
            (Status(matches: 1_200, active: true, done: true), "\(FindController.format(1_200)) matches"),
            (Status(matches: 12, current: 2, active: true, done: true), "3 of 12"),
            (Status(matches: 12, current: 2, active: true, done: true, linesSkipped: true), "3 of 12, some lines skipped"),
            (Status(matches: 12, current: 12, active: true, done: true), "12 matches"),
            (
                Status(matches: TesseraSession.searchMaxMatches, current: 3, active: true, done: true, truncated: true),
                "more than \(max)"
            ),
            (
                Status(
                    matches: TesseraSession.searchMaxMatches, active: true, done: true, truncated: true,
                    linesSkipped: true),
                "more than \(max), some lines skipped"
            ),
        ]
        for (status, text) in cases {
            XCTAssertEqual(FindController.countText(for: status), text, "\(status)")
        }
    }

    // MARK: Controller states

    func testTypingIsDebouncedAndReturnSendsTheQueryAtOnce() async throws {
        FindController.debounce = .milliseconds(200)
        let view = try await makeView(showing: ["hit one", "hit two"])
        view.showFindBar(nil)
        let calls = view.find.searchCallCount
        for prefix in ["h", "hi", "hit"] { try type(prefix, into: view) }
        XCTAssertTrue(view.find.hasPendingQuery)
        XCTAssertEqual(view.find.searchCallCount, calls, "nothing reached the core while typing")
        XCTAssertFalse(try status(view).active)
        try await Task.sleep(for: .milliseconds(400))
        XCTAssertFalse(view.find.hasPendingQuery)
        XCTAssertEqual(view.find.searchCallCount, calls + 2, "one start and one status read, once")
        XCTAssertTrue(try status(view).active)

        try type("two", into: view)
        XCTAssertTrue(view.find.hasPendingQuery)
        view.findNextMatch(nil)
        XCTAssertFalse(view.find.hasPendingQuery, "⌘G sends it at once")
        try await tick(view, until: "found") { try status(view).current != nil }
        XCTAssertEqual(try status(view).matches, 1)
    }

    func testAClosedBarMakesNoSearchCallPerFrame() async throws {
        let view = try makeView("printf 'hit\\r\\n'; exec cat")
        for _ in 0..<50 { view.displayLinkDidFire() }
        XCTAssertNil(view.findController, "no search before the first ⌘F")
        try type("hit", into: view)
        try await tick(view, until: "found") { try status(view).matches == 1 && status(view).done }
        view.closeFindBar()
        let calls = view.find.searchCallCount
        let deliveries = router.deliveries
        try view.session.write(Array("hit\n".utf8))
        let deadline = ContinuousClock.now + .seconds(10)
        while router.deliveries == deliveries && ContinuousClock.now < deadline {
            view.displayLinkDidFire()
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertGreaterThan(router.deliveries, deliveries, "output arrived while closed")
        for _ in 0..<100 { view.displayLinkDidFire() }
        XCTAssertEqual(view.find.searchCallCount, calls, "zero search calls per frame while closed")
        XCTAssertFalse(view.find.needsStep)
    }

    func testAMillionLinesStepOnceATickAndFinish() async throws {
        try startCore("[terminal]\nscrollback_lines = 1000100\n")
        // No window: the session keeps 8 columns (1M lines of history are
        // about 250 MB at that width).
        let view = try makeView("seq 1 1000000; exec cat > /dev/null", windowed: false, rows: 4, cols: 8)
        try await tick(view, until: "a million lines of history", timeout: .seconds(120)) {
            try view.session.scrollPosition.history >= 999_990
        }
        XCTAssertEqual(FindController.stepBudgetMicros, 4_000, "the budget docs/security.md gives the find bar")
        try type("zebra", into: view)
        // Every display link tick counted (the helper would add its own).
        var ticks = 0
        var tickTimes: [Duration] = []
        let steps = view.find.stepCount
        let deadline = ContinuousClock.now + .seconds(120)
        while try !status(view).done {
            guard ContinuousClock.now < deadline else { return XCTFail("the scan did not finish") }
            let before = view.find.stepCount
            let start = ContinuousClock.now
            view.displayLinkDidFire()
            tickTimes.append(ContinuousClock.now - start)
            ticks += 1
            XCTAssertLessThanOrEqual(view.find.stepCount - before, 1, "at most one step a tick")
            try await Task.sleep(for: .milliseconds(1))
        }
        tickTimes.sort()
        let median = tickTimes[tickTimes.count / 2]
        print("find 1M lines: \(ticks) ticks, median \(median), max \(tickTimes.last ?? .zero)")
        // About 30 ms of scanning here: a budget of a second would finish
        // in one tick.
        XCTAssertGreaterThan(ticks, 2, "a 4 ms budget spreads a million lines over ticks")
        XCTAssertEqual(view.find.stepCount - steps, ticks, "one step a tick until done")
        XCTAssertLessThan(median, .milliseconds(16), "a tick stays within a frame")
        XCTAssertEqual(try status(view).matches, 0)

        // More than the cap.
        try type("1", into: view)
        try await tick(view, until: "the cap", timeout: .seconds(120)) { try status(view).truncated }
        XCTAssertEqual(view.find.countText, "more than \(FindController.format(TesseraSession.searchMaxMatches))")
        try await tick(view, until: "done", timeout: .seconds(120)) { try status(view).done }
        XCTAssertEqual(try status(view).matches, TesseraSession.searchMaxMatches)
    }

    func testAFloodWithTheBarOpenStepsOnceAFrameAndClosesPromptly() async throws {
        try startCore()
        let footprint = { () -> UInt64 in
            var usage = rusage_info_v4()
            let status = withUnsafeMutablePointer(to: &usage) {
                $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) { proc_pid_rusage(getpid(), RUSAGE_INFO_V4, $0) }
            }
            return status == 0 ? usage.ri_phys_footprint : 0
        }
        let view = try makeView("exec yes hit", windowed: false, rows: 24, cols: 80)
        try type("hit", into: view)
        // Warm-up: the history fills to its cap (10 000 lines).
        try await tick(view, until: "the history is full") { try view.session.scrollPosition.history >= 10_000 }
        let baseline = footprint()
        var peak = baseline
        var ticks = 0
        var worst: Duration = .zero
        let deliveries = router.deliveries
        let steps = view.find.stepCount
        // The most matches any frame reported: each new generation of
        // output restarts the search, so the count read at one moment can
        // be 0 (#427).
        var mostMatches = 0
        let end = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < end {
            let start = ContinuousClock.now
            view.displayLinkDidFire()
            worst = max(worst, ContinuousClock.now - start)
            ticks += 1
            mostMatches = max(mostMatches, try status(view).matches)
            try await Task.sleep(for: .milliseconds(16))
            peak = max(peak, footprint())
        }
        let stepped = view.find.stepCount - steps
        let delivered = router.deliveries - deliveries
        XCTAssertLessThanOrEqual(stepped, ticks, "at most one step a frame, however much output")
        XCTAssertGreaterThan(stepped, ticks / 2, "the search kept up")
        XCTAssertGreaterThan(mostMatches, 0, "the search found the flood's lines")
        let closeStart = ContinuousClock.now
        view.closeFindBar()
        try view.session.close()
        core?.shutdown()
        core = nil
        let closeTime = ContinuousClock.now - closeStart
        let growthMB = Double(peak - min(peak, baseline)) / 1_048_576
        print("find flood: \(ticks) ticks, \(stepped) steps, \(delivered) deliveries, worst tick \(worst), growth \(Int(growthMB)) MB, close \(closeTime)")
        XCTAssertLessThan(growthMB, 100, "the footprint grew \(growthMB) MB")
        XCTAssertLessThan(closeTime, .seconds(3), "closing took \(closeTime)")
    }

    // MARK: Helpers

    private func key(
        _ characters: String, code: UInt16, _ flags: NSEvent.ModifierFlags, in window: NSWindow
    ) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                windowNumber: window.windowNumber, context: nil, characters: characters,
                charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code))
    }
}

/// The find bar in real tabs and panes (#46b): each pane keeps its own
/// search across pane and tab switches, a maximized pane keeps its bar,
/// the field's focus moves the tab's focused pane, and closing a pane
/// frees its bar, controller, view and session.
@MainActor
final class FindBarPaneTests: XCTestCase {
    private var appDelegate: AppDelegate!
    /// What every pane's child prints.
    private static let lines = ["alpha", "beta", "beta"]

    override func setUp() async throws {
        FindController.debounce = .zero
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        let text = Self.lines.joined(separator: #"\r\n"#)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "printf '\(text)'; exec cat > /dev/null"])
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            for pane in controller.panes { pane.view.inputContext?.deactivate() }
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
        appDelegate = nil
        FindController.debounce = .milliseconds(120)
    }

    private func openTab() throws -> TerminalWindowController {
        let controller = try appDelegate.newWindow()
        controller.window?.setContentSize(NSSize(width: 1000, height: 600))
        controller.paneContainer.layoutSubtreeIfNeeded()
        return controller
    }

    private func split(_ controller: TerminalWindowController) throws -> Pane {
        let pane = try XCTUnwrap(controller.splitFocusedPane(.right))
        controller.paneContainer.layoutSubtreeIfNeeded()
        return pane
    }

    private func search(_ query: String, in pane: Pane) async throws {
        try await waitForLines(Self.lines, on: pane.view)
        pane.view.showFindBar(nil)
        let bar = try XCTUnwrap(pane.view.findBar)
        bar.field.stringValue = query
        bar.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: bar.field))
        let deadline = ContinuousClock.now + .seconds(20)
        while try !(pane.session.searchStatus.done && pane.session.searchStatus.matches > 0) {
            guard ContinuousClock.now < deadline else { return XCTFail("\(query) not found") }
            pane.view.displayLinkDidFire()
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    func testEachPaneKeepsItsOwnSearchAcrossPaneAndTabSwitches() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller)
        controller.focus(left)
        try await search("alpha", in: left)
        controller.focus(right)
        XCTAssertTrue(controller.focusedPane === right)
        try await search("beta", in: right)
        XCTAssertEqual(try left.session.searchStatus.matches, 1)
        XCTAssertEqual(try right.session.searchStatus.matches, 2)

        controller.focus(left)
        let tab = try appDelegate.newTab(in: try XCTUnwrap(controller.window))
        XCTAssertNil(tab.focusedPane.view.findController, "a new tab's pane has no search")
        controller.window?.tabGroup?.selectedWindow = controller.window
        for (pane, query, matches) in [(left, "alpha", 1), (right, "beta", 2)] {
            XCTAssertNotNil(pane.view.findBar, "\(query): the bar stays open")
            XCTAssertEqual(pane.view.find.query, query)
            XCTAssertEqual(pane.view.findBar?.field.stringValue, query)
            XCTAssertTrue(try pane.session.searchStatus.active, "\(query): the search stays")
            XCTAssertEqual(try pane.session.searchStatus.matches, matches)
        }
    }

    func testTheFieldTakingTheKeyboardMovesThePaneFocus() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller)
        right.view.showFindBar(nil)
        controller.focus(left)
        XCTAssertTrue(controller.focusedPane === left)
        let window = try XCTUnwrap(controller.window)
        XCTAssertTrue(window.makeFirstResponder(try XCTUnwrap(right.view.findBar).field))
        XCTAssertTrue(controller.focusedPane === right, "the field's pane is the focused one")
        XCTAssertTrue(controller.paneContainer.activePane === right)
    }

    func testAMaximizedPaneKeepsItsFindBarAndComesBackToItsPlace() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller)
        let container = controller.paneContainer
        try await search("beta", in: right)
        let frames = [left, right].map { container.convert($0.view.bounds, from: $0.view) }

        controller.toggleZoomPane(nil)
        container.layoutSubtreeIfNeeded()
        XCTAssertTrue(controller.isZoomed)
        XCTAssertEqual(container.convert(right.view.bounds, from: right.view), container.bounds)
        let bar = try XCTUnwrap(right.view.findBar, "the bar stays with its pane")
        XCTAssertEqual(bar.frame.maxX, right.view.bounds.width - FindBarView.margin - TerminalScroller.width(for: right.view.scroller.effectiveStyle), accuracy: 1, "in the maximized pane's corner")
        // A bar opened while maximized, on the shown pane.
        right.view.closeFindBar()
        try await search("alpha", in: right)

        controller.toggleZoomPane(nil)
        container.layoutSubtreeIfNeeded()
        XCTAssertFalse(controller.isZoomed)
        XCTAssertEqual([left, right].map { container.convert($0.view.bounds, from: $0.view) }, frames, "both back in place")
        XCTAssertNotNil(right.view.findBar)
        XCTAssertEqual(try right.session.searchStatus.matches, 1)
        // Nothing but split views and panes are left in the layout.
        func strays(in view: NSView) -> [NSView] {
            view.subviews.flatMap { sub -> [NSView] in
                if sub is NSSplitView { return strays(in: sub) }
                return sub is TerminalSurfaceView ? [] : [sub]
            }
        }
        XCTAssertEqual(strays(in: container), [], "the stand-in is gone")
        // Again, so a second maximize finds its own stand-in.
        controller.focus(left)
        controller.toggleZoomPane(nil)
        controller.toggleZoomPane(nil)
        container.layoutSubtreeIfNeeded()
        XCTAssertEqual([left, right].map { container.convert($0.view.bounds, from: $0.view) }, frames)
        XCTAssertEqual(strays(in: container), [])
    }

    func testUnmaximizingPutsThePaneBackInPlaceOfItsOwnStandIn() async throws {
        let controller = try openTab()
        let left = controller.focusedPane
        let right = try split(controller)
        let slot = try XCTUnwrap(right.view.superview as? NSSplitView)
        let index = try XCTUnwrap(slot.subviews.firstIndex(of: right.view))
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.isZoomed)
        // Another view that is no pane, ahead of the stand-in: the stand-in
        // is known by identity, not as the first such view.
        let decoy = NSView()
        slot.addSubview(decoy, positioned: .below, relativeTo: slot.subviews.first)
        controller.toggleZoomPane(nil)
        XCTAssertFalse(controller.isZoomed)
        XCTAssertTrue(decoy.superview === slot, "the other view stays")
        XCTAssertTrue(right.view.superview === slot)
        decoy.removeFromSuperview()
        XCTAssertEqual(slot.subviews.firstIndex(of: right.view), index, "back in its own place")
        XCTAssertTrue(left.view.superview === slot)
    }

    func testClosingAPaneReleasesItsFindBarControllerViewAndSession() async throws {
        weak var weakBar: FindBarView?
        weak var weakFind: FindController?
        weak var weakView: TerminalSurfaceView?
        weak var weakSession: TesseraSession?
        weak var weakController: TerminalWindowController?
        try autoreleasepool {
            let controller = try openTab()
            weakController = controller
            let pane = try split(controller)
            pane.view.showFindBar(nil)
            let bar = try XCTUnwrap(pane.view.findBar)
            bar.field.stringValue = "beta"
            bar.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: bar.field))
            _ = controller.window?.makeFirstResponder(bar.field)
            weakBar = bar
            weakFind = pane.view.findController
            weakView = pane.view
            weakSession = pane.session
            pane.view.inputContext?.activate()
        }
        let found = ContinuousClock.now + .seconds(20)
        while (try? weakSession?.searchStatus.matches) != 2 && ContinuousClock.now < found {
            autoreleasepool { weakView?.displayLinkDidFire() }
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertEqual(try weakSession?.searchStatus.matches, 2, "searching when it closes")
        autoreleasepool { weakController?.closeFocusedPane(nil) }
        let deadline = ContinuousClock.now + .seconds(10)
        while (weakBar != nil || weakFind != nil || weakView != nil || weakSession != nil) && ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertNil(weakBar, "bar")
        XCTAssertNil(weakFind, "controller")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakSession, "session")
        XCTAssertEqual(weakController?.panes.count, 1, "the tab stays")
    }
}
