import AppKit
import Synchronization
import TesseraKit
@preconcurrency import UserNotifications
import XCTest

@testable import Tessera

/// Records what the notifier posts and removes.
@MainActor
private final class RecordingPoster: CommandNotificationPosting {
    var posts: [CommandNotification] = []
    var removed: [[String]] = []
    var removedAll = 0

    func post(_ notification: CommandNotification) { posts.append(notification) }
    func removeDelivered(identifiers: [String]) { removed.append(identifiers) }
    func removeAllDelivered() { removedAll += 1 }
}

/// A notification centre that answers as told and records every call.
private final class FakeCenter: UserNotificationCentering {
    struct State {
        var status: UNAuthorizationStatus = .notDetermined
        /// The status once the request is answered.
        var answer: UNAuthorizationStatus = .authorized
        var requestError: NSError?
        var addError: NSError?
        var requests: [UNAuthorizationOptions] = []
        var adds: [CommandNotification] = []
        var delegates = 0
    }

    let state: Mutex<State>

    init(_ configure: (inout State) -> Void = { _ in }) {
        var initial = State()
        configure(&initial)
        state = Mutex(initial)
    }

    func authorizationStatus() async -> UNAuthorizationStatus { state.withLock { $0.status } }

    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool {
        state.withLock { $0.requests.append(options) }
        // The prompt is up for a while: posts made meanwhile wait (U03).
        try await Task.sleep(for: .milliseconds(100))
        return try state.withLock {
            if let error = $0.requestError { throw error }
            $0.status = $0.answer
            return $0.answer == .authorized
        }
    }

    func add(_ notification: CommandNotification) async throws {
        try state.withLock {
            if let error = $0.addError { throw error }
            $0.adds.append(notification)
        }
    }

    func removeDeliveredNotifications(withIdentifiers identifiers: [String]) {}
    func removeAllDeliveredNotifications() {}
    func setDelegate(_ delegate: NotificationDelegate) { state.withLock { $0.delegates += 1 } }
}

/// Command-finished notifications (#470, ADR 0048): the text (T rows), the
/// permission (U rows), the focus (F rows), the marks (D8) and the trigger
/// as the app runs it (A rows).
@MainActor
final class CommandNotifierTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var notifier: CommandNotifier { appDelegate.commandNotifier }
    private var poster: RecordingPoster!
    private var savedPoster: (any CommandNotificationPosting)!
    private var savedFocus: CommandNotifier.Focus!
    private var savedActivate: (@MainActor () -> Void)!
    private var savedOnCommandFinished: ((TesseraSession, TesseraCommandFinish, TerminalWindowController) -> Void)?
    private var activations = 0
    /// What a delegate's clicks opened.
    private var opened: [UInt64?] = []
    /// Every finish the router delivered, by session.
    private var finishes: [(UInt64, TesseraCommandFinish)] = []
    private var previous: String?
    private var dir: URL?
    /// The app is active (the seam's; no test reads the real state).
    private var isActive = true

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        poster = RecordingPoster()
        savedPoster = notifier.poster
        savedFocus = notifier.focus
        savedActivate = notifier.activateApplication
        notifier.poster = poster
        // A window is key when it is its tab group's selected one (the
        // hosted console has no key window, H1).
        notifier.focus = .init(
            isApplicationActive: { [unowned self] in isActive },
            isKeyWindow: { window in window.tabGroup.map { $0.selectedWindow === window } ?? true })
        notifier.activateApplication = { [unowned self] in activations += 1 }
        let original = appDelegate.router.onCommandFinished
        savedOnCommandFinished = original
        appDelegate.router.onCommandFinished = { [unowned self] session, finish, controller in
            finishes.append((session.id, finish))
            original?(session, finish, controller)
        }
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.router.onCommandFinished = savedOnCommandFinished
        notifier.poster = savedPoster
        notifier.focus = savedFocus
        notifier.activateApplication = savedActivate
        appDelegate.spawnOptions = .init()
        if dir != nil {
            try appDelegate.replaceCore(start: false)
        }
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        appDelegate = nil
    }

    // MARK: Helpers

    /// Waits up to `seconds` for `condition`.
    private func waitUntil(_ what: String, seconds: Int = 15, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(seconds)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// Gives the app a core of its own on `config`.
    private func startCore(config: String) throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-notify-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        try appDelegate.replaceCore(start: true)
    }

    private static func config(_ mode: String) -> String {
        "[notifications]\ncommand_finished = \"\(mode)\"\ncommand_finished_after = 1\n"
    }

    /// A shell that runs one command of a real two seconds, marked by OSC
    /// 133 C and D;0 as shell integration marks it, then echoes input.
    private static let longCommand = TesseraSession.SpawnOptions(
        program: "/bin/sh", arguments: ["-c", #"printf '\033]133;C\007'; sleep 2; printf '\033]133;D;0\007'; exec cat"#])

    /// Tab 1 (cat), then tab 2 opened by the New Tab action running
    /// `tab2`, then tab 1 selected again.
    private func openTwoTabs(tab2: TesseraSession.SpawnOptions) throws -> (TerminalWindowController, TerminalWindowController, NSWindowTabGroup) {
        let first = try appDelegate.newWindow()
        appDelegate.spawnOptions = tab2
        first.newWindowForTab(nil)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        let second = try XCTUnwrap(appDelegate.windowControllers.last)
        XCTAssertFalse(second === first)
        let group = try XCTUnwrap(first.window?.tabGroup)
        XCTAssertEqual(group.windows.count, 2)
        group.selectedWindow = first.window
        return (first, second, group)
    }

    /// Calls the notifier as the router does, recording what it asked.
    private func finish(
        _ pane: Pane, in controller: TerminalWindowController, exitCode: Int32? = 0, milliseconds: UInt64 = 12_000,
        answer: TesseraCommandFinishedDecision = .markAndPost
    ) -> Bool? {
        var focused: Bool?
        notifier.commandDidFinish(
            TesseraCommandFinish(exitCode: exitCode, durationMilliseconds: milliseconds), of: pane.session,
            in: controller
        ) { duration, isFocused in
            XCTAssertEqual(duration, milliseconds)
            focused = isFocused
            return answer
        }
        return focused
    }

    private func split(_ controller: TerminalWindowController) throws -> (left: Pane, right: Pane) {
        controller.window?.setContentSize(NSSize(width: 1200, height: 800))
        controller.paneContainer.layoutSubtreeIfNeeded()
        let left = controller.focusedPane
        let right = try XCTUnwrap(controller.splitFocusedPane(.right), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        return (left, right)
    }

    // MARK: Text (T rows)

    func testTheTitleAndBodySayHowItEndedAndHowLongItTook() {
        typealias T = CommandNotificationText
        XCTAssertEqual(T.title(exitCode: 0), "Command succeeded")
        XCTAssertEqual(T.title(exitCode: 1), "Command failed")
        XCTAssertEqual(T.title(exitCode: 130), "Command failed")
        XCTAssertEqual(T.title(exitCode: -1), "Command failed")
        XCTAssertEqual(T.title(exitCode: nil), "Command finished")
        XCTAssertEqual(T.body(durationMilliseconds: 2_345, exitCode: 0), "Took 2s, exit status 0")
        XCTAssertEqual(T.body(durationMilliseconds: 61_000, exitCode: nil), "Took 1m 1s")
        XCTAssertEqual(T.body(durationMilliseconds: 12_000, exitCode: 130), "Took 12s, exit status 130")
        for (milliseconds, text) in [
            (0, "0s"), (999, "0s"), (1_000, "1s"), (59_999, "59s"), (60_000, "1m"), (3_600_000, "1h"),
            (3_725_000, "1h 2m 5s"), (3_605_000, "1h 5s"), (90_000_000, "25h"),
        ] as [(UInt64, String)] {
            XCTAssertEqual(T.duration(milliseconds: milliseconds), text, "\(milliseconds) ms")
        }
        XCTAssertFalse(T.duration(milliseconds: .max).isEmpty, "the longest duration formats")
    }

    func testTheSubtitleIsSanitisedAndCappedOnACharacterBoundary() {
        typealias T = CommandNotificationText
        XCTAssertEqual(T.subtitle("make test"), "make test")
        XCTAssertEqual(T.subtitle("a\nb\r\u{7}c\u{1B}[31m"), "abc[31m", "C0 controls go")
        XCTAssertEqual(T.subtitle("x\u{7F}\u{85}\u{9B}y"), "xy", "DEL and C1 go")
        XCTAssertEqual(T.subtitle("\u{202E}txt.exe\u{2066}\u{200F}\u{061C}"), "txt.exe", "bidi formatting goes")
        XCTAssertEqual(T.subtitle("one\u{2028}two\u{2029}"), "onetwo")
        XCTAssertEqual(T.subtitle("café ✓"), "café ✓", "printable text stays")
        for blank in ["", "   ", "\n\t", "\u{200B}", "\u{0301}", "\u{FEFF}\u{00A0}", "\u{202E}\u{2066}"] {
            XCTAssertNil(T.subtitle(blank), "nothing visible: \(blank.debugDescription)")
        }
        let cap = TesseraApp.maxNotificationTextBytes
        XCTAssertEqual(cap, 256)
        XCTAssertEqual(T.subtitle(String(repeating: "a", count: 300))?.utf8.count, cap)
        XCTAssertEqual(T.subtitle(String(repeating: "é", count: 200)), String(repeating: "é", count: 128))
        XCTAssertEqual(T.subtitle(String(repeating: "€", count: 100)), String(repeating: "€", count: 85))
        let family = "👨‍👩‍👧"
        XCTAssertEqual(family.utf8.count, 18)
        XCTAssertEqual(T.subtitle(String(repeating: family, count: 20)), String(repeating: family, count: 14))
        XCTAssertEqual(T.subtitle(String(repeating: "a", count: 256)), String(repeating: "a", count: 256))
        XCTAssertEqual(T.subtitle(String(repeating: "\n", count: 10_000) + "x"), "x", "removed text is not counted")
    }

    func testTheContentCarriesTheSessionIdAloneAndNoSound() {
        let notification = CommandNotification(
            identifier: CommandNotification.identifier(for: 42), sessionID: 42, title: "Command failed",
            subtitle: "make", body: "Took 12s, exit status 2")
        let content = notification.content()
        XCTAssertEqual(content.title, "Command failed")
        XCTAssertEqual(content.subtitle, "make")
        XCTAssertEqual(content.body, "Took 12s, exit status 2")
        XCTAssertNil(content.sound, "D9")
        XCTAssertEqual(content.threadIdentifier, "tessera.session.42")
        XCTAssertEqual(content.userInfo.keys.map { $0 as? String }, ["session"], "T24: no text in userInfo")
        XCTAssertEqual(CommandNotification.sessionID(from: content.userInfo), 42)
        XCTAssertNil(CommandNotification.sessionID(from: [:]))
        XCTAssertNil(CommandNotification.sessionID(from: ["session": "42"]), "only a number is an id")
        XCTAssertEqual(CommandNotification.identifier(for: 42), "tessera.command-finished.42")
        let untitled = CommandNotification(
            identifier: "i", sessionID: 1, title: "Command finished", subtitle: nil, body: "Took 1s")
        XCTAssertEqual(untitled.content().subtitle, "")
    }

    // MARK: Permission (U rows)

    private func drain(_ poster: UserNotificationPoster) async throws {
        try await waitUntil("the posts ended") { poster.postsInFlight == 0 }
    }

    private static func notification(_ id: UInt64) -> CommandNotification {
        .init(
            identifier: CommandNotification.identifier(for: id), sessionID: id, title: "Command succeeded",
            subtitle: nil, body: "Took 2s, exit status 0")
    }

    /// U02, U03, U13: installing asks nothing; the first post asks once
    /// for alerts only; posts made while it is asked post when it is
    /// answered; later posts never ask.
    func testTheFirstPostAsksOnceAndLaterPostsNever() async throws {
        let center = FakeCenter()
        let poster = UserNotificationPoster(center: center)
        poster.install(NotificationDelegate { _ in })
        XCTAssertEqual(center.state.withLock { $0.delegates }, 1)
        XCTAssertEqual(center.state.withLock { $0.requests }, [], "U02: nothing asked at launch")
        poster.post(Self.notification(1))
        poster.post(Self.notification(2))
        try await drain(poster)
        XCTAssertEqual(center.state.withLock { $0.requests }, [[.alert]], "U03: one request, alerts only")
        XCTAssertEqual(center.state.withLock { $0.adds }.map(\.sessionID).sorted(), [1, 2], "both posted once answered")
        poster.post(Self.notification(3))
        try await drain(poster)
        XCTAssertEqual(center.state.withLock { $0.requests }.count, 1, "never again")
        XCTAssertEqual(center.state.withLock { $0.adds }.map(\.sessionID).last, 3)
        XCTAssertEqual(center.state.withLock { $0.adds }.count, 3)
        XCTAssertEqual(poster.loggedProblems, 0)
    }

    /// D10, U03: a pane seen while the first permission prompt is up
    /// (its mark cleared, its notification removed) posts nothing once the
    /// prompt is answered; another pane's waiting post still posts.
    func testAPaneSeenWhileThePromptIsUpPostsNothingAfterIt() async throws {
        let center = FakeCenter()
        let real = UserNotificationPoster(center: center)
        notifier.poster = real
        let (first, second, group) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        let (left, right) = try split(first)
        first.focus(left)
        _ = finish(second.focusedPane, in: second)
        _ = finish(right, in: first)
        XCTAssertTrue(second.isCommandMarked(second.focusedPane))
        try await waitUntil("the prompt is up") { center.state.withLock { $0.requests.count } == 1 }
        group.selectedWindow = second.window
        XCTAssertTrue(second.markedSessionIDs.isEmpty, "seen while the prompt is up")
        try await drain(real)
        XCTAssertEqual(center.state.withLock { $0.adds }.map(\.sessionID), [right.session.id])
        XCTAssertEqual(first.markedSessionIDs, [right.session.id])
    }

    /// U04: a refusal posts nothing, logs once and never asks again.
    func testARefusalLogsOnceAndNeverAsksAgain() async throws {
        let center = FakeCenter { $0.answer = .denied }
        let poster = UserNotificationPoster(center: center)
        for id in UInt64(1)...3 { poster.post(Self.notification(id)) }
        try await drain(poster)
        poster.post(Self.notification(4))
        try await drain(poster)
        XCTAssertEqual(center.state.withLock { $0.requests }.count, 1)
        XCTAssertEqual(center.state.withLock { $0.adds }, [])
        XCTAssertEqual(poster.loggedProblems, 1, "logged once")

        let denied = FakeCenter { $0.status = .denied }
        let refused = UserNotificationPoster(center: denied)
        for id in UInt64(1)...3 { refused.post(Self.notification(id)) }
        try await drain(refused)
        XCTAssertEqual(denied.state.withLock { $0.requests }, [], "a decided refusal is never asked again")
        XCTAssertEqual(refused.loggedProblems, 1)
    }

    /// U04: a failed request or post (notifications not allowed, code 1)
    /// logs once.
    func testFailuresLogOnce() async throws {
        let error = NSError(domain: UNErrorDomain, code: 1)
        let center = FakeCenter {
            $0.requestError = error
            $0.answer = .denied
        }
        let poster = UserNotificationPoster(center: center)
        poster.post(Self.notification(1))
        poster.post(Self.notification(2))
        try await drain(poster)
        XCTAssertEqual(center.state.withLock { $0.requests }.count, 1)
        XCTAssertEqual(poster.loggedProblems, 1)

        let failing = FakeCenter {
            $0.status = .authorized
            $0.addError = error
        }
        let adding = UserNotificationPoster(center: failing)
        for id in UInt64(1)...3 { adding.post(Self.notification(id)) }
        try await drain(adding)
        XCTAssertEqual(adding.loggedProblems, 1)
    }

    /// U06: shown while the app is frontmost, without a sound; U07: a
    /// click passes the session id on.
    func testTheDelegatePresentsBannersAndPassesClicksOn() async {
        XCTAssertEqual(NotificationDelegate.presentationOptions, [.banner, .list])
        let delegate = NotificationDelegate { [unowned self] in opened.append($0) }
        await delegate.open(7)
        await delegate.open(nil)
        XCTAssertEqual(opened, [7, nil])
        let userInfo: [AnyHashable: Any] = [CommandNotification.sessionKey: NSNumber(value: 9)]
        await delegate.receive(action: UNNotificationDismissActionIdentifier, userInfo: userInfo)
        await delegate.receive(action: "other", userInfo: userInfo)
        XCTAssertEqual(opened, [7, nil], "only a click opens")
        await delegate.receive(action: UNNotificationDefaultActionIdentifier, userInfo: userInfo)
        XCTAssertEqual(opened, [7, nil, 9])
    }

    /// A07: launching installed the delegate on the real centre and asked
    /// nothing (the real poster has made no post).
    func testLaunchInstalledTheDelegateAndAskedNothing() throws {
        let real = try XCTUnwrap(savedPoster as? UserNotificationPoster)
        XCTAssertEqual(real.postsInFlight, 0)
        XCTAssertEqual(real.loggedProblems, 0)
        XCTAssertTrue(UNUserNotificationCenter.current().delegate === appDelegate.notificationDelegate)
    }

    // MARK: Focus (F rows)

    func testFocusIsTheFocusedPaneOfTheSelectedTabOfTheKeyWindowOfTheActiveApp() throws {
        let (first, second, group) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        XCTAssertEqual(finish(first.focusedPane, in: first, answer: .nothing), true, "F01")
        XCTAssertEqual(finish(second.focusedPane, in: second, answer: .nothing), false, "F03: a background tab")
        isActive = false
        XCTAssertEqual(finish(first.focusedPane, in: first, answer: .nothing), false, "F04: another app in front")
        isActive = true
        group.selectedWindow = second.window
        XCTAssertEqual(finish(second.focusedPane, in: second, answer: .nothing), true, "F08: judged at the finish")
        let (left, right) = try split(second)
        XCTAssertEqual(finish(right, in: second, answer: .nothing), true)
        XCTAssertEqual(finish(left, in: second, answer: .nothing), false, "F02: the split pane not focused")
        notifier.focus.isKeyWindow = { _ in false }
        XCTAssertEqual(finish(right, in: second, answer: .nothing), false, "F05: the window is not key")
        notifier.focus.isKeyWindow = { _ in true }
        XCTAssertEqual(
            finish(first.focusedPane, in: first, answer: .nothing), false, "a key window whose tab is not selected")
        XCTAssertTrue(poster.posts.isEmpty)
        XCTAssertTrue(first.markedSessionIDs.isEmpty && second.markedSessionIDs.isEmpty, "nothing marks nothing")
    }

    // MARK: Decisions and marks

    func testAMarkMarksTheTabAndAPostCarriesThePanesTitle() throws {
        let (first, second, _) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        let pane = second.focusedPane
        second.session(pane.session, didSetTitle: "make\u{202E} test")
        let title = second.window?.title
        XCTAssertEqual(finish(pane, in: second, answer: .mark), false)
        XCTAssertTrue(second.isCommandMarked(pane), "D7: a finish past the limit still marks")
        XCTAssertTrue(second.window?.tab.accessoryView === second.commandMarkView, "D11")
        XCTAssertTrue(poster.posts.isEmpty, "a mark posts nothing")
        XCTAssertEqual(second.window?.title, title, "the title is untouched")

        _ = finish(pane, in: second, exitCode: 2, answer: .markAndPost)
        let post = try XCTUnwrap(poster.posts.first)
        XCTAssertEqual(poster.posts.count, 1)
        XCTAssertEqual(
            post,
            CommandNotification(
                identifier: CommandNotification.identifier(for: pane.session.id), sessionID: pane.session.id,
                title: "Command failed", subtitle: "make test", body: "Took 12s, exit status 2"))
        XCTAssertNil(first.window?.tab.accessoryView, "the other tab is not marked")

        // An untitled pane has no subtitle (T16).
        let untitled = first.focusedPane
        isActive = false
        _ = finish(untitled, in: first, exitCode: nil, answer: .markAndPost)
        XCTAssertEqual(poster.posts.last?.subtitle, nil)
        XCTAssertEqual(poster.posts.last?.title, "Command finished")
    }

    /// D8, D10: a mark clears when its own pane gets the keyboard, with its
    /// delivered notification; the tab shows the mark while any pane has
    /// one.
    func testMarksArePerPaneAndClearWhenThePaneGetsTheKeyboard() throws {
        let controller = try appDelegate.newWindow()
        let (left, right) = try split(controller)
        isActive = false
        _ = finish(left, in: controller)
        _ = finish(right, in: controller)
        XCTAssertEqual(controller.markedSessionIDs, [left.session.id, right.session.id])
        isActive = true
        controller.focus(left)
        XCTAssertFalse(controller.isCommandMarked(left))
        XCTAssertTrue(controller.isCommandMarked(right), "the split pane keeps its mark")
        XCTAssertTrue(controller.window?.tab.accessoryView === controller.commandMarkView)
        XCTAssertEqual(poster.removed, [[CommandNotification.identifier(for: left.session.id)]])
        controller.focus(right)
        XCTAssertTrue(controller.markedSessionIDs.isEmpty)
        XCTAssertNil(controller.window?.tab.accessoryView)
        XCTAssertEqual(poster.removed.last, [CommandNotification.identifier(for: right.session.id)])

        // Closing a marked pane drops its mark.
        isActive = false
        _ = finish(left, in: controller)
        controller.focus(left)
        controller.closeFocusedPane(nil)
        XCTAssertTrue(controller.markedSessionIDs.isEmpty)
        XCTAssertNil(controller.window?.tab.accessoryView)

        notifier.removeAllDelivered()
        XCTAssertEqual(poster.removedAll, 1, "D10: all of them at the quit")
    }

    /// D10: the window becoming key (another window of the app was) clears
    /// the mark of its focused pane.
    func testTheWindowBecomingKeyClearsTheMarkOfItsFocusedPane() throws {
        let controller = try appDelegate.newWindow()
        let pane = controller.focusedPane
        isActive = false
        _ = finish(pane, in: controller)
        XCTAssertTrue(controller.isCommandMarked(pane))
        isActive = true
        XCTAssertTrue(controller.isCommandMarked(pane), "nothing has asked yet")
        controller.windowDidBecomeKey(Notification(name: NSWindow.didBecomeKeyNotification, object: controller.window))
        XCTAssertFalse(controller.isCommandMarked(pane))
        XCTAssertEqual(poster.removed, [[CommandNotification.identifier(for: pane.session.id)]])
    }

    /// D10 (M2): the app coming back to the front clears the mark of the
    /// pane that has the keyboard now, through AppKit's notification to the
    /// app's delegate, even when no window became key meanwhile; a
    /// background tab keeps its mark.
    func testComingBackToTheAppClearsTheMarkOfThePaneWithTheKeyboard() throws {
        let (first, second, _) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        isActive = false
        _ = finish(first.focusedPane, in: first)
        _ = finish(second.focusedPane, in: second)
        XCTAssertTrue(first.isCommandMarked(first.focusedPane))
        isActive = true
        XCTAssertTrue(first.isCommandMarked(first.focusedPane), "nothing has asked yet")
        NotificationCenter.default.post(name: NSApplication.didBecomeActiveNotification, object: NSApp)
        XCTAssertTrue(first.markedSessionIDs.isEmpty)
        XCTAssertEqual(second.markedSessionIDs, [second.focusedPane.session.id], "the background tab keeps its mark")
        XCTAssertEqual(poster.removed, [[CommandNotification.identifier(for: first.focusedPane.session.id)]])
    }

    /// D10: quitting removes every notification the app delivered.
    func testQuittingRemovesEveryDeliveredNotification() throws {
        try startCore(config: Self.config("unfocused"))
        XCTAssertEqual(poster.removedAll, 0)
        appDelegate.applicationWillTerminate(Notification(name: NSApplication.willTerminateNotification, object: NSApp))
        XCTAssertEqual(poster.removedAll, 1)
    }

    /// U07-U09: a click selects its session's tab and pane; an id not open
    /// here only brings the app to the front.
    func testAClickSelectsTheTabAndPaneOfItsSession() throws {
        let (first, second, group) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        let (left, right) = try split(second)
        second.focus(right)
        group.selectedWindow = first.window
        isActive = false
        _ = finish(left, in: second)
        XCTAssertTrue(second.isCommandMarked(left))
        isActive = true
        notifier.open(sessionID: left.session.id)
        XCTAssertEqual(activations, 1)
        XCTAssertTrue(group.selectedWindow === second.window)
        XCTAssertTrue(second.focusedPane === left)
        XCTAssertFalse(second.isCommandMarked(left), "seen: the mark clears")

        group.selectedWindow = first.window
        for id in [UInt64.max, nil] {
            notifier.open(sessionID: id)
            XCTAssertTrue(group.selectedWindow === first.window, "U08: nothing selected")
        }
        XCTAssertEqual(activations, 3, "U09: the app still comes to the front")
    }

    // MARK: The trigger as the app runs it (A rows)

    /// A01, A04: a real two-second command in tab 2, opened by New Tab
    /// while tab 1 is selected, posts once for tab 2's session and marks
    /// tab 2; selecting tab 2 clears the mark and its notification.
    func testALongCommandInABackgroundTabPostsOnceAndMarksIt() async throws {
        try startCore(config: Self.config("unfocused"))
        let (first, second, group) = try openTwoTabs(tab2: Self.longCommand)
        let session = second.focusedPane.session.id
        try await waitUntil("the command finished") { !finishes.isEmpty }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(finishes.map(\.0), [session])
        XCTAssertEqual(finishes.first?.1.exitCode, 0)
        XCTAssertGreaterThanOrEqual(finishes.first?.1.durationMilliseconds ?? 0, 1_900)
        XCTAssertEqual(poster.posts.map(\.sessionID), [session], "exactly one post, for tab 2")
        XCTAssertEqual(poster.posts.first?.title, "Command succeeded")
        XCTAssertEqual(second.markedSessionIDs, [session])
        XCTAssertTrue(second.window?.tab.accessoryView === second.commandMarkView)
        XCTAssertNil(first.window?.tab.accessoryView)

        group.selectedWindow = second.window
        XCTAssertTrue(second.markedSessionIDs.isEmpty, "A04: selecting the tab clears its mark")
        XCTAssertNil(second.window?.tab.accessoryView)
        XCTAssertEqual(poster.removed, [[CommandNotification.identifier(for: session)]])
    }

    /// A02: the same command in the tab that has the keyboard does nothing.
    func testALongCommandInTheFocusedTabDoesNothing() async throws {
        try startCore(config: Self.config("unfocused"))
        let (_, second, group) = try openTwoTabs(tab2: Self.longCommand)
        group.selectedWindow = second.window
        try await waitUntil("the command finished") { !finishes.isEmpty }
        XCTAssertTrue(poster.posts.isEmpty)
        XCTAssertTrue(second.markedSessionIDs.isEmpty)
    }

    /// A03: in the split pane that is not focused, it posts once and marks
    /// that pane.
    func testALongCommandInAnUnfocusedSplitPanePostsAndMarksThatPane() async throws {
        try startCore(config: Self.config("unfocused"))
        let controller = try appDelegate.newWindow()
        appDelegate.spawnOptions = Self.longCommand
        let (left, right) = try split(controller)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        controller.focus(left)
        try await waitUntil("the command finished") { !finishes.isEmpty }
        XCTAssertEqual(poster.posts.map(\.sessionID), [right.session.id])
        XCTAssertEqual(controller.markedSessionIDs, [right.session.id])
        controller.focus(right)
        XCTAssertTrue(controller.markedSessionIDs.isEmpty)
    }

    /// A05, D6: off posts and marks nothing.
    func testOffPostsAndMarksNothing() async throws {
        try startCore(config: Self.config("off"))
        let (_, second, _) = try openTwoTabs(tab2: Self.longCommand)
        try await waitUntil("the command finished") { !finishes.isEmpty }
        XCTAssertTrue(poster.posts.isEmpty)
        XCTAssertTrue(second.markedSessionIDs.isEmpty)
        XCTAssertNil(second.window?.tab.accessoryView)
    }

    /// S6, D7: a flood of qualifying finishes through the app's own path
    /// (the router and the core's decision) posts ten and marks one tab.
    func testAFloodOfFinishesPostsTenAndMarksOnce() throws {
        try startCore(config: Self.config("unfocused"))
        let (_, second, _) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
        let session = second.focusedPane.session
        let route = try XCTUnwrap(appDelegate.router.onCommandFinished)
        let start = ContinuousClock.now
        for _ in 0..<10_000 {
            route(session, TesseraCommandFinish(exitCode: 0, durationMilliseconds: 5_000), second)
        }
        let elapsed = ContinuousClock.now - start
        XCTAssertEqual(poster.posts.count, 10, "at most ten a minute")
        XCTAssertEqual(second.markedSessionIDs, [session.id])
        XCTAssertLessThan(elapsed, .seconds(5), "\(elapsed)")
    }

    // MARK: Lifetime

    /// A closed marked tab releases its controller: neither the mark, the
    /// tab-selection observer nor the notifier keeps it.
    func testClosingAMarkedTabReleasesItsController() async throws {
        weak var weakController: TerminalWindowController?
        weak var weakMark: NSView?
        try autoreleasepool {
            let (_, second, _) = try openTwoTabs(tab2: .init(program: "/bin/cat"))
            isActive = false
            _ = finish(second.focusedPane, in: second)
            XCTAssertFalse(second.markedSessionIDs.isEmpty)
            weakController = second
            weakMark = second.commandMarkView
            second.focusedPane.view.inputContext?.deactivate()
        }
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the tab is freed") { weakController == nil && weakMark == nil }
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
    }
}
