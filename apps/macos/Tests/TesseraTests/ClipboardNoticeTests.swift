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

    /// A tab running `script` (sh), then `cat`.
    private func openTab(_ script: String) throws -> TerminalSurfaceView {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", script + "; exec cat"])
        return try appDelegate.newWindow().focusedPane.view
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
            weakView = controller.focusedPane.view
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
