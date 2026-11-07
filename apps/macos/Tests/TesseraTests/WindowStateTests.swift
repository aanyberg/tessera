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
        XCTAssertEqual(Set(appDelegate.windowControllers.map(\.focusedPane.session.id)).count, 6, "a session per tab")
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

    // MARK: Pane layouts (state file v2, #38b)

    private typealias Layout = PaneLayout

    /// L | (R1 / R2).
    private static let threePanes = Layout.split(
        .right, ratio: 0.3, first: .leaf, second: .split(.below, ratio: 0.6, first: .leaf, second: .leaf))

    /// A tree of `count` panes in a row, each split taking one more pane
    /// off the right: `count - 1` splits deep.
    private static func chain(_ count: Int, ratio: Double = 0.5) -> Layout {
        count <= 1 ? .leaf : .split(.right, ratio: ratio, first: .leaf, second: chain(count - 1, ratio: ratio))
    }

    /// A balanced tree of `count` panes: about log2(count) deep.
    private static func balanced(_ count: Int) -> Layout {
        guard count > 1 else { return .leaf }
        return .split(.below, ratio: 0.5, first: balanced(count / 2), second: balanced(count - count / 2))
    }

    func testAThreePaneTabIsRestoredWithItsTreeAndDividersWithinACell() throws {
        let screens = NSScreen.screens.map(\.visibleFrame)
        guard let main = screens.first, main.width >= 1000, main.height >= 700 else {
            throw XCTSkip("no screen big enough to place the window")
        }
        let frame = CGRect(x: main.minX + 20, y: main.minY + 20, width: 960, height: 640)
        let stacked = Layout.split(.below, ratio: 0.5, first: .leaf, second: .leaf)
        let state = WindowState(windows: [
            .init(frame: frame, tabCount: 3, tabs: [.init(tree: stacked), .init(tree: Self.threePanes), .init(tree: nil)])
        ])

        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: screens))

        XCTAssertEqual(appDelegate.windowControllers.map(\.panes.count), [2, 3, 1], "the first tab's tree too")
        let tab = appDelegate.windowControllers[1]
        XCTAssertEqual(Set(appDelegate.windowControllers.flatMap(\.sessions).map(\.id)).count, 6, "a session per pane")
        XCTAssertTrue(tab.focusedPane === tab.panes[0], "the first pane has the focus")
        tab.paneContainer.layoutSubtreeIfNeeded()
        let bounds = tab.paneContainer.bounds
        let frames = tab.paneContainer.paneFrames.map(\.frame)
        let cell = tab.panes[0].view.metrics.map {
            CGSize(width: CGFloat($0.cellWidthPx) / CGFloat($0.scale), height: CGFloat($0.cellHeightPx) / CGFloat($0.scale))
        } ?? PaneContainerView.fallbackCellSize
        XCTAssertEqual(frames[0].width, 0.3 * (bounds.width - 1), accuracy: cell.width, "the outer divider")
        XCTAssertEqual(frames[1].minX, frames[0].maxX + 1)
        XCTAssertEqual(frames[1].height, 0.6 * (bounds.height - 1), accuracy: cell.height, "the inner divider")
        XCTAssertEqual(frames[2].minY, frames[1].maxY + 1)
        XCTAssertEqual(frames[1].width, frames[2].width)
        // And saved again as it came back.
        let saved = try XCTUnwrap(appDelegate.currentWindowState().windows.first)
        XCTAssertEqual(saved.tabCount, 3)
        guard case .split(.below, let half, .leaf, .leaf) = saved.layout(ofTab: 0)
        else { return XCTFail("the first tab: \(String(describing: saved.layout(ofTab: 0)))") }
        XCTAssertEqual(half, 0.5, accuracy: cell.height / bounds.height)
        XCTAssertNil(saved.layout(ofTab: 2))
        guard case .split(.right, let outer, .leaf, .split(.below, let inner, .leaf, .leaf)) = saved.layout(ofTab: 1)
        else { return XCTFail("the tree: \(String(describing: saved.layout(ofTab: 1)))") }
        XCTAssertEqual(outer, 0.3, accuracy: cell.width / bounds.width)
        XCTAssertEqual(inner, 0.6, accuracy: cell.height / bounds.height)
    }

    func testAFirstFormatFileRestoresOnePanePerTab() throws {
        let json = #"{"windows":[{"frame":[[40,60],[700,450]],"tabCount":2}]}"#
        defaults.set(Data(json.utf8), forKey: WindowStateStore.key)
        let state = try XCTUnwrap(WindowStateStore(defaults: defaults).load())
        XCTAssertEqual(state, WindowState(windows: [.init(frame: CGRect(x: 40, y: 60, width: 700, height: 450), tabCount: 2)]))
        XCTAssertNil(state.windows[0].tabs)
        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: NSScreen.screens.map(\.visibleFrame)))
        XCTAssertEqual(appDelegate.windowControllers.map(\.panes.count), [1, 1])
    }

    /// S5: every hostile layout restores as one pane per tab, or within
    /// the session cap, and none crashes.
    func testHostileLayoutsRestoreOnePaneOrAreCapped() {
        let frame = CGRect(x: 100, y: 100, width: 700, height: 500)
        let table: [(String, Layout, expected: Int)] = [
            ("16 panes", Self.balanced(16), 16),
            ("17 panes", Self.balanced(17), 1),
            ("16 panes 15 splits deep", Self.chain(16), 16),
            ("17 panes 16 splits deep", Self.chain(17), 1),
            ("NaN", Self.chain(2, ratio: .nan), 1),
            ("infinity", Self.chain(2, ratio: .infinity), 1),
            ("0", Self.chain(2, ratio: 0), 1),
            ("1", Self.chain(2, ratio: 1), 1),
            ("-0.5", Self.chain(2, ratio: -0.5), 1),
            ("2", Self.chain(2, ratio: 2), 1),
            ("a nested NaN", .split(.right, ratio: 0.5, first: .leaf, second: Self.chain(2, ratio: .nan)), 1),
            ("a lone pane", .leaf, 1),
        ]
        for (name, layout, expected) in table {
            let state = WindowState(windows: [.init(frame: frame, tabCount: 1, tabs: [.init(tree: layout)])])
            let plan = state.restorePlan(screens: [Self.screen])
            XCTAssertEqual(plan.count, 1, name)
            XCTAssertEqual(plan.first?.layout(ofTab: 0)?.leafCount ?? 1, expected, name)
        }

        // More than 64 panes in all: tabs that do not fit what is left are
        // one pane, and the tabs past the cap are dropped.
        let full = Self.balanced(16)
        let state = WindowState(windows: [
            .init(frame: frame, tabCount: 5, tabs: [full, full, full, Self.balanced(10), full].map { .init(tree: $0) }),
            .init(frame: frame, tabCount: 9, tabs: [.init(tree: Self.balanced(4))]),
            .init(frame: frame, tabCount: 3),
        ])
        let plan = state.restorePlan(screens: [Self.screen])
        let panes = plan.map { window in (0..<window.tabCount).map { window.layout(ofTab: $0)?.leafCount ?? 1 } }
        XCTAssertEqual(panes, [[16, 16, 16, 10, 1], [4, 1]], "the third window is past the cap: \(panes)")
        XCTAssertEqual(panes.flatMap { $0 }.reduce(0, +), WindowState.maxRestoredSessions)
    }

    /// The shape of `layout` without its ratios: `right(p,below(p,p))`.
    private static func shape(_ layout: Layout) -> String {
        switch layout {
        case .leaf: "p"
        case .split(let direction, _, let first, let second): "\(direction)(\(shape(first)),\(shape(second)))"
        }
    }

    /// ⌘D splits the focused pane and focuses the new one, so 15 of them
    /// make 16 panes 15 splits deep, the deepest tree a tab can hold: it is
    /// saved, read back and restored with all 16 panes in the same shape.
    func testSixteenPanesMadeByRepeatedSplitRightAreRestored() throws {
        let screens = NSScreen.screens.map(\.visibleFrame)
        guard let main = screens.first else { throw XCTSkip("no screen") }
        let controller = try appDelegate.newWindow(
            frame: CGRect(x: main.minX, y: main.minY, width: main.width, height: 400))
        controller.paneContainer.layoutSubtreeIfNeeded()
        let pane = PaneContainerView.minimumSize(of: controller.focusedPane.view)
        let needed = CGFloat(PaneTree<Pane>.maxLeaves) * (pane.width + PaneSplitView.thickness)
        guard needed <= controller.paneContainer.bounds.width else {
            throw XCTSkip("16 panes side by side need \(needed) pt, the screen is \(main.width) pt wide")
        }
        for _ in 1..<PaneTree<Pane>.maxLeaves {
            XCTAssertNotNil(try controller.splitFocusedPane(.right))
            controller.paneContainer.layoutSubtreeIfNeeded()
        }
        XCTAssertEqual(controller.paneContainer.tree.depth, PaneTree<Pane>.maxLeaves - 1)
        let layout = try XCTUnwrap(controller.paneLayout)
        let store = WindowStateStore(defaults: defaults)
        store.save(appDelegate.currentWindowState())
        let loaded = try XCTUnwrap(store.load())
        controller.window?.close()

        XCTAssertTrue(appDelegate.restoreWindows(from: loaded, screens: screens))

        let restored = try XCTUnwrap(appDelegate.windowControllers.last)
        XCTAssertFalse(restored === controller)
        XCTAssertEqual(restored.panes.count, PaneTree<Pane>.maxLeaves)
        XCTAssertEqual(restored.paneLayout.map(Self.shape), Self.shape(layout))
    }

    /// ⌘D and ⌘⇧D in turn, each splitting the pane the last one made: 16
    /// panes 15 splits deep. No window is big enough to show them (the
    /// last pane gets 1/256 of the width), so they are checked as far as
    /// the state file and the restore plan, which keep all 16.
    func testSixteenPanesSplitRightAndBelowInTurnAreSavedAndPlanned() throws {
        var tree = PaneTree(0)
        for pane in 1..<PaneTree<Int>.maxLeaves {
            XCTAssertTrue(tree.split(pane - 1, pane.isMultiple(of: 2) ? .below : .right, adding: pane))
        }
        XCTAssertEqual(tree.depth, PaneTree<Int>.maxLeaves - 1)
        let layout = Layout(tree)
        let state = WindowState(windows: [
            .init(frame: CGRect(x: 100, y: 100, width: 700, height: 500), tabCount: 1, tabs: [.init(tree: layout)])
        ])
        let store = WindowStateStore(defaults: defaults)
        store.save(state)

        let loaded = try XCTUnwrap(store.load())

        XCTAssertEqual(loaded, state)
        let planned = try XCTUnwrap(loaded.restorePlan(screens: [Self.screen]).first?.layout(ofTab: 0))
        XCTAssertEqual(planned.leafCount, PaneTree<Int>.maxLeaves)
        XCTAssertEqual(Self.shape(planned), Self.shape(layout))
        XCTAssertTrue(Self.shape(planned).hasPrefix("right(p,below(p,right(p,"), Self.shape(planned))
    }

    /// S5 in the file itself: what JSON can hold that the model cannot.
    func testHostileStateFilesDecodeToOnePanePerTabOrNothing() throws {
        let store = WindowStateStore(defaults: defaults)
        func load(_ tabs: String) -> WindowState? {
            let json = #"{"windows":[{"frame":[[40,60],[700,450]],"tabCount":2,"tabs":"# + tabs + "}]}"
            defaults.set(Data(json.utf8), forKey: WindowStateStore.key)
            return store.load()
        }
        let split = #"{"split":{"direction":"right","ratio":0.5,"first":{"leaf":{}},"second":{"leaf":{}}}}"#
        XCTAssertEqual(load("[{\"tree\":\(split)}]")?.windows.first?.layout(ofTab: 0)?.leafCount, 2, "a good one")
        let hostile: [(String, String)] = [
            ("tabs not a list", #""tabs""#),
            ("a tab not an object", "[5]"),
            ("a leaf not an object", #"[{"tree":{"leaf":5}}]"#),
            ("a ratio as a string", #"[{"tree":{"split":{"direction":"right","ratio":"nan","first":{"leaf":{}},"second":{"leaf":{}}}}}]"#),
            ("a ratio too large for a double", #"[{"tree":{"split":{"direction":"right","ratio":1e999,"first":{"leaf":{}},"second":{"leaf":{}}}}}]"#),
            ("an unknown direction", #"[{"tree":{"split":{"direction":"up","ratio":0.5,"first":{"leaf":{}},"second":{"leaf":{}}}}}]"#),
            ("a split without a child", #"[{"tree":{"split":{"direction":"right","ratio":0.5,"first":{"leaf":{}}}}}]"#),
            ("neither leaf nor split", #"[{"tree":{"pane":{}}}]"#),
            ("nested too deep to decode", "[{\"tree\":" + String(repeating: #"{"split":{"direction":"right","ratio":0.5,"first":{"leaf":{}},"second":"#, count: 40) + #"{"leaf":{}}"# + String(repeating: "}}", count: 40) + "}]"),
        ]
        for (name, tabs) in hostile {
            let state = load(tabs)
            // Either the window with one pane per tab, or (a document the
            // parser refuses) no state at all: one fresh window.
            if let window = state?.windows.first {
                XCTAssertEqual(window.tabCount, 2, name)
                let plan = try XCTUnwrap(state?.restorePlan(screens: [Self.screen]).first, name)
                XCTAssertEqual((0..<plan.tabCount).map { plan.layout(ofTab: $0)?.leafCount ?? 1 }, [1, 1], name)
            }
        }
        // The decoder itself stops at the depth a restorable tree can
        // have, before the parser's limit (about 256 splits) would.
        func nested(_ depth: Int) -> Data {
            Data(
                (String(repeating: #"{"split":{"direction":"right","ratio":0.5,"first":{"leaf":{}},"second":"#, count: depth)
                    + #"{"leaf":{}}"# + String(repeating: "}}", count: depth)).utf8)
        }
        XCTAssertNoThrow(try JSONDecoder().decode(PaneLayout.self, from: nested(PaneLayout.maxDepth)))
        XCTAssertThrowsError(try JSONDecoder().decode(PaneLayout.self, from: nested(200)), "200 splits deep")
        // A document nested far past any parser's limit: refused, not a
        // crash (the whole state is unreadable, so one fresh window opens).
        let depth = 100_000
        defaults.set(
            Data(("[" + String(repeating: #"{"split":{"first":"#, count: depth) + String(repeating: "}}", count: depth) + "]").utf8),
            forKey: WindowStateStore.key)
        XCTAssertNil(store.load(), "a document 100,000 levels deep")
    }

    /// A restored layout whose panes at their smallest size do not fit the
    /// window comes back as one pane, without a crash.
    func testALayoutThatDoesNotFitItsWindowRestoresOnePane() throws {
        let screens = NSScreen.screens.map(\.visibleFrame)
        guard let main = screens.first else { throw XCTSkip("no screen") }
        let frame = CGRect(x: main.minX + 20, y: main.minY + 20, width: 200, height: 120)
        let state = WindowState(windows: [.init(frame: frame, tabCount: 1, tabs: [.init(tree: Self.balanced(16))])])
        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: screens))
        XCTAssertEqual(appDelegate.windowControllers.map(\.panes.count), [1])
    }

    /// Only split tabs are saved with a tree; a state of single panes is
    /// the first format, which an older Tessera reads.
    func testTheStateKeepsATreeOnlyForASplitTab() throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        var json = String(decoding: try JSONEncoder().encode(appDelegate.currentWindowState()), as: UTF8.self)
        XCTAssertFalse(json.contains("tabs"), json)
        second.window?.setContentSize(NSSize(width: 900, height: 600))
        XCTAssertNotNil(try second.splitFocusedPane(.below))
        let state = appDelegate.currentWindowState()
        XCTAssertNil(state.windows[0].layout(ofTab: 0))
        XCTAssertEqual(state.windows[0].layout(ofTab: 1), .split(.below, ratio: 0.5, first: .leaf, second: .leaf))
        json = String(decoding: try JSONEncoder().encode(state), as: UTF8.self)
        XCTAssertTrue(json.contains(#""tabs":[{},{"tree":{"split":"#), json)
        XCTAssertEqual(try JSONDecoder().decode(WindowState.self, from: Data(json.utf8)), state)
    }

    /// A split, a close and a divider drag each save the state.
    func testPaneChangesSaveTheState() async throws {
        let store = WindowStateStore(defaults: defaults)
        let controller = try appDelegate.newWindow()
        controller.window?.setContentSize(NSSize(width: 900, height: 600))
        appDelegate.windowStateStore = store
        _ = try XCTUnwrap(controller.splitFocusedPane(.right))
        try await waitUntil("the split is saved") { store.load()?.windows.first?.layout(ofTab: 0)?.leafCount == 2 }
        let splitView = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView)
        splitView.setPosition(300, ofDividerAt: 0)
        try await waitUntil("the drag is saved") {
            guard case .split(_, let ratio, _, _) = store.load()?.windows.first?.layout(ofTab: 0) else { return false }
            return abs(ratio - 300 / 899) < 0.002
        }
        controller.closeFocusedPane(nil)
        try await waitUntil("the close is saved") {
            store.load().map { $0.windows.first?.tabs == nil } ?? false
        }
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
