import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Records close confirmations instead of showing alerts; tests answer the
/// sheets themselves.
@MainActor
final class RecordingCloseConfirmationPresenter: CloseConfirmationPresenter {
    struct Sheet {
        let confirmation: CloseConfirmation
        let window: NSWindow
        let answer: @MainActor (Bool) -> Void
    }

    private(set) var sheets: [Sheet] = []
    private(set) var modals: [CloseConfirmation] = []
    /// What ``confirmApplicationModally(_:)`` answers.
    var modalAnswer = false
    /// Runs while ``confirmApplicationModally(_:)`` asks, as the main queue
    /// runs under a real alert's `runModal`.
    var whileAskingModally: (@MainActor () -> Void)?

    func confirm(
        _ confirmation: CloseConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void
    ) {
        sheets.append(Sheet(confirmation: confirmation, window: window, answer: completion))
    }

    func confirmApplicationModally(_ confirmation: CloseConfirmation) -> Bool {
        modals.append(confirmation)
        whileAskingModally?()
        return modalAnswer
    }
}

/// Closing a tab, a window or the application asks first while a session's
/// shell runs a job, and only then (ADR 0031, D4). Sessions run an
/// interactive `/bin/sh`, which starts jobs in their own process groups as
/// a login shell does.
@MainActor
final class CloseConfirmationTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var presenter: RecordingCloseConfirmationPresenter!
    private var previousPresenter: (any CloseConfirmationPresenter)?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        presenter = RecordingCloseConfirmationPresenter()
        previousPresenter = appDelegate.closeConfirmationPresenter
        appDelegate.closeConfirmationPresenter = presenter
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        appDelegate.closeConfirmationPresenter = previousPresenter
        appDelegate = nil
        presenter = nil
    }

    // MARK: Tabs

    func testClosingAnIdleShellDoesNotAsk() throws {
        let controller = try appDelegate.newWindow()
        let session = controller.focusedPane.session

        controller.window?.performClose(nil)

        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        XCTAssertThrowsError(try session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
    }

    func testClosingATabRunningSleepAsksAndCancelKeepsIt() async throws {
        let windows = try openTabs(2)
        let busy = try controller(of: windows[0])
        try await start("sleep 100", in: busy)

        windows[0].performClose(nil)

        XCTAssertEqual(presenter.sheets.count, 1)
        let sheet = try XCTUnwrap(presenter.sheets.first)
        XCTAssertEqual(sheet.confirmation, CloseConfirmation(scope: .tab, processNames: ["sleep"]))
        XCTAssertTrue(sheet.window === windows[0])
        XCTAssertTrue(windows[0].tabGroup?.selectedWindow === windows[0], "the asking tab is selected")
        XCTAssertTrue(appDelegate.windowControllers.contains { $0 === busy }, "open while asking")

        windows[0].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1, "a second ⌘W while asking does not ask again")

        sheet.answer(false)
        XCTAssertEqual(appDelegate.windowControllers.count, 2, "Cancel keeps the tab")
        XCTAssertEqual(try busy.focusedPane.session.foregroundProcess()?.name, "sleep", "and its job")
        XCTAssertNil(busy.pendingClose)

        windows[0].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 2, "asks again after Cancel")
        presenter.sheets[1].answer(true)
        XCTAssertFalse(appDelegate.windowControllers.contains { $0 === busy }, "Close closes the tab")
        XCTAssertThrowsError(try busy.focusedPane.session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertEqual(appDelegate.windowControllers.count, 1, "the other tab stays")
    }

    func testAfterTheJobEndsTheTabClosesWithoutAsking() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        try controller.focusedPane.session.write([0x03])
        try await waitUntil("Ctrl-C ends sleep") { try controller.focusedPane.session.foregroundProcess() == nil }

        controller.window?.performClose(nil)

        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    /// The process names itself, so its name reaches the alert only
    /// filtered: no escape sequence, bell or line break.
    func testAJobWithControlCharactersInItsNameIsShownFiltered() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-34-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let hostile = "z\u{1B}]0;x\u{7}\nq"
        // A program of its own: a copy of /bin/sleep loses its platform
        // signature and is killed at exec.
        try compileSleeper(to: dir.appendingPathComponent(hostile))
        // The shell cannot be sent the name as typed input (the line editor
        // would take the escape as a key), so a script `exec`s it.
        let controller = try appDelegate.newWindow()
        let job = try await runSleeper(in: dir, session: controller.focusedPane.session, "the copied sleep runs")
        XCTAssertTrue(job.name.unicodeScalars.contains { $0.properties.generalCategory == .control }, job.name)

        controller.window?.performClose(nil)

        let confirmation = try XCTUnwrap(presenter.sheets.first?.confirmation)
        XCTAssertEqual(confirmation.processNames, [CloseConfirmation.displayName(job.name)])
        for text in [confirmation.messageText, confirmation.informativeText] {
            XCTAssertFalse(text.contains("\u{1B}"), text)
            XCTAssertFalse(text.contains("\u{7}"), text)
        }
        XCTAssertFalse(confirmation.processNames[0].contains("\n"))
        XCTAssertTrue(confirmation.processNames[0].hasPrefix("z\u{FFFD}]0;x\u{FFFD}"), confirmation.processNames[0])
    }

    // MARK: Windows

    func testClosingAWindowAsksOnceForAllItsBusyTabs() async throws {
        let windows = try openTabs(3)
        let other = try appDelegate.newWindow()
        try await start("sleep 100", in: try controller(of: windows[0]))
        try await start("cat", in: try controller(of: windows[2]))
        try await start("sleep 100", in: other)
        let closing = try controller(of: windows[1])

        closing.closeTabGroup(nil)

        XCTAssertEqual(presenter.sheets.count, 1, "one sheet for the window, not one per tab")
        let sheet = try XCTUnwrap(presenter.sheets.first)
        XCTAssertEqual(sheet.confirmation, CloseConfirmation(scope: .window, processNames: ["sleep", "cat"]))
        XCTAssertTrue(sheet.window === windows[1])
        windows[0].performClose(nil)
        closing.closeTabGroup(nil)
        XCTAssertEqual(presenter.sheets.count, 1, "no second question while the window asks")

        sheet.answer(false)
        XCTAssertEqual(appDelegate.windowControllers.count, 4, "Cancel keeps every tab")

        closing.closeTabGroup(nil)
        XCTAssertEqual(presenter.sheets.count, 2)
        presenter.sheets[1].answer(true)
        XCTAssertEqual(presenter.sheets.count, 2, "the tabs close without asking again")
        XCTAssertEqual(appDelegate.windowControllers.map(ObjectIdentifier.init), [ObjectIdentifier(other)])
        XCTAssertFalse(windows.contains { $0.isVisible })
    }

    /// ⇧⌘W on a tab while another tab of the window asks about itself:
    /// the window asks on the current tab, listing every busy tab, and
    /// Close closes them all, the one asking included. Sent to the tab
    /// that asks, ⇧⌘W is ignored, as ⌘W is.
    func testClosingAWindowWhileOneOfItsTabsAsksAsksForTheWindow() async throws {
        let windows = try openTabs(3)
        try await start("sleep 100", in: try controller(of: windows[1]))
        try await start("cat", in: try controller(of: windows[2]))
        windows[1].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1)
        let asking = try controller(of: windows[1])

        asking.closeTabGroup(nil)
        XCTAssertEqual(presenter.sheets.count, 1, "the asking tab does not ask a second time")

        let closing = try controller(of: windows[0])
        closing.closeTabGroup(nil)

        XCTAssertEqual(presenter.sheets.count, 2, "the window asks while a tab asks")
        let sheet = try XCTUnwrap(presenter.sheets.dropFirst().first)
        XCTAssertEqual(sheet.confirmation, CloseConfirmation(scope: .window, processNames: ["sleep", "cat"]))
        XCTAssertTrue(sheet.window === windows[0])
        XCTAssertEqual(asking.pendingClose, .tab, "the tab's own question stays")
        XCTAssertEqual(closing.pendingClose, .window)

        sheet.answer(true)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "Close closes every tab")
        presenter.sheets[0].answer(true)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    func testClosingAWindowOfIdleTabsDoesNotAsk() throws {
        let windows = try openTabs(3)

        try controller(of: windows[0]).closeTabGroup(nil)

        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    // MARK: AppKit's own multi-tab closes

    /// AppKit's tab closes (the tab bar's Close Other Tabs, Close Tabs to
    /// the Right, Close Window from the Window menu) ask each tab through
    /// `windowShouldClose`: a busy tab's sheet must hold back only that tab,
    /// so the idle tabs close and every busy tab asks. (`NSApplication`'s
    /// `closeAll:` is the single Close All since #526, one alert for every
    /// window: ``QuitAndCloseAllTests``.)
    func testCloseOtherTabsClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseOtherTabs:", on: 0, keeps: [0])
    }

    func testCloseTabsAfterClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseTabsAfter:", on: 0, keeps: [0])
    }

    func testCloseTabbedWindowGroupClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseTabbedWindowGroup:", on: 0, keeps: [])
    }

    /// The shell of a tab exits cleanly while the tab's close sheet (a real
    /// `NSAlert`) is up: the tab closes, the sheet ends as Cancel, and the
    /// alert and the tab's controller are freed. (The closed window itself
    /// may outlive them: AppKit keeps it as the previous key window.)
    func testATabWhoseShellExitsUnderItsSheetFreesTheSheetAndTheTab() async throws {
        appDelegate.closeConfirmationPresenter = AlertCloseConfirmationPresenter()
        weak var weakController: TerminalWindowController?
        weak var weakSheet: NSWindow?
        var window: NSWindow?
        func openAndAsk() async throws {
            let controller = try appDelegate.newWindow()
            weakController = controller
            window = controller.window
            try await start("sleep 100", in: controller)
            autoreleasepool {
                controller.window?.performClose(nil)
                weakSheet = controller.window?.attachedSheet
            }
        }
        try await openAndAsk()
        XCTAssertNotNil(weakSheet, "the busy tab asks on a sheet")

        // As the router delivers a clean exit, in a pool of its own: the
        // test's own pool would keep what AppKit autoreleases until the
        // test ends.
        autoreleasepool { weakController.map { $0.session($0.focusedPane.session, didExitWithCode: 0, signal: 0) } }

        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "the tab closed")
        XCTAssertEqual(window?.sheets.count, 0, "its sheet ended")
        try? await waitUntil("the closed tab's sheet and controller are freed") {
            weakSheet == nil && weakController == nil
        }
        XCTAssertNil(weakSheet, "the alert's panel")
        XCTAssertNil(weakController, "the tab's controller")
        window = nil
    }

    /// The window's sheet (⇧⌘W, a real `NSAlert`) is up on a tab whose shell
    /// then exits cleanly: the sheet ends as Cancel, not Close, so the
    /// window's busy tab stays open with its job, and nothing still asks.
    func testAWindowSheetEndedByItsTabClosingCancels() async throws {
        appDelegate.closeConfirmationPresenter = AlertCloseConfirmationPresenter()
        let windows = try openTabs(2)
        let asking = try controller(of: windows[0])
        let busy = try controller(of: windows[1])
        try await start("sleep 100", in: busy)
        autoreleasepool { asking.closeTabGroup(nil) }
        XCTAssertNotNil(windows[0].attachedSheet, "the window asks on a sheet")
        XCTAssertEqual(asking.pendingClose, .window)

        autoreleasepool { asking.session(asking.focusedPane.session, didExitWithCode: 0, signal: 0) }

        XCTAssertEqual(windows[0].sheets.count, 0, "the sheet ended")
        XCTAssertEqual(appDelegate.windowControllers.map(ObjectIdentifier.init), [ObjectIdentifier(busy)], "the busy tab stays")
        XCTAssertTrue(windows[1].isVisible)
        XCTAssertEqual(try busy.focusedPane.session.foregroundProcess()?.name, "sleep", "and its job")
        XCTAssertNil(busy.pendingClose)
        XCTAssertNil(asking.pendingClose, "the ended sheet asks no more")
    }

    /// ⌘W on an idle tab closes it while another tab of the window asks.
    func testAnIdleTabClosesWhileItsNeighbourAsks() async throws {
        let windows = try openTabs(2)
        try await start("sleep 100", in: try controller(of: windows[0]))
        windows[0].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1)

        windows[1].performClose(nil)

        XCTAssertEqual(presenter.sheets.count, 1)
        XCTAssertFalse(windows[1].isVisible, "the idle tab closed")
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
    }

    /// Tabs [idle, busy, idle, busy]: `action` sent to tab `target` closes
    /// the idle tabs it covers at once and asks each busy one on its own
    /// sheet; after Close only the tabs at `keeps` stay.
    private func checkAppKitClose(_ action: String, on target: Int, keeps: [Int]) async throws {
        let selector = NSSelectorFromString(action)
        let windows = try openTabs(4)
        for index in [1, 3] {
            try await start("sleep 100", in: try controller(of: windows[index]))
        }
        windows[0].tabGroup?.selectedWindow = windows[0]
        let receiver = windows[target]
        guard receiver.responds(to: selector) else {
            throw XCTSkip("\(type(of: receiver)) does not answer \(action) on this macOS")
        }

        receiver.perform(selector, with: nil)

        XCTAssertFalse(windows[2].isVisible, "\(action): the idle tab after a busy one closes")
        XCTAssertEqual(
            Set(presenter.sheets.map { ObjectIdentifier($0.window) }), Set([1, 3].map { ObjectIdentifier(windows[$0]) }),
            "\(action): each busy tab asks")
        for sheet in presenter.sheets {
            sheet.answer(true)
        }
        let open = appDelegate.windowControllers.compactMap(\.window).map(ObjectIdentifier.init)
        XCTAssertEqual(Set(open), Set(keeps.map { ObjectIdentifier(windows[$0]) }), action)
    }

    // MARK: Quit

    func testQuitAsksOnceListingEveryBusySession() async throws {
        let windows = try openTabs(2)
        let second = try appDelegate.newWindow()
        try await start("sleep 100", in: try controller(of: windows[1]))
        try await start("cat", in: second)

        presenter.modalAnswer = false
        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateCancel)
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .quit, processNames: ["sleep", "cat"])])
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel keeps everything open")
        XCTAssertTrue(presenter.sheets.isEmpty)

        presenter.modalAnswer = true
        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertEqual(presenter.modals.count, 2)
    }

    /// `SIGTERM`, `SIGINT` and `SIGHUP` quit without asking: nobody may be
    /// there to answer.
    func testASignalQuitDoesNotAsk() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        appDelegate.terminatesWithoutConfirmation = true
        defer { appDelegate.terminatesWithoutConfirmation = false }

        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertTrue(presenter.modals.isEmpty)
    }

    /// A quit Apple event that names no reason (the Dock's Quit,
    /// `NSRunningApplication.terminate()`) with no confirmation up asks as
    /// ⌘Q does (D4b), and its Cancel answers the event with
    /// `userCanceledErr` (#458). The event goes through the Apple Event
    /// Manager to the handler the app installed at launch.
    func testAQuitEventWithNothingUpAsksAndCancelAnswersTheEvent() async throws {
        try await assertAQuitEventWithNothingUpAsks(reason: nil)
    }

    /// The system's quit (a logout names its reason) with no confirmation
    /// up asks too, as ⌘Q does: only a confirmation already up makes it
    /// quit without asking. Its Cancel answers the event with
    /// `userCanceledErr`, so the logout stops rather than waiting on the
    /// app.
    func testALogoutQuitEventWithNothingUpAsksAndCancelAnswersTheEvent() async throws {
        try await assertAQuitEventWithNothingUpAsks(reason: OSType(kAEReallyLogOut))
    }

    /// Another application-modal alert up (an error) does not keep a quit
    /// event from asking, as with AppKit's own handler: only the quit
    /// alert already asking does.
    func testAQuitEventWithAnotherModalAlertUpAsks() async throws {
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
            defer: false)
        let modal = NSApp.beginModalSession(for: panel)
        defer {
            NSApp.endModalSession(modal)
            panel.close()
        }
        XCTAssertNotNil(NSApp.modalWindow, "a modal alert is up")
        try await assertAQuitEventWithNothingUpAsks(reason: nil)
    }

    /// Sends this process a quit Apple event naming `reason` (none if
    /// `nil`) with a busy session and the quit answered Cancel.
    private func assertAQuitEventWithNothingUpAsks(
        reason: OSType?, file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        presenter.modalAnswer = false

        let event = NSAppleEventDescriptor(
            eventClass: AEEventClass(kCoreEventClass), eventID: AEEventID(kAEQuitApplication),
            targetDescriptor: .currentProcess(), returnID: AEReturnID(kAutoGenerateReturnID),
            transactionID: AETransactionID(kAnyTransactionID))
        if let reason {
            event.setParam(NSAppleEventDescriptor(enumCode: reason), forKeyword: AEKeyword(kAEQuitReason))
        }
        var error: Int32?
        do {
            let reply = try event.sendEvent(options: [.waitForReply], timeout: 5)
            error = reply.paramDescriptor(forKeyword: keyErrorNumber)?.int32Value
        } catch let failure as NSError {
            error = Int32(failure.code)
        }

        XCTAssertEqual(
            presenter.modals, [CloseConfirmation(scope: .quit, processNames: ["sleep"])], "it asks once", file: file,
            line: line)
        XCTAssertEqual(error, Int32(userCanceledErr), "Cancel answers the event as cancelled", file: file, line: line)
        XCTAssertEqual(appDelegate.windowControllers.count, 1, "Cancel keeps everything open", file: file, line: line)
        XCTAssertFalse(appDelegate.terminatesWithoutConfirmation, "the next quit asks again", file: file, line: line)
    }

    func testQuitWithIdleSessionsDoesNotAsk() throws {
        _ = try openTabs(2)

        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertTrue(presenter.modals.isEmpty)
    }

    // MARK: Text

    func testDisplayNameFiltersWhatAProcessCallsItself() {
        XCTAssertEqual(CloseConfirmation.displayName("vim"), "vim")
        XCTAssertEqual(CloseConfirmation.displayName("ssh-agent ünï"), "ssh-agent ünï")
        XCTAssertEqual(CloseConfirmation.displayName("\u{1B}[31mred\u{7}"), "\u{FFFD}[31mred\u{FFFD}")
        XCTAssertEqual(CloseConfirmation.displayName("a\nb\rc\td"), "a\u{FFFD}b\u{FFFD}c\u{FFFD}d")
        XCTAssertEqual(CloseConfirmation.displayName("a\u{85}b\u{9B}c"), "a\u{FFFD}b\u{FFFD}c", "C1 controls")
        XCTAssertEqual(CloseConfirmation.displayName("\u{202E}gpj.exe"), "\u{FFFD}gpj.exe", "bidi override")
        XCTAssertEqual(CloseConfirmation.displayName("a\u{2028}b"), "a\u{FFFD}b")
        XCTAssertEqual(CloseConfirmation.displayName(""), CloseConfirmation.unnamedProcess)
        XCTAssertEqual(CloseConfirmation.displayName("   "), CloseConfirmation.unnamedProcess)
        XCTAssertEqual(CloseConfirmation.displayName(" top "), "top")
    }

    func testAlertTextNamesEveryProcessUpToTheLimit() {
        let tab = CloseConfirmation(scope: .tab, processNames: ["sleep"])
        XCTAssertEqual(tab.messageText, "Close this tab?")
        XCTAssertEqual(tab.informativeText, "Closing this tab will terminate the running process:\n• sleep")
        XCTAssertEqual(tab.confirmButtonTitle, "Close")

        let window = CloseConfirmation(scope: .window, processNames: ["vim", "ssh"])
        XCTAssertEqual(window.messageText, "Close this window?")
        XCTAssertEqual(
            window.informativeText, "Closing this window will terminate the running processes:\n• vim\n• ssh")

        let names = (1...13).map { "job\($0)" }
        let quit = CloseConfirmation(scope: .quit, processNames: names)
        XCTAssertEqual(quit.messageText, "Quit Tessera?")
        XCTAssertEqual(quit.confirmButtonTitle, "Quit")
        let lines = quit.informativeText.split(separator: "\n").map(String.init)
        XCTAssertEqual(lines.first, "Quitting will terminate the running processes:")
        XCTAssertEqual(Array(lines.dropFirst()), (1...10).map { "• job\($0)" } + ["• and 3 more"])
    }

    // MARK: Helpers

    /// Opens a window with `count` tabs and returns them in tab-bar order.
    private func openTabs(_ count: Int) throws -> [NSWindow] {
        let window = try XCTUnwrap(appDelegate.newWindow().window)
        for _ in 1..<count {
            _ = try appDelegate.newTab(in: window.tabGroup?.windows.last ?? window)
        }
        let windows = window.tabGroup?.windows ?? [window]
        XCTAssertEqual(windows.count, count)
        return windows
    }

    private func controller(of window: NSWindow) throws -> TerminalWindowController {
        try XCTUnwrap(appDelegate.windowControllers.first { $0.window === window })
    }

    /// Types `command` into the tab's shell and waits until it runs in the
    /// foreground.
    private func start(_ command: String, in controller: TerminalWindowController) async throws {
        let name = String(command.prefix { $0 != " " })
        try controller.focusedPane.session.write(Array("\(command)\n".utf8))
        _ = try await poll("\(name) runs in the foreground") {
            try controller.focusedPane.session.foregroundProcess().flatMap { $0.name == name ? $0 : nil }
        }
    }

    private func poll<T>(
        _ description: String, timeout: Duration = .seconds(5), _ value: () throws -> T?
    ) async throws -> T {
        let deadline = ContinuousClock.now + timeout
        while true {
            if let value = try value() { return value }
            guard ContinuousClock.now < deadline else {
                throw PollTimedOut(description: "timed out after \(timeout): \(description)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () throws -> Bool
    ) async throws {
        _ = try await poll(description, timeout: timeout) { try condition() ? true : nil }
    }
}

/// Builds a program that sleeps 100 s at `binary` with the toolchain's
/// C compiler, or skips the test without one.
func compileSleeper(to binary: URL) throws {
    let compiler = Process()
    compiler.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
    compiler.arguments = ["clang", "-x", "c", "-", "-o", binary.path]
    let source = Pipe()
    compiler.standardInput = source
    compiler.standardOutput = FileHandle.nullDevice
    compiler.standardError = FileHandle.nullDevice
    try compiler.run()
    source.fileHandleForWriting.write(Data("#include <unistd.h>\nint main(void) { sleep(100); return 0; }\n".utf8))
    try source.fileHandleForWriting.close()
    compiler.waitUntilExit()
    guard compiler.terminationStatus == 0 else {
        throw XCTSkip("no C compiler to build a program with a hostile name (xcrun clang exited \(compiler.terminationStatus))")
    }
}

struct PollTimedOut: Error, CustomStringConvertible {
    let description: String
}

/// Runs the sleeper ``compileSleeper(to:)`` built in `dir` (its name, which
/// may hold anything, starts with "z" and ends with "q") in `session`, the
/// way a user would, through a script that `exec`s it, and returns it once
/// it is the session's foreground job.
///
/// The shell, the script and the exec take seconds on a loaded CI runner
/// under TSan (#599), so it waits up to `timeout`; but it fails as soon as
/// the script's shell (which wrote its pid before the exec) is gone without
/// having become the sleeper.
@MainActor
func runSleeper(
    in dir: URL, session: TesseraSession, _ description: String, timeout: Duration = .seconds(30)
) async throws -> TesseraSession.ForegroundProcess {
    let script = dir.appendingPathComponent("run.sh")
    let pidFile = dir.appendingPathComponent("pid")
    try #"""
        d="$(dirname "$0")"
        echo $$ > "$d/pid.tmp" && mv "$d/pid.tmp" "$d/pid"
        exec "$d"/z*q

        """#.write(to: script, atomically: true, encoding: .utf8)
    try session.write(Array("sh \(script.path)\n".utf8))
    let deadline = ContinuousClock.now + timeout
    while true {
        if let job = try session.foregroundProcess(), job.name.hasPrefix("z") { return job }
        if let text = try? String(contentsOf: pidFile, encoding: .utf8),
            let pid = Int32(text.trimmingCharacters(in: .whitespacesAndNewlines)), kill(pid, 0) != 0, errno == ESRCH
        {
            throw PollTimedOut(description: "the script's shell (\(pid)) exited before it ran: \(description)")
        }
        guard ContinuousClock.now < deadline else {
            throw PollTimedOut(description: "timed out after \(timeout): \(description)")
        }
        try await Task.sleep(for: .milliseconds(20))
    }
}

// MARK: - Termination signals with a confirmation up (#441)

/// `kill -TERM`, `-INT` and `-HUP` end the app while a close sheet or the
/// quit alert is up (#441, ADR 0031). A signal ends the process it is sent
/// to, so each case runs in a child: a second copy of this test host
/// (``TerminationSignalChild``) that opens a terminal the way the app does,
/// starts a job in its `/bin/sh -i`, asks to close it (the real
/// ``AlertCloseConfirmationPresenter`` shows the sheet) or to quit (the
/// real quit alert), and waits. This test is the watcher: it sends the
/// signal and times the child's exit. Nothing is posted or called in the
/// child but what a user's ⌘W or ⌘Q would.
@MainActor
final class TerminationSignalTests: XCTestCase {
    /// How long the child may take to exit after the signal (the issue
    /// measured more than 20 s).
    private static let exitDeadline: Duration = .seconds(3)

    /// #525: only a thread that spent the whole sample in TSan's fork
    /// interceptor at an atfork handler counts as the #278 hang (stacks as
    /// `sample` prints them, from the CI hang of run 44504840654).
    func testOnlyAWholeSampleInTheForkInterceptorIsTheForkHang() {
        let hung = [
            "Call graph:",
            "    696 Thread_139287   DispatchQueue_1: com.apple.main-thread  (serial)",
            "    + 696 start  (in dyld) + 6076  [0x195ab2b98]",
            "    +   696 AppDelegate.spawn(profile:)  (in Tessera) + 980  [0x1021c7598]  AppDelegate.swift:688",
            "    +     696 wrap_fork  (in libclang_rt.tsan_osx_dynamic.dylib) + 128  [0x1042bd384]",
            "    +       696 fork  (in libsystem_c.dylib) + 36  [0x195d06c40]",
            "    +         696 libSystem_atfork_prepare  (in libSystem.B.dylib) + 44  [0x1a40c5184]",
            "    +           696 _objc_atfork_prepare  (in libobjc.A.dylib) + 552  [0x195a75b3c]",
            "    +             696 __ulock_wait2  (in libsystem_kernel.dylib) + 8  [0x195e1fa54]",
            "    696 Thread_139316   DispatchQueue_2: com.apple.libdispatch-manager  (serial)",
            "    + 696 start_wqthread  (in libsystem_pthread.dylib) + 8  [0x195e4eb74]",
        ]
        XCTAssertTrue(TerminationSignalChild.isForkHang(hung), "the CI hang")
        // A fork that was only passing through during the sample.
        let passing = hung.map { line in
            line.contains("fork") || line.contains("ulock") ? line.replacingOccurrences(of: "696", with: "3") : line
        }
        XCTAssertFalse(TerminationSignalChild.isForkHang(passing), "a fork in progress")
        // The fork interceptor without an atfork handler or TSan frame under it.
        XCTAssertFalse(
            TerminationSignalChild.isForkHang(hung.filter { !$0.contains("atfork") }), "no atfork handler")
        // A main thread waiting in its run loop, with wrap_fork named only
        // in the output the diagnosis also carries (never passed here).
        let idle = [
            "Call graph:",
            "    696 Thread_139287   DispatchQueue_1: com.apple.main-thread  (serial)",
            "    + 696 start  (in dyld) + 6076  [0x195ab2b98]",
            "    +   696 mach_msg2_trap  (in libsystem_kernel.dylib) + 8  [0x195e10c34]",
        ]
        XCTAssertFalse(TerminationSignalChild.isForkHang(idle), "a plain wait")
    }

    func testASignalEndsTheAppWithACloseSheetUp() async throws {
        for (signal, name) in [(SIGTERM, "SIGTERM"), (SIGINT, "SIGINT"), (SIGHUP, "SIGHUP")] {
            let result = try await run(.sheet, command: "sleep 100", signal: signal)
            assertQuit(result, within: Self.exitDeadline, "\(name), close sheet")
        }
    }

    func testASignalEndsTheAppWithTheQuitAlertUp() async throws {
        for (signal, name) in [(SIGTERM, "SIGTERM"), (SIGINT, "SIGINT"), (SIGHUP, "SIGHUP")] {
            let result = try await run(.alert, command: "sleep 100", signal: signal)
            assertQuit(result, within: Self.exitDeadline, "\(name), quit alert")
        }
    }

    /// The core's shutdown runs on that path: a shell that ignores `SIGHUP`
    /// is killed and reaped (after the core's grace, about 2 s) before the
    /// app exits.
    func testASignalWithASheetUpReapsAShellThatIgnoresHangup() async throws {
        let result = try await run(.sheet, command: "trap '' HUP; echo $$ > shell.pid; sleep 100", signal: SIGTERM)
        assertQuit(result, within: Self.exitDeadline + .seconds(3), "a shell that ignores SIGHUP")
        XCTAssertNotNil(result.shellPid, "the shell wrote its pid")
        XCTAssertEqual(result.shellRemains, false, "the shell is gone once the app has exited")
    }

    /// When `terminate` cannot end the app (here a modal session that
    /// ignores the abort), the app ends itself after
    /// ``AppDelegate/terminationGrace``, with the window state saved
    /// and the shell that ignores `SIGHUP` reaped.
    func testASignalEndsTheAppWhenSomethingRefusesToQuit() async throws {
        let result = try await run(
            .stubbornModal, command: "trap '' HUP; echo $$ > shell.pid; sleep 100", signal: SIGTERM)
        assertQuit(
            result, within: Self.exitDeadline + .seconds(3), "a modal session that ignores the abort",
            throughTerminate: false)
        XCTAssertNotNil(result.shellPid, "the shell wrote its pid")
        XCTAssertEqual(result.shellRemains, false, "the shell is gone once the app has exited")
    }

    // MARK: Quit Apple events (#458)

    /// The quit event a logout, restart or shutdown sends (it names its
    /// reason) ends the app with a close sheet up, without asking again.
    /// The reason is sent as a parameter, and for a logout also as an
    /// attribute.
    func testASystemQuitEventEndsTheAppWithACloseSheetUp() async throws {
        let reasons: [(OSType, Bool, String)] = [
            (OSType(kAEReallyLogOut), false, "log out"), (OSType(kAEReallyLogOut), true, "log out (attribute)"),
            (OSType(kAERestart), false, "restart"), (OSType(kAEShutDown), false, "shut down"),
        ]
        for (reason, asAttribute, name) in reasons {
            let result = try await run(
                .sheet, command: "sleep 100", trigger: .systemQuitEvent(reason: reason, asAttribute: asAttribute))
            assertQuit(result, within: Self.exitDeadline, "a \(name) quit event, close sheet")
        }
    }

    /// With the quit alert up (⌘Q with a busy session), the system's quit
    /// ends it and the quit goes on: the windows are saved, through
    /// `terminate`.
    func testASystemQuitEventEndsTheAppWithTheQuitAlertUp() async throws {
        let result = try await run(
            .alert, command: "sleep 100", trigger: .systemQuitEvent(reason: OSType(kAEReallyLogOut)))
        assertQuit(result, within: Self.exitDeadline, "a log out quit event, quit alert")
    }

    /// The core's shutdown runs on that path too: a shell that ignores
    /// `SIGHUP` is reaped before the app exits.
    func testASystemQuitEventWithASheetUpReapsAShellThatIgnoresHangup() async throws {
        let result = try await run(
            .sheet, command: "trap '' HUP; echo $$ > shell.pid; sleep 100",
            trigger: .systemQuitEvent(reason: OSType(kAEShutDown)))
        assertQuit(
            result, within: Self.exitDeadline + .seconds(3), "a shut down quit event, a shell that ignores SIGHUP")
        XCTAssertNotNil(result.shellPid, "the shell wrote its pid")
        XCTAssertEqual(result.shellRemains, false, "the shell is gone once the app has exited")
    }

    /// Any other quit event (`NSRunningApplication.terminate()`, the
    /// Dock's Quit) with a close sheet up ends the sheet as Cancel and asks
    /// to quit, as ⌘Q would: quitting ends more than the sheet asked
    /// about. The app runs on, asking; a signal then ends it.
    func testAQuitEventWithACloseSheetUpEndsItAndAsksToQuit() async throws {
        let result = try await run(
            .sheet, command: "sleep 100", trigger: .quitEvent, thenExpect: "sheets=0 modal=Cancel,Quit",
            followedBy: SIGTERM)
        XCTAssertEqual(result.stateAfterTrigger, "sheets=0 modal=Cancel,Quit", "the sheet ended, the quit alert asks")
        XCTAssertEqual(result.ranOnAfterTrigger, true, "the app runs on while it asks")
        assertQuit(result, within: Self.exitDeadline, "the signal after the quit event")
    }

    /// The same quit event with the quit alert already up changes nothing:
    /// it already asks.
    func testAQuitEventWithTheQuitAlertUpLeavesItAsking() async throws {
        let result = try await run(
            .alert, command: "sleep 100", trigger: .quitEvent, thenExpect: "sheets=0 modal=Cancel,Quit",
            followedBy: SIGTERM)
        XCTAssertEqual(result.stateAfterTrigger, "sheets=0 modal=Cancel,Quit", "the quit alert still asks")
        XCTAssertEqual(result.ranOnAfterTrigger, true, "the app runs on while it asks")
        assertQuit(result, within: Self.exitDeadline, "the signal after the quit event")
    }

    /// With another application-modal alert up (an error), the quit event
    /// asks as ⌘Q does, as AppKit's own handler did: the quit alert shows
    /// over the error's (whose button is OK).
    func testAQuitEventWithAnErrorAlertUpAsksToQuit() async throws {
        let result = try await run(
            .errorAlert, command: "sleep 100", trigger: .quitEvent, thenExpect: "sheets=0 modal=Cancel,Quit",
            followedBy: SIGTERM)
        XCTAssertEqual(result.stateAfterTrigger, "sheets=0 modal=Cancel,Quit", "the quit alert asks")
        XCTAssertEqual(result.ranOnAfterTrigger, true, "the app runs on while it asks")
        assertQuit(result, within: Self.exitDeadline, "the signal after the quit event")
    }

    /// With idle sessions there is nothing to ask about: the quit event
    /// quits through `terminate` with the error alert still up.
    func testAQuitEventWithAnErrorAlertUpAndIdleSessionsQuits() async throws {
        let result = try await run(.idleErrorAlert, command: "sleep 100", trigger: .quitEvent)
        assertQuit(result, within: Self.exitDeadline, "a quit event, error alert, idle sessions")
    }

    // MARK: Close All and Quit's alternate (#464)

    /// A signal or the system's quit with Close All's alert up (⌥⌘W, a job
    /// running) ends the alert, and the app quits without asking, its
    /// windows saved, through `terminate`.
    func testASignalOrTheSystemsQuitEndsTheAppWithTheCloseAllAlertUp() async throws {
        let triggers: [(Trigger, String)] = [
            (.signal(SIGTERM), "SIGTERM"), (.systemQuitEvent(reason: OSType(kAEReallyLogOut)), "a log out quit event"),
        ]
        for (trigger, name) in triggers {
            let result = try await run(.closeAllAlert, command: "sleep 100", trigger: trigger)
            assertQuit(result, within: Self.exitDeadline, "\(name), Close All alert")
        }
    }

    /// Any other quit event with Close All's alert up asks to quit, the
    /// quit alert over Close All's; a signal then ends both.
    func testAQuitEventWithTheCloseAllAlertUpAsksToQuit() async throws {
        let result = try await run(
            .closeAllAlert, command: "sleep 100", trigger: .quitEvent, thenExpect: "sheets=0 modal=Cancel,Quit",
            followedBy: SIGTERM)
        XCTAssertEqual(result.stateAfterTrigger, "sheets=0 modal=Cancel,Quit", "the quit alert asks")
        XCTAssertEqual(result.ranOnAfterTrigger, true, "the app runs on while it asks")
        assertQuit(result, within: Self.exitDeadline, "the signal after the quit event")
    }

    /// A signal or the system's quit with the quit alert of Quit and Keep
    /// Windows up (⌥⌘Q, windows close on quit) quits without asking, and
    /// keeps the alternate's choice: the next launch restores the windows.
    func testASignalOrTheSystemsQuitWithTheQuitAlternatesAlertUpKeepsItsChoice() async throws {
        let triggers: [(Trigger, String)] = [
            (.signal(SIGTERM), "SIGTERM"), (.systemQuitEvent(reason: OSType(kAEShutDown)), "a shut down quit event"),
        ]
        for (trigger, name) in triggers {
            let result = try await run(
                .quitAlternateAlert, command: "sleep 100", trigger: trigger, quitKeepsWindows: false)
            assertQuit(result, within: Self.exitDeadline, "\(name), quit alternate's alert")
            XCTAssertEqual(result.restoresAtNextLaunch, true, "\(name): the next launch keeps the windows")
        }
    }

    /// ⌥⌘Q with two windows, three tabs and two jobs asks once, Quit
    /// quits through `terminate` and saves every window, and the next
    /// launch makes the other choice than the setting: Quit and Keep
    /// Windows restores them though windows close on quit, Quit and Close
    /// All Windows restores none though they are kept. A save while the
    /// alert asks takes no part in it.
    func testTheQuitAlternateAsksOnceQuitsAndMakesTheOtherWindowChoice() async throws {
        for keeps in [false, true] {
            let result = try await run(.quitAlternate, command: "sleep 100", trigger: nil, quitKeepsWindows: keeps)
            let what = keeps ? "Quit and Close All Windows" : "Quit and Keep Windows"
            let tail = result.output.split(separator: "\n").suffix(15).joined(separator: "\n")
            XCTAssertNotNil(result.exitTime, "\(what): quits on its own; output:\n\(tail)")
            XCTAssertEqual(result.reason, .exit, what)
            XCTAssertEqual(result.status, 0, "\(what): exit status; output:\n\(tail)")
            XCTAssertTrue(result.quitThroughTerminate, "\(what): through terminate")
            XCTAssertEqual(result.confirmations, ["quit sleep,cat"], "\(what): asks once, for both jobs")
            XCTAssertEqual(
                result.savedState?.windows.map(\.tabCount).sorted(), [1, 2], "\(what): every window is saved")
            XCTAssertEqual(result.restoresAtNextLaunch, !keeps, "\(what): the next launch's choice")
        }
    }

    // MARK: Helpers

    /// What ends the child.
    enum Trigger {
        case signal(Int32)
        /// `NSRunningApplication.terminate()`: a quit Apple event that
        /// names no reason.
        case quitEvent
        /// A quit Apple event naming its reason (`kAEQuitReason`), as the
        /// system sends for a logout, restart or shutdown: as a parameter,
        /// or as an attribute.
        case systemQuitEvent(reason: OSType, asAttribute: Bool = false)
    }

    struct Result {
        /// From the (last) trigger to the child's exit; `nil` if it was
        /// still running 10 s later (it was then killed).
        let exitTime: Duration?
        /// With a follow-up trigger: the child's sheets and modal alert
        /// (``TerminationSignalChild/stateFile``) once the first trigger's
        /// expected state showed, or the last one seen if it never did.
        var stateAfterTrigger: String?
        /// With a follow-up trigger: whether the child still ran longer
        /// than ``AppDelegate/terminationGrace`` after the first trigger,
        /// in the expected state.
        var ranOnAfterTrigger: Bool?
        let reason: Process.TerminationReason
        let status: Int32
        let savedState: WindowState?
        let shellPid: pid_t?
        /// Whether the shell that wrote `shell.pid` still existed (even as
        /// a zombie) when the app had exited; `nil` without that file.
        let shellRemains: Bool?
        /// Whether `NSApplication.willTerminateNotification` was posted:
        /// the app quit through `terminate` rather than its own `exit`.
        let quitThroughTerminate: Bool
        /// The choice the quit recorded for the next launch
        /// (``WindowStateStore/restoresAtNextLaunchKey``), if any.
        let restoresAtNextLaunch: Bool?
        /// The questions ``TerminationSignalChild/AnsweringPresenter``
        /// answered.
        let confirmations: [String]
        let output: String
    }

    /// Sends `pid` a quit Apple event naming `reason` (as a parameter or
    /// an attribute), without waiting for a reply.
    private static func sendQuitEvent(to pid: pid_t, reason: OSType, asAttribute: Bool) throws {
        let event = NSAppleEventDescriptor(
            eventClass: AEEventClass(kCoreEventClass), eventID: AEEventID(kAEQuitApplication),
            targetDescriptor: NSAppleEventDescriptor(processIdentifier: pid),
            returnID: AEReturnID(kAutoGenerateReturnID), transactionID: AETransactionID(kAnyTransactionID))
        let value = NSAppleEventDescriptor(enumCode: reason)
        if asAttribute {
            event.setAttribute(value, forKeyword: AEKeyword(kAEQuitReason))
        } else {
            event.setParam(value, forKeyword: AEKeyword(kAEQuitReason))
        }
        _ = try event.sendEvent(options: [.noReply], timeout: 5)
    }

    private func send(_ trigger: Trigger, to child: Process) throws {
        switch trigger {
        case .signal(let signal):
            XCTAssertEqual(kill(child.processIdentifier, signal), 0)
        case .quitEvent:
            let application = try XCTUnwrap(
                NSRunningApplication(processIdentifier: child.processIdentifier), "the child is a running application")
            XCTAssertTrue(application.terminate(), "the quit event is sent")
        case .systemQuitEvent(let reason, let asAttribute):
            try Self.sendQuitEvent(to: child.processIdentifier, reason: reason, asAttribute: asAttribute)
        }
    }

    private func assertQuit(
        _ result: Result, within deadline: Duration, _ what: String, throughTerminate: Bool = true,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let tail = result.output.split(separator: "\n").suffix(15).joined(separator: "\n")
        guard let time = result.exitTime else {
            return XCTFail("\(what): still running 10 s after the signal; output:\n\(tail)", file: file, line: line)
        }
        XCTAssertLessThan(time, deadline, "\(what): exits in time", file: file, line: line)
        XCTAssertEqual(result.reason, .exit, "\(what): exits rather than dying of the signal", file: file, line: line)
        XCTAssertEqual(result.status, 0, "\(what): exit status; output:\n\(tail)", file: file, line: line)
        XCTAssertEqual(
            result.savedState?.windows.count, 1, "\(what): the window state is saved; output:\n\(tail)", file: file,
            line: line)
        XCTAssertEqual(result.savedState?.windows.first?.tabCount, 1, "\(what): with its tab", file: file, line: line)
        XCTAssertEqual(
            result.quitThroughTerminate, throughTerminate, "\(what): quits through terminate; output:\n\(tail)",
            file: file, line: line)
    }

    /// Starts a child for `mode` running `command`, waits until its sheet
    /// or alert is up, sends `signal` and waits for the exit.
    private func run(_ mode: TerminationSignalChild.Mode, command: String, signal: Int32) async throws -> Result {
        try await run(mode, command: command, trigger: .signal(signal))
    }

    /// Starts a child for `mode` running `command`, waits until its sheet
    /// or alert is up, sends `trigger` and waits for the exit. With
    /// `expected` and `followUp`, it first waits for the child to show the
    /// `expected` state and run on in it, then sends the `followUp` signal
    /// and times the exit from there.
    ///
    /// Without a trigger, the child is expected to quit on its own once
    /// ready. `quitKeepsWindows` stands in for macOS's setting.
    ///
    /// A child whose session spawn hangs in Thread Sanitizer's fork hook
    /// (#278, not fixed by design; the watchdog retries such a hang in the
    /// test host, but this one is in the child, which never gets ready) is
    /// started once more in a fresh directory: nothing was sent to it yet.
    /// The hang is found by sampling a child still not ready after
    /// ``forkHangProbe`` (that deadlock never resolves), so a retry costs
    /// about 25 s and a test with one stays well within the default 120 s
    /// allowance and the watchdog's 150 s. A second hang fails.
    private func run(
        _ mode: TerminationSignalChild.Mode, command: String, trigger: Trigger?, thenExpect expected: String? = nil,
        followedBy followUp: Int32? = nil, quitKeepsWindows: Bool? = nil
    ) async throws -> Result {
        func attempt() async throws -> Result {
            try await runOnce(
                mode, command: command, trigger: trigger, thenExpect: expected, followedBy: followUp,
                quitKeepsWindows: quitKeepsWindows)
        }
        do {
            return try await attempt()
        } catch let hang as ChildForkHang {
            print("TSan fork hang in the child (#278), retried once; the first try:\n\(hang)")
            do {
                return try await attempt()
            } catch let again as ChildForkHang {
                throw PollTimedOut(description: "the second try hung in TSan's fork hook too (#278): \(again)")
            }
        }
    }

    /// How long a loaded child may go without getting ready before it is
    /// sampled for the #278 fork hang, and again after each such period
    /// (a child gets ready in 11-16 s on CI).
    private static let forkHangProbe: Duration = .seconds(20)

    /// A child that never got ready because its main thread is stuck in
    /// Thread Sanitizer's fork hook (#278).
    private struct ChildForkHang: Error, CustomStringConvertible {
        let description: String
    }

    private func runOnce(
        _ mode: TerminationSignalChild.Mode, command: String, trigger: Trigger?, thenExpect expected: String?,
        followedBy followUp: Int32?, quitKeepsWindows: Bool?
    ) async throws -> Result {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-441-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        // A suite at a path in `dir`, not in ~/Library/Preferences:
        // cfprefsd writes what the child saved to its file seconds after
        // the child exits, after any cleanup here, and would leave it there
        // (#496).
        let suite = dir.appendingPathComponent("defaults").path
        let request = TerminationSignalChild.Request(
            mode: mode, command: command, suite: suite, quitKeepsWindows: quitKeepsWindows)
        try JSONEncoder().encode(request).write(to: dir.appendingPathComponent(TerminationSignalChild.requestFile))
        var jobPid: pid_t?
        var shellPid: pid_t?
        defer {
            // The job may outlive the app (it ignores SIGHUP, or its shell
            // was killed first).
            for pid in [jobPid, shellPid].compactMap(\.self) { kill(pid, SIGKILL) }
            try? FileManager.default.removeItem(at: dir)
        }

        let child = try TerminationSignalChild.launch(in: dir)
        let ready = dir.appendingPathComponent(TerminationSignalChild.readyFile)
        let failure = dir.appendingPathComponent(TerminationSignalChild.errorFile)
        let stages = dir.appendingPathComponent(TerminationSignalChild.stagesFile)
        let launched = ContinuousClock.now
        var nextProbe = Self.forkHangProbe
        let inFork = { (graph: [String]?) in graph.map(TerminationSignalChild.isForkHang) ?? false }
        while !FileManager.default.fileExists(atPath: ready.path) {
            if let error = try? String(contentsOf: failure, encoding: .utf8) {
                let diagnosis = TerminationSignalChild.diagnose(child, in: dir)
                if child.isRunning { kill(child.processIdentifier, SIGKILL) }
                child.waitUntilExit()
                throw PollTimedOut(description: "the child failed: \(error)\n\(diagnosis)")
            }
            // A child whose test bundle never loaded (XCTest not injected)
            // runs as the plain app and would never get ready.
            let loaded = FileManager.default.fileExists(atPath: stages.path)
            let waited = ContinuousClock.now - launched
            let timedOut = !child.isRunning || waited >= .seconds(60) || (!loaded && waited >= .seconds(20))
            var graph: [String]?
            if !timedOut, loaded, waited >= nextProbe {
                nextProbe += Self.forkHangProbe
                graph = TerminationSignalChild.callGraph(of: child, in: dir)
            }
            let forkHang = loaded && child.isRunning && inFork(graph)
            if timedOut || forkHang {
                let what: String
                if forkHang {
                    what = "hung in TSan's fork hook"
                } else if !child.isRunning {
                    what = "exited"
                } else {
                    what = loaded ? "never got ready" : "never loaded the test bundle"
                }
                if child.isRunning, graph == nil { graph = TerminationSignalChild.callGraph(of: child, in: dir) }
                let diagnosis = TerminationSignalChild.diagnose(child, in: dir, graph: graph)
                let hung = forkHang || (loaded && child.isRunning && inFork(graph))
                if child.isRunning { kill(child.processIdentifier, SIGKILL) }
                child.waitUntilExit()
                let description = "the child \(what) after \(waited)\n\(diagnosis)"
                if hung { throw ChildForkHang(description: description) }
                throw PollTimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(20))
        }
        jobPid = try JSONDecoder().decode(TerminationSignalChild.Ready.self, from: Data(contentsOf: ready)).jobPid
        shellPid = (try? String(contentsOf: dir.appendingPathComponent("shell.pid"), encoding: .utf8))
            .flatMap { pid_t($0.trimmingCharacters(in: .whitespacesAndNewlines)) }

        if let trigger {
            try send(trigger, to: child)
        }
        var stateAfterTrigger: String?
        var ranOnAfterTrigger: Bool?
        if let expected, let followUp {
            // The expected state must show and hold past the grace after
            // which ``AppDelegate/terminateWithoutAsking()`` would exit.
            let stateFile = dir.appendingPathComponent(TerminationSignalChild.stateFile)
            func state() -> String? { try? String(contentsOf: stateFile, encoding: .utf8) }
            let sent = ContinuousClock.now
            while state() != expected, child.isRunning, ContinuousClock.now - sent < .seconds(5) {
                try await Task.sleep(for: .milliseconds(20))
            }
            if state() == expected {
                try await Task.sleep(for: .milliseconds(1500))
            }
            stateAfterTrigger = state()
            ranOnAfterTrigger = child.isRunning && stateAfterTrigger == expected
            if child.isRunning {
                try send(.signal(followUp), to: child)
            }
        }
        let start = ContinuousClock.now
        var exitTime: Duration?
        while ContinuousClock.now - start < .seconds(10) {
            if !child.isRunning {
                exitTime = ContinuousClock.now - start
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        if exitTime == nil {
            kill(child.processIdentifier, SIGKILL)
        }
        child.waitUntilExit()
        // Before the cleanup above kills it.
        let shellRemains = shellPid.map { kill($0, 0) == 0 }
        let savedDefaults = UserDefaults(suiteName: suite)
        let saved = savedDefaults.flatMap { WindowStateStore(defaults: $0).load() }
        let restoresAtNextLaunch = savedDefaults?.object(forKey: WindowStateStore.restoresAtNextLaunchKey) as? Bool
        let confirmations = ((try? String(
            contentsOf: dir.appendingPathComponent(TerminationSignalChild.confirmationsFile), encoding: .utf8)) ?? "")
            .split(separator: "\n").map(String.init)
        let output = (try? String(contentsOf: dir.appendingPathComponent("output.log"), encoding: .utf8)) ?? ""
        let quitThroughTerminate = FileManager.default.fileExists(
            atPath: dir.appendingPathComponent(TerminationSignalChild.willTerminateFile).path)
        return Result(
            exitTime: exitTime, stateAfterTrigger: stateAfterTrigger, ranOnAfterTrigger: ranOnAfterTrigger,
            reason: child.terminationReason, status: child.terminationStatus, savedState: saved, shellPid: shellPid,
            shellRemains: shellRemains, quitThroughTerminate: quitThroughTerminate,
            restoresAtNextLaunch: restoresAtNextLaunch, confirmations: confirmations, output: output)
    }
}

/// The child side of ``TerminationSignalTests``: a copy of the test host
/// started with ``directoryVariable`` set runs this from the test bundle's
/// start (``HostDisplaySettings``) instead of any test, and never returns.
/// It reports through files in that directory: ``readyFile`` once the
/// sheet or alert is up, ``errorFile`` if it could not get there.
@MainActor
enum TerminationSignalChild {
    nonisolated static let directoryVariable = "TESSERA_TESTS_SIGNAL_CHILD"
    static let requestFile = "request.json"
    static let readyFile = "ready.json"
    static let errorFile = "error.txt"
    /// One line per stage the child reached (``mark(_:)``).
    nonisolated static let stagesFile = "stages.log"
    /// How the parent launched the child, for a failure's diagnosis.
    static let launchFile = "launch.txt"
    /// Written when `NSApplication.willTerminateNotification` is posted.
    static let willTerminateFile = "will-terminate"
    /// The child's sheets and modal alert once its sheet or alert is up,
    /// rewritten as they change: `sheets=<count> modal=<button titles, sorted>`
    /// (``reportState(in:)``).
    static let stateFile = "state.txt"

    enum Mode: String, Codable {
        /// ⌘W: the close sheet.
        case sheet
        /// ⌘Q: the application-modal quit alert.
        case alert
        /// A modal session whose runner ignores `abortModal`: something
        /// that keeps `terminate` from ending the app.
        case stubbornModal
        /// An application-modal error alert (``AppDelegate/presentOpenError(_:)``),
        /// the job still running.
        case errorAlert
        /// The same with the job ended first: nothing to ask about.
        case idleErrorAlert
        /// ⌥⌘W: Close All's application-modal alert (#464).
        case closeAllAlert
        /// ⌥⌘Q: the quit alert of Quit's alternate (#464).
        case quitAlternateAlert
        /// ⌥⌘Q with two windows, three tabs and two jobs (`sleep` and
        /// `cat`), answered Quit by ``AnsweringPresenter``: the child quits
        /// on its own.
        case quitAlternate
    }

    struct Request: Codable {
        let mode: Mode
        /// Typed into the shell; runs in the child's directory.
        let command: String
        /// The defaults suite the window state is saved to.
        let suite: String
        /// What macOS's "Close windows when quitting an application" says
        /// (``AppDelegate/quitKeepsWindows``); `nil` for the real setting.
        var quitKeepsWindows: Bool?
    }

    /// Where ``AnsweringPresenter`` writes each question it answered, one
    /// line each: `<scope> <names, comma-separated>`.
    static let confirmationsFile = "confirmations.txt"

    /// Answers every application-modal question Quit (or Close), after a
    /// save of the window state such as one the main queue could run while
    /// a real alert asks; a sheet is answered Cancel. Each question is
    /// written to ``confirmationsFile``.
    final class AnsweringPresenter: CloseConfirmationPresenter {
        private let file: URL
        private let whileAsking: @MainActor () -> Void

        init(file: URL, whileAsking: @escaping @MainActor () -> Void) {
            self.file = file
            self.whileAsking = whileAsking
        }

        private func record(_ confirmation: CloseConfirmation, _ kind: String) {
            let line = "\(kind)\(confirmation.scope) \(confirmation.processNames.joined(separator: ","))\n"
            let previous = (try? String(contentsOf: file, encoding: .utf8)) ?? ""
            try? (previous + line).write(to: file, atomically: true, encoding: .utf8)
        }

        func confirm(
            _ confirmation: CloseConfirmation, sheetFor window: NSWindow,
            completion: @escaping @MainActor (Bool) -> Void
        ) {
            record(confirmation, "sheet ")
            completion(false)
        }

        func confirmApplicationModally(_ confirmation: CloseConfirmation) -> Bool {
            record(confirmation, "")
            whileAsking()
            return true
        }
    }

    struct Ready: Codable {
        let jobPid: pid_t
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    /// Starts a copy of this test host that runs the child in `dir`
    /// (``requestFile`` there says what), its output in `output.log`.
    /// XCTest in the copy reads a copy of this run's configuration that
    /// reports to no IDE, from a file.
    static func launch(in dir: URL) throws -> Process {
        guard let configurationClass = NSClassFromString("XCTestConfiguration") as? NSObject.Type,
            let active = configurationClass.perform(NSSelectorFromString("activeTestConfiguration"))?
                .takeUnretainedValue() as? NSObject
        else {
            // Not a skip: a skip would hide every proof of #441 on CI.
            throw Failure(description: "this XCTest has no XCTestConfiguration to start a child test host with")
        }
        let unarchiver = try NSKeyedUnarchiver(
            forReadingFrom: try NSKeyedArchiver.archivedData(withRootObject: active, requiringSecureCoding: false))
        unarchiver.requiresSecureCoding = false
        guard let configuration = unarchiver.decodeObject(forKey: NSKeyedArchiveRootObjectKey) as? NSObject else {
            throw Failure(description: "the test configuration did not copy")
        }
        configuration.setValue(false, forKey: "reportResultsToIDE")
        configuration.setValue(UUID(), forKey: "sessionIdentifier")
        let configurationFile = dir.appendingPathComponent("child.xctestconfiguration")
        try NSKeyedArchiver.archivedData(withRootObject: configuration, requiringSecureCoding: false)
            .write(to: configurationFile)

        var environment = ProcessInfo.processInfo.environment
        let inherited = environment.filter { $0.key.hasPrefix("DYLD_") || $0.key.hasPrefix("XC") }
        environment["XCTestConfigurationFilePath"] = configurationFile.path
        environment["XCTestSessionIdentifier"] = nil
        // XCTest's injector removes itself from DYLD_INSERT_LIBRARIES once
        // loaded, so a child would start as the plain app. Xcode 26 and
        // later name it in XCTestBundleInjectPath; Xcode 16 embeds it in the
        // host's Frameworks and names it nowhere, but it stays loaded here.
        guard let inject = injectorPath() ?? environment["XCTestBundleInjectPath"] else {
            throw Failure(description: "no XCTest injector to start a child test host with; environment: \(inherited)")
        }
        let inserted = (environment["DYLD_INSERT_LIBRARIES"] ?? "").split(separator: ":").map(String.init)
        if !inserted.contains(inject) {
            environment["DYLD_INSERT_LIBRARIES"] = (inserted + [inject]).joined(separator: ":")
        }
        environment[directoryVariable] = dir.path
        // Thread Sanitizer sleeps 1 s at exit by default (2 s measured); the
        // exit deadline measures the app, not that.
        environment["TSAN_OPTIONS"] = [environment["TSAN_OPTIONS"], "atexit_sleep_ms=0"]
            .compactMap(\.self).filter { !$0.isEmpty }.joined(separator: ":")

        let output = dir.appendingPathComponent("output.log")
        FileManager.default.createFile(atPath: output.path, contents: Data())
        let child = Process()
        child.executableURL = Bundle.main.executableURL
        child.arguments = Array(ProcessInfo.processInfo.arguments.dropFirst())
        child.environment = environment
        child.currentDirectoryURL = dir
        child.standardOutput = try FileHandle(forWritingTo: output)
        child.standardError = child.standardOutput
        let launchSummary = [
            "inherited: \(inherited.sorted { $0.key < $1.key })",
            "child DYLD_INSERT_LIBRARIES: \(environment["DYLD_INSERT_LIBRARIES"] ?? "")",
        ].joined(separator: "\n")
        try launchSummary.write(to: dir.appendingPathComponent(launchFile), atomically: true, encoding: .utf8)
        try child.run()
        return child
    }

    /// The path of XCTest's injector library loaded in this process.
    private static func injectorPath() -> String? {
        (0..<_dyld_image_count()).lazy.compactMap { _dyld_get_image_name($0).map { String(cString: $0) } }
            .first { URL(fileURLWithPath: $0).lastPathComponent == "libXCTestBundleInject.dylib" }
    }

    /// Records that the child reached `stage` (``stagesFile``); a no-op in
    /// a process that is no child.
    nonisolated static func mark(_ stage: String) {
        guard let path = ProcessInfo.processInfo.environment[directoryVariable] else { return }
        let file = URL(fileURLWithPath: path).appendingPathComponent(stagesFile)
        if !FileManager.default.fileExists(atPath: file.path) {
            FileManager.default.createFile(atPath: file.path, contents: Data())
        }
        guard let handle = try? FileHandle(forWritingTo: file) else { return }
        defer { try? handle.close() }
        _ = try? handle.seekToEnd()
        try? handle.write(contentsOf: Data("\(stage)\n".utf8))
    }

    /// What the parent knows of a child that did not get ready: whether it
    /// runs or how it ended, the stages it reached, how it was launched,
    /// its output and, while it runs, where its threads are (from `graph`,
    /// a ``callGraph(of:in:)`` already taken, or a new sample).
    static func diagnose(_ child: Process, in dir: URL, graph: [String]? = nil) -> String {
        func read(_ name: String) -> String {
            (try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8)) ?? "(none)"
        }
        var lines = [
            child.isRunning
                ? "state: running (pid \(child.processIdentifier))"
                : "state: \(child.terminationReason == .exit ? "exited with status" : "died of signal") "
                    + "\(child.terminationStatus)",
            "stages:\n\(read(stagesFile))",
            read(launchFile),
            "output (tail):\n\(read("output.log").suffix(2000))",
        ]
        if let graph = graph ?? (child.isRunning ? callGraph(of: child, in: dir) : nil) {
            let text = read(sampleFile).split(separator: "\n", omittingEmptySubsequences: false)
            let images = text.drop { !$0.contains("Binary Images:") }
                .filter { $0.contains("XCTest") || $0.contains("Inject") || $0.contains("tsan") }
            lines.append("threads:\n\(graph.joined(separator: "\n"))")
            lines.append("XCTest images:\n\(images.joined(separator: "\n"))")
        }
        return lines.joined(separator: "\n")
    }

    private static let sampleFile = "sample.txt"

    /// How many lines of the call graph a ``diagnose(_:in:graph:)`` keeps
    /// (#526: a 40-line cut hid a stalled launch). At least 200, the number
    /// #526 asks for. The child is a copy of the test host, so its graph can
    /// be as large as the host's: the watchdog's sample of the host in the
    /// macOS 27 #278 hang (CI run 44917832924, 71 threads stuck in the
    /// runtime) had a 986-line call graph.
    static let callGraphLineLimit = 1000

    /// Samples the running `child` for a second (into ``sampleFile``) and
    /// returns every thread's stack: a stalled launch can sit deeper than
    /// the first screenful, or on another thread than the main one.
    static func callGraph(of child: Process, in dir: URL) -> [String] {
        let sample = Process()
        sample.executableURL = URL(fileURLWithPath: "/usr/bin/sample")
        let file = dir.appendingPathComponent(sampleFile)
        try? FileManager.default.removeItem(at: file)
        sample.arguments = ["\(child.processIdentifier)", "1", "-mayDie", "-file", file.path]
        sample.standardOutput = FileHandle.nullDevice
        sample.standardError = FileHandle.nullDevice
        if (try? sample.run()) != nil { sample.waitUntilExit() }
        let text = ((try? String(contentsOf: file, encoding: .utf8)) ?? "")
            .split(separator: "\n", omittingEmptySubsequences: false)
        return text.drop { !$0.contains("Call graph:") }
            .prefix { !$0.contains("Total number in stack") }
            .prefix(callGraphLineLimit)
            .map(String.init)
    }

    /// Whether a thread of `graph` (a ``callGraph(of:in:)``) spent the whole
    /// sample inside Thread Sanitizer's fork interceptor, at an atfork
    /// prepare handler or in the TSan runtime: the #278 deadlock. Only the
    /// sample's stacks are read, never the child's output.
    static func isForkHang(_ graph: [String]) -> Bool {
        // A line is "<indent and tree marks><count> <frame>"; a thread's
        // first line is "    <count> Thread_<id>...".
        func count(_ line: String) -> Int? {
            let trimmed = line.drop { " +!:|".contains($0) }
            return Int(trimmed.prefix { $0.isNumber })
        }
        var threads: [[String]] = []
        for line in graph.dropFirst() {
            if line.drop(while: { $0 == " " }).drop(while: \.isNumber).hasPrefix(" Thread_") {
                threads.append([line])
            } else if !threads.isEmpty {
                threads[threads.count - 1].append(line)
            }
        }
        return threads.contains { thread in
            guard let total = count(thread[0]) else { return false }
            let stuck = thread.contains { $0.contains("wrap_fork") && count($0) == total }
            return stuck && thread.contains { $0.contains("atfork_prepare") || $0.contains("__tsan::") }
        }
    }

    /// Runs the child if this process is one; then never returns.
    static func runIfRequested() {
        guard let path = ProcessInfo.processInfo.environment[directoryVariable] else { return }
        let dir = URL(fileURLWithPath: path)
        mark("the test bundle started")
        // A cap of the child's own, whatever its mode: the quit alert's
        // `runModal` would wait forever if the parent died before
        // signalling (Xcode's Stop, a crash).
        let failure = dir.appendingPathComponent(errorFile)
        DispatchQueue.global().asyncAfter(deadline: .now() + .seconds(90)) {
            try? "no signal came within 90 s".write(to: failure, atomically: true, encoding: .utf8)
            _exit(3)
        }
        do {
            try run(in: dir)
            report("no signal ended the app within 30 s", in: dir)
            exit(3)
        } catch {
            report("\(error)", in: dir)
            exit(2)
        }
    }

    /// Atomic: the parent decodes the file as soon as it exists.
    private static func reportReady(_ jobPid: pid_t, in dir: URL) throws {
        try JSONEncoder().encode(Ready(jobPid: jobPid))
            .write(to: dir.appendingPathComponent(readyFile), options: .atomic)
    }

    /// Reports ready once an application-modal window is up.
    private static func reportReadyOnceModal(_ jobPid: pid_t, in dir: URL) {
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(20)) {
            MainActor.assumeIsolated {
                guard NSApp.modalWindow != nil else { return reportReadyOnceModal(jobPid, in: dir) }
                do { try reportReady(jobPid, in: dir) } catch { report("\(error)", in: dir) }
            }
        }
    }

    /// Writes ``stateFile`` whenever the sheets or the modal alert change,
    /// from the main queue (which runs in a modal session too).
    private static func reportState(in dir: URL, last: String? = nil) {
        func buttons(in view: NSView?) -> [String] {
            guard let view else { return [] }
            return ((view as? NSButton).map { [$0.title] } ?? []) + view.subviews.flatMap { buttons(in: $0) }
        }
        let sheets = NSApp.windows.reduce(0) { $0 + $1.sheets.count }
        let modal = buttons(in: NSApp.modalWindow?.contentView).filter { !$0.isEmpty }.sorted().joined(separator: ",")
        let state = "sheets=\(sheets) modal=\(modal)"
        if state != last {
            try? state.write(to: dir.appendingPathComponent(stateFile), atomically: true, encoding: .utf8)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(20)) {
            MainActor.assumeIsolated { reportState(in: dir, last: state) }
        }
    }

    private static func report(_ error: String, in dir: URL) {
        try? error.write(to: dir.appendingPathComponent(errorFile), atomically: true, encoding: .utf8)
    }

    private static func run(in dir: URL) throws {
        let request = try JSONDecoder().decode(
            Request.self, from: Data(contentsOf: dir.appendingPathComponent(requestFile)))
        guard let appDelegate = NSApp.delegate as? AppDelegate, let defaults = UserDefaults(suiteName: request.suite)
        else { throw Failure(description: "no app delegate or defaults suite") }
        appDelegate.windowStateStore = WindowStateStore(defaults: defaults)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"], workingDirectory: dir)
        if let keeps = request.quitKeepsWindows {
            appDelegate.quitKeepsWindows = { keeps }
        }
        let controller = try appDelegate.newWindow()
        mark("a window opened")
        let session = controller.focusedPane.session
        try session.write(Array("\(request.command)\n".utf8))
        var job: TesseraSession.ForegroundProcess?
        let started = try spin(timeout: .seconds(10)) {
            job = try session.foregroundProcess().flatMap { $0.name == "sleep" ? $0 : nil }
            return job != nil
        }
        guard started, let job, let window = controller.window else {
            throw Failure(description: "sleep did not start in the foreground")
        }
        mark("the job runs in the foreground")
        // Only the signal's quit may save it from here on.
        defaults.removeObject(forKey: WindowStateStore.key)
        let willTerminate = dir.appendingPathComponent(willTerminateFile)
        _ = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification, object: nil, queue: nil
        ) { _ in
            FileManager.default.createFile(atPath: willTerminate.path, contents: Data())
        }
        reportState(in: dir)
        switch request.mode {
        case .sheet:
            window.performClose(nil)
            guard try spin(timeout: .seconds(10), until: { window.attachedSheet != nil }) else {
                throw Failure(description: "no close sheet")
            }
            try reportReady(job.pid, in: dir)
        case .alert:
            // ⌘Q's path: the alert runs modally inside `terminate`, so the
            // ready file is written from within its modal session (the
            // main queue runs there).
            reportReadyOnceModal(job.pid, in: dir)
            NSApp.terminate(nil)
            throw Failure(description: "the quit alert's terminate returned: the app was not quit")
        case .stubbornModal:
            let panel = NSPanel(
                contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
                defer: false)
            let modal = NSApp.beginModalSession(for: panel)
            try reportReady(job.pid, in: dir)
            let deadline = ContinuousClock.now + .seconds(30)
            while ContinuousClock.now < deadline {
                // The signal's abort makes this return `.abort`, ignored;
                // the run loop still runs (the main queue with it).
                _ = NSApp.runModalSession(modal)
                RunLoop.current.run(mode: .modalPanel, before: Date(timeIntervalSinceNow: 0.02))
            }
            NSApp.endModalSession(modal)
            return
        case .errorAlert, .idleErrorAlert:
            if request.mode == .idleErrorAlert {
                kill(job.pid, SIGKILL)
                let idle = try spin(timeout: .seconds(10)) {
                    CloseConfirmation(.quit, sessions: appDelegate.windowControllers.flatMap(\.sessions)) == nil
                }
                guard idle else { throw Failure(description: "the session stayed busy") }
            }
            reportReadyOnceModal(job.pid, in: dir)
            appDelegate.presentOpenError(Failure(description: "an error"))
            throw Failure(description: "the error alert returned: the app was not quit")
        case .closeAllAlert, .quitAlternateAlert:
            // As the keyboard sends ⌥⌘W or ⌥⌘Q to the menu bar; the alert
            // runs modally inside, so ready is written from its session.
            reportReadyOnceModal(job.pid, in: dir)
            let closesAll = request.mode == .closeAllAlert
            guard let bar = NSApp.mainMenu, let key = keyDown(closesAll ? "w" : "q", keyCode: closesAll ? 13 : 12)
            else { throw Failure(description: "no menu bar or key event") }
            _ = bar.performKeyEquivalent(with: key)
            throw Failure(description: "the alert returned: the app was not quit")
        case .quitAlternate:
            _ = try appDelegate.newTab(in: window)
            let second = try appDelegate.newWindow()
            try second.focusedPane.session.write(Array("cat\n".utf8))
            let catRuns = try spin(timeout: .seconds(10)) {
                try second.focusedPane.session.foregroundProcess()?.name == "cat"
            }
            guard catRuns else { throw Failure(description: "cat did not start in the foreground") }
            mark("two windows, three tabs, two jobs")
            appDelegate.closeConfirmationPresenter = AnsweringPresenter(
                file: dir.appendingPathComponent(confirmationsFile)
            ) { [weak appDelegate] in appDelegate?.saveWindowState() }
            try reportReady(job.pid, in: dir)
            guard let bar = NSApp.mainMenu, let key = keyDown("q", keyCode: 12) else {
                throw Failure(description: "no menu bar or key event")
            }
            _ = bar.performKeyEquivalent(with: key)
            throw Failure(description: "⌥⌘Q returned: the app was not quit")
        }
        try spin(timeout: .seconds(30)) { false }
    }

    /// Runs the app's event loop, as the app's own loop would (the
    /// signal's handler runs on the main queue), until `condition` holds or
    /// `timeout` has passed; whether it held.
    @discardableResult
    private static func spin(timeout: Duration, until condition: () throws -> Bool) throws -> Bool {
        let deadline = ContinuousClock.now + timeout
        while try !condition() {
            guard ContinuousClock.now < deadline else { return false }
            if let event = NSApp.nextEvent(
                matching: .any, until: Date(timeIntervalSinceNow: 0.02), inMode: .default, dequeue: true)
            {
                NSApp.sendEvent(event)
            }
        }
        return true
    }
}
