import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// #722: a session's spawn (its program lookup, `#!` check and PTY open)
/// runs off the main thread. The window or tab shows its placeholder at
/// once and becomes the terminal when the spawn returns; the main thread
/// keeps running meanwhile. Lookups are slowed with the core's
/// `tessera_test_delay_program_lookup` on a program path of this test's own.
@MainActor
final class SpawnOffMainTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var directory: URL!
    /// A link to `/bin/sh` that only these tests' sessions run, so the
    /// delay on its lookup slows nothing else.
    private var program: URL!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("SpawnOffMain-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        program = directory.appendingPathComponent("sh")
        try? FileManager.default.createSymbolicLink(atPath: program.path, withDestinationPath: "/bin/sh")
        appDelegate.spawnOptions = .init(program: program.path, arguments: ["-c", "printf ready; exec cat"])
        appDelegate.spawnsInBackground = true
    }

    override func tearDown() async throws {
        if let program, TesseraTestsCoreTestHooksLinked() {
            _ = tessera_test_delay_program_lookup(program.path, 0)
        }
        // Whatever a failed test left starting finishes before the next.
        appDelegate.windowStateStore = nil
        waitFor("no spawn runs", timeout: 30) { self.appDelegate.spawnsInFlight == 0 }
        let windows = appDelegate.windowControllers.compactMap { $0.window } + appDelegate.pendingTerminals.map { $0.window }
        for window in windows {
            window.close()
        }
        appDelegate.spawnsInBackground = false
        appDelegate.spawnOptions = .init()
        if let directory {
            try? FileManager.default.removeItem(at: directory)
        }
        appDelegate = nil
    }

    private func delayLookups(by milliseconds: UInt64) throws {
        guard TesseraTestsCoreTestHooksLinked() else {
            throw XCTSkip("the test host does not export the core's test hooks (a core built without test-hooks)")
        }
        XCTAssertEqual(tessera_test_delay_program_lookup(program.path, milliseconds), 0)
    }

    /// A key press as the keyboard sends it to `window` (a letter at its
    /// US/QWERTY key code, or Return).
    private func type(_ characters: String, keyCode: UInt16, into window: NSWindow) throws {
        let event = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, characters: characters,
                charactersIgnoringModifiers: characters, isARepeat: false, keyCode: keyCode))
        window.sendEvent(event)
    }

    /// Sessions that write the line they read to a file of this test's, for
    /// what reached them.
    private func echoToFile() -> URL {
        let file = directory.appendingPathComponent("typed")
        appDelegate.spawnOptions = .init(
            program: program.path, arguments: ["-c", "printf ready; IFS= read -r line; printf %s \"$line\" > '\(file.path)'; exec cat"])
        return file
    }

    /// Runs the main run loop until `condition` holds, failing after
    /// `timeout` seconds.
    private func waitFor(
        _ description: String, timeout: TimeInterval = 20, file: StaticString = #filePath, line: UInt = #line,
        _ condition: @escaping @MainActor () -> Bool
    ) {
        let deadline = Date(timeIntervalSinceNow: timeout)
        while !condition() {
            guard Date() < deadline else {
                XCTFail("timed out waiting until \(description)", file: file, line: line)
                return
            }
            RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: 0.01))
        }
    }

    /// Runs the main run loop until a task queued on the main actor now has
    /// run, and so every task queued before it (a coalesced save).
    private func waitForQueuedTasks(file: StaticString = #filePath, line: UInt = #line) {
        let ran = SpawnOutcome<Void>()
        Task { @MainActor in ran.value = .success(()) }
        waitFor("the main actor's queued tasks run", file: file, line: line) { ran.value != nil }
    }

    /// The main thread keeps running while a 3 s lookup holds a spawn that
    /// ⌘N started the way the menu does: a main-queue heartbeat never stops
    /// for long, the window is there at once with its placeholder, and it
    /// becomes the terminal when the spawn returns.
    func testTheMainThreadKeepsRunningWhileALookupHoldsTheSpawn() throws {
        try delayLookups(by: 3000)
        var lastTick = Date()
        var longestGap: TimeInterval = 0
        let heartbeat = Timer(timeInterval: 0.01, repeats: true) { _ in
            MainActor.assumeIsolated {
                longestGap = max(longestGap, -lastTick.timeIntervalSinceNow)
                lastTick = Date()
            }
        }
        RunLoop.main.add(heartbeat, forMode: .common)
        defer { heartbeat.invalidate() }
        let started = Date()

        appDelegate.openNewWindow(nil)

        let opened = -started.timeIntervalSinceNow
        XCTAssertLessThan(opened, 1.5, "⌘N returned at once")
        XCTAssertEqual(appDelegate.pendingTerminals.count, 1, "the window waits for its session")
        let window = try XCTUnwrap(appDelegate.pendingTerminals.first?.window)
        XCTAssertTrue(window.isVisible, "the window is on screen while its session starts")
        XCTAssertTrue(window.contentView is SpawnPlaceholderView, "it shows the placeholder")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)

        waitFor("the session runs") { !self.appDelegate.windowControllers.isEmpty }
        let waited = -started.timeIntervalSinceNow
        XCTAssertGreaterThanOrEqual(waited, 2.5, "the spawn waited on the slow lookup")
        let controller = try XCTUnwrap(appDelegate.windowControllers.first)
        XCTAssertTrue(controller.window === window, "the session's terminal took the placeholder's window")
        XCTAssertTrue(window.contentView is TerminalWindowContentView)
        XCTAssertTrue(appDelegate.pendingTerminals.isEmpty)
        // The session can arrive in the same run loop pass as the end of a
        // hold, before the next tick measures it: let the heartbeat tick.
        let ticked = lastTick
        waitFor("the heartbeat ticks again") { lastTick > ticked }
        // A spawn on the main thread stops the heartbeat for the whole 3 s.
        XCTAssertLessThan(longestGap, 1.5, "the main thread was held \(longestGap) s")
    }

    /// Closing the tab while its session starts closes the session when it
    /// arrives: nothing attaches, and the window, its placeholder and the
    /// session are all released.
    func testClosingAWindowWhileItsSessionStartsClosesTheSession() throws {
        try delayLookups(by: 1000)
        weak var weakWindow: NSWindow?
        weak var weakPending: PendingTerminal?
        var outcome: Result<TerminalWindowController, any Error>?
        autoreleasepool {
            let window = appDelegate.openWindow { outcome = $0 }
            weakWindow = window
            weakPending = appDelegate.pendingTerminals.first
            if let window {
                try? type("x", keyCode: 7, into: window)
            }
            XCTAssertEqual(weakPending?.typed.count, 1)
            window?.performClose(nil)
            XCTAssertEqual(weakPending?.typed.count ?? 0, 0, "the close dropped the typed keys")
        }
        XCTAssertTrue(appDelegate.pendingTerminals.isEmpty, "the closed window is no longer waited for")
        waitFor("the spawn returns") { outcome != nil }
        guard case .failure(let error) = outcome else {
            return XCTFail("a closed window got a terminal: \(String(describing: outcome))")
        }
        XCTAssertTrue(error is CancellationError, "\(error)")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "nothing attached to the closed window")
        waitFor("the window and its placeholder are released") { weakWindow == nil && weakPending == nil }
    }

    /// A restore opens every tab at once, in its saved order, each showing
    /// its placeholder; the sessions start one after the other in that
    /// order, and each tab becomes its own session's terminal.
    func testARestoreKeepsTheTabOrderWhileTheSessionsStart() throws {
        try delayLookups(by: 300)
        let state = WindowState(windows: [
            .init(frame: CGRect(x: 100, y: 100, width: 700, height: 450), tabCount: 3, selectedTab: 1)
        ])
        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: [CGRect(x: 0, y: 0, width: 1440, height: 875)]))
        let tabs = try XCTUnwrap((appDelegate.pendingTerminals.first?.window)?.tabs)
        XCTAssertEqual(tabs.count, 3, "every tab opened at once")
        XCTAssertEqual(appDelegate.pendingTerminals.map { $0.window }, tabs as? [TerminalWindow], "in tab order")
        XCTAssertTrue(tabs[1].tabGroup?.selectedWindow === tabs[1], "the saved tab is selected")

        waitFor("every session runs") { self.appDelegate.windowControllers.count == 3 }
        let ids = tabs.map { tab in
            appDelegate.windowControllers.first { $0.window === tab }?.sessions.first?.id ?? 0
        }
        XCTAssertEqual(ids, ids.sorted(), "the sessions started in tab order")
        XCTAssertFalse(ids.contains(0), "every tab has its session")
        XCTAssertTrue(tabs[1].tabGroup?.selectedWindow === tabs[1], "the selection stayed")
    }

    /// A window-state save during a restore, once the first tab's session
    /// runs while the others' still start, keeps every tab with its saved
    /// layout and the selected tab (adversarial A1): a crash or kill then
    /// loses none of them. The first tab, split, saves its saved layout
    /// while its second pane's session starts. A tab closed while its
    /// session starts is left out of the next save.
    func testASaveWhileARestoresSessionsStartKeepsEveryTabAndItsLayout() throws {
        try delayLookups(by: 500)
        let defaults = try XCTUnwrap(CountingDefaults(suiteName: TestDefaultsSuite.make()))
        let store = WindowStateStore(defaults: defaults)
        appDelegate.windowStateStore = store
        let sideBySide = PaneLayout.split(.right, ratio: 0.5, first: .leaf, second: .leaf)
        let stacked = PaneLayout.split(
            .below, ratio: 0.5, first: .leaf, second: .split(.right, ratio: 0.5, first: .leaf, second: .leaf))
        let layouts: [PaneLayout?] = [sideBySide, nil, stacked, nil]
        let state = WindowState(windows: [
            .init(
                frame: CGRect(x: 100, y: 100, width: 900, height: 600), tabCount: 4,
                tabs: layouts.map { WindowState.Tab(tree: $0) }, selectedTab: 1)
        ])
        XCTAssertTrue(appDelegate.restoreWindows(from: state, screens: [CGRect(x: 0, y: 0, width: 1440, height: 875)]))
        let tabs = try XCTUnwrap((appDelegate.pendingTerminals.first?.window)?.tabs)
        XCTAssertEqual(tabs.count, 4)
        XCTAssertEqual(defaults.stateSaves, 0, "nothing is saved before a session runs")

        waitFor("the first tab's session runs") { !self.appDelegate.windowControllers.isEmpty }
        // The attach queued its save on the main actor.
        waitForQueuedTasks()
        XCTAssertGreaterThan(defaults.stateSaves, 0, "the attach saved the window state")
        XCTAssertEqual(appDelegate.windowControllers.count, 1)
        XCTAssertEqual(appDelegate.pendingTerminals.count, 3, "the other tabs' sessions still start")
        XCTAssertEqual(appDelegate.windowControllers.first?.panes.count, 1, "the first tab's second pane still starts")
        let frame = tabs[1].frame
        XCTAssertEqual(
            store.load(),
            WindowState(windows: [
                .init(frame: frame, tabCount: 4, tabs: layouts.map { WindowState.Tab(tree: $0) }, selectedTab: 1)
            ]),
            "every tab is saved, with its layout")

        // Closed while its session starts: not saved.
        let closed = try XCTUnwrap(appDelegate.pendingTerminals.last?.window)
        XCTAssertTrue(closed === tabs[3])
        closed.close()
        waitForQueuedTasks()
        XCTAssertEqual(
            store.load(),
            WindowState(windows: [
                .init(frame: frame, tabCount: 3, tabs: layouts.prefix(3).map { WindowState.Tab(tree: $0) }, selectedTab: 1)
            ]),
            "the closed tab is left out")

        waitFor("every tab has its panes", timeout: 30) {
            self.appDelegate.pendingTerminals.isEmpty && self.appDelegate.spawnsInFlight == 0
                && self.appDelegate.windowControllers.count == 3
        }
        waitForQueuedTasks()
        let kept = try XCTUnwrap(store.load()?.windows.first)
        XCTAssertEqual(kept.tabCount, 3)
        XCTAssertEqual((0..<3).map { kept.layout(ofTab: $0)?.leafCount ?? 1 }, [2, 1, 3], "each tab has its panes")
        XCTAssertEqual(kept.selectedTab, 1)
    }

    /// A split asked for while its session starts, in a tab that closes
    /// meanwhile, closes that session and adds no pane.
    func testASplitWhoseTabClosesWhileItsSessionStartsAddsNothing() throws {
        try delayLookups(by: 1000)
        appDelegate.spawnsInBackground = false
        let controller = try appDelegate.newWindow()
        appDelegate.spawnsInBackground = true
        var outcome: Result<Pane?, any Error>?
        controller.beginSplit(.right) { outcome = $0 }
        XCTAssertNil(outcome, "the split waits for its session")
        XCTAssertEqual(controller.panes.count, 1)
        controller.window?.close()
        waitFor("the split's spawn returns") { outcome != nil }
        guard case .failure(let error) = outcome else {
            return XCTFail("a closed tab was split: \(String(describing: outcome))")
        }
        XCTAssertTrue(error is CancellationError, "\(error)")
        XCTAssertEqual(controller.panes.count, 1)
    }

    /// A split in a tab that stays open gets its pane once the session runs.
    func testASplitGetsItsPaneOnceItsSessionRuns() throws {
        try delayLookups(by: 500)
        appDelegate.spawnsInBackground = false
        let controller = try appDelegate.newWindow()
        appDelegate.spawnsInBackground = true
        var outcome: Result<Pane?, any Error>?
        controller.beginSplit(.right) { outcome = $0 }
        waitFor("the split's spawn returns") { outcome != nil }
        let pane = try XCTUnwrap(try XCTUnwrap(outcome).get())
        XCTAssertEqual(controller.panes.count, 2)
        XCTAssertTrue(controller.focusedPane === pane)
    }

    /// The core shut down (as at a quit) while a session starts: the
    /// shutdown waits for the spawn, the free closes the session, the
    /// window closes and nothing attaches.
    func testShuttingTheCoreDownWhileASessionStartsAttachesNothing() throws {
        try delayLookups(by: 1000)
        var outcome: Result<TerminalWindowController, any Error>?
        let window = try XCTUnwrap(appDelegate.openWindow { outcome = $0 })
        XCTAssertEqual(appDelegate.spawnsInFlight, 1)
        // Into the slow lookup, so the shutdown meets the spawn in the core.
        let opened = Date()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.3))
        XCTAssertNil(outcome, "the spawn still runs")
        try appDelegate.replaceCore(start: false)
        XCTAssertGreaterThan(-opened.timeIntervalSinceNow, 0.8, "the shutdown waited for the spawn")
        waitFor("the spawn returns") { outcome != nil }
        guard case .failure = outcome else {
            return XCTFail("a session of a freed core attached: \(String(describing: outcome))")
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        XCTAssertFalse(window.isVisible, "the window closed")
    }

    /// #722 B2: keys typed while the placeholder shows reach the session
    /// once it runs, before what is typed after, in order.
    func testKeysTypedWhileTheSessionStartsReachIt() throws {
        let file = echoToFile()
        try delayLookups(by: 800)
        appDelegate.openNewWindow(nil)
        let pending = try XCTUnwrap(appDelegate.pendingTerminals.first)
        let window = pending.window
        try type("x", keyCode: 7, into: window)
        try type("x", keyCode: 7, into: window)
        XCTAssertEqual(pending.typed.count, 2, "the placeholder kept the keys")
        waitFor("the session runs") { !self.appDelegate.windowControllers.isEmpty }
        try type("y", keyCode: 16, into: window)
        try type("\r", keyCode: 36, into: window)
        waitFor("the line reaches the session") { FileManager.default.fileExists(atPath: file.path) }
        waitFor("the line is written") { (try? String(contentsOf: file, encoding: .utf8)) == "xxy" }
        XCTAssertEqual(try? String(contentsOf: file, encoding: .utf8), "xxy")
        XCTAssertTrue(pending.typed.isEmpty, "the keys were handed over once")
    }

    /// #722 B2: at most `maxTypedKeys` keys are kept while a session starts;
    /// the rest are dropped and counted, and the kept ones reach it.
    func testKeysTypedPastTheCapWhileTheSessionStartsAreDropped() throws {
        let file = echoToFile()
        try delayLookups(by: 1500)
        appDelegate.openNewWindow(nil)
        let pending = try XCTUnwrap(appDelegate.pendingTerminals.first)
        let cap = PendingTerminal.maxTypedKeys
        for _ in 0..<(cap + 44) {
            try type("x", keyCode: 7, into: pending.window)
        }
        XCTAssertEqual(pending.typed.count, cap)
        XCTAssertEqual(pending.droppedKeys, 44)
        waitFor("the session runs") { !self.appDelegate.windowControllers.isEmpty }
        try type("\r", keyCode: 36, into: pending.window)
        waitFor("the line is written") { (try? String(contentsOf: file, encoding: .utf8))?.isEmpty == false }
        XCTAssertEqual(try? String(contentsOf: file, encoding: .utf8), String(repeating: "x", count: cap))
    }

    /// #722 B1: a failed spawn's alert leaves the main queue running: a
    /// block queued on it while the alert is up runs before the alert ends
    /// (session deliveries and the termination signal sources, #441, are
    /// such blocks).
    func testTheMainQueueRunsWhileAFailedSpawnsAlertIsUp() throws {
        try delayLookups(by: 1000)
        appDelegate.openNewWindow(nil)
        let pending = try XCTUnwrap(appDelegate.pendingTerminals.first)
        try type("x", keyCode: 7, into: pending.window)
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.3))
        // Fails the spawn (the core is freed under it); ⌘N's completion
        // shows the error alert.
        try appDelegate.replaceCore(start: false)
        let probe = ModalProbe()
        let watch = Timer(timeInterval: 0.01, repeats: true) { timer in
            let done = MainActor.assumeIsolated { probe.tick() }
            if done { timer.invalidate() }
        }
        RunLoop.main.add(watch, forMode: .common)
        defer { watch.invalidate() }
        waitFor("the alert came and went", timeout: 30) { probe.ranWhileModal != nil && NSApp.modalWindow == nil }
        XCTAssertEqual(probe.ranWhileModal, true, "a main-queue block waited for the alert")
        XCTAssertTrue(pending.typed.isEmpty, "the failed spawn dropped the typed keys")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
    }
}

/// Watches for an application-modal alert from a run loop timer: once one
/// is up it queues a main-queue block, then ends the alert when the block
/// has run, or after 2 s without it, recording which.
@MainActor
private final class ModalProbe {
    private var queued: Date?
    private var queueRan = false
    private(set) var ranWhileModal: Bool?

    /// One tick of the timer; true once the alert was ended.
    func tick() -> Bool {
        guard NSApp.modalWindow != nil else { return false }
        guard let since = queued else {
            queued = Date()
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self.queueRan = true }
            }
            return false
        }
        guard queueRan || -since.timeIntervalSinceNow > 2 else { return false }
        ranWhileModal = queueRan
        NSApp.abortModal()
        return true
    }
}
