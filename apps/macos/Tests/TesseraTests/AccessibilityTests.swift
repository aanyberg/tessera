import AppKit
import TesseraKit
import XCTest

@testable import Tessera

// MARK: Test doubles

/// Records what ``TerminalAccessibility`` posts, with the scheduler's time.
@MainActor
final class PostRecorder: AccessibilityPosting {
    struct Post {
        /// Weak, so recording a post never keeps a closed view alive.
        weak var element: AnyObject?
        var notification: NSAccessibility.Notification
        var userInfo: [NSAccessibility.NotificationUserInfoKey: Any]?
        var time: CFTimeInterval
    }

    private(set) var posts: [Post] = []
    var clock: () -> CFTimeInterval

    init(clock: @escaping () -> CFTimeInterval) {
        self.clock = clock
    }

    func post(
        element: Any, notification: NSAccessibility.Notification,
        userInfo: [NSAccessibility.NotificationUserInfoKey: Any]?
    ) {
        posts.append(Post(element: element as AnyObject, notification: notification, userInfo: userInfo, time: clock()))
    }

    var notifications: [NSAccessibility.Notification] { posts.map(\.notification) }

    func clear() { posts.removeAll() }

    /// The most posts in any half-open second `[t, t + 1)`.
    var mostPostsInASecond: Int {
        let times = posts.map(\.time)
        return times.indices.map { start in times[start...].prefix { $0 < times[start] + 1 }.count }.max() ?? 0
    }
}

/// A clock the test advances and the one-shot timers it runs.
@MainActor
final class ManualScheduler: AccessibilityScheduling {
    var now: CFTimeInterval = 1000
    private var pending: [(at: CFTimeInterval, work: @MainActor () -> Void)] = []
    var pendingCount: Int { pending.count }
    private(set) var mostPending = 0

    func schedule(after delay: CFTimeInterval, _ work: @escaping @MainActor () -> Void) {
        pending.append((now + delay, work))
        mostPending = max(mostPending, pending.count)
    }

    /// Moves the clock on by `interval`, running each timer due on the way
    /// at its time.
    func advance(by interval: CFTimeInterval) {
        let end = now + interval
        while let next = pending.indices.min(by: { pending[$0].at < pending[$1].at }), pending[next].at <= end {
            let timer = pending.remove(at: next)
            now = max(now, timer.at)
            timer.work()
        }
        now = end
    }
}

/// Stands in for the machine's VoiceOver.
@MainActor
final class FakeClientMonitor: AssistiveClientMonitoring {
    var isAttached: Bool {
        didSet { for handler in handlers { handler() } }
    }
    private var handlers: [@MainActor () -> Void] = []

    init(attached: Bool) {
        isAttached = attached
    }

    func observe(_ changed: @escaping @MainActor () -> Void) -> AnyObject {
        handlers.append(changed)
        return NSObject()
    }
}

private struct TimedOut: Error, CustomStringConvertible {
    var description: String
}

@MainActor
private func waitUntil(_ description: String, seconds: Int = 10, _ condition: () throws -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(seconds)
    while try !condition() {
        guard ContinuousClock.now < deadline else {
            XCTFail("timed out: \(description)")
            throw TimedOut(description: description)
        }
        try await Task.sleep(for: .milliseconds(10))
    }
}

// MARK: The text area (fixture rows A and C)

/// The NSAccessibility contract of the terminal view (#52, ADR 0041,
/// accessibility fixture rows A and C) on a view backed by a real session
/// of a core of the test's own, outside any window unless a row needs
/// one. The child prints the fixture's B1 grid with echo off and then
/// copies its input to its output.
@MainActor
final class TerminalAccessibilityViewTests: XCTestCase {
    private var app: TesseraApp!
    private var session: TesseraSession!
    private var view: TerminalSurfaceView!
    private var window: NSWindow?

    private static let b1 = "ls\na.txt  b.txt\n$ "

    override func setUp() async throws {
        app = try TesseraApp()
        session = try app.spawn(
            .init(
                program: "/bin/sh", arguments: ["-c", #"stty -echo -icanon; printf 'ls\r\na.txt  b.txt\r\n$ '; exec cat"#],
                rows: 6, cols: 20))
        view = TerminalSurfaceView(session: session)
        // No client: nothing is posted while the rows read the view.
        view.accessibility.monitor = FakeClientMonitor(attached: false)
        try await waitForValue(Self.b1)
    }

    override func tearDown() async throws {
        window?.close()
        window = nil
        view = nil
        session = nil
        app?.shutdown()
        app = nil
    }

    /// Output reaches a real view through the router; here the test tells
    /// the view, as the router would, until it reads `expected`.
    private func waitForValue(_ expected: String) async throws {
        try await waitUntil("the view reads \(expected.debugDescription)") {
            view.sessionDidOutput()
            return view.accessibilityValue() as? String == expected
        }
    }

    /// 8 x 16 pt cells at scale 1 with no padding, as fixture C assumes.
    private static let metrics = TesseraSurface.Metrics(
        widthPx: 160, heightPx: 96, scale: 1, cellWidthPx: 8, cellHeightPx: 16, baselinePx: 12, paddingPx: 0,
        rows: 6, cols: 20, fontSizePt: 13, cursorRow: 2, cursorCol: 2, cursorOnScreen: true, cursorWide: false)

    func testA0TheViewIsATextAreaLabelledWithoutATitle() {
        XCTAssertTrue(view.isAccessibilityElement())
        XCTAssertEqual(view.accessibilityRole(), .textArea)
        XCTAssertEqual(view.accessibilityLabel(), "terminal", "A0b: never empty")
        XCTAssertFalse(view.isAccessibilityFocused(), "not in a key window")
    }

    /// A0c, F2 (amended): the selected range is never settable; the value
    /// is settable (a no-op setter, for Full Keyboard Access's Space) only
    /// while VoiceOver is off, so QuickNav never sees a settable value.
    func testA0cTheValueIsSettableOnlyWithoutVoiceOverAndNothingSetTakesEffect() throws {
        let setValue = #selector(NSView.setAccessibilityValue(_:))
        let setRange = #selector(NSView.setAccessibilitySelectedTextRange(_:))
        XCTAssertNotEqual(
            TerminalSurfaceView.instanceMethod(for: setValue), NSView.instanceMethod(for: setValue),
            "the view has its own value setter")
        XCTAssertFalse(view.accessibility.monitor.isAttached)
        XCTAssertTrue(view.isAccessibilitySelectorAllowed(setValue), "VoiceOver off: settable for FKA")
        XCTAssertFalse(view.isAccessibilitySelectorAllowed(setRange))
        XCTAssertTrue(view.isAccessibilitySelectorAllowed(#selector(NSView.accessibilityValue)))

        view.setAccessibilityValue("rm -rf ~")
        XCTAssertEqual(view.accessibilityValue() as? String, Self.b1, "D-6: ignored")
        view.setAccessibilitySelectedTextRange(NSRange(location: 0, length: 2))
        XCTAssertFalse(try session.hasSelection, "D-6: ignored")
        XCTAssertEqual(view.accessibilitySelectedTextRange(), NSRange(location: 18, length: 0))

        view.accessibility.monitor = FakeClientMonitor(attached: true)
        XCTAssertFalse(view.isAccessibilitySelectorAllowed(setValue), "VoiceOver on: QuickNav")
        XCTAssertFalse(view.isAccessibilitySelectorAllowed(setRange))
        view.accessibility.monitor = FakeClientMonitor(attached: false)
        XCTAssertTrue(view.isAccessibilitySelectorAllowed(setValue), "VoiceOver off again")
    }

    func testA1ToA13TheValueAndItsLookups() throws {
        XCTAssertEqual(view.accessibilityValue() as? String, Self.b1)
        XCTAssertEqual(view.accessibilityNumberOfCharacters(), 18, "A1")
        XCTAssertEqual(view.accessibilityVisibleCharacterRange(), NSRange(location: 0, length: 18), "A2")
        XCTAssertEqual(view.accessibilityRange(forLine: 1), NSRange(location: 3, length: 13), "A3: with its newline")
        XCTAssertEqual(view.accessibilityRange(forLine: 2), NSRange(location: 16, length: 2), "A3: the last has none")
        XCTAssertEqual(view.accessibilityRange(forLine: 3), NSRange(location: NSNotFound, length: 0), "A4")
        XCTAssertEqual(view.accessibilityLine(for: 18), 2, "A5: the end is the last line")
        XCTAssertEqual(view.accessibilityLine(for: 500), 2, "A5: clamped")
        XCTAssertEqual(view.accessibilityString(for: NSRange(location: 3, length: 5)), "a.txt", "A6")
        XCTAssertEqual(view.accessibilityString(for: NSRange(location: 16, length: 100)), "$ ", "A6: clamped")
        XCTAssertNil(view.accessibilityString(for: NSRange(location: NSNotFound, length: 0)), "A6")
        // A6: any range a client sends is clamped, without overflowing.
        let edges: [(NSRange, String?)] = [
            (NSRange(location: 3, length: Int.max), "a.txt  b.txt\n$ "),
            (NSRange(location: 0, length: Int.max), Self.b1),
            (NSRange(location: 17, length: Int.max), " "),
            (NSRange(location: 18, length: Int.max), ""),
            (NSRange(location: Int.max - 1, length: Int.max), ""),
            (NSRange(location: -3, length: 2), "ls"),
            (NSRange(location: 3, length: -1), ""),
            (NSRange(location: Int.min, length: Int.max), Self.b1),
            (NSRange(location: NSNotFound, length: Int.max), nil),
        ]
        for (range, expected) in edges {
            XCTAssertEqual(view.accessibilityString(for: range), expected, "A6: \(range.location), \(range.length)")
        }
        XCTAssertEqual(view.accessibilityRange(for: 4), NSRange(location: 4, length: 1), "A7")
        XCTAssertEqual(view.accessibilitySelectedTextRange(), NSRange(location: 18, length: 0), "A10")
        XCTAssertEqual(view.accessibilitySelectedTextRanges(), [NSValue(range: NSRange(location: 18, length: 0))])
        XCTAssertEqual(view.accessibilitySelectedText(), "", "A12")
        XCTAssertEqual(view.accessibilityInsertionPointLineNumber(), 2, "A13")
        try session.selectBegin(col: 0, row: 1, granularity: .character, block: false)
        try session.selectExtend(col: 4, row: 1)
        view.sessionDidOutput()
        XCTAssertEqual(view.accessibilitySelectedTextRange(), NSRange(location: 3, length: 5), "B19")
        XCTAssertEqual(view.accessibilitySelectedText(), "a.txt")
    }

    func testTheValueFollowsOutputOnlyOnceTheViewIsTold() async throws {
        try session.write(Data("x".utf8))
        let deadline = ContinuousClock.now + .seconds(5)
        while try session.accessibleText().string != Self.b1 + "x", ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(view.accessibilityValue() as? String, Self.b1, "the snapshot is kept until a change")
        view.sessionDidOutput()
        XCTAssertEqual(view.accessibilityValue() as? String, Self.b1 + "x")
    }

    func testCTheCellRectsOfRanges() throws {
        func rect(_ location: Int, _ length: Int) throws -> CGRect {
            let text = try session.accessibleText()
            return TerminalSurfaceView.cellRect(
                text.cells(for: NSRange(location: location, length: length)), cellWidth: 8, cellHeight: 16,
                padding: 0)
        }
        XCTAssertEqual(try rect(3, 5), CGRect(x: 0, y: 16, width: 40, height: 16), "C1")
        XCTAssertEqual(try rect(0, 5), CGRect(x: 0, y: 0, width: 16, height: 32), "C2")
        XCTAssertEqual(try rect(18, 0), CGRect(x: 16, y: 32, width: 0, height: 16), "C6")
        XCTAssertEqual(try rect(0, 0), CGRect(x: 0, y: 0, width: 0, height: 16), "C7")
        XCTAssertEqual(try rect(17, 50), CGRect(x: 8, y: 32, width: 8, height: 16), "C8: clamped")
        let padded = TerminalSurfaceView.cellRect(
            .init(row: 1, col: 2, rows: 2, cols: 3), cellWidth: 8, cellHeight: 16, padding: 4)
        XCTAssertEqual(padded, CGRect(x: 20, y: 20, width: 24, height: 32))
    }

    func testC9FramesAreInScreenCoordinatesAndNeedAWindow() {
        view.metrics = Self.metrics
        XCTAssertEqual(view.accessibilityFrame(for: NSRange(location: 3, length: 5)), .zero, "no window")
        XCTAssertEqual(view.accessibilityRange(for: NSPoint(x: 1, y: 1)), NSRange(location: 0, length: 0))

        let window = NSWindow(
            contentRect: NSRect(x: 200, y: 300, width: 160, height: 96), styleMask: [.borderless], backing: .buffered,
            defer: false)
        window.isReleasedWhenClosed = false
        self.window = window
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 160, height: 96))
        window.contentView = container
        view.frame = container.bounds
        // The surface attaches in a window and reads its own metrics; the
        // rows here use the fixture's.
        container.addSubview(view)
        view.metrics = Self.metrics
        view.sessionDidOutput()

        let frame = view.accessibilityFrame(for: NSRange(location: 3, length: 5))
        // Row 1 of 6, 16 pt from the top of a 96 pt view: y 64 from the bottom.
        let expected = window.convertToScreen(view.convert(NSRect(x: 0, y: 64, width: 40, height: 16), to: nil))
        XCTAssertEqual(frame, expected, "C1 on screen")
        XCTAssertEqual(view.accessibilityFrame(for: NSRange(location: NSNotFound, length: 0)), .zero, "C8")

        // A8: the character under a point; above or left of the text, the start.
        let onB = window.convertToScreen(view.convert(NSRect(x: 7 * 8 + 1, y: 64 + 1, width: 0, height: 0), to: nil))
        XCTAssertEqual(view.accessibilityRange(for: onB.origin), NSRange(location: 10, length: 1), "the b of b.txt")
        let left = window.convertToScreen(view.convert(NSRect(x: -20, y: 64, width: 0, height: 0), to: nil))
        XCTAssertEqual(view.accessibilityRange(for: left.origin), NSRange(location: 0, length: 0))
        let right = window.convertToScreen(view.convert(NSRect(x: 150, y: 90, width: 0, height: 0), to: nil))
        XCTAssertEqual(view.accessibilityRange(for: right.origin), NSRange(location: 1, length: 1), "row 0's last")
    }
}

// MARK: Notifications (fixture rows D)

/// When and how ``TerminalAccessibility`` tells VoiceOver about output
/// (#52, accessibility fixture rows N1 to N18, docs/security.md S6), with a
/// fake client, a recorder for the posts and a clock the test advances. The
/// element is a plain view in a window; the snapshots come from a real
/// session whose child prints a prompt with echo off and copies its input.
@MainActor
final class TerminalAccessibilityNotificationTests: XCTestCase {
    private var app: TesseraApp!
    private var session: TesseraSession!
    private var element: NSView!
    private var window: NSWindow!
    private var scheduler: ManualScheduler!
    private var recorder: PostRecorder!
    private var monitor: FakeClientMonitor!
    private var accessibility: TerminalAccessibility!
    /// Snapshots the controller took.
    private var reads = 0

    override func setUp() async throws {
        app = try TesseraApp()
        session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "stty -echo -icanon; printf '$ '; exec cat"], rows: 6, cols: 20))
        try await waitForText("$ ")
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.borderless], backing: .buffered,
            defer: false)
        window.isReleasedWhenClosed = false
        element = NSView(frame: NSRect(x: 0, y: 0, width: 200, height: 100))
        window.contentView = element
        scheduler = ManualScheduler()
        recorder = PostRecorder { [unowned self] in scheduler.now }
        monitor = FakeClientMonitor(attached: false)
        let session = session!
        accessibility = TerminalAccessibility(
            textProvider: { [unowned self] in
                reads += 1
                return try session.accessibleText()
            }, monitor: monitor)
        accessibility.element = element
        accessibility.poster = recorder
        accessibility.scheduler = scheduler
    }

    override func tearDown() async throws {
        accessibility = nil
        window.close()
        window = nil
        session = nil
        app?.shutdown()
        app = nil
    }

    private func waitForText(_ expected: String) async throws {
        try await waitUntil("the session reads \(expected.debugDescription)") {
            try session.accessibleText().string == expected
        }
    }

    /// Attaches the client and lets its first `valueChanged` out.
    private func attach() {
        monitor.isAttached = true
        scheduler.advance(by: 0.01)
        XCTAssertEqual(recorder.notifications, [.valueChanged], "N3: one valueChanged on attach")
        XCTAssertTrue(recorder.posts.first?.element === element)
        recorder.clear()
        reads = 0
    }

    func testN2WithoutAClientNothingIsPostedOrRead() {
        for _ in 0..<1000 {
            accessibility.contentChanged()
            scheduler.advance(by: 0.001)
        }
        scheduler.advance(by: 5)
        XCTAssertEqual(reads, 0, "no accessible_text call")
        XCTAssertTrue(recorder.posts.isEmpty)
        XCTAssertEqual(scheduler.mostPending, 0, "no timer")
    }

    func testN3AttachPostsOnceAndDetachCancels() async throws {
        attach()
        try session.write(Data("one\n".utf8))
        try await waitForText("$ one\n")
        accessibility.contentChanged()
        monitor.isAttached = false
        scheduler.advance(by: 2)
        XCTAssertTrue(recorder.posts.isEmpty, "the pending settle was dropped")
        XCTAssertEqual(reads, 0)
        accessibility.contentChanged()
        scheduler.advance(by: 2)
        XCTAssertTrue(recorder.posts.isEmpty)
    }

    /// N3: output printed while VoiceOver was off is not announced when it
    /// comes back; attaching again only posts `valueChanged`.
    func testN3OutputWhileDetachedIsNotAnnouncedOnReattach() async throws {
        attach()
        monitor.isAttached = false
        try session.write(Data("unheard\n".utf8))
        try await waitForText("$ unheard\n")
        accessibility.contentChanged()
        scheduler.advance(by: 2)
        XCTAssertTrue(recorder.posts.isEmpty)
        monitor.isAttached = true
        scheduler.advance(by: 2)
        XCTAssertEqual(recorder.notifications, [.valueChanged], "no announcement of what was printed meanwhile")
    }

    /// N15 end to end: a resize (a split, a zoom, a window resize) reads
    /// none of the old output; output after it is announced.
    func testN15AResizeAnnouncesNothingButLaterOutputIs() async throws {
        let script =
            #"stty -echo -icanon; i=0; while [ $i -lt 40 ]; do printf 'line %02d some output text here\r\n' $i; i=$((i+1)); done; printf '$ '; exec cat"#
        let history = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", script], rows: 24, cols: 80))
        try await waitUntil("the history is printed") { try history.accessibleText().string.hasSuffix("$ ") }
        accessibility.textProvider = { try history.accessibleText() }
        attach()
        for (rows, cols) in [(UInt16(11), UInt16(80)), (24, 80), (20, 66), (21, 66)] {
            let before = try history.accessibleText().lineCount
            try history.resize(rows: rows, cols: cols)
            try await waitUntil("resized to \(rows)x\(cols)") { try history.accessibleText().lineCount != before }
            accessibility.contentChanged()
            scheduler.advance(by: 0.2)
            XCTAssertFalse(recorder.notifications.isEmpty, "\(rows)x\(cols): the change is told")
            XCTAssertFalse(
                recorder.notifications.contains(.announcementRequested), "\(rows)x\(cols): no old output read")
            recorder.clear()
            scheduler.advance(by: 1)
        }
        try history.write(Data("fresh\n".utf8))
        try await waitUntil("the output arrived") { try history.accessibleText().string.contains("fresh") }
        accessibility.contentChanged()
        scheduler.advance(by: 0.2)
        let announced = recorder.posts.first { $0.notification == .announcementRequested }
        XCTAssertEqual(announced?.userInfo?[.announcement] as? String, "$ fresh")
    }

    func testN1N6N7N9OutputIsAnnouncedFirstOnceItSettles() async throws {
        attach()
        try session.write(Data("one\ntwo\n".utf8))
        try await waitForText("$ one\ntwo\n")
        accessibility.contentChanged()
        scheduler.advance(by: 0.1)
        accessibility.contentChanged()
        scheduler.advance(by: 0.1)
        XCTAssertTrue(recorder.posts.isEmpty, "N5: re-armed by the second change")
        scheduler.advance(by: 0.06)
        XCTAssertEqual(recorder.notifications, [.announcementRequested, .valueChanged, .selectedTextChanged], "N7")
        XCTAssertEqual(reads, 1, "one snapshot per settle")
        let announcement = try XCTUnwrap(recorder.posts.first)
        XCTAssertTrue(announcement.element === NSApp, "N9: to the app, not the view")
        XCTAssertEqual(announcement.userInfo?[.announcement] as? String, "$ one\ntwo")
        XCTAssertEqual(
            announcement.userInfo?[.priority] as? Int, NSAccessibilityPriorityLevel.medium.rawValue, "D-4")
        XCTAssertTrue(recorder.posts.dropFirst().allSatisfy { $0.element === element })
        XCTAssertEqual(recorder.posts[1].time - 1000, 0.26, accuracy: 0.001, "0.15 s after the last change")
    }

    func testN17ACursorThatOnlyMovesPostsSelectedTextChangedAlone() async throws {
        try session.write(Data("abc".utf8))
        try await waitForText("$ abc")
        attach()
        try session.write(Data("\u{1b}[D".utf8))
        try await waitUntil("the cursor moved") { try session.accessibleText().insertionPoint == 4 }
        accessibility.contentChanged()
        scheduler.advance(by: 0.2)
        XCTAssertEqual(recorder.notifications, [.selectedTextChanged])
        accessibility.contentChanged()
        scheduler.advance(by: 0.2)
        XCTAssertEqual(recorder.notifications, [.selectedTextChanged], "nothing changed: nothing posted")
    }

    func testN18AnElementOutOfItsWindowPostsNothing() async throws {
        attach()
        try session.write(Data("one\n".utf8))
        try await waitForText("$ one\n")
        accessibility.contentChanged()
        window.contentView = nil
        scheduler.advance(by: 1)
        XCTAssertTrue(recorder.posts.isEmpty)
        XCTAssertEqual(reads, 0, "not even read")
    }

    /// Two snapshots the provider alternates between, so every settle has
    /// something to tell.
    private func alternatingSnapshots() async throws {
        let prompt = try session.accessibleText()
        try session.write(Data("one\ntwo\n".utf8))
        try await waitForText("$ one\ntwo\n")
        let output = try session.accessibleText()
        var flip = false
        accessibility.textProvider = { [unowned self] in
            reads += 1
            flip.toggle()
            return flip ? output : prompt
        }
    }

    func testN4BurstsStayWithinTenPostsASecond() async throws {
        try await alternatingSnapshots()
        attach()
        // Short bursts just over the settle delay apart: unthrottled, about
        // six settles of three posts a second.
        for _ in 0..<20 {
            for _ in 0..<10 {
                accessibility.contentChanged()
                scheduler.advance(by: 0.001)
            }
            scheduler.advance(by: 0.16)
        }
        scheduler.advance(by: 3)
        XCTAssertLessThanOrEqual(recorder.mostPostsInASecond, TerminalAccessibility.postsPerSecond)
        XCTAssertGreaterThanOrEqual(recorder.posts.count, 20, "it kept telling")
        XCTAssertFalse(recorder.notifications.contains(.layoutChanged), "N8")
        XCTAssertLessThanOrEqual(scheduler.mostPending, 1, "one timer at a time")
    }

    func testN5AFloodIsToldOnceASecondAndOnceAfterItStops() async throws {
        try await alternatingSnapshots()
        attach()
        let start = scheduler.now
        // 10 000 callbacks a second for 3.5 s.
        for _ in 0..<35_000 {
            accessibility.contentChanged()
            scheduler.advance(by: 0.0001)
        }
        let last = scheduler.now
        scheduler.advance(by: 2)
        XCTAssertLessThanOrEqual(recorder.mostPostsInASecond, TerminalAccessibility.postsPerSecond)
        let settles = recorder.posts.filter { $0.notification == .valueChanged }.map(\.time)
        XCTAssertEqual(settles.count, 4, "after 1, 2 and 3 s of output, then after it stops")
        XCTAssertEqual(settles.first.map { $0 - start } ?? 0, 1, accuracy: 0.001, "the longest wait")
        XCTAssertEqual(settles.last.map { $0 - last } ?? 0, 0.15, accuracy: 0.001, "N5: the trailing edge")
        XCTAssertEqual(reads, 4, "one snapshot per settle, none per callback")
        XCTAssertLessThanOrEqual(scheduler.mostPending, 1)
    }
}

// MARK: The view and the app

/// The terminal view's own accessibility through the app: output reaches
/// ``TerminalAccessibility``, the label is the sanitised pane title, a
/// real flood is bounded, and closing a tab releases everything
/// (docs/security.md S3, S6).
@MainActor
final class TerminalAccessibilityAppTests: XCTestCase {
    private var appDelegate: AppDelegate!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
    }

    func testS3TheLabelIsTheSanitisedPaneTitle() async throws {
        // A title with a right-to-left override in it.
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", #"printf '\033]2;ev\342\200\256il\007'; exec cat"#])
        let controller = try appDelegate.newWindow()
        let pane = controller.focusedPane
        try await waitUntil("the title arrived") { !pane.title.isEmpty }
        let label = try XCTUnwrap(pane.view.accessibilityLabel())
        XCTAssertEqual(label, pane.title)
        XCTAssertFalse(label.unicodeScalars.contains { $0.value == 0x202E }, "S3")
        XCTAssertTrue(label.hasPrefix("ev"))
    }

    func testOutputFromTheRouterReachesTheController() async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "stty -echo -icanon; exec cat"])
        let controller = try appDelegate.newWindow()
        let view = controller.focusedPane.view
        let scheduler = ManualScheduler()
        let recorder = PostRecorder { scheduler.now }
        view.accessibility.scheduler = scheduler
        view.accessibility.poster = recorder
        // The view started with the machine's VoiceOver, which may be on
        // and may have a settle pending on the main queue: detached first,
        // so attaching is a change whatever the machine says. The first
        // settle takes the snapshot later output is announced against.
        view.accessibility.monitor = FakeClientMonitor(attached: false)
        view.accessibility.monitor = FakeClientMonitor(attached: true)
        view.accessibility.contentChanged()
        try await waitUntil("the first settle") {
            scheduler.advance(by: 0.2)
            return !recorder.notifications.isEmpty
        }
        XCTAssertEqual(recorder.notifications, [.valueChanged], "the first settle only tells the client to read")
        recorder.clear()
        try controller.focusedPane.session.write(Data("hello\n".utf8))
        try await waitUntil("the output was told") {
            scheduler.advance(by: 0.2)
            return recorder.notifications.contains(.announcementRequested)
        }
        XCTAssertEqual(
            recorder.posts.first { $0.notification == .announcementRequested }?.userInfo?[.announcement] as? String,
            "hello")
    }

    /// `seq` floods a view whose (fake) VoiceOver is attached, on the real
    /// clock and timers: at most ten posts in any second, memory flat, and
    /// the tab closes at once and releases its view, controller and
    /// session.
    func testAFloodIsBoundedAndClosingReleasesEverything() async throws {
        appDelegate.spawnOptions = .init(program: "/usr/bin/seq", arguments: ["1", "1000000000"])
        weak var weakController: TerminalWindowController?
        weak var weakView: TerminalSurfaceView?
        weak var weakAccessibility: TerminalAccessibility?
        weak var weakSession: TesseraSession?
        let recorder = PostRecorder { CACurrentMediaTime() }
        try autoreleasepool {
            let controller = try appDelegate.newWindow()
            weakController = controller
            let view = controller.focusedPane.view
            weakView = view
            weakAccessibility = view.accessibility
            weakSession = controller.focusedPane.session
            view.accessibility.poster = recorder
            view.accessibility.monitor = FakeClientMonitor(attached: true)
            // As AppKit does for the key window's first responder.
            view.inputContext?.activate()
        }
        // The first seconds after the window opens add one-off steps: a
        // runner that renders the key window builds drawables and glyph
        // atlases (40 to 45 MB seen on CI), and the host frees what opening
        // the window used (about 165 MB, locally about 1 s in). So the
        // baseline waits, sampling every 100 ms, until the footprint levels
        // off: at most 8 MiB between the highest and lowest sample of the
        // last 2 s, after 2 s and before 8 s. Growth is then the median of
        // the last second, 3 s later, less the median of the baseline's last
        // second, which a passing spike or drop does not move and a steady
        // leak does. The bound is a rate, growth faster than about
        // 11 MiB/s: with about one settle (two posts) a second, 16 MiB
        // leaked per snapshot or 8 MiB per post fails it, and 8 MiB per
        // snapshot or 4 MiB per post can pass (Debug, TSan, key windows).
        let second = 10
        var samples: [Int] = []
        func sample() async throws {
            try await Task.sleep(for: .milliseconds(100))
            samples.append(Self.footprint())
        }
        func spread(_ last: ArraySlice<Int>) -> Int { (last.max() ?? 0) - (last.min() ?? 0) }
        func median(_ last: ArraySlice<Int>) -> Int { last.sorted()[last.count / 2] }
        repeat {
            try await sample()
        } while samples.count < 8 * second
            && (samples.count < 2 * second || spread(samples.suffix(2 * second)) > 8 << 20)
        let warmUp = samples.count
        let footprintBefore = median(samples.suffix(second))
        for _ in 0..<(3 * second) {
            try await sample()
        }
        let footprintAfter = median(samples.suffix(second))
        // On a pass too, for the steps a runner shows.
        print(
            "A11Y-FLOOD-FOOTPRINT: warm-up \(warmUp) samples; MiB "
                + samples.map { String(format: "%.1f", Double($0) / 1_048_576) }.joined(separator: " "))
        XCTAssertGreaterThan(recorder.posts.count, 2, "the flood was told")
        XCTAssertLessThanOrEqual(recorder.mostPostsInASecond, TerminalAccessibility.postsPerSecond, "S6")
        // Two snapshots of a 24 x 80 view are a few kB; the bound is loose
        // for the allocator.
        XCTAssertLessThan(footprintAfter - footprintBefore, 32 << 20, "memory stays flat")
        let closing = ContinuousClock.now
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the tab is freed") {
            weakController == nil && weakView == nil && weakAccessibility == nil && weakSession == nil
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakAccessibility, "accessibility")
        XCTAssertNil(weakSession, "session")
        let latency = ContinuousClock.now - closing
        print("accessibility flood: \(recorder.posts.count) posts, footprint +\((footprintAfter - footprintBefore) >> 10) KiB in 3 s, freed \(latency) after close")
        XCTAssertLessThan(latency, .seconds(3), "closing is not held up by the flood")
        let quiet = recorder.posts.count
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(recorder.posts.count, quiet, "nothing after the close")
    }

    private static func footprint() -> Int {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
        let status = withUnsafeMutablePointer(to: &info) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
            }
        }
        return status == KERN_SUCCESS ? Int(info.phys_footprint) : 0
    }
}

// MARK: Display options (fixture rows E)

/// Reduce Motion on the scroll bar (fixture E10 to E12): a flash shows at
/// once and hides after the hold, with no animation, and a change during a
/// flash settles it. An overlay scroller with history, on the test's clock
/// and hide timer.
@MainActor
final class ReduceMotionScrollerTests: XCTestCase {
    private var scroller: TerminalScroller!
    private var now: CFTimeInterval = 100
    private var hides: [(at: CFTimeInterval, run: @MainActor () -> Void)] = []

    override func setUp() async throws {
        scroller = TerminalScroller()
        // Off unless a test turns it on, whatever the machine's setting.
        scroller.reducesMotion = false
        scroller.preferredStyleOverride = .overlay
        scroller.now = { [unowned self] in now }
        scroller.scheduleHide = { [unowned self] delay, hide in hides.append((now + delay, hide)) }
        scroller.update(.init(offset: 0, history: 100, rows: 24))
    }

    /// Moves the clock on, running the hides due.
    private func advance(by interval: CFTimeInterval) {
        let end = now + interval
        while let index = hides.indices.min(by: { hides[$0].at < hides[$1].at }), hides[index].at <= end {
            let hide = hides.remove(at: index)
            now = hide.at
            hide.run()
        }
        now = end
    }

    private var flashAnimation: CAAnimation? { scroller.layer?.animation(forKey: "flash") }

    func testE10AFlashUnderReduceMotionIsNotAnimated() {
        scroller.reducesMotion = true
        scroller.flash()
        XCTAssertNil(flashAnimation, "no keyframe animation")
        XCTAssertTrue(scroller.isRevealed)
        XCTAssertEqual(scroller.alphaValue, 1)
        XCTAssertEqual(scroller.layer?.opacity, 1)
        advance(by: TerminalScroller.holdDuration - 0.01)
        XCTAssertEqual(scroller.alphaValue, 1, "held")
        advance(by: 0.02)
        XCTAssertFalse(scroller.isRevealed)
        XCTAssertEqual(scroller.alphaValue, 0, "hidden after the hold")
        XCTAssertNil(flashAnimation, "again with no animation")
    }

    func testAFlashDuringAFlashMovesTheHideWithOneTimer() {
        scroller.reducesMotion = true
        scroller.flash()
        advance(by: 0.8)
        scroller.flash()
        XCTAssertEqual(hides.count, 1, "one hide pending")
        advance(by: 0.5)
        XCTAssertEqual(scroller.alphaValue, 1, "the second flash holds it")
        advance(by: 0.6)
        XCTAssertEqual(scroller.alphaValue, 0)
        XCTAssertTrue(hides.isEmpty)
    }

    func testE10WithoutReduceMotionTheFlashFades() {
        scroller.reducesMotion = false
        scroller.flash()
        XCTAssertTrue(flashAnimation is CAKeyframeAnimation, "the regression guard")
        XCTAssertEqual(scroller.alphaValue, 0, "the animation shows it")
        XCTAssertTrue(hides.isEmpty)
    }

    func testE12TurningReduceMotionOnDuringAFadeSettles() {
        scroller.flash()
        advance(by: 0.5)
        scroller.reducesMotion = true
        XCTAssertNil(flashAnimation, "the animation is removed")
        XCTAssertEqual(scroller.alphaValue, 1, "shown until the flash would have ended")
        advance(by: TerminalScroller.holdDuration + TerminalScroller.fadeDuration - 0.5 + 0.01)
        XCTAssertEqual(scroller.alphaValue, 0)
        XCTAssertFalse(scroller.isRevealed)
    }

    func testTurningReduceMotionOffDuringAFlashStillHidesIt() {
        scroller.reducesMotion = true
        scroller.flash()
        advance(by: 0.3)
        scroller.reducesMotion = false
        XCTAssertEqual(scroller.alphaValue, 1)
        advance(by: 1)
        XCTAssertEqual(scroller.alphaValue, 0)
    }
}

/// The app hands the system's Increase Contrast to the core with Reduce
/// Transparency and its Reduce Motion to the scroll bars (fixture E8, E9,
/// E14), read through the ``AppDelegate`` seams (the test's, never the
/// machine's settings), on the test's configuration.
@MainActor
final class DisplayOptionsSettingTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?
    private var contrast = false
    private var motion = false

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { false }
        appDelegate.increasesContrast = { [unowned self] in contrast }
        appDelegate.reducesMotion = { [unowned self] in motion }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-contrast-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute configuration directory")
        try "[appearance]\ndim_inactive_panes = 0.4\n".write(
            to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        try appDelegate.replaceCore(start: false)
        appDelegate.spawnOptions = .init()
        HostDisplaySettings.pin(appDelegate)
        appDelegate = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func postSettingChange() {
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
    }

    func testIncreaseContrastStopsThePaneDimmingAndUnchangedSettingsMakeNoCall() async throws {
        try appDelegate.replaceCore(start: true)
        let controller = try appDelegate.newWindow()
        XCTAssertEqual(appDelegate.paneDimming, 0.4, accuracy: 1e-6)
        XCTAssertEqual(controller.paneContainer.inactivePaneDimming, 0.4, accuracy: 1e-6)
        let calls = appDelegate.displayOptionsCallCount

        postSettingChange()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(appDelegate.displayOptionsCallCount, calls, "E9: nothing changed, no call")

        contrast = true
        postSettingChange()
        try await waitUntil("the dimming is off") { appDelegate.paneDimming == 0 }
        XCTAssertEqual(controller.paneContainer.inactivePaneDimming, 0, "E14")
        XCTAssertEqual(appDelegate.displayOptionsCallCount, calls + 1)

        contrast = false
        postSettingChange()
        try await waitUntil("the dimming is back") { appDelegate.paneDimming > 0.39 }
        XCTAssertEqual(appDelegate.displayOptionsCallCount, calls + 2)
    }

    func testOnlyTheWorkspaceCentreIsObserved() async throws {
        try appDelegate.replaceCore(start: true)
        let calls = appDelegate.displayOptionsCallCount
        contrast = true
        NotificationCenter.default.post(name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(appDelegate.displayOptionsCallCount, calls, "E2: the default centre is not AppKit's")
        postSettingChange()
        try await waitUntil("applied") { appDelegate.displayOptionsCallCount == calls + 1 }
    }

    func testReduceMotionReachesEveryScrollBarAndNewOnes() async throws {
        try appDelegate.replaceCore(start: true)
        let first = try appDelegate.newWindow()
        XCTAssertFalse(first.focusedPane.view.scroller.reducesMotion)
        motion = true
        postSettingChange()
        try await waitUntil("the scroll bar follows") { first.focusedPane.view.scroller.reducesMotion }
        let second = try appDelegate.newWindow()
        XCTAssertTrue(second.focusedPane.view.scroller.reducesMotion, "a new view starts with it")
        motion = false
        postSettingChange()
        try await waitUntil("both follow") {
            !first.focusedPane.view.scroller.reducesMotion && !second.focusedPane.view.scroller.reducesMotion
        }
    }
}
