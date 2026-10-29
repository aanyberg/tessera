import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The window and tab model, driven through the running app (these tests are
/// hosted by Tessera.app, which opens no window of its own when it hosts
/// tests). Sessions run `cat`, which lives until it is hung up, instead of
/// the login shell.
@MainActor
final class WindowTabTests: XCTestCase {
    private var appDelegate: AppDelegate!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        appDelegate = nil
    }

    // MARK: Windows

    func testNewWindowCreatesOneControllerWithALiveSession() throws {
        let controller = try appDelegate.newWindow()

        XCTAssertEqual(appDelegate.windowControllers.map(ObjectIdentifier.init), [ObjectIdentifier(controller)])
        let window = try XCTUnwrap(controller.window)
        XCTAssertTrue(window.isVisible)
        XCTAssertEqual(window.title, "Terminal")
        XCTAssertEqual(window.subtitle, "")
        XCTAssertEqual(window.tabbingMode, .preferred)
        XCTAssertEqual(window.tabbingIdentifier, "com.aanyberg.tessera.terminal")
        XCTAssertTrue(window.contentView === controller.contentView)
        // A live session accepts input and a resize.
        XCTAssertNoThrow(try controller.session.write(Array("hello\n".utf8)))
        XCTAssertNoThrow(try controller.session.resize(rows: 30, cols: 100))
    }

    func testNewWindowStandsAloneInItsOwnTabGroup() throws {
        let first = try XCTUnwrap(appDelegate.newWindow().window)
        let second = try XCTUnwrap(appDelegate.newWindow().window)

        XCTAssertEqual(appDelegate.windowControllers.count, 2)
        XCTAssertEqual(tabs(of: first), [first])
        XCTAssertEqual(tabs(of: second), [second])
        XCTAssertNotEqual(first.frame.origin, second.frame.origin, "new windows cascade")
    }

    // MARK: Tabs

    func testNewTabJoinsTheWindowsTabGroupWithItsOwnSession() throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)

        let second = try appDelegate.newTab(in: window)

        let tab = try XCTUnwrap(second.window)
        XCTAssertEqual(tabs(of: window), [window, tab])
        XCTAssertTrue(window.tabGroup === tab.tabGroup)
        XCTAssertNotEqual(first.session.id, second.session.id)
        XCTAssertEqual(appDelegate.windowControllers.count, 2)
        XCTAssertNoThrow(try second.session.resize(rows: 24, cols: 80))
    }

    /// The core's `appearanceDidChange(opaque:)` reaches every tab through the
    /// session router, the background tab included: each window follows the
    /// opacity and each view's display link wakes to draw the new colours.
    func testAnAppearanceChangeReachesEveryTab() throws {
        let first = try appDelegate.newWindow()
        let second = try appDelegate.newTab(in: try XCTUnwrap(first.window))
        let controllers = [first, second]
        let views = try controllers.map { try XCTUnwrap($0.contentView as? TerminalSurfaceView) }
        guard views.allSatisfy(\.surface.isAttached) else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        let router = SessionRouter()
        for controller in controllers {
            router.register(controller)
        }
        for view in views {
            view.occlusionOverride = false
            view.displayLinkDidFire()
            XCTAssertTrue(view.isDisplayLinkPaused, "idle before the change")
        }

        router.appearanceDidChange(opaque: false)
        for (controller, view) in zip(controllers, views) {
            let window = try XCTUnwrap(controller.window)
            XCTAssertFalse(window.isOpaque, "tab \(controller.session.id) is translucent")
            XCTAssertEqual(window.backgroundColor, .clear)
            XCTAssertFalse(view.isDisplayLinkPaused, "tab \(controller.session.id) redraws")
            view.displayLinkDidFire()
        }

        router.appearanceDidChange(opaque: true)
        for controller in controllers {
            XCTAssertTrue(try XCTUnwrap(controller.window).isOpaque)
        }
    }

    func testTabBarPlusButtonAddsATabAfterThatWindow() throws {
        let windows = try openTabs(2)
        let first = try XCTUnwrap(appDelegate.windowControllers.first { $0.window === windows[0] })

        first.newWindowForTab(nil)

        let added = try XCTUnwrap(appDelegate.windowControllers.last)
        let tab = try XCTUnwrap(added.window)
        XCTAssertEqual(appDelegate.windowControllers.count, 3)
        XCTAssertEqual(tabs(of: windows[0]), [windows[0], tab, windows[1]])
        XCTAssertEqual(Set(appDelegate.windowControllers.map(\.session.id)).count, 3, "each tab has its own session")
        XCTAssertNoThrow(try added.session.resize(rows: 24, cols: 80))
    }

    func testNewTabWithoutAWindowOpensAStandaloneWindow() throws {
        let controller = try appDelegate.newTab(in: nil)

        let window = try XCTUnwrap(controller.window)
        XCTAssertTrue(window.isVisible)
        XCTAssertEqual(tabs(of: window), [window])
    }

    func testNumberedTabSelectionSelectsThatTab() throws {
        let windows = try openTabs(3)
        let first = try XCTUnwrap(appDelegate.windowControllers.first)

        for index in [2, 0, 1] {
            first.selectTabAtIndex(menuItem(tag: index))
            XCTAssertTrue(windows[0].tabGroup?.selectedWindow === windows[index], "⌘\(index + 1)")
        }
    }

    func testNumberedTabSelectionIsDisabledPastTheLastTab() throws {
        _ = try openTabs(2)
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)

        XCTAssertTrue(controller.validateMenuItem(menuItem(tag: 1)))
        XCTAssertFalse(controller.validateMenuItem(menuItem(tag: 2)))
        controller.selectTabAtIndex(menuItem(tag: 8))
        XCTAssertEqual(appDelegate.windowControllers.count, 2)
    }

    func testNextAndPreviousTabWrapAround() throws {
        let windows = try openTabs(3)
        let group = try XCTUnwrap(windows[0].tabGroup)
        group.selectedWindow = windows[2]

        group.selectedWindow?.selectNextTab(nil)
        XCTAssertTrue(group.selectedWindow === windows[0], "next from the last tab wraps to the first")
        group.selectedWindow?.selectNextTab(nil)
        XCTAssertTrue(group.selectedWindow === windows[1])
        group.selectedWindow = windows[0]
        group.selectedWindow?.selectPreviousTab(nil)
        XCTAssertTrue(group.selectedWindow === windows[2], "previous from the first tab wraps to the last")
        group.selectedWindow?.selectPreviousTab(nil)
        XCTAssertTrue(group.selectedWindow === windows[1])
    }

    func testNextTabInASingleTabWindowKeepsIt() throws {
        let window = try XCTUnwrap(appDelegate.newWindow().window)

        window.selectNextTab(nil)
        window.selectPreviousTab(nil)

        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertTrue(window.isVisible)
    }

    // MARK: Closing

    func testClosingATabClosesItsSessionAndForgetsTheController() throws {
        let windows = try openTabs(2)
        let controller = try XCTUnwrap(appDelegate.windowControllers.last)
        let session = controller.session

        windows[1].performClose(nil)

        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertFalse(appDelegate.windowControllers.contains { $0 === controller })
        XCTAssertThrowsError(try session.close()) { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertEqual(tabs(of: windows[0]), [windows[0]])
    }

    func testCloseWindowClosesEveryTabOfThatWindowOnly() throws {
        let windows = try openTabs(3)
        let other = try XCTUnwrap(appDelegate.newWindow().window)
        let closed = appDelegate.windowControllers.filter { controller in
            windows.contains { $0 === controller.window }
        }
        let sessions = closed.map(\.session)
        XCTAssertEqual(sessions.count, 3)

        closed[1].closeTabGroup(nil)

        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertTrue(appDelegate.windowControllers.first?.window === other)
        XCTAssertTrue(other.isVisible)
        XCTAssertFalse(windows.contains { $0.isVisible })
        for session in sessions {
            XCTAssertThrowsError(try session.close()) { error in
                XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
            }
        }
    }

    func testClosingTheLastTabLeavesTheAppRunningWithNoWindows() async throws {
        let controller = try appDelegate.newWindow()
        let session = controller.session

        controller.window?.performClose(nil)

        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        XCTAssertFalse(appDelegate.applicationShouldTerminateAfterLastWindowClosed(NSApp))
        XCTAssertThrowsError(try session.close())
        // Give AppKit's last-window-closed check a chance to run.
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertTrue(NSApp.isRunning)
        XCTAssertFalse(NSApp.windows.contains { $0.isVisible && $0.tabbingIdentifier == TerminalWindowController.tabbingIdentifier })
    }

    func testReopenWithNoWindowOpensOne() throws {
        XCTAssertFalse(appDelegate.applicationShouldHandleReopen(NSApp, hasVisibleWindows: false))
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertTrue(appDelegate.applicationShouldHandleReopen(NSApp, hasVisibleWindows: true))
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
    }

    // MARK: Session exit

    func testCleanExitClosesItsTab() async throws {
        let first = try appDelegate.newWindow()
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "exit 0"])
        let exiting = try appDelegate.newTab(in: first.window)
        let session = exiting.session

        try await waitUntil("the exited tab closes") {
            !self.appDelegate.windowControllers.contains { $0 === exiting }
        }
        XCTAssertFalse(exiting.window?.isVisible ?? false)
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertThrowsError(try session.close()) { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
    }

    func testFailedExitKeepsTheTabAndShowsTheStatus() async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "exit 3"])
        let controller = try appDelegate.newWindow()

        let terminal = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        try await waitUntil("the exit status shows") {
            terminal.status == "shell exited with code 3"
        }
        XCTAssertTrue(appDelegate.windowControllers.contains { $0 === controller })
        XCTAssertTrue(controller.window?.isVisible ?? false)
    }

    func testKilledSessionKeepsTheTabAndShowsTheSignal() async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", "kill -KILL $$"])
        let controller = try appDelegate.newWindow()

        let terminal = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        try await waitUntil("the signal shows") {
            terminal.status == "shell killed by signal 9"
        }
        XCTAssertTrue(appDelegate.windowControllers.contains { $0 === controller })
    }

    // MARK: Titles

    func testSessionTitleBecomesTheWindowTitle() throws {
        let controller = try appDelegate.newWindow()

        controller.sessionDidSetTitle("vim notes.txt")
        XCTAssertEqual(controller.window?.title, "vim notes.txt")
        controller.sessionDidSetTitle("")
        XCTAssertEqual(controller.window?.title, "Terminal")
        XCTAssertEqual(controller.window?.subtitle, "")
    }

    // MARK: Menu

    func testMenuKeyEquivalents() throws {
        let items = allItems(of: try XCTUnwrap(NSApp.mainMenu))
        func item(_ action: Selector) -> NSMenuItem? { items.first { $0.action == action } }

        let expected: [(Selector, String, NSEvent.ModifierFlags)] = [
            (#selector(AppDelegate.openNewWindow(_:)), "n", .command),
            (#selector(AppDelegate.openNewTab(_:)), "t", .command),
            (#selector(NSWindow.performClose(_:)), "w", .command),
            (#selector(TerminalWindowController.closeTabGroup(_:)), "w", [.command, .shift]),
            (#selector(NSWindow.selectNextTab(_:)), "}", .command),
            (#selector(NSWindow.selectPreviousTab(_:)), "{", .command),
            (#selector(NSApplication.terminate(_:)), "q", .command),
        ]
        for (action, key, modifiers) in expected {
            let menuItem = try XCTUnwrap(item(action), "no item for \(action)")
            XCTAssertEqual(menuItem.keyEquivalent, key, "\(action)")
            XCTAssertEqual(menuItem.keyEquivalentModifierMask, modifiers, "\(action)")
        }
        for action in [
            #selector(NSWindow.moveTabToNewWindow(_:)), #selector(NSWindow.mergeAllWindows(_:)),
            #selector(NSWindow.toggleTabBar(_:)),
        ] {
            XCTAssertNotNil(item(action), "no item for \(action)")
        }
        let numbered = items.filter { $0.action == #selector(TerminalWindowController.selectTabAtIndex(_:)) }
        XCTAssertEqual(numbered.map(\.keyEquivalent), (1...9).map(String.init))
        XCTAssertEqual(numbered.map(\.tag), Array(0..<9))
        XCTAssertTrue(NSApp.windowsMenu?.items.contains { $0.action == #selector(NSWindow.selectNextTab(_:)) } ?? false)
    }

    // MARK: Helpers

    /// Opens a window with `count` tabs and returns them in tab-bar order.
    private func openTabs(_ count: Int) throws -> [NSWindow] {
        let window = try XCTUnwrap(appDelegate.newWindow().window)
        for _ in 1..<count {
            _ = try appDelegate.newTab(in: window.tabGroup?.windows.last ?? window)
        }
        let windows = tabs(of: window)
        XCTAssertEqual(windows.count, count)
        return windows
    }

    private func tabs(of window: NSWindow) -> [NSWindow] {
        window.tabGroup?.windows ?? [window]
    }

    private func menuItem(tag: Int) -> NSMenuItem {
        let item = NSMenuItem(
            title: "", action: #selector(TerminalWindowController.selectTabAtIndex(_:)), keyEquivalent: "")
        item.tag = tag
        return item
    }

    private func allItems(of menu: NSMenu) -> [NSMenuItem] {
        menu.items.flatMap { [$0] + ($0.submenu.map(allItems(of:)) ?? []) }
    }

    /// Polls `condition` on the main actor, letting session events through,
    /// until it holds or `timeout` passes.
    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out after \(timeout): \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(20))
        }
    }
}
