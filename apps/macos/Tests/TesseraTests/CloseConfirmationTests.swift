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
    private(set) var quits: [CloseConfirmation] = []
    /// What ``confirmQuit(_:)`` answers.
    var quitAnswer = false

    func confirm(
        _ confirmation: CloseConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void
    ) {
        sheets.append(Sheet(confirmation: confirmation, window: window, answer: completion))
    }

    func confirmQuit(_ confirmation: CloseConfirmation) -> Bool {
        quits.append(confirmation)
        return quitAnswer
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
        let script = dir.appendingPathComponent("run.sh")
        try "exec \"$(dirname \"$0\")\"/z*q\n".write(to: script, atomically: true, encoding: .utf8)
        let controller = try appDelegate.newWindow()
        try controller.focusedPane.session.write(Array("sh \(script.path)\n".utf8))
        let job = try await poll("the copied sleep runs") {
            try controller.focusedPane.session.foregroundProcess().flatMap { $0.name.hasPrefix("z") ? $0 : nil }
        }
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
    /// the Right, Close Window from the Window menu, Close All) ask each tab
    /// through `windowShouldClose`: a busy tab's sheet must hold back only
    /// that tab, so the idle tabs close and every busy tab asks.
    func testCloseOtherTabsClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseOtherTabs:", on: 0, keeps: [0])
    }

    func testCloseTabsAfterClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseTabsAfter:", on: 0, keeps: [0])
    }

    func testCloseTabbedWindowGroupClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("performCloseTabbedWindowGroup:", on: 0, keeps: [])
    }

    func testCloseAllClosesTheIdleTabsAndAsksEveryBusyOne() async throws {
        try await checkAppKitClose("closeAll:", on: nil, keeps: [])
    }

    /// Close All (⌥⌘W, the alternate AppKit adds to Close Tab) over two
    /// windows: every idle tab closes, every busy tab of either window asks
    /// once on its own sheet, and a second Close All while they ask adds
    /// none. Cancel keeps a tab; Close closes it.
    func testCloseAllAsksOncePerBusyTabOfEveryWindow() async throws {
        let selector = NSSelectorFromString("closeAll:")
        guard NSApp.responds(to: selector) else {
            throw XCTSkip("NSApplication does not answer closeAll: on this macOS")
        }
        let first = try openTabs(2)
        let second = try openTabs(3)
        let busy = [first[1], second[0], second[1]]
        for window in busy {
            try await start("sleep 100", in: try controller(of: window))
        }

        NSApp.perform(selector, with: nil)

        XCTAssertFalse(first[0].isVisible, "idle tabs close")
        XCTAssertFalse(second[2].isVisible, "idle tabs close")
        XCTAssertEqual(
            Set(presenter.sheets.map { ObjectIdentifier($0.window) }), Set(busy.map(ObjectIdentifier.init)),
            "every busy tab asks")
        XCTAssertEqual(presenter.sheets.count, 3, "once each")
        XCTAssertTrue(
            presenter.sheets.allSatisfy { $0.confirmation == CloseConfirmation(scope: .tab, processNames: ["sleep"]) })

        NSApp.perform(selector, with: nil)
        XCTAssertEqual(presenter.sheets.count, 3, "a second Close All while they ask asks nothing more")

        for sheet in presenter.sheets {
            sheet.answer(sheet.window !== second[1])
        }
        XCTAssertEqual(
            appDelegate.windowControllers.compactMap(\.window).map(ObjectIdentifier.init), [ObjectIdentifier(second[1])],
            "Cancel keeps its tab, Close closes the others")
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

    /// Tabs [idle, busy, idle, busy]: `action` sent to tab `target` (or to
    /// the application) closes the idle tabs it covers at once and asks each
    /// busy one on its own sheet; after Close only the tabs at `keeps` stay.
    private func checkAppKitClose(_ action: String, on target: Int?, keeps: [Int]) async throws {
        let selector = NSSelectorFromString(action)
        let windows = try openTabs(4)
        for index in [1, 3] {
            try await start("sleep 100", in: try controller(of: windows[index]))
        }
        windows[0].tabGroup?.selectedWindow = windows[0]
        let receiver: NSObject = target.map { windows[$0] } ?? NSApp
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

        presenter.quitAnswer = false
        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateCancel)
        XCTAssertEqual(presenter.quits, [CloseConfirmation(scope: .quit, processNames: ["sleep", "cat"])])
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel keeps everything open")
        XCTAssertTrue(presenter.sheets.isEmpty)

        presenter.quitAnswer = true
        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertEqual(presenter.quits.count, 2)
    }

    /// `SIGTERM`, `SIGINT` and `SIGHUP` quit without asking: nobody may be
    /// there to answer.
    func testASignalQuitDoesNotAsk() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        appDelegate.terminatesWithoutConfirmation = true
        defer { appDelegate.terminatesWithoutConfirmation = false }

        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertTrue(presenter.quits.isEmpty)
    }

    func testQuitWithIdleSessionsDoesNotAsk() throws {
        _ = try openTabs(2)

        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)
        XCTAssertTrue(presenter.quits.isEmpty)
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

    /// Builds a program that sleeps 100 s at `binary` with the toolchain's
    /// C compiler, or skips the test without one.
    private func compileSleeper(to binary: URL) throws {
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

struct PollTimedOut: Error, CustomStringConvertible {
    let description: String
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
    /// ``AppDelegate/signalTerminationGrace``, with the window state saved
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

    // MARK: Helpers

    struct Result {
        /// From the signal to the child's exit; `nil` if it was still
        /// running 10 s later (it was then killed).
        let exitTime: Duration?
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
        let output: String
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
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-441-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let suite = "com.aanyberg.tessera.tests.\(UUID().uuidString)"
        let request = TerminationSignalChild.Request(mode: mode, command: command, suite: suite)
        try JSONEncoder().encode(request).write(to: dir.appendingPathComponent(TerminationSignalChild.requestFile))
        var jobPid: pid_t?
        var shellPid: pid_t?
        defer {
            // The job may outlive the app (it ignores SIGHUP, or its shell
            // was killed first).
            for pid in [jobPid, shellPid].compactMap(\.self) { kill(pid, SIGKILL) }
            UserDefaults.standard.removePersistentDomain(forName: suite)
            try? FileManager.default.removeItem(at: dir)
        }

        let child = try TerminationSignalChild.launch(in: dir)
        let ready = dir.appendingPathComponent(TerminationSignalChild.readyFile)
        let failure = dir.appendingPathComponent(TerminationSignalChild.errorFile)
        let stages = dir.appendingPathComponent(TerminationSignalChild.stagesFile)
        let launched = ContinuousClock.now
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
            guard child.isRunning, waited < .seconds(60), loaded || waited < .seconds(20) else {
                let what = !child.isRunning ? "exited" : loaded ? "never got ready" : "never loaded the test bundle"
                let diagnosis = TerminationSignalChild.diagnose(child, in: dir)
                if child.isRunning { kill(child.processIdentifier, SIGKILL) }
                child.waitUntilExit()
                throw PollTimedOut(description: "the child \(what) after \(waited)\n\(diagnosis)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
        jobPid = try JSONDecoder().decode(TerminationSignalChild.Ready.self, from: Data(contentsOf: ready)).jobPid
        shellPid = (try? String(contentsOf: dir.appendingPathComponent("shell.pid"), encoding: .utf8))
            .flatMap { pid_t($0.trimmingCharacters(in: .whitespacesAndNewlines)) }

        XCTAssertEqual(kill(child.processIdentifier, signal), 0)
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
        let saved = UserDefaults(suiteName: suite).flatMap { WindowStateStore(defaults: $0).load() }
        let output = (try? String(contentsOf: dir.appendingPathComponent("output.log"), encoding: .utf8)) ?? ""
        let quitThroughTerminate = FileManager.default.fileExists(
            atPath: dir.appendingPathComponent(TerminationSignalChild.willTerminateFile).path)
        return Result(
            exitTime: exitTime, reason: child.terminationReason, status: child.terminationStatus, savedState: saved,
            shellPid: shellPid, shellRemains: shellRemains, quitThroughTerminate: quitThroughTerminate, output: output)
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

    enum Mode: String, Codable {
        /// ⌘W: the close sheet.
        case sheet
        /// ⌘Q: the application-modal quit alert.
        case alert
        /// A modal session whose runner ignores `abortModal`: something
        /// that keeps `terminate` from ending the app.
        case stubbornModal
    }

    struct Request: Codable {
        let mode: Mode
        /// Typed into the shell; runs in the child's directory.
        let command: String
        /// The defaults suite the window state is saved to.
        let suite: String
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
    /// its output and, while it runs, where its main thread is.
    static func diagnose(_ child: Process, in dir: URL) -> String {
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
        if child.isRunning {
            let sample = Process()
            sample.executableURL = URL(fileURLWithPath: "/usr/bin/sample")
            let file = dir.appendingPathComponent("sample.txt")
            sample.arguments = ["\(child.processIdentifier)", "1", "-mayDie", "-file", file.path]
            sample.standardOutput = FileHandle.nullDevice
            sample.standardError = FileHandle.nullDevice
            if (try? sample.run()) != nil { sample.waitUntilExit() }
            let text = read("sample.txt").split(separator: "\n", omittingEmptySubsequences: false)
            let graph = text.drop { !$0.contains("Call graph:") }.prefix(40)
            let images = text.drop { !$0.contains("Binary Images:") }
                .filter { $0.contains("XCTest") || $0.contains("Inject") || $0.contains("tsan") }
            lines.append("main thread:\n\(graph.joined(separator: "\n"))")
            lines.append("XCTest images:\n\(images.joined(separator: "\n"))")
        }
        return lines.joined(separator: "\n")
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

    private static func reportReady(_ jobPid: pid_t, in dir: URL) throws {
        try JSONEncoder().encode(Ready(jobPid: jobPid)).write(to: dir.appendingPathComponent(readyFile))
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
