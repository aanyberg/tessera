import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// User defaults kept in memory that count the window state's saves.
///
/// Nothing reaches the suite named at `init`: a suite's file stays in
/// ~/Library/Preferences even after its domain is removed, as cfprefsd
/// writes the empty domain back after the test host quits. The typed
/// getters and setters (`bool(forKey:)`, `data(forKey:)`, `set(_:forKey:)`
/// with a `Bool`) go through these three overrides.
final class CountingDefaults: UserDefaults, @unchecked Sendable {
    private(set) var stateSaves = 0
    private var values: [String: Any] = [:]

    override func object(forKey defaultName: String) -> Any? {
        values[defaultName]
    }

    override func set(_ value: Any?, forKey defaultName: String) {
        if defaultName == WindowStateStore.key {
            stateSaves += 1
        }
        values[defaultName] = value
    }

    override func removeObject(forKey defaultName: String) {
        values[defaultName] = nil
    }
}

/// Each window's frame and tab count are saved as they change and on quit,
/// and recreated at launch with fresh shells (ADR 0031, D4c).
@MainActor
final class WindowStateTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var suiteName: String!
    private var defaults: CountingDefaults!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        XCTAssertNil(appDelegate.windowStateStore, "hosted tests must not save to the user's defaults")
        suiteName = "com.aanyberg.tessera.tests.\(UUID().uuidString)"
        defaults = try XCTUnwrap(CountingDefaults(suiteName: suiteName))
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        appDelegate.windowStateStore = nil
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        appDelegate = nil
        defaults = nil
        XCTAssertNil(UserDefaults.standard.persistentDomain(forName: suiteName), "nothing was written to the suite")
    }

    private static let screen = CGRect(x: 0, y: 0, width: 1440, height: 875)
    private static let secondScreen = CGRect(x: 1440, y: -200, width: 1920, height: 1055)

    // MARK: Store

    func testStateRoundTripsThroughTheStore() {
        let store = WindowStateStore(defaults: defaults)
        XCTAssertNil(store.load(), "nothing saved yet")
        let state = WindowState(windows: [
            .init(frame: CGRect(x: 10.5, y: 20, width: 640, height: 400), tabCount: 3),
            .init(frame: CGRect(x: -900, y: 300, width: 800, height: 600), tabCount: 1),
        ])

        store.save(state)

        XCTAssertEqual(store.load(), state)
        XCTAssertEqual(WindowStateStore(defaults: defaults).load(), state)
        XCTAssertNotNil(defaults.data(forKey: "TesseraWindowState.v1"))
    }

    func testAnUnreadableStateLoadsAsNone() {
        let store = WindowStateStore(defaults: defaults)
        defaults.set(Data("not json".utf8), forKey: WindowStateStore.key)
        XCTAssertNil(store.load())
        defaults.set("a string", forKey: WindowStateStore.key)
        XCTAssertNil(store.load())
    }

    func testRestoreFollowsTheCloseWindowsWhenQuittingSetting() throws {
        defaults.set(false, forKey: "NSQuitAlwaysKeepsWindows")
        XCTAssertFalse(WindowStateStore.quitKeepsWindows(defaults), "Close windows when quitting: on")
        defaults.set(true, forKey: "NSQuitAlwaysKeepsWindows")
        XCTAssertTrue(WindowStateStore.quitKeepsWindows(defaults), "Close windows when quitting: off")
        // In memory, so a global NSQuitAlwaysKeepsWindows does not show here.
        defaults.removeObject(forKey: "NSQuitAlwaysKeepsWindows")
        XCTAssertFalse(WindowStateStore.quitKeepsWindows(defaults), "unset, macOS's default: no restore, as AppKit")
    }

    // MARK: Restore plan

    func testRestorePlanForTwoWindowsOfThreeTabs() {
        let frames = [CGRect(x: 100, y: 100, width: 700, height: 500), CGRect(x: 200, y: 50, width: 900, height: 600)]
        let state = WindowState(windows: frames.map { .init(frame: $0, tabCount: 3) })

        let plan = state.restorePlan(screens: [Self.screen])

        XCTAssertEqual(plan, state.windows, "frames on screen are kept")
    }

    func testAnOffScreenFrameIsMovedOntoTheMainScreen() {
        let gone = CGRect(x: 5000, y: 3000, width: 800, height: 600)
        XCTAssertEqual(
            WindowState.clamp(gone, to: [Self.screen, Self.secondScreen]),
            CGRect(x: 640, y: 275, width: 800, height: 600), "to the main screen's far corner")
        let left = CGRect(x: -2000, y: -500, width: 800, height: 600)
        XCTAssertEqual(WindowState.clamp(left, to: [Self.screen]), CGRect(x: 0, y: 0, width: 800, height: 600))
        let plan = WindowState(windows: [.init(frame: gone, tabCount: 2)]).restorePlan(screens: [Self.screen])
        XCTAssertEqual(plan, [.init(frame: CGRect(x: 640, y: 275, width: 800, height: 600), tabCount: 2)])
    }

    func testAFrameStaysOnTheScreenItOverlapsMost() {
        let onSecond = CGRect(x: 1400, y: 100, width: 800, height: 600)
        XCTAssertEqual(
            WindowState.clamp(onSecond, to: [Self.screen, Self.secondScreen]),
            CGRect(x: 1440, y: 100, width: 800, height: 600))
        let insideSecond = CGRect(x: 2000, y: -100, width: 800, height: 600)
        XCTAssertEqual(WindowState.clamp(insideSecond, to: [Self.screen, Self.secondScreen]), insideSecond)
    }

    func testAFrameLargerThanItsScreenIsShrunkAndATinyOneGrown() {
        let huge = CGRect(x: -10, y: -10, width: 4000, height: 3000)
        XCTAssertEqual(WindowState.clamp(huge, to: [Self.screen]), Self.screen)
        let tiny = CGRect(x: 100, y: 100, width: 1, height: 1)
        XCTAssertEqual(WindowState.clamp(tiny, to: [Self.screen]), CGRect(x: 100, y: 100, width: 160, height: 80))
        XCTAssertEqual(WindowState.clamp(huge, to: []), huge, "no screen: unchanged")
    }

    func testRestorePlanDropsUnusableWindowsAndCapsTheSessions() {
        let frame = CGRect(x: 100, y: 100, width: 700, height: 500)
        let state = WindowState(windows: [
            .init(frame: CGRect(x: CGFloat.nan, y: 0, width: 700, height: 500), tabCount: 2),
            .init(frame: CGRect(x: 0, y: 0, width: CGFloat.infinity, height: 500), tabCount: 2),
            .init(frame: CGRect(x: 0, y: 0, width: 0, height: 500), tabCount: 2),
            .init(frame: frame, tabCount: 0),
            .init(frame: frame, tabCount: -5),
            .init(frame: frame, tabCount: 50),
            .init(frame: frame, tabCount: 1_000_000),
            .init(frame: frame, tabCount: 1),
        ])

        let plan = state.restorePlan(screens: [Self.screen])

        XCTAssertEqual(plan.map(\.tabCount), [1, 1, 50, 12], "64 sessions at most")
        XCTAssertEqual(plan.map(\.tabCount).reduce(0, +), WindowState.maxRestoredSessions)
        XCTAssertTrue(plan.allSatisfy { $0.frame == frame })
    }

    // MARK: Restoring and saving windows

    func testRestoringTwoWindowsOfThreeTabsRecreatesThemWithFreshSessions() throws {
        let screens = NSScreen.screens.map(\.visibleFrame)
        guard let main = screens.first, main.width >= 700, main.height >= 450 else {
            throw XCTSkip("no screen big enough to place the windows")
        }
        let state = WindowState(windows: [
            .init(frame: CGRect(x: main.minX + 20, y: main.minY + 30, width: 600, height: 400), tabCount: 3),
            .init(frame: CGRect(x: main.minX + 80, y: main.minY + 10, width: 620, height: 420), tabCount: 3),
        ])

        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: screens))

        XCTAssertEqual(appDelegate.windowControllers.count, 6)
        XCTAssertEqual(Set(appDelegate.windowControllers.map(\.session.id)).count, 6, "a session per tab")
        XCTAssertEqual(appDelegate.currentWindowState(), state)
        let groups = Set(appDelegate.windowControllers.compactMap { $0.window?.tabGroup }.map(ObjectIdentifier.init))
        XCTAssertEqual(groups.count, 2)
    }

    func testNoSavedStateOrNoWindowsRestoresNothing() {
        XCTAssertFalse(appDelegate.restoreWindows(from: nil, screens: [Self.screen]))
        XCTAssertFalse(appDelegate.restoreWindows(from: WindowState(windows: []), screens: [Self.screen]))
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }

    func testTheStateIsSavedAsWindowsAndTabsOpenMoveAndClose() async throws {
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store
        let window = try XCTUnwrap(appDelegate.newWindow().window)
        let background = try appDelegate.newTab(in: window)
        _ = try appDelegate.newTab(in: window)
        await Task.yield()
        XCTAssertEqual(store.load(), WindowState(windows: [.init(frame: window.frame, tabCount: 3)]))

        // The selected tab is the window on screen; the others follow it.
        let selected = try XCTUnwrap(window.tabGroup?.selectedWindow)
        let moved = selected.frame.offsetBy(dx: 30, dy: -20)
        selected.setFrame(moved, display: false)
        try await waitUntil("the moved frame is saved") { store.load()?.windows.first?.frame == moved }

        // A tab that is not selected: nothing moves or becomes main.
        XCTAssertFalse(window.tabGroup?.selectedWindow === background.window)
        background.window?.close()
        await Task.yield()
        XCTAssertEqual(store.load()?.windows.map(\.tabCount), [2])

        let other = try XCTUnwrap(appDelegate.newWindow().window)
        await Task.yield()
        XCTAssertEqual(store.load()?.windows.map(\.tabCount), [2, 1])
        XCTAssertEqual(store.load()?.windows.last?.frame, other.frame)
    }

    /// Quitting saves the windows as they are, whatever is still queued.
    func testQuittingSavesTheWindowState() throws {
        let window = try XCTUnwrap(appDelegate.newWindow().window)
        _ = try appDelegate.newTab(in: window)
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store

        XCTAssertEqual(appDelegate.applicationShouldTerminate(NSApp), .terminateNow)

        XCTAssertEqual(store.load(), WindowState(windows: [.init(frame: window.frame, tabCount: 2)]))
    }

    /// Layout changes come per event (a live resize sends one per mouse
    /// move): a flood of them costs one save.
    func testAFloodOfLayoutChangesSavesOnce() async throws {
        let controller = try appDelegate.newWindow()
        appDelegate.windowStateStore = WindowStateStore(defaults: defaults)

        let start = ContinuousClock.now
        for _ in 0..<100_000 {
            appDelegate.terminalWindowControllerDidChangeLayout(controller)
        }
        let elapsed = ContinuousClock.now - start
        await Task.yield()

        XCTAssertEqual(defaults.stateSaves, 1)
        print("100,000 layout changes queued in \(elapsed), saved \(defaults.stateSaves) time(s)")
        await Task.yield()
        XCTAssertEqual(defaults.stateSaves, 1, "nothing more queued")
    }

    /// Polls `condition`, letting the main actor run, until it holds or
    /// `timeout` passes.
    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else {
                throw PollTimedOut(description: "timed out after \(timeout): \(description)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
    }
}
