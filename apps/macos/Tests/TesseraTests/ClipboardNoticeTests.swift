import AppKit
import Darwin
import TesseraKit
import XCTest

@testable import Tessera

/// The notice of a program's clipboard write (#471, ADR 0022 D-NOTICE52):
/// at most one a second per pane, later writes counted into the next; the
/// user's own Copy shows none. Hosted tests open tabs as the app does
/// (`newWindow()`) with children that write OSC 52, and post nothing
/// themselves.
@MainActor
final class ClipboardNoticeTests: XCTestCase {
    private typealias Throttle = ClipboardNoticeThrottle
    private var appDelegate: AppDelegate!
    private var pasteboard: NSPasteboard!
    private var savedProgramPasteboard: NSPasteboard!
    private var previous: String?
    private var dir: URL?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        // Programs write here, never the user's clipboard.
        pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.osc52.\(UUID().uuidString)"))
        savedProgramPasteboard = appDelegate.programPasteboard
        appDelegate.programPasteboard = pasteboard
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        if dir != nil {
            try appDelegate.replaceCore(start: false)
        }
        appDelegate.programPasteboard = savedProgramPasteboard
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        pasteboard.releaseGlobally()
        pasteboard = nil
        appDelegate = nil
    }

    // MARK: Helpers

    private func waitUntil(_ what: String, seconds: Int = 10, _ condition: () throws -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(seconds)
        while try !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    private func startCore(config: String) throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-osc52-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        try appDelegate.replaceCore(start: true)
        appDelegate.programPasteboard = pasteboard
    }

    /// A tab running `script` (sh), then `cat`. Its pane counts as on
    /// screen (``onScreen(_:)``).
    private func openTab(_ script: String) throws -> TerminalSurfaceView {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", script + "; exec cat"])
        return onScreen(try appDelegate.newWindow().focusedPane.view)
    }

    /// The hosted test app's windows report themselves occluded, and a
    /// notice waits for its pane to be on screen (#747): `view`'s window
    /// counts as visible. Whether the pane is hidden by an ancestor or in
    /// a background tab still counts.
    @discardableResult
    private func onScreen(_ view: TerminalSurfaceView) -> TerminalSurfaceView {
        view.occlusionOverride = false
        return view
    }

    /// Lets the child of `view`, waiting in `read go` after printing
    /// "ready", go on.
    private func release(_ view: TerminalSurfaceView) async throws {
        try await waitUntil("the child is ready") { try view.session.accessibleText().string.contains("ready") }
        try view.session.write(Array("\n".utf8))
    }

    /// OSC 52 writing "hello" (5 bytes).
    private static let hello = #"printf '\033]52;c;aGVsbG8=\007'"#
    /// OSC 52 writing "hi!" (3 bytes).
    private static let hi = #"printf '\033]52;c;aGkh\007'"#

    private static func write(_ bytes: Int, _ writes: Int = 1) -> TesseraClipboardWrite {
        TesseraClipboardWrite(bytes: bytes, writes: writes)
    }

    // MARK: The throttle

    func testTheFirstWriteShowsAndLaterOnesWaitForTheInterval() {
        var throttle = Throttle()
        let t0 = ContinuousClock.now
        XCTAssertEqual(throttle.offer(Self.write(5), at: t0), .init(writes: 1, bytes: 5), "the first shows now")
        XCTAssertNil(throttle.dueAt, "nothing held")
        XCTAssertNil(throttle.offer(Self.write(7), at: t0 + .milliseconds(100)), "within the interval: held")
        XCTAssertNil(throttle.offer(Self.write(9, 3), at: t0 + .milliseconds(900)))
        XCTAssertEqual(throttle.held, .init(writes: 4, bytes: 9), "counted, the latest size")
        XCTAssertEqual(throttle.dueAt, t0 + .seconds(1))
        XCTAssertNil(throttle.release(at: t0 + .milliseconds(999)), "not due yet")
        XCTAssertEqual(throttle.release(at: t0 + .seconds(1)), .init(writes: 4, bytes: 9), "due: one notice for all")
        XCTAssertNil(throttle.release(at: t0 + .seconds(5)), "shown once")
        XCTAssertNil(throttle.offer(Self.write(1), at: t0 + .milliseconds(1500)), "a second from the counted one")
        XCTAssertEqual(throttle.offer(Self.write(2), at: t0 + .seconds(3)), .init(writes: 2, bytes: 2), "late: folded")
        XCTAssertNil(throttle.held)
    }

    func testAHiddenPanesWritesAreHeldAsOneNoticeUntilReleased() {
        var throttle = Throttle()
        let t0 = ContinuousClock.now
        throttle.hold(Self.write(5))
        throttle.hold(Self.write(7, 2))
        XCTAssertEqual(throttle.held, .init(writes: 3, bytes: 7), "one notice, counted")
        XCTAssertNil(throttle.dueAt, "none showed before: due at once")
        XCTAssertEqual(throttle.release(at: t0), .init(writes: 3, bytes: 7))
        XCTAssertNil(throttle.held)
        // Held within a second of a notice: it waits out the interval.
        throttle.hold(Self.write(1))
        XCTAssertEqual(throttle.dueAt, t0 + .seconds(1))
        XCTAssertNil(throttle.release(at: t0 + .milliseconds(500)))
        XCTAssertEqual(throttle.release(at: t0 + .seconds(1)), .init(writes: 1, bytes: 1))
        _ = throttle.offer(Self.write(1, .max), at: t0 + .seconds(1))
        throttle.hold(Self.write(1, .max))
        throttle.hold(Self.write(1, .max))
        XCTAssertEqual(throttle.held?.writes, .max, "saturates")
    }

    func testTheCountSaturates() {
        var throttle = Throttle()
        let t0 = ContinuousClock.now
        _ = throttle.offer(Self.write(1), at: t0)
        _ = throttle.offer(Self.write(1, .max), at: t0)
        _ = throttle.offer(Self.write(1, .max), at: t0)
        XCTAssertEqual(throttle.held?.writes, .max)
    }

    func testTheNoticeNamesTheSizeAndCountOnly() {
        XCTAssertEqual(Throttle.Notice(writes: 1, bytes: 1).text, "A program copied 1 byte to the clipboard")
        // Numbers in the user's locale.
        XCTAssertEqual(
            Throttle.Notice(writes: 1, bytes: 1_048_576).text,
            "A program copied \(1_048_576.formatted()) bytes to the clipboard")
        XCTAssertEqual(
            Throttle.Notice(writes: 12, bytes: 5).text, "A program copied to the clipboard at least 12 times, last 5 bytes")
    }

    func testAClosedPanesNoticeSaysSoAndCountsWithThePanesOwn() {
        let closed = Throttle.Notice(writes: 2, bytes: 5).fromClosedPane
        XCTAssertEqual(closed, .init(writes: 2, bytes: 5, closedWrites: 2))
        XCTAssertEqual(
            Throttle.Notice(writes: 1, bytes: 5, closedWrites: 1).text,
            "A program in a closed pane copied 5 bytes to the clipboard")
        XCTAssertEqual(
            closed.text, "A program in a closed pane copied to the clipboard at least 2 times, last 5 bytes")
        var throttle = Throttle()
        let t0 = ContinuousClock.now
        XCTAssertEqual(throttle.offer(Self.write(3), at: t0), .init(writes: 1, bytes: 3))
        XCTAssertNil(throttle.offer(closed, at: t0 + .milliseconds(100)), "the same interval as the pane's own")
        throttle.hold(Self.write(7))
        let mixed = throttle.takeHeld()
        XCTAssertEqual(mixed, .init(writes: 3, bytes: 7, closedWrites: 2))
        XCTAssertEqual(
            mixed?.text, "Programs here and in a closed pane copied to the clipboard at least 3 times, last 7 bytes")
        XCTAssertNil(throttle.held, "taken")
    }

    func testTheNoticeViewLetsClicksThrough() {
        let view = ClipboardNoticeView(frame: NSRect(x: 0, y: 0, width: 200, height: 30))
        XCTAssertNil(view.hitTest(NSPoint(x: 10, y: 10)))
        view.text = "A program copied 1 byte to the clipboard"
        XCTAssertEqual(view.accessibilityLabel(), view.text)
        XCTAssertEqual(view.accessibilityRole(), .staticText)
        XCTAssertFalse(view.label.isAccessibilityElement(), "read once, as the notice")
    }

    // MARK: As the app shows it

    func testAProgramsWriteShowsOneNoticeInItsPane() async throws {
        let view = try openTab(Self.hello)
        try await waitUntil("the write reaches the pasteboard") { pasteboard.string(forType: .string) == "hello" }
        try await waitUntil("the pane shows its notice") { view.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(view.clipboardNotice.lastShown, .init(writes: 1, bytes: 5))
        XCTAssertEqual(view.clipboardNotice.view?.text, "A program copied 5 bytes to the clipboard")
        XCTAssertIdentical(view.clipboardNotice.view?.superview, view, "in the pane")
        XCTAssertTrue(view.clipboardNotice.isShowing)
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(view.clipboardNotice.shownCount, 1, "once per write")
        try await waitUntil("the notice goes away") { !view.clipboardNotice.isShowing }
    }

    func testABurstShowsOneNoticeNowAndOneCountedNoticeASecondLater() async throws {
        // Once the tab has settled (a new tab's first turns are busy): the
        // first write, then three more within the second. The core keeps
        // the last write of an output chunk, and a reader slowed by load
        // folds writes into one chunk, so the counts are ranges; only a
        // stall of 0.7 s folds everything into the first notice.
        let view = try openTab(
            "echo ready; read go; \(Self.hello); sleep 0.4; \(Self.hi); sleep 0.15; \(Self.hello); sleep 0.15; \(Self.hi)")
        try await waitUntil("the child is ready") { try view.session.accessibleText().string.contains("ready") }
        try view.session.write(Array("\n".utf8))
        try await waitUntil("the first notice") { view.clipboardNotice.shownCount == 1 }
        let first = try XCTUnwrap(view.clipboardNotice.lastShownAt)
        let firstShown = try XCTUnwrap(view.clipboardNotice.lastShown)
        XCTAssertLessThan(firstShown.writes, 4, "the first shows before the rest arrive")
        if firstShown.writes == 1 { XCTAssertEqual(firstShown.bytes, 5) }
        try await waitUntil("the counted notice") { view.clipboardNotice.shownCount == 2 }
        let second = try XCTUnwrap(view.clipboardNotice.lastShownAt)
        let counted = try XCTUnwrap(view.clipboardNotice.lastShown)
        XCTAssert(
            (2...4).contains(firstShown.writes + counted.writes),
            "every chunk's write is counted once: \(firstShown.writes) + \(counted.writes)")
        XCTAssertEqual(counted.bytes, 3, "the last size")
        XCTAssertGreaterThanOrEqual(second - first, Throttle.minimumInterval, "rate limited")
        XCTAssertEqual(view.clipboardNotice.view?.text, counted.text)
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(view.clipboardNotice.shownCount, 2, "nothing else held")
    }

    func testTheUsersOwnCopyShowsNoNotice() async throws {
        let view = try openTab("echo copy-me")
        view.pasteboard = pasteboard
        try await waitUntil("the text is on screen") { try view.session.accessibleText().string.contains("copy-me") }
        view.selectAll(nil)
        view.copy(nil)
        XCTAssertEqual(pasteboard.string(forType: .string)?.contains("copy-me"), true)
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(view.clipboardNotice.shownCount, 0)
        XCTAssertNil(view.clipboardNotice.view, "never made")
    }

    func testRefusedWritesShowNoNotice() async throws {
        try startCore(config: "[terminal]\nclipboard_write = false\n")
        let view = try openTab("\(Self.hello); echo done")
        try await waitUntil("the child ran") { try view.session.accessibleText().string.contains("done") }
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertNil(pasteboard.string(forType: .string))
        XCTAssertEqual(view.clipboardNotice.shownCount, 0)
    }

    func testUnderReduceMotionTheNoticeShowsWithoutAFade() async throws {
        let view = try openTab(Self.hello)
        view.scroller.reducesMotion = true
        try await waitUntil("the notice") { view.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(view.clipboardNotice.view?.alphaValue, 1)
    }

    // MARK: Never unseen (#747)

    func testTheNoticeSitsBelowTheFindBarNeverUnderIt() async throws {
        let view = try openTab("echo ready; read go; \(Self.hello)")
        let window = try XCTUnwrap(view.window)
        window.setContentSize(NSSize(width: window.contentMinSize.width, height: 400))
        window.layoutIfNeeded()
        view.showFindBar(nil)
        let bar = try XCTUnwrap(view.findBar)
        try await release(view)
        try await waitUntil("the notice") { view.clipboardNotice.shownCount == 1 }
        let notice = try XCTUnwrap(view.clipboardNotice.view)
        func assertClear(_ what: String, line: UInt = #line) {
            view.layoutSubtreeIfNeeded()
            XCTAssertFalse(
                notice.frame.intersects(bar.frame), "\(what): \(notice.frame) under \(bar.frame)", line: line)
            XCTAssertLessThanOrEqual(notice.frame.maxY, bar.frame.minY, "\(what): below the bar", line: line)
            XCTAssertTrue(
                view.bounds.contains(notice.frame), "\(what): \(notice.frame) in the pane \(view.bounds)", line: line)
            XCTAssertGreaterThan(notice.frame.width, 0, line: line)
        }
        XCTAssertGreaterThan(bar.frame.width, view.bounds.width / 2, "the narrowest pane: the bar spans it")
        assertClear("shown with the bar open, at the narrowest width")
        view.closeFindBar()
        view.layoutSubtreeIfNeeded()
        XCTAssertEqual(
            notice.frame.maxY, view.bounds.height - PaneClipboardNotice.margin, accuracy: 1, "back at the top")
        view.showFindBar(nil)
        let reopened = try XCTUnwrap(view.findBar)
        view.layoutSubtreeIfNeeded()
        XCTAssertFalse(notice.frame.intersects(reopened.frame), "the bar opened over a notice: \(notice.frame)")
        let order = { (subview: NSView) in view.subviews.firstIndex { $0 === subview } ?? -1 }
        XCTAssertGreaterThan(order(notice), order(reopened), "the notice is the higher subview")
        // Too short for both: as low as it fits, over the bar.
        window.setContentSize(window.contentMinSize)
        window.layoutIfNeeded()
        view.layoutSubtreeIfNeeded()
        XCTAssertTrue(view.bounds.contains(notice.frame), "\(notice.frame) in the smallest pane \(view.bounds)")
        XCTAssertLessThan(notice.frame.minY, reopened.frame.minY, "lower than the bar")
        let minimum = window.contentMinSize
        window.setContentSize(NSSize(width: minimum.width + 400, height: minimum.height + 200))
        window.layoutIfNeeded()
        view.layoutSubtreeIfNeeded()
        XCTAssertFalse(notice.frame.intersects(reopened.frame), "after a resize: \(notice.frame)")
        XCTAssertTrue(view.clipboardNotice.isShowing)
    }

    func testAWriteInAPaneBehindAMaximizedOneShowsWhenThePaneIsShown() async throws {
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello); exec cat"])
        let controller = try appDelegate.newWindow()
        let hidden = onScreen(controller.focusedPane.view)
        let shown = onScreen(try XCTUnwrap(controller.splitFocusedPane(.right)).view)
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.isZoomed)
        XCTAssertFalse(hidden.isShownToUser, "behind the maximized pane")
        XCTAssertTrue(shown.isShownToUser)
        try await release(hidden)
        try await waitUntil("the write reaches the pasteboard") { pasteboard.string(forType: .string) == "hello" }
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(hidden.clipboardNotice.shownCount, 0, "held while the pane is hidden")
        XCTAssertFalse(hidden.clipboardNotice.isShowing)
        XCTAssertEqual(shown.clipboardNotice.shownCount, 0, "not shown on another pane")
        controller.toggleZoomPane(nil)
        XCTAssertFalse(controller.isZoomed)
        XCTAssertEqual(hidden.clipboardNotice.shownCount, 1, "shown as the pane is")
        XCTAssertEqual(hidden.clipboardNotice.lastShown, .init(writes: 1, bytes: 5))
        XCTAssertTrue(hidden.clipboardNotice.isShowing)
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertTrue(hidden.clipboardNotice.isShowing, "its full time from when it shows")
    }

    func testAWriteInABackgroundTabShowsWhenItsTabIsSelected() async throws {
        let background = try openTab("echo ready; read go; \(Self.hello)")
        let window = try XCTUnwrap(background.window as? TerminalWindow)
        let front = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let frontWindow = try XCTUnwrap(front.window as? TerminalWindow)
        XCTAssertTrue(window.tabGroup?.selectedWindow === frontWindow)
        XCTAssertFalse(background.isShownToUser, "in a background tab")
        try await release(background)
        try await waitUntil("the write reaches the pasteboard") { pasteboard.string(forType: .string) == "hello" }
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(background.clipboardNotice.shownCount, 0, "held while the tab is in the background")
        XCTAssertEqual(front.clipboardNotice.shownCount, 0, "not shown on another tab")
        frontWindow.selectPreviousTab(nil)
        XCTAssertTrue(window.tabGroup?.selectedWindow === window)
        XCTAssertEqual(background.clipboardNotice.shownCount, 1, "shown as its tab is selected")
        XCTAssertTrue(background.clipboardNotice.isShowing)
        // Away again at once, for longer than a notice stays: it waits.
        window.selectNextTab(nil)
        XCTAssertFalse(background.isShownToUser)
        try await Task.sleep(for: ClipboardNoticeView.holdDuration + .seconds(1))
        frontWindow.selectPreviousTab(nil)
        XCTAssertTrue(background.clipboardNotice.isShowing, "shown again as the tab is back")
        XCTAssertEqual(background.clipboardNotice.shownCount, 1, "the same notice")
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertTrue(background.clipboardNotice.isShowing, "its full time again")
    }

    func testANoticeHeldAsItsTabGoesToTheBackgroundWaitsForTheTab() async throws {
        let background = try openTab("echo ready; read go; \(Self.hello); read go; \(Self.hi)")
        let window = try XCTUnwrap(background.window as? TerminalWindow)
        let front = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let frontWindow = try XCTUnwrap(front.window as? TerminalWindow)
        frontWindow.selectPreviousTab(nil)
        try await release(background)
        try await waitUntil("the first notice") { background.clipboardNotice.shownCount == 1 }
        try background.session.write(Array("\n".utf8))
        try await waitUntil("the second write is held") {
            background.clipboardNotice.throttle.held != nil || background.clipboardNotice.shownCount > 1
        }
        guard background.clipboardNotice.shownCount == 1 else {
            throw XCTSkip("the second write came after the interval (a loaded host)")
        }
        // Away before the held notice falls due: its timer finds the tab
        // hidden and leaves it held.
        window.selectNextTab(nil)
        XCTAssertFalse(background.isShownToUser)
        try await Task.sleep(for: Throttle.minimumInterval + .milliseconds(500))
        XCTAssertEqual(background.clipboardNotice.shownCount, 1, "held while the tab is in the background")
        frontWindow.selectPreviousTab(nil)
        XCTAssertEqual(background.clipboardNotice.shownCount, 2, "shown as the tab is back")
        XCTAssertEqual(background.clipboardNotice.lastShown, .init(writes: 1, bytes: 3))
    }

    // MARK: A pane that closes (#763)

    func testAWriteFromAPaneThatExitsShowsOnThePaneTheFocusMovesTo() async throws {
        weak var weakClosed: TerminalSurfaceView?
        let remaining = try openTab("echo kept")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        // Writes, then exits 0 at once: its pane closes.
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        try autoreleasepool {
            let closing = try XCTUnwrap(controller.splitFocusedPane(.right)).view
            onScreen(closing)
            weakClosed = closing
        }
        try await release(try XCTUnwrap(weakClosed))
        try await waitUntil("the pane closes") { controller.panes.count == 1 }
        XCTAssertEqual(pasteboard.string(forType: .string), "hello")
        try await waitUntil("the notice shows on the pane left") { remaining.clipboardNotice.shownCount == 1 }
        XCTAssertIdentical(controller.focusedPane.view, remaining)
        XCTAssertEqual(remaining.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
        XCTAssertEqual(remaining.clipboardNotice.view?.text, "A program in a closed pane copied 5 bytes to the clipboard")
        XCTAssertTrue(remaining.clipboardNotice.isShowing)
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertTrue(remaining.clipboardNotice.isShowing, "its full time")
        try? await waitUntil("the closed pane is freed") { weakClosed == nil }
        XCTAssertNil(weakClosed, "the closed pane's view")
    }

    func testAWriteFromATabThatExitsShowsOnTheTabLeftInItsWindow() async throws {
        let kept = try openTab("echo kept")
        let window = try XCTUnwrap(kept.window as? TerminalWindow)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let closing = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        XCTAssertFalse(kept.isShownToUser, "in a background tab")
        try await release(closing)
        try await waitUntil("the tab closes") { appDelegate.windowControllers.count == 1 }
        XCTAssertEqual(pasteboard.string(forType: .string), "hello")
        try await waitUntil("the notice shows on the tab left") { kept.clipboardNotice.shownCount == 1 }
        XCTAssertTrue(kept.isShownToUser)
        XCTAssertEqual(kept.clipboardNotice.view?.text, "A program in a closed pane copied 5 bytes to the clipboard")
    }

    func testAClosedTabsNoticeWaitsForAHiddenSuccessor() async throws {
        // The window left is not on screen (the test marks it occluded, as
        // a minimised one is): the notice waits for it.
        let kept = try openTab("echo kept")
        kept.occlusionOverride = true
        // Writes, then exits 0: its window closes.
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let other = onScreen(try appDelegate.newWindow().focusedPane.view)
        try await release(other)
        try await waitUntil("the other window closes") { appDelegate.windowControllers.count == 1 }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(kept.clipboardNotice.shownCount, 0, "held while the window is hidden")
        XCTAssertEqual(kept.clipboardNotice.throttle.held, .init(writes: 1, bytes: 5, closedWrites: 1))
        kept.occlusionOverride = false
        XCTAssertEqual(kept.clipboardNotice.shownCount, 1, "shown as the window is")
    }

    /// How long each of a starting tab's two waits lasts
    /// (``openStartingTab(in:program:completion:)``): under the 5 s a
    /// program lookup may take before its spawn fails.
    private static let startingWait: UInt64 = 3500

    /// A tab of `window` opened as ⌘T opens it, whose session starts in
    /// the background and, through `program` (a symlink of this test's),
    /// only after about twice ``startingWait``: it shows its placeholder
    /// meanwhile. The core starts sessions one at a time, so the tab's
    /// spawn first waits for a window's opened just before it, whose
    /// lookup is slowed by ``startingWait``, and then for its own lookup,
    /// slowed as much: the tab starts long after a closing tab's writes
    /// have moved to it, however slow the host, while each lookup stays
    /// under its limit. The window closes as soon as its session runs, and
    /// at the latest, with the tab, as the test ends.
    private func openStartingTab(
        in window: NSWindow?, program: URL, completion: @escaping @MainActor (Bool) -> Void
    ) throws -> NSWindow {
        guard TesseraTestsCoreTestHooksLinked() else {
            throw XCTSkip("the test host does not export the core's test hooks (a core built without test-hooks)")
        }
        let first = try symlink(to: "/bin/sh")
        XCTAssertEqual(tessera_test_delay_program_lookup(first.path, Self.startingWait), 0)
        XCTAssertEqual(tessera_test_delay_program_lookup(program.path, Self.startingWait), 0)
        let background = appDelegate.spawnsInBackground
        appDelegate.spawnsInBackground = true
        defer { appDelegate.spawnsInBackground = background }
        let startingBefore = appDelegate.pendingTerminals.count
        appDelegate.spawnOptions = .init(program: first.path, arguments: ["-c", "exec cat"])
        let helper = try XCTUnwrap(appDelegate.openWindow { (try? $0.get())?.window?.close() })
        appDelegate.spawnOptions = .init(program: program.path, arguments: ["-c", "echo kept; exec cat"])
        let tab = try XCTUnwrap(appDelegate.openTab(in: window) { completion((try? $0.get()) != nil) })
        // A test that fails early leaves neither starting into the next.
        addTeardownBlock { @MainActor [weak helper, weak tab, delegate = self.appDelegate] in
            guard let appDelegate = delegate else { return }
            for window in [helper, tab].compactMap({ $0 }) where appDelegate.isTerminalWindow(window) {
                window.close()
            }
            let deadline = ContinuousClock.now + .seconds(20)
            while appDelegate.spawnsInFlight > 0, ContinuousClock.now < deadline {
                try? await Task.sleep(for: .milliseconds(10))
            }
        }
        XCTAssertEqual(appDelegate.pendingTerminals.count, startingBefore + 2, "both starting")
        return tab
    }

    /// The writes a closed tab left with `tab` while its session starts:
    /// fails at once if `tab` is no longer starting, as then they went to
    /// its running pane instead and the test proves nothing.
    private func waitForWritesHeld(by tab: NSWindow) async throws -> ClipboardNoticeThrottle.Notice? {
        func pending() throws -> PendingTerminal {
            try XCTUnwrap(
                appDelegate.pendingTerminals.first { $0.window === tab },
                "the tab started before the writes moved to it: its start was not held long enough")
        }
        // They move once AppKit has picked the tab left.
        try await waitUntil("the starting tab holds the writes") { try pending().clipboardNotice != nil }
        return try pending().clipboardNotice
    }

    private func symlink(to target: String) throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-osc52-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let link = dir.appendingPathComponent("sh")
        try FileManager.default.createSymbolicLink(atPath: link.path, withDestinationPath: target)
        addTeardownBlock {
            _ = tessera_test_delay_program_lookup(link.path, 0)
            try? FileManager.default.removeItem(at: dir)
        }
        return link
    }

    func testAWriteFromATabThatExitsWaitsForTheStartingTabItsTabBarSelects() async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let writer = try appDelegate.newWindow()
        let writing = onScreen(writer.focusedPane.view)
        try await waitUntil("the child is ready") { try writing.session.accessibleText().string.contains("ready") }
        var started: Bool?
        let tab = try openStartingTab(in: writer.window, program: try symlink(to: "/bin/sh")) { started = $0 }
        try await waitUntil("the starting tab is selected") { writer.window?.tabGroup?.selectedWindow === tab }
        try writing.session.write(Array("\n".utf8))
        try await waitUntil("the writing tab closes") { !appDelegate.windowControllers.contains { $0 === writer } }
        XCTAssertEqual(pasteboard.string(forType: .string), "hello")
        let expected = ClipboardNoticeThrottle.Notice(writes: 1, bytes: 5, closedWrites: 1)
        let kept = try await waitForWritesHeld(by: tab)
        XCTAssertEqual(kept, .init(writes: 1, bytes: 5), "kept for the starting tab")
        try await waitUntil("the starting tab runs", seconds: 20) { started != nil }
        XCTAssertEqual(started, true)
        let controller = try XCTUnwrap(appDelegate.windowControllers.first { $0.window === tab })
        let view = controller.focusedPane.view
        // A locked console's windows are not visible, a CI runner's are:
        // the pane shows the writes as it runs if the user can see it, and
        // holds them until then otherwise.
        if view.clipboardNotice.shownCount == 0 {
            XCTAssertFalse(view.isShownToUser)
            XCTAssertEqual(view.clipboardNotice.throttle.held, expected, "held until shown")
            onScreen(view)
        } else {
            XCTAssertNil(view.clipboardNotice.throttle.held, "shown as it runs")
        }
        XCTAssertEqual(view.clipboardNotice.shownCount, 1, "shown on the tab the user is left with")
        XCTAssertEqual(view.clipboardNotice.lastShown, expected)
        XCTAssertEqual(view.clipboardNotice.view?.text, "A program in a closed pane copied 5 bytes to the clipboard")
    }

    func testAStartingSuccessorThatFailsToStartPassesTheWritesOn() async throws {
        let other = try openTab("echo kept")
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let writer = try appDelegate.newWindow()
        let writing = onScreen(writer.focusedPane.view)
        try await waitUntil("the child is ready") { try writing.session.accessibleText().string.contains("ready") }
        // Its program is missing: its session never runs.
        let missing = try symlink(to: "/nonexistent-\(UUID().uuidString)")
        var started: Bool?
        let tab = try openStartingTab(in: writer.window, program: missing) { started = $0 }
        try await waitUntil("the starting tab is selected") { writer.window?.tabGroup?.selectedWindow === tab }
        try writing.session.write(Array("\n".utf8))
        try await waitUntil("the writing tab closes") { !appDelegate.windowControllers.contains { $0 === writer } }
        let kept = try await waitForWritesHeld(by: tab)
        XCTAssertEqual(kept, .init(writes: 1, bytes: 5), "kept for the starting tab")
        XCTAssertEqual(other.clipboardNotice.shownCount, 0, "kept for the starting tab")
        try await waitUntil("the starting tab fails", seconds: 20) { started != nil }
        XCTAssertEqual(started, false)
        try await waitUntil("the writes go on to the window left") { other.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(other.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
    }

    func testANoticeSeenInFullDoesNotMoveWhenItsPaneCloses() async throws {
        let remaining = try openTab("echo kept")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "\(Self.hello); read go"])
        let closing = onScreen(try XCTUnwrap(controller.splitFocusedPane(.right)).view)
        try await waitUntil("the notice shows") { closing.clipboardNotice.shownCount == 1 }
        try await waitUntil("the notice goes away", seconds: 10) { !closing.clipboardNotice.isShowing }
        try closing.session.write(Array("\n".utf8))
        try await waitUntil("the pane closes") { controller.panes.count == 1 }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(remaining.clipboardNotice.shownCount, 0, "seen: nothing moves")
    }

    func testANoticeFadingOutDoesNotMoveWhenItsPaneCloses() async throws {
        let remaining = try openTab("echo kept")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "\(Self.hello); exec cat"])
        let closing = onScreen(try XCTUnwrap(controller.splitFocusedPane(.right)).view)
        // The CI host reports Reduce Motion, which skips the fade.
        closing.scroller.reducesMotion = false
        try await waitUntil("the notice shows") { closing.clipboardNotice.shownCount == 1 }
        let shownAt = try XCTUnwrap(closing.clipboardNotice.lastShownAt)
        try await Task.sleep(until: shownAt + ClipboardNoticeView.holdDuration + .milliseconds(60), clock: .continuous)
        guard closing.clipboardNotice.isShowing else {
            throw XCTSkip("the fade ended before the pane could close (a loaded host)")
        }
        // Seen its full time, fading out: closed with ⌘W.
        controller.focus(try XCTUnwrap(controller.panes.first { $0.view === closing }))
        controller.closeFocusedPane(nil)
        XCTAssertEqual(controller.panes.count, 1)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(remaining.clipboardNotice.shownCount, 0, "seen: nothing moves")
    }

    func testANoticeFadingOutAsItsTabHidesIsNotShownAgain() async throws {
        let background = try openTab("echo ready; read go; \(Self.hello)")
        background.scroller.reducesMotion = false
        let window = try XCTUnwrap(background.window as? TerminalWindow)
        let front = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let frontWindow = try XCTUnwrap(front.window as? TerminalWindow)
        frontWindow.selectPreviousTab(nil)
        try await release(background)
        try await waitUntil("the notice shows") { background.clipboardNotice.shownCount == 1 }
        let shownAt = try XCTUnwrap(background.clipboardNotice.lastShownAt)
        try await Task.sleep(until: shownAt + ClipboardNoticeView.holdDuration + .milliseconds(60), clock: .continuous)
        guard background.clipboardNotice.isShowing else {
            throw XCTSkip("the fade ended before the tab could hide (a loaded host)")
        }
        window.selectNextTab(nil)
        XCTAssertFalse(background.isShownToUser)
        try await Task.sleep(for: .milliseconds(500))
        frontWindow.selectPreviousTab(nil)
        XCTAssertTrue(background.isShownToUser)
        XCTAssertFalse(background.clipboardNotice.isShowing, "seen its full time: not shown again")
        XCTAssertEqual(background.clipboardNotice.shownCount, 1)
    }

    func testANoticeHiddenBeforeItsTimeMovesWhenItsTabCloses() async throws {
        let kept = try openTab("echo kept")
        let window = try XCTUnwrap(kept.window as? TerminalWindow)
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello); exec cat"])
        let closing = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let closingWindow = try XCTUnwrap(closing.window as? TerminalWindow)
        try await release(closing)
        try await waitUntil("the notice shows") { closing.clipboardNotice.shownCount == 1 }
        // Away at once, for longer than a notice stays: it has not been seen.
        // Closing the background tab selects it for a moment, which shows
        // the notice again before it moves, so this covers the tab path,
        // not a pane closing while hidden (the maximized-pane test does).
        closingWindow.selectPreviousTab(nil)
        XCTAssertFalse(closing.isShownToUser)
        try await Task.sleep(for: ClipboardNoticeView.holdDuration + .milliseconds(500))
        XCTAssertEqual(kept.clipboardNotice.shownCount, 0)
        closingWindow.close()
        try await waitUntil("the notice shows on the tab left") { kept.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(kept.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
    }

    func testANoticeBackFromAHiddenTabMovesWhenItsTabClosesAtOnce() async throws {
        let kept = try openTab("echo kept")
        let window = try XCTUnwrap(kept.window as? TerminalWindow)
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello); exec cat"])
        let closing = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let closingWindow = try XCTUnwrap(closing.window as? TerminalWindow)
        try await release(closing)
        try await waitUntil("the notice shows") { closing.clipboardNotice.shownCount == 1 }
        closingWindow.selectPreviousTab(nil)
        try await Task.sleep(for: ClipboardNoticeView.holdDuration + .milliseconds(500))
        // Back: it shows its full time again, and the tab closes at once.
        window.selectNextTab(nil)
        XCTAssertTrue(closing.isShownToUser)
        XCTAssertTrue(closing.clipboardNotice.isShowing)
        closingWindow.close()
        try await waitUntil("the notice shows on the tab left") { kept.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(kept.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
    }

    func testANoticeHiddenBeforeItsTimeMovesWhenItsPaneExits() async throws {
        let remaining = try openTab("echo kept")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        // Writes, shows its notice, then exits 0 once let go: its pane closes.
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "\(Self.hello); read go"])
        let closing = onScreen(try XCTUnwrap(controller.splitFocusedPane(.right)).view)
        try await waitUntil("the notice shows") { closing.clipboardNotice.shownCount == 1 }
        // Behind the maximized pane left at once, for longer than a notice
        // stays: it has not been seen.
        controller.focus(try XCTUnwrap(controller.panes.first { $0.view === remaining }))
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.isZoomed)
        XCTAssertFalse(closing.isShownToUser)
        try await Task.sleep(for: ClipboardNoticeView.holdDuration + .milliseconds(500))
        XCTAssertFalse(closing.isShownToUser)
        XCTAssertEqual(remaining.clipboardNotice.shownCount, 0)
        try closing.session.write(Array("\n".utf8))
        try await waitUntil("the pane closes") { controller.panes.count == 1 }
        try await waitUntil("the notice shows on the pane left") { remaining.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(remaining.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
    }

    func testAHeldWriteOfAPaneThatExitsShowsOnThePaneLeft() async throws {
        let remaining = try openTab("echo kept")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        // Writes, then exits 0 at once: its pane closes.
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let closing = onScreen(try XCTUnwrap(controller.splitFocusedPane(.right)).view)
        // Behind the maximized pane left: the write is held, never shown.
        controller.focus(try XCTUnwrap(controller.panes.first { $0.view === remaining }))
        controller.toggleZoomPane(nil)
        XCTAssertTrue(controller.isZoomed)
        XCTAssertFalse(closing.isShownToUser)
        try await release(closing)
        try await waitUntil("the pane closes") { controller.panes.count == 1 }
        XCTAssertEqual(pasteboard.string(forType: .string), "hello")
        XCTAssertEqual(closing.clipboardNotice.shownCount, 0, "held in the closing pane")
        try await waitUntil("the notice shows on the pane left") { remaining.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(remaining.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
    }

    func testAClosedTabsWritesGoToItsTabBarBeforeAFrontWindow() async throws {
        let kept = try openTab("echo kept")
        let window = try XCTUnwrap(kept.window as? TerminalWindow)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "echo ready; read go; \(Self.hello)"])
        let closing = onScreen(try appDelegate.newTab(in: window).focusedPane.view)
        let closingWindow = try XCTUnwrap(closing.window)
        // Another window, in front of the closing tab's.
        let other = try openTab("echo other")
        let otherWindow = try XCTUnwrap(other.window)
        let order = NSApp.orderedWindows
        guard let otherIndex = order.firstIndex(of: otherWindow),
            let closingIndex = order.firstIndex(of: closingWindow), otherIndex < closingIndex
        else {
            throw XCTSkip("the host does not stack the windows as they open")
        }
        try await release(closing)
        try await waitUntil("the tab closes") { appDelegate.windowControllers.count == 2 }
        try await waitUntil("the notice shows on the tab left in its tab bar") { kept.clipboardNotice.shownCount == 1 }
        XCTAssertEqual(kept.clipboardNotice.lastShown, .init(writes: 1, bytes: 5, closedWrites: 1))
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(other.clipboardNotice.shownCount, 0, "not on the front window")
    }

    // MARK: Under a sheet (#763)

    func testANoticeWaitsForASheetOnItsTabToEnd() async throws {
        let view = try openTab("echo ready; read go; \(Self.hello)")
        let window = try XCTUnwrap(view.window)
        let sheet = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 500, height: 324), styleMask: [.titled],
            backing: .buffered, defer: false)
        sheet.isReleasedWhenClosed = false
        window.beginSheet(sheet) { _ in }
        try await waitUntil("the sheet is up") { window.attachedSheet === sheet }
        try await release(view)
        try await waitUntil("the write reaches the pasteboard") { pasteboard.string(forType: .string) == "hello" }
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(view.clipboardNotice.shownCount, 0, "held while the sheet covers the pane")
        window.endSheet(sheet)
        try await waitUntil("the notice shows as the sheet ends") { view.clipboardNotice.shownCount == 1 }
        XCTAssertTrue(view.clipboardNotice.isShowing)
        // A sheet that begins while the notice is up: it shows again after.
        window.beginSheet(sheet) { _ in }
        try await waitUntil("the sheet is up again") { window.attachedSheet === sheet }
        try await Task.sleep(for: ClipboardNoticeView.holdDuration + .milliseconds(500))
        window.endSheet(sheet)
        try await waitUntil("the sheet ends") { window.attachedSheet == nil }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertTrue(view.clipboardNotice.isShowing, "shown again, its full time")
        XCTAssertEqual(view.clipboardNotice.shownCount, 1, "the same notice")
    }

    // MARK: Bounds and lifetime

    func testAFloodOfWritesKeepsNoticesMemoryAndCloseBounded() async throws {
        // Warm-up: the process's first window sets up Metal and fonts.
        let warmUp = try openTab("echo warm")
        try await waitUntil("the warm-up shows output") { try warmUp.session.accessibleText().string.contains("warm") }
        warmUp.window?.close()
        let start = Self.physFootprint()
        let began = ContinuousClock.now
        let view = try openTab(
            #"i=0; while [ $i -lt 400000 ]; do printf '\033]52;c;aGVsbG8=\007'; i=$((i+1)); done; echo flood-done"#)
        let window = try XCTUnwrap(view.window)
        let deadline = ContinuousClock.now + .seconds(120)
        var worstLag = Duration.zero
        while try !view.session.accessibleText().string.contains("flood-done"), ContinuousClock.now < deadline {
            let before = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(10))
            worstLag = max(worstLag, ContinuousClock.now - before - .milliseconds(10))
        }
        let elapsed = ContinuousClock.now - began
        let growthMB = Double(Int64(Self.physFootprint()) - Int64(start)) / 1_048_576
        let shown = view.clipboardNotice.shownCount
        let seconds = Int(elapsed.components.seconds) + 2
        let closeStart = ContinuousClock.now
        window.close()
        let closeTime = ContinuousClock.now - closeStart
        print("osc52 flood: \(shown) notices in \(elapsed), growth \(Int(growthMB)) MB, lag \(worstLag), close \(closeTime)")
        XCTAssertGreaterThanOrEqual(shown, 1)
        XCTAssertLessThanOrEqual(shown, seconds, "at most one notice a second")
        XCTAssertLessThan(growthMB, 150, "footprint grew \(growthMB) MB")
        XCTAssertLessThan(worstLag, .milliseconds(500), "main actor lagged \(worstLag)")
        XCTAssertLessThan(closeTime, .seconds(3), "closing took \(closeTime)")
    }

    func testClosingATabWithANoticeHeldFreesItsViewControllerAndSession() async throws {
        weak var weakController: TerminalWindowController?
        weak var weakView: TerminalSurfaceView?
        weak var weakSession: TesseraSession?
        weak var weakNotice: ClipboardNoticeView?
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "\(Self.hello); sleep 0.2; \(Self.hi); exec cat"])
        try autoreleasepool {
            let controller = try appDelegate.newWindow()
            weakController = controller
            weakView = onScreen(controller.focusedPane.view)
            weakSession = controller.focusedPane.session
        }
        try await waitUntil("a notice shows") { weakView?.clipboardNotice.shownCount == 1 }
        weakNotice = weakView?.clipboardNotice.view
        try await Task.sleep(for: .milliseconds(400))
        XCTAssertNotNil(weakNotice)
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("everything is freed") {
            weakController == nil && weakView == nil && weakSession == nil && weakNotice == nil
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakSession, "session")
        XCTAssertNil(weakNotice, "notice view")
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
}
