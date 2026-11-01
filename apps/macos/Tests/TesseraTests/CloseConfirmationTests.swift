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
        let session = controller.session

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
        XCTAssertEqual(try busy.session.foregroundProcess()?.name, "sleep", "and its job")
        XCTAssertNil(busy.pendingClose)

        windows[0].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 2, "asks again after Cancel")
        presenter.sheets[1].answer(true)
        XCTAssertFalse(appDelegate.windowControllers.contains { $0 === busy }, "Close closes the tab")
        XCTAssertThrowsError(try busy.session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertEqual(appDelegate.windowControllers.count, 1, "the other tab stays")
    }

    func testAfterTheJobEndsTheTabClosesWithoutAsking() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        try controller.session.write([0x03])
        try await waitUntil("Ctrl-C ends sleep") { try controller.session.foregroundProcess() == nil }

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
        try controller.session.write(Array("sh \(script.path)\n".utf8))
        let job = try await poll("the copied sleep runs") {
            try controller.session.foregroundProcess().flatMap { $0.name.hasPrefix("z") ? $0 : nil }
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
        try controller.session.write(Array("\(command)\n".utf8))
        _ = try await poll("\(name) runs in the foreground") {
            try controller.session.foregroundProcess().flatMap { $0.name == name ? $0 : nil }
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
