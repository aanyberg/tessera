import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// A key-down of ⌥⌘ and `key` (`keyCode` its US key), as the keyboard
/// sends it, to hand the installed menu bar (`performKeyEquivalent`).
@MainActor
func keyDown(
    _ key: String, keyCode: UInt16, _ modifiers: NSEvent.ModifierFlags = [.command, .option]
) -> NSEvent? {
    NSEvent.keyEvent(
        with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0, windowNumber: 0, context: nil,
        characters: key, charactersIgnoringModifiers: key, isARepeat: false, keyCode: keyCode)
}

/// Quit's alternate (⌥⌘Q: Quit and Keep Windows, or Quit and Close All
/// Windows) and Close Tab's (⌥⌘W: Close All) ask once, application-modally,
/// for every busy session (#464, ADR 0031). Both are driven as the
/// keyboard drives them, through the installed menu bar's key
/// equivalents, on windows opened as ⌘N and ⌘T open them; nothing is
/// posted. Only Cancel is answered here (Quit would end the test host):
/// ``TerminationSignalTests`` quits a child copy of it.
@MainActor
final class QuitAndCloseAllTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var presenter: RecordingCloseConfirmationPresenter!
    private var previousPresenter: (any CloseConfirmationPresenter)?
    private var previousQuitKeepsWindows: (@MainActor () -> Bool)?
    private var defaults: CountingDefaults!
    /// What macOS's "Close windows when quitting an application" says, as
    /// ``AppDelegate/quitKeepsWindows`` reads it here.
    private var keepsWindows = false
    /// The default key map, to put back on the installed menus after a
    /// test re-binds them.
    private var defaultKeymap: TesseraKeymap?
    private var cores: [TesseraApp] = []
    private var configDirs: [URL] = []
    private var previousConfigDir: String?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertNil(appDelegate.windowStateStore, "hosted tests must not save to the user's defaults")
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        presenter = RecordingCloseConfirmationPresenter()
        previousPresenter = appDelegate.closeConfirmationPresenter
        appDelegate.closeConfirmationPresenter = presenter
        previousQuitKeepsWindows = appDelegate.quitKeepsWindows
        appDelegate.quitKeepsWindows = { [weak self] in self?.keepsWindows ?? false }
        defaults = try XCTUnwrap(CountingDefaults(suiteName: "com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        appDelegate.windowStateStore = WindowStateStore(defaults: defaults)
        previousConfigDir = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.settingsWindowController?.close()
        if let defaultKeymap, let bar = NSApp.mainMenu {
            MainMenu.apply(defaultKeymap, to: bar)
        }
        for core in cores { core.shutdown() }
        cores = []
        if let previousConfigDir {
            setenv("TESSERA_CONFIG_DIR", previousConfigDir, 1)
        } else {
            unsetenv("TESSERA_CONFIG_DIR")
        }
        for dir in configDirs { try? FileManager.default.removeItem(at: dir) }
        appDelegate.windowStateStore = nil
        appDelegate.spawnOptions = .init()
        appDelegate.closeConfirmationPresenter = previousPresenter
        if let previousQuitKeepsWindows { appDelegate.quitKeepsWindows = previousQuitKeepsWindows }
        NSApp.mainMenu?.items.first?.submenu?.update()
        appDelegate = nil
        presenter = nil
    }

    // MARK: Menus

    /// One Close All, Tessera's (``AppDelegate/closeAllWindows(_:)``), right
    /// after Close Tab so it shows under ⌥, and none of AppKit's
    /// (`closeAll:`, which it adds only to a menu bar without a ⌥⌘W item);
    /// Quit's alternate right after Quit.
    func testTheMenusHaveTesserasCloseAllAndAQuitAlternateAfterTheirItems() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        XCTAssertEqual(Self.items(of: bar).filter { $0.action == NSSelectorFromString("closeAll:") }.map(\.title), [])

        try assertAlternate(
            #selector(AppDelegate.closeAllWindows(_:)), title: "Close All", key: "w", follows: "close_tab", in: bar)
        try assertAlternate(
            #selector(AppDelegate.quitWithTheOtherWindowChoice(_:)), title: "Quit and Keep Windows", key: "q",
            follows: "quit", in: bar)
    }

    /// AppKit adds its own Quit alternate ("Quit and Keep Windows",
    /// `terminate:`, which records no window choice) after Quit of an active
    /// app about a second after launch, and on later menu-bar
    /// customisations; the app removes it as it is added. After 1.5 s and a
    /// menu update (where the host is active, AppKit's has been added by
    /// then), and once an item like AppKit's is inserted after Quit, only
    /// Tessera's alternate follows Quit.
    func testAppKitsQuitAlternateIsRemovedAndOnlyTesserasFollowsQuit() async throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let appMenu = try XCTUnwrap(bar.items.first?.submenu)
        try await Task.sleep(for: .milliseconds(1500))
        bar.update()
        appMenu.update()
        try assertOnlyTesserasQuitAlternate(in: bar, "after launch")

        let appKits = NSMenuItem(
            title: "Quit and Keep Windows", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "")
        appKits.keyEquivalentModifierMask = [.command, .option]
        appKits.isAlternate = true
        appKits.identifier = MainMenu.appKitQuitAlternate
        let quit = try XCTUnwrap(MainMenu.item(forCommand: "quit", in: bar))
        appMenu.insertItem(appKits, at: appMenu.index(of: quit) + 1)
        try assertOnlyTesserasQuitAlternate(in: bar, "AppKit's inserted")
        XCTAssertNil(appKits.menu, "AppKit's is out of the menu")
    }

    /// A `[keys]` re-binding or unbinding of Close All after launch
    /// (``MainMenu/apply(_:to:)``) brings AppKit's back neither then nor
    /// when the menu bar is set again: with ⌥⌘W unbound, ⌥⌘W closes
    /// nothing, and the new chord closes every window.
    func testARebindOrUnbindOfCloseAllBringsBackNoAppKitCloseAll() async throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        defaultKeymap = try startCore(config: "").keymap()
        let configs = [
            ("rebound", "[keys]\n'cmd+alt+w' = 'none'\n'cmd+ctrl+w' = 'close_all_windows'\n"),
            ("unbound", "[keys]\n'cmd+alt+w' = 'none'\n"),
        ]
        for (name, config) in configs {
            MainMenu.apply(try startCore(config: config).keymap(), to: bar)
            NSApp.mainMenu = bar
            bar.update()
            for menu in bar.items.compactMap(\.submenu) { menu.update() }

            XCTAssertEqual(
                Self.items(of: bar).filter { $0.action == NSSelectorFromString("closeAll:") }.map(\.title), [], name)
            let closeAll = try XCTUnwrap(MainMenu.item(forCommand: "close_all_windows", in: bar))
            XCTAssertEqual(closeAll.keyEquivalent, name == "rebound" ? "w" : "", name)

            _ = try appDelegate.newWindow()
            let pressed = try XCTUnwrap(keyDown("w", keyCode: 13))
            XCTAssertFalse(bar.performKeyEquivalent(with: pressed), "\(name): ⌥⌘W runs nothing")
            XCTAssertEqual(appDelegate.windowControllers.count, 1, "\(name): ⌥⌘W closes nothing")
            if name == "rebound" {
                let chord = try XCTUnwrap(keyDown("w", keyCode: 13, [.command, .control]))
                XCTAssertTrue(bar.performKeyEquivalent(with: chord), "⌃⌘W is Close All")
                XCTAssertTrue(appDelegate.windowControllers.isEmpty, "⌃⌘W closes every window")
            }
            for controller in appDelegate.windowControllers { controller.window?.close() }
        }
        XCTAssertTrue(presenter.modals.isEmpty, "idle sessions: nothing asks")
    }

    /// Right after Quit is Tessera's alternate, and no other item of
    /// `bar` is AppKit's or another alternate that quits (`terminate:`).
    private func assertOnlyTesserasQuitAlternate(
        in bar: NSMenu, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let items = Self.items(of: bar)
        XCTAssertEqual(
            items.filter { $0.identifier == MainMenu.appKitQuitAlternate }.map(\.title), [], message,
            file: file, line: line)
        XCTAssertEqual(
            items.filter { $0.isAlternate && $0.action == #selector(NSApplication.terminate(_:)) }.map(\.title), [],
            message, file: file, line: line)
        let quit = try XCTUnwrap(MainMenu.item(forCommand: "quit", in: bar), file: file, line: line)
        let appMenu = try XCTUnwrap(quit.menu, file: file, line: line)
        let next = appMenu.items[appMenu.index(of: quit) + 1]
        XCTAssertEqual(next.identifier?.rawValue, "quit_keep_windows", message, file: file, line: line)
        XCTAssertTrue(next.isAlternate, message, file: file, line: line)
    }

    // MARK: Close All

    /// ⌥⌘W over windows of [idle, busy] and [busy, idle] tabs: one
    /// application-modal alert names both jobs, and no tab asks on a sheet
    /// of its own. Cancel closes nothing and leaves nothing asking; Close
    /// closes every tab, ends every session and frees every controller.
    func testCloseAllAsksOnceForEveryWindowAndCloseClosesThemAll() async throws {
        let first = try openTabs(2)
        let second = try openTabs(2)
        try await start("sleep 100", in: try controller(of: first[1]))
        try await start("cat", in: try controller(of: second[0]))
        let sessions = appDelegate.windowControllers.map(\.focusedPane.session)
        presenter.modalAnswer = false

        XCTAssertTrue(try pressCloseAll(), "⌥⌘W runs Close All")

        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep", "cat"])])
        XCTAssertTrue(presenter.sheets.isEmpty, "no tab asks on a sheet of its own")
        XCTAssertEqual(NSApp.windows.reduce(0) { $0 + $1.sheets.count }, 0)
        XCTAssertEqual(appDelegate.windowControllers.count, 4, "Cancel closes nothing")
        XCTAssertTrue(appDelegate.windowControllers.allSatisfy { $0.pendingClose == nil }, "nothing asks on")
        XCTAssertEqual(try controller(of: first[1]).focusedPane.session.foregroundProcess()?.name, "sleep")

        presenter.modalAnswer = true
        var weakRefs: [WeakController] = appDelegate.windowControllers.map(WeakController.init)
        autoreleasepool {
            XCTAssertTrue((try? pressCloseAll()) ?? false)
        }

        XCTAssertEqual(presenter.modals.count, 2, "asked once more, once")
        XCTAssertTrue(presenter.sheets.isEmpty, "and no tab asked")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "Close closes every tab of every window")
        XCTAssertFalse((first + second).contains { $0.isVisible })
        for session in sessions {
            XCTAssertThrowsError(try session.foregroundProcess()) {
                XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
            }
        }
        try? await waitUntil("the closed tabs' controllers are freed") { weakRefs.allSatisfy { $0.value == nil } }
        XCTAssertEqual(weakRefs.filter { $0.value != nil }.count, 0, "every controller is freed")
        weakRefs = []
    }

    /// With every session at its prompt, ⌥⌘W closes every window and the
    /// Settings window without asking; the Settings window asks nothing.
    func testCloseAllWithIdleSessionsClosesEveryWindowAndSettingsWithoutAsking() async throws {
        _ = try openTabs(2)
        _ = try appDelegate.newWindow()
        appDelegate.openSettings(nil)
        XCTAssertNotNil(appDelegate.settingsWindowController, "Settings opened")

        XCTAssertTrue(try pressCloseAll())

        XCTAssertTrue(presenter.modals.isEmpty, "nothing to ask about")
        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "every window closes")
        XCTAssertNil(appDelegate.settingsWindowController, "and Settings")
    }

    /// The Settings window closes with the terminal windows after Close
    /// too, and is named in no question.
    func testCloseAllClosesSettingsAfterAskingOnlyAboutTheTerminals() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        appDelegate.openSettings(nil)
        presenter.modalAnswer = false

        XCTAssertTrue(try pressCloseAll())
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep"])])
        XCTAssertNotNil(appDelegate.settingsWindowController, "Cancel keeps Settings open")

        presenter.modalAnswer = true
        XCTAssertTrue(try pressCloseAll())
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        XCTAssertNil(appDelegate.settingsWindowController, "Close closes Settings")
    }

    /// While a tab asks about itself on its sheet, ⌥⌘W is ignored, as ⇧⌘W
    /// on that tab is; once the tab's question is answered it asks again.
    func testCloseAllWhileATabAsksIsIgnored() async throws {
        let windows = try openTabs(2)
        let asking = try controller(of: windows[0])
        try await start("sleep 100", in: asking)
        windows[0].performClose(nil)
        XCTAssertEqual(presenter.sheets.count, 1)
        XCTAssertEqual(asking.pendingClose, .tab)

        _ = try pressCloseAll()

        XCTAssertTrue(presenter.modals.isEmpty, "Close All asks nothing while a tab asks")
        XCTAssertEqual(appDelegate.windowControllers.count, 2, "and closes nothing")

        presenter.sheets[0].answer(false)
        presenter.modalAnswer = false
        XCTAssertTrue(try pressCloseAll())
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep"])])
    }

    /// Close All ends a sheet on the Settings window as Cancel, once, before
    /// it closes that window, as a quit does: neither is left behind.
    func testCloseAllEndsASheetOnSettingsAsCancel() async throws {
        _ = try appDelegate.newWindow()
        appDelegate.openSettings(nil)
        let settings = try XCTUnwrap(appDelegate.settingsWindowController?.window)
        let sheet = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
            defer: true)
        sheet.isReleasedWhenClosed = false
        var responses: [NSApplication.ModalResponse] = []
        settings.beginSheet(sheet) { responses.append($0) }
        try await waitUntil("the sheet is up") { settings.sheets.contains(sheet) }

        XCTAssertTrue(try pressCloseAll())

        try await waitUntil("the sheet answered") { !responses.isEmpty }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(responses, [.cancel], "its Cancel path, once")
        XCTAssertTrue(settings.sheets.isEmpty)
        XCTAssertFalse(sheet.isVisible)
        XCTAssertFalse(settings.isVisible)
        XCTAssertNil(appDelegate.settingsWindowController)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    // MARK: AppKit's closeAll:

    /// AppKit's own Close All, `closeAll:`, sent with no target as a menu
    /// item sends it, over windows of [idle, busy] and [busy] tabs: the
    /// application object (``TesseraApplication``), which answers it ahead
    /// of its delegate, runs the single Close All, one application-modal
    /// alert naming both jobs and no tab's sheet. Cancel closes nothing;
    /// Close closes every tab and frees every controller.
    func testAppKitsCloseAllThroughTheResponderChainAsksOnceForEveryWindow() async throws {
        let first = try openTabs(2)
        let second = try openTabs(1)
        try await start("sleep 100", in: try controller(of: first[1]))
        try await start("cat", in: try controller(of: second[0]))
        presenter.modalAnswer = false

        XCTAssertTrue(sendCloseAll(), "the responder chain answers closeAll:")

        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep", "cat"])])
        XCTAssertTrue(presenter.sheets.isEmpty, "no tab asks on a sheet of its own")
        XCTAssertEqual(NSApp.windows.reduce(0) { $0 + $1.sheets.count }, 0)
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel closes nothing")
        XCTAssertTrue(appDelegate.windowControllers.allSatisfy { $0.pendingClose == nil }, "nothing asks on")

        presenter.modalAnswer = true
        var weakRefs: [WeakController] = appDelegate.windowControllers.map(WeakController.init)
        autoreleasepool {
            XCTAssertTrue(sendCloseAll())
        }

        XCTAssertEqual(presenter.modals.count, 2, "asked once more, once")
        XCTAssertTrue(presenter.sheets.isEmpty, "and no tab asked")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "Close closes every tab of every window")
        XCTAssertFalse((first + second).contains { $0.isVisible })
        try? await waitUntil("the closed tabs' controllers are freed") { weakRefs.allSatisfy { $0.value == nil } }
        XCTAssertEqual(weakRefs.filter { $0.value != nil }.count, 0, "every controller is freed")
        weakRefs = []
    }

    /// `closeAll:` with a sheet up on the Settings window and a busy tab:
    /// Cancel leaves Settings open with its sheet; Close ends the sheet as
    /// Cancel, once, then closes Settings with the terminals.
    func testAppKitsCloseAllEndsASheetOnSettingsOrLeavesSettingsOpen() async throws {
        let controller = try appDelegate.newWindow()
        try await start("sleep 100", in: controller)
        appDelegate.openSettings(nil)
        let settings = try XCTUnwrap(appDelegate.settingsWindowController?.window)
        let sheet = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
            defer: true)
        sheet.isReleasedWhenClosed = false
        var responses: [NSApplication.ModalResponse] = []
        settings.beginSheet(sheet) { responses.append($0) }
        try await waitUntil("the sheet is up") { settings.sheets.contains(sheet) }
        presenter.modalAnswer = false

        XCTAssertTrue(sendCloseAll())

        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep"])])
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(responses.isEmpty, "Cancel leaves the sheet")
        XCTAssertTrue(settings.sheets.contains(sheet))
        XCTAssertNotNil(appDelegate.settingsWindowController, "and Settings open")
        XCTAssertEqual(appDelegate.windowControllers.count, 1)

        presenter.modalAnswer = true
        XCTAssertTrue(sendCloseAll())

        try await waitUntil("the sheet answered") { !responses.isEmpty }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(responses, [.cancel], "its Cancel path, once")
        XCTAssertTrue(settings.sheets.isEmpty)
        XCTAssertFalse(sheet.isVisible)
        XCTAssertFalse(settings.isVisible)
        XCTAssertNil(appDelegate.settingsWindowController)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    /// Sends AppKit's `closeAll:` with no target, through the responder
    /// chain, as its menu item does.
    private func sendCloseAll() -> Bool {
        NSApp.sendAction(NSSelectorFromString("closeAll:"), to: nil, from: nil)
    }

    // MARK: Option-click on a close button

    /// Clicks the window's close button as the mouse does, through
    /// `NSApp.sendEvent`, with `modifiers` held: AppKit's button tracks the
    /// press until the release queued behind it.
    private func clickCloseButton(of window: NSWindow, _ modifiers: NSEvent.ModifierFlags) throws {
        let button = try XCTUnwrap(window.standardWindowButton(.closeButton))
        let location = button.convert(NSPoint(x: button.bounds.midX, y: button.bounds.midY), to: nil)
        NSApp.postEvent(try mouseEvent(.leftMouseUp, at: location, in: window, modifiers), atStart: false)
        NSApp.sendEvent(try mouseEvent(.leftMouseDown, at: location, in: window, modifiers))
    }

    private func mouseEvent(
        _ type: NSEvent.EventType, at location: NSPoint, in window: NSWindow, _ modifiers: NSEvent.ModifierFlags
    ) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: modifiers,
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber, context: nil,
                eventNumber: 1, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1))
    }

    /// Option-click on the close button with two busy windows, one of two
    /// tabs: one application-modal alert for every job, no tab's sheet.
    /// Cancel closes nothing; a second Option-click answered Close closes
    /// everything (by ``AppDelegate/closeAllWindows(_:)``, whose freeing of
    /// the controllers the ⌥⌘W test checks). Driven by the click alone.
    func testOptionClickOnACloseButtonAsksOnceForEveryWindow() async throws {
        let first = try openTabs(2)
        let second = try openTabs(1)
        try await start("sleep 100", in: try controller(of: first[0]))
        try await start("cat", in: try controller(of: second[0]))
        presenter.modalAnswer = false

        try clickCloseButton(of: second[0], [.option])

        try await waitUntil("Close All asks") { !presenter.modals.isEmpty }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep", "cat"])])
        XCTAssertTrue(presenter.sheets.isEmpty, "no tab asks on a sheet of its own")
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel closes nothing")
        XCTAssertTrue(second[0].isVisible && first[0].tabGroup?.windows.count == 2, "both windows stay")
        XCTAssertTrue(appDelegate.windowControllers.allSatisfy { $0.pendingClose == nil }, "nothing asks on")

        presenter.modalAnswer = true
        try clickCloseButton(of: second[0], [.option])

        try await waitUntil("every window closes") { appDelegate.windowControllers.isEmpty }
        XCTAssertEqual(presenter.modals.count, 2, "asked once more, once")
        XCTAssertTrue(presenter.sheets.isEmpty)
    }

    /// Option-click on the close button of a window with tabs (AppKit's
    /// tab-stack close, which offers every other tab its close before it
    /// sends `closeAll:` at the end of that loop, in place of the clicked tab's
    /// own close), the other tab busy,
    /// beside an idle window: one Close All alert, after AppKit's round, and
    /// no tab's sheet; Cancel closes nothing. Close then closes everything,
    /// each tab once (adversarial #526c B1).
    func testOptionClickOnATabbedWindowIsOneCloseAll() async throws {
        let tabs = try openTabs(2)
        let other = try openTabs(1)
        try await start("sleep 100", in: try controller(of: tabs[0]))
        let selected = try XCTUnwrap(tabs[0].tabGroup?.selectedWindow)
        var closes: [ObjectIdentifier: Int] = [:]
        let observer = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: nil, queue: nil
        ) { note in
            guard let window = note.object as? NSWindow else { return }
            MainActor.assumeIsolated { closes[ObjectIdentifier(window), default: 0] += 1 }
        }
        defer { NotificationCenter.default.removeObserver(observer) }
        presenter.modalAnswer = false

        var stacks: [String] = []
        presenter.whileAskingModally = { stacks.append(Thread.callStackSymbols.joined(separator: "\n")) }

        try clickCloseButton(of: selected, [.option])

        try await waitUntil("Close All asks") { !presenter.modals.isEmpty }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep"])])
        XCTAssertTrue(presenter.sheets.isEmpty, "no tab asks on a sheet of its own")
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel closes nothing")
        XCTAssertTrue(closes.isEmpty, "no window closed")
        XCTAssertEqual(stacks.count, 1)
        XCTAssertFalse(
            stacks.contains { $0.contains("attemptToCloseEntireStack") || $0.contains("_closeAllWindowsExceptItem") },
            "Close All asks after AppKit has offered every tab its close, not inside that loop")

        presenter.modalAnswer = true
        try clickCloseButton(of: try XCTUnwrap(tabs[0].tabGroup?.selectedWindow), [.option])

        try await waitUntil("every window closes") { appDelegate.windowControllers.isEmpty }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(presenter.modals.count, 2, "asked once more, once")
        XCTAssertTrue(presenter.sheets.isEmpty)
        for window in tabs + other {
            XCTAssertEqual(closes[ObjectIdentifier(window)], 1, "each tab closes once")
        }
    }

    /// The same with both tabs idle and the other window busy: Cancel
    /// closes neither idle tab.
    func testOptionClickOnAnIdleTabbedWindowCancelClosesNothing() async throws {
        let tabs = try openTabs(2)
        let other = try openTabs(1)
        try await start("sleep 100", in: try controller(of: other[0]))
        let selected = try XCTUnwrap(tabs[0].tabGroup?.selectedWindow)
        presenter.modalAnswer = false

        try clickCloseButton(of: selected, [.option])

        try await waitUntil("Close All asks") { !presenter.modals.isEmpty }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep"])])
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel closes nothing")
        XCTAssertTrue(tabs.allSatisfy(\.isVisible), "both idle tabs stay")
    }

    /// With every session at its prompt the Option-click closes every
    /// window, without a question.
    func testOptionClickOnACloseButtonWithIdleSessionsClosesEveryWindow() async throws {
        let first = try openTabs(2)
        _ = try openTabs(1)

        try clickCloseButton(of: first[0], [.option])

        try await waitUntil("every window closes") { appDelegate.windowControllers.isEmpty }
        XCTAssertTrue(presenter.modals.isEmpty, "nothing to ask about")
        XCTAssertTrue(presenter.sheets.isEmpty)
    }

    /// Option-click on the Settings window's own close button, with two
    /// busy windows: the same one alert and no tab's sheet. Cancel closes
    /// nothing, Settings included; answered Close, everything closes.
    func testOptionClickOnTheSettingsCloseButtonIsCloseAll() async throws {
        let first = try openTabs(2)
        let second = try openTabs(1)
        try await start("sleep 100", in: try controller(of: first[0]))
        try await start("cat", in: try controller(of: second[0]))
        appDelegate.openSettings(nil)
        let settings = try XCTUnwrap(appDelegate.settingsWindowController?.window)
        presenter.modalAnswer = false

        try clickCloseButton(of: settings, [.option])

        try await waitUntil("Close All asks") { !presenter.modals.isEmpty }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .allWindows, processNames: ["sleep", "cat"])])
        XCTAssertTrue(presenter.sheets.isEmpty, "no tab asks on a sheet of its own")
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel closes no tab")
        XCTAssertTrue(settings.isVisible, "Cancel keeps Settings")
        XCTAssertNotNil(appDelegate.settingsWindowController)

        presenter.modalAnswer = true
        try clickCloseButton(of: settings, [.option])

        try await waitUntil("every window closes") {
            appDelegate.windowControllers.isEmpty && appDelegate.settingsWindowController == nil
        }
        XCTAssertEqual(presenter.modals.count, 2, "asked once more, once")
        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertFalse(settings.isVisible)
    }

    /// Option-click on a terminal's close button: Settings waits for the
    /// answer like the terminals, and stays on Cancel.
    func testCancellingAnOptionClickOnATerminalKeepsSettings() async throws {
        let window = try openTabs(1)[0]
        try await start("sleep 100", in: try controller(of: window))
        appDelegate.openSettings(nil)
        let settings = try XCTUnwrap(appDelegate.settingsWindowController?.window)
        presenter.modalAnswer = false

        try clickCloseButton(of: window, [.option])

        try await waitUntil("Close All asks") { !presenter.modals.isEmpty }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(presenter.modals.count, 1)
        XCTAssertTrue(settings.isVisible, "Cancel keeps Settings")
        XCTAssertNotNil(appDelegate.settingsWindowController)
    }

    /// A click without Option on the Settings button closes only Settings,
    /// and asks nothing about the busy terminal.
    func testAPlainClickOnTheSettingsCloseButtonClosesOnlySettings() async throws {
        let window = try openTabs(1)[0]
        try await start("sleep 100", in: try controller(of: window))
        appDelegate.openSettings(nil)
        let settings = try XCTUnwrap(appDelegate.settingsWindowController?.window)

        try clickCloseButton(of: settings, [])

        try await waitUntil("Settings closes") { appDelegate.settingsWindowController == nil }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(settings.isVisible)
        XCTAssertTrue(presenter.modals.isEmpty, "no Close All")
        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
    }

    /// A click without Option stays the tab's own close: its own sheet, and
    /// the other window is not asked about.
    func testAPlainClickOnACloseButtonAsksOnlyForItsTab() async throws {
        let first = try openTabs(1)
        let second = try openTabs(1)
        try await start("sleep 100", in: try controller(of: first[0]))
        try await start("cat", in: try controller(of: second[0]))

        try clickCloseButton(of: second[0], [])

        try await waitUntil("the tab asks") { !presenter.sheets.isEmpty }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(presenter.sheets.count, 1)
        XCTAssertTrue(presenter.modals.isEmpty, "no Close All")
        presenter.sheets[0].answer(false)
        XCTAssertEqual(appDelegate.windowControllers.count, 2)
    }

    // MARK: Quit's alternate

    /// ⌥⌘Q with two windows, three tabs and two jobs asks once, as ⌘Q
    /// does: one `.quit` alert naming both jobs, no sheet. Cancel keeps
    /// everything and leaves no window choice behind, even when the window
    /// state was saved while the alert asked: the next quit's save records
    /// none.
    func testTheQuitAlternateAsksOnceAndCancelLeavesNoWindowChoice() async throws {
        let first = try openTabs(2)
        let second = try appDelegate.newWindow()
        try await start("sleep 100", in: try controller(of: first[1]))
        try await start("cat", in: second)
        presenter.modalAnswer = false
        presenter.whileAskingModally = { [weak self] in self?.appDelegate.saveWindowState() }
        // Quit would end the test host.
        guard CloseConfirmation(.quit, sessions: appDelegate.windowControllers.flatMap(\.sessions)) != nil else {
            return XCTFail("the sessions must be busy, or ⌥⌘Q quits the test host")
        }

        XCTAssertTrue(try pressQuitAlternate(), "⌥⌘Q runs Quit's alternate")

        XCTAssertEqual(presenter.modals, [CloseConfirmation(scope: .quit, processNames: ["sleep", "cat"])])
        XCTAssertTrue(presenter.sheets.isEmpty)
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "Cancel keeps everything open")
        XCTAssertGreaterThan(defaults.stateSaves, 0, "the state was saved while the alert asked")
        XCTAssertNil(defaults.object(forKey: WindowStateStore.restoresAtNextLaunchKey), "no choice for the next launch")

        presenter.whileAskingModally = nil
        appDelegate.saveWindowState(quitting: true)
        XCTAssertNil(
            defaults.object(forKey: WindowStateStore.restoresAtNextLaunchKey),
            "a later quit (⌘Q) makes no choice of its own")
    }

    /// The alternate's title follows macOS's setting each time the menu is
    /// validated (as it opens or a key equivalent is matched): Quit and
    /// Keep Windows while windows close on quit, Quit and Close All Windows
    /// while they are kept.
    func testTheQuitAlternateTitleFollowsTheSetting() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let item = try XCTUnwrap(
            Self.items(of: bar).first { $0.action == #selector(AppDelegate.quitWithTheOtherWindowChoice(_:)) })

        keepsWindows = true
        item.menu?.update()
        XCTAssertEqual(item.title, "Quit and Close All Windows")

        keepsWindows = false
        item.menu?.update()
        XCTAssertEqual(item.title, "Quit and Keep Windows")
    }

    /// The title reads the setting as macOS stores it, while the app runs:
    /// `NSQuitAlwaysKeepsWindows` in the defaults (here the volatile
    /// argument domain, which the app's defaults read first and which
    /// writes nothing to disk).
    func testTheQuitAlternateTitleFollowsTheDefaultChangingWhileTheAppRuns() throws {
        if let previousQuitKeepsWindows { appDelegate.quitKeepsWindows = previousQuitKeepsWindows }
        let item = try XCTUnwrap(
            NSApp.mainMenu.map(Self.items(of:))?.first {
                $0.action == #selector(AppDelegate.quitWithTheOtherWindowChoice(_:))
            })
        let standard = UserDefaults.standard
        let previous = standard.volatileDomain(forName: UserDefaults.argumentDomain)
        defer {
            standard.setVolatileDomain(previous, forName: UserDefaults.argumentDomain)
            item.menu?.update()
        }
        for keeps in [true, false, true] {
            var domain = previous
            domain["NSQuitAlwaysKeepsWindows"] = keeps
            standard.setVolatileDomain(domain, forName: UserDefaults.argumentDomain)
            XCTAssertEqual(WindowStateStore.quitKeepsWindows(standard), keeps)

            item.menu?.update()

            XCTAssertEqual(item.title, AppDelegate.quitAlternateTitle(quitKeepsWindows: keeps))
        }
        XCTAssertEqual(AppDelegate.quitAlternateTitle(quitKeepsWindows: true), "Quit and Close All Windows")
        XCTAssertEqual(AppDelegate.quitAlternateTitle(quitKeepsWindows: false), "Quit and Keep Windows")
    }

    /// A launch restores the saved windows as the last quit chose, once:
    /// the launch after follows the setting again.
    func testALaunchFollowsTheQuitsWindowChoiceOnce() throws {
        _ = try openTabs(2)
        _ = try appDelegate.newWindow()
        appDelegate.saveWindowState()
        closeAll()
        let store = try XCTUnwrap(appDelegate.windowStateStore)

        keepsWindows = false
        XCTAssertFalse(appDelegate.restoreWindowsAtLaunch(from: store), "the setting: windows close on quit")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)

        store.setRestoresAtNextLaunch(true)
        XCTAssertTrue(appDelegate.restoreWindowsAtLaunch(from: store), "Quit and Keep Windows")
        XCTAssertEqual(appDelegate.windowControllers.count, 3, "both windows, three tabs")
        closeAll()
        XCTAssertFalse(appDelegate.restoreWindowsAtLaunch(from: store), "the launch after follows the setting")

        keepsWindows = true
        store.setRestoresAtNextLaunch(false)
        XCTAssertFalse(appDelegate.restoreWindowsAtLaunch(from: store), "Quit and Close All Windows")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        XCTAssertTrue(appDelegate.restoreWindowsAtLaunch(from: store), "the launch after follows the setting")
        XCTAssertEqual(appDelegate.windowControllers.count, 3)
    }

    // MARK: Text

    /// A job that names itself with a bidi override and a line break is
    /// shown filtered in Close All's alert and in the quit alert.
    func testAHostileJobNameIsFilteredInBothAlerts() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-464-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try compileSleeper(to: dir.appendingPathComponent("z\u{202E}x\nq"))
        let controller = try appDelegate.newWindow()
        let job = try await runSleeper(in: dir, pane: controller.focusedPane, "the hostile sleeper runs")
        XCTAssertTrue(job.name.contains("\u{202E}") || job.name.contains("\n"), "the name is hostile: \(job.name)")
        presenter.modalAnswer = false

        XCTAssertTrue(try pressCloseAll())
        XCTAssertTrue(try pressQuitAlternate())

        XCTAssertEqual(presenter.modals.map(\.scope), [.allWindows, .quit])
        for confirmation in presenter.modals {
            XCTAssertEqual(confirmation.processNames, [CloseConfirmation.displayName(job.name)])
            let name = try XCTUnwrap(confirmation.processNames.first)
            XCTAssertFalse(name.contains("\u{202E}"), name)
            XCTAssertFalse(name.contains("\n"), name)
            XCTAssertFalse(confirmation.informativeText.contains("\u{202E}"), confirmation.informativeText)
            XCTAssertEqual(
                confirmation.informativeText.split(separator: "\n").count, 2, "one line for the job: \(confirmation)")
        }
        XCTAssertEqual(appDelegate.windowControllers.count, 1, "Cancel, twice")
    }

    // MARK: Helpers

    private struct WeakController {
        weak var value: TerminalWindowController?
        init(_ value: TerminalWindowController) { self.value = value }
    }

    /// Every item of `menu` and its submenus, depth first.
    private static func items(of menu: NSMenu) -> [NSMenuItem] {
        menu.items.flatMap { [$0] + ($0.submenu.map(items(of:)) ?? []) }
    }

    /// The one item with `action` is an alternate with ⌥⌘`key` and
    /// `title`, sent through the responder chain, right after the item of
    /// command `primary`, with which it shares its key.
    private func assertAlternate(
        _ action: Selector, title: String, key: String, follows primary: String, in bar: NSMenu,
        file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let matches = Self.items(of: bar).filter { $0.action == action }
        XCTAssertEqual(matches.count, 1, "\(action)", file: file, line: line)
        let item = try XCTUnwrap(matches.first, file: file, line: line)
        item.menu?.update()
        XCTAssertTrue(item.isAlternate, "\(action)", file: file, line: line)
        XCTAssertEqual(item.title, title, file: file, line: line)
        XCTAssertNil(item.target, "\(action)", file: file, line: line)
        let (itemKey, itemMask) = authoredKeyEquivalent(of: item)
        XCTAssertEqual(itemKey, key, file: file, line: line)
        XCTAssertEqual(itemMask.intersection(.deviceIndependentFlagsMask), [.command, .option], file: file, line: line)
        let primaryItem = try XCTUnwrap(MainMenu.item(forCommand: primary, in: bar), file: file, line: line)
        let menu = try XCTUnwrap(item.menu, file: file, line: line)
        XCTAssertTrue(primaryItem.menu === menu, "\(action) is in \(primary)'s menu", file: file, line: line)
        XCTAssertEqual(
            menu.index(of: item), menu.index(of: primaryItem) + 1, "right after \(primary)", file: file, line: line)
        XCTAssertEqual(authoredKeyEquivalent(of: primaryItem).key, key, file: file, line: line)
    }

    private func pressCloseAll() throws -> Bool {
        try XCTUnwrap(NSApp.mainMenu).performKeyEquivalent(with: try XCTUnwrap(keyDown("w", keyCode: 13)))
    }

    private func pressQuitAlternate() throws -> Bool {
        try XCTUnwrap(NSApp.mainMenu).performKeyEquivalent(with: try XCTUnwrap(keyDown("q", keyCode: 12)))
    }

    private func closeAll() {
        for controller in appDelegate.windowControllers { controller.window?.close() }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    /// A core of this test's own on a directory with `config` as its
    /// `config.toml`, shut down in `tearDown`.
    private func startCore(config: String) throws -> TesseraApp {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-464-keys-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        configDirs.append(dir)
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        cores.append(core)
        return core
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
    /// foreground (``startForegroundJob(_:in:)``).
    private func start(_ command: String, in controller: TerminalWindowController) async throws {
        _ = try await startForegroundJob(command, in: controller.focusedPane)
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
