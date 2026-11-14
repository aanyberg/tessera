import AppKit
import Metal
import TesseraKit
import XCTest

@testable import Tessera

private struct BlurTimedOut: Error, CustomStringConvertible {
    var description: String
}

/// Stands in for WindowServer (``WindowBlurRadius/backend`` and
/// ``WindowBlurRadius/windowNumber``): records every radius set, and gives
/// a window the test "shows" a number, since the tests' windows are never
/// on screen (and must not depend on being so, locally or on CI).
@MainActor
private final class BlurRecorder {
    struct Call: Equatable {
        var windowNumber: Int
        var radius: UInt32
    }

    private(set) var calls: [Call] = []
    /// What the call returns: 0 for submitted.
    var result: Int32 = 0
    private var numbers: [ObjectIdentifier: Int] = [:]
    private var nextNumber = 9000

    var backend: WindowBlurRadius.Backend {
        WindowBlurRadius.Backend { [unowned self] number, radius in
            calls.append(Call(windowNumber: number, radius: radius))
            return result
        }
    }

    func number(of window: NSWindow) -> Int? {
        numbers[ObjectIdentifier(window)]
    }

    /// Puts `window` "on screen" with a new number, as AppKit posts it.
    @discardableResult
    func show(_ window: NSWindow) -> Int {
        nextNumber += 1
        numbers[ObjectIdentifier(window)] = nextNumber
        NotificationCenter.default.post(name: NSWindow.didChangeOcclusionStateNotification, object: window)
        return nextNumber
    }

    /// Takes `window` "off screen": it has no number, and nothing is posted.
    func hide(_ window: NSWindow) {
        numbers[ObjectIdentifier(window)] = nil
    }

    func calls(for window: NSWindow) -> [UInt32] {
        guard let number = number(of: window) else { return [] }
        return calls.filter { $0.windowNumber == number }.map(\.radius)
    }
}

/// The window's blur radius (#390, ADR 0045) through the seam that records
/// the private call: set once the window is on screen, only while every
/// translucent pane of the tab blurs, 0 again for a mixed or opaque tab,
/// clamped, the panes' own material hidden meanwhile and the strip's kept
/// (D4); a missing or failing call falls back to the material and logs once.
/// On a core of the test's own, as ``TransparencyTests``.
@MainActor
final class BlurRadiusTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var host: TransparencyHost?
    private var recorder: BlurRecorder!
    private var savedBackend: WindowBlurRadius.Backend?
    private var savedWindowNumber: (@MainActor (NSWindow) -> Int?)!

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-blur-radius-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try TransparencyTests.config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        recorder = BlurRecorder()
        savedBackend = WindowBlurRadius.backend
        savedWindowNumber = WindowBlurRadius.windowNumber
        WindowBlurRadius.backend = recorder.backend
        WindowBlurRadius.windowNumber = { [recorder] in recorder?.number(of: $0) }
        WindowBlurRadius.resetLoggedFallback()
    }

    override func tearDown() async throws {
        if let host {
            for controller in host.controllers {
                controller.window?.close()
            }
            host.core.shutdown()
        }
        host = nil
        WindowBlurRadius.backend = savedBackend
        WindowBlurRadius.windowNumber = savedWindowNumber
        WindowBlurRadius.resetLoggedFallback()
        recorder = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func startHost() throws -> TransparencyHost {
        let host = TransparencyHost(core: try TesseraApp())
        self.host = host
        return host
    }

    private func requireSurfaces(_ panes: [Pane]) throws {
        guard MTLCreateSystemDefaultDevice() != nil, panes.allSatisfy(\.view.surface.isAttached) else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
    }

    private func split(_ controller: TerminalWindowController, profile: String?) throws -> Pane {
        host?.nextProfile = profile
        let pane = try XCTUnwrap(controller.splitFocusedPane(.right), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        return pane
    }

    private func content(_ controller: TerminalWindowController) throws -> TerminalWindowContentView {
        try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
    }

    private func waitUntil(_ description: String, _ condition: () throws -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while try !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                throw BlurTimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// Asserts whether the window is blurred at its radius: the panes'
    /// own materials hidden then, shown otherwise; the strip keeps its
    /// material, shown, either way (D4).
    private func assertRadius(
        _ controller: TerminalWindowController, _ active: Bool, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let content = try content(controller)
        XCTAssertEqual(content.usesBlurRadius, active, "\(what): blurred at the radius", file: file, line: line)
        for pane in controller.panes {
            if let backdrop = pane.view.backdrop {
                XCTAssertEqual(backdrop.isHidden, active, "\(what): the pane's own material", file: file, line: line)
            }
        }
        let strip = try XCTUnwrap(content.titlebarBackdrop, "\(what): the strip is blurred", file: file, line: line)
        XCTAssertTrue(strip.superview === content, "\(what): the strip's material in the window", file: file, line: line)
        XCTAssertFalse(strip.isHidden, "\(what): the strip keeps its material (D4)", file: file, line: line)
        XCTAssertEqual(strip.material, .fullScreenUI, "\(what)", file: file, line: line)
        XCTAssertEqual(strip.blendingMode, .behindWindow, "\(what)", file: file, line: line)
        XCTAssertEqual(strip.state, .active, "\(what)", file: file, line: line)
    }

    // MARK: The lookup

    /// The functions are found on this macOS (the CI runner's included);
    /// a missing one would leave every window on the material.
    func testTheSystemsCallIsFound() {
        XCTAssertNotNil(WindowBlurRadius.systemBackend(), "CGSSetWindowBackgroundBlurRadius and CGSMainConnectionID")
    }

    /// The radius passed is never above the largest (a larger one, or a
    /// negative one wrapped, gives the same blur as 64).
    func testTheCallsRadiusIsClamped() {
        XCTAssertTrue(WindowBlurRadius.set(500, windowNumber: 7))
        XCTAssertTrue(WindowBlurRadius.set(UInt32.max, windowNumber: 7))
        XCTAssertTrue(WindowBlurRadius.set(64, windowNumber: 7))
        XCTAssertTrue(WindowBlurRadius.set(63, windowNumber: 7))
        XCTAssertEqual(recorder.calls.map(\.radius), [64, 64, 64, 63])
    }

    /// The real window number guard: a window that is not on screen (never
    /// ordered in, deferred or not) has no number to set a radius on, since
    /// a call then has no effect and is lost (ADR 0045).
    func testAWindowNotOnScreenHasNoNumber() throws {
        let windowNumber = try XCTUnwrap(savedWindowNumber)
        for deferred in [true, false] {
            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
                defer: deferred)
            window.isReleasedWhenClosed = false
            XCTAssertFalse(window.isVisible)
            XCTAssertNil(windowNumber(window), "deferred \(deferred): number \(window.windowNumber)")
            window.close()
        }
    }

    // MARK: When the window is blurred at the radius

    func testARadiusBlursTheWholeWindowOnceItIsOnScreen() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        let window = try XCTUnwrap(controller.window)
        let pane = controller.focusedPane
        XCTAssertNotNil(pane.view.backdrop, "a blurred pane")

        controller.windowBlurRadius = 20
        XCTAssertTrue(recorder.calls.isEmpty, "no call before the window is on screen (its number is -1)")
        try assertRadius(controller, false, "not on screen: the material")

        let number = recorder.show(window)
        XCTAssertEqual(recorder.calls, [.init(windowNumber: number, radius: 20)], "set once it is on screen")
        try assertRadius(controller, true, "on screen")

        try content(controller).updateWindowBlur()
        controller.windowBlurRadius = 20
        XCTAssertEqual(recorder.calls.count, 1, "nothing changed: no call")

        controller.windowBlurRadius = 30
        controller.windowBlurRadius = 200
        XCTAssertEqual(recorder.calls(for: window), [20, 30, 64], "each change, clamped to 64")

        controller.windowBlurRadius = 0
        XCTAssertEqual(recorder.calls(for: window), [20, 30, 64, 0], "0 turns it off")
        try assertRadius(controller, false, "radius 0: the material again")

        controller.windowBlurRadius = 10
        let renumbered = recorder.show(window)
        XCTAssertEqual(
            recorder.calls.suffix(2), [.init(windowNumber: number, radius: 10), .init(windowNumber: renumbered, radius: 10)],
            "a new window number gets the radius again")
        try assertRadius(controller, true, "renumbered")
    }

    /// #405 m1, n3: while the window is off screen (an unselected tab,
    /// minimized) it keeps the radius set on it, so an update meanwhile (a
    /// theme reload, a new pane) keeps the panes' own material hidden, and
    /// reselecting the tab does not flash it.
    func testAnUpdateWhileOffScreenKeepsTheBlurState() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        let window = try XCTUnwrap(controller.window)
        recorder.show(window)
        controller.windowBlurRadius = 20
        try assertRadius(controller, true, "on screen")

        recorder.hide(window)
        for pane in controller.panes {
            pane.view.appearanceDidChange()
        }
        try assertRadius(controller, true, "off screen, the theme reloaded")
        let pane = try split(controller, profile: nil)
        try requireSurfaces([pane])
        XCTAssertNotNil(pane.view.backdrop, "a blurred pane")
        try assertRadius(controller, true, "off screen, a new pane")
        XCTAssertEqual(recorder.calls(for: window), [], "no call without a number")
        XCTAssertEqual(recorder.calls.map(\.radius), [20])

        recorder.show(window)
        try assertRadius(controller, true, "back on screen")
        XCTAssertEqual(recorder.calls(for: window), [20], "set on the new number")
    }

    /// D5 (ii): a tab with a translucent pane that asks for no blur is not
    /// blurred at the radius; each pane keeps its own material (or none).
    /// An opaque pane does not count.
    func testAMixedTabKeepsEachPanesMaterial() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let window = try XCTUnwrap(controller.window)
        recorder.show(window)
        controller.windowBlurRadius = 20
        try requireSurfaces(controller.panes)
        XCTAssertEqual(recorder.calls(for: window), [20])

        let solid = try split(controller, profile: "solid")
        try requireSurfaces([solid])
        XCTAssertEqual(recorder.calls(for: window), [20], "an opaque pane leaves the condition met")
        try assertRadius(controller, true, "with an opaque pane")

        let clear = try split(controller, profile: "clear")
        try requireSurfaces([clear])
        XCTAssertEqual(recorder.calls(for: window), [20, 0], "a translucent pane without blur: radius 0")
        try assertRadius(controller, false, "mixed")
        XCTAssertNotNil(controller.panes.first?.view.backdrop, "the blurred pane keeps its material")
        XCTAssertNil(clear.view.backdrop, "the clear pane stays unblurred")

        controller.closeFocusedPane(nil)
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertEqual(controller.panes.count, 2)
        XCTAssertEqual(recorder.calls(for: window), [20, 0, 20], "without it the window is blurred again")
        try assertRadius(controller, true, "the clear pane closed")
    }

    /// A pane hidden behind a maximized one still counts (E7): a hidden
    /// translucent pane without blur keeps the window off the radius.
    func testAHiddenPaneBehindAMaximizedOneCounts() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let window = try XCTUnwrap(controller.window)
        recorder.show(window)
        controller.windowBlurRadius = 20
        let frosted = controller.focusedPane
        let clear = try split(controller, profile: "clear")
        try requireSurfaces([frosted, clear])
        XCTAssertEqual(recorder.calls(for: window), [20, 0])

        controller.focus(frosted)
        XCTAssertTrue(controller.toggleZoom(), "the blurred pane maximized")
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertTrue(clear.view.isHiddenOrHasHiddenAncestor, "the clear pane is hidden behind it")
        try content(controller).updateWindowBlur()
        XCTAssertEqual(recorder.calls(for: window), [20, 0], "the hidden clear pane still asks for no blur")
        try assertRadius(controller, false, "maximized over a hidden clear pane")
    }

    /// An opaque window never gets a radius; one that turns opaque (a
    /// reload) gets 0, and the radius back once translucent again.
    func testAnOpaqueWindowSetsRadiusZero() async throws {
        let host = try startHost()
        let solid = try host.open(profile: "solid")
        recorder.show(try XCTUnwrap(solid.window))
        solid.windowBlurRadius = 20
        try requireSurfaces(solid.panes)
        XCTAssertTrue(recorder.calls.isEmpty, "an opaque window makes no call")

        let controller = try host.open(profile: nil)
        let window = try XCTUnwrap(controller.window)
        recorder.show(window)
        controller.windowBlurRadius = 20
        try requireSurfaces(controller.panes)
        XCTAssertEqual(recorder.calls(for: window), [20])

        try TransparencyTests.config.replacingOccurrences(of: "opacity = 0.6", with: "opacity = 1.0")
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the window turns opaque") { window.isOpaque }
        XCTAssertEqual(recorder.calls(for: window), [20, 0], "opaque: radius 0")
        XCTAssertFalse(try content(controller).usesBlurRadius)

        try TransparencyTests.config
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("translucent again") { !window.isOpaque }
        XCTAssertEqual(recorder.calls(for: window), [20, 0, 20])
        try assertRadius(controller, true, "translucent again")
        XCTAssertTrue(recorder.calls(for: try XCTUnwrap(solid.window)).isEmpty, "the opaque window still has none")
    }

    // MARK: The fallback

    func testAMissingCallFallsBackToTheMaterialAndLogsOnce() throws {
        WindowBlurRadius.backend = nil
        let host = try startHost()
        let first = try host.open(profile: nil)
        let second = try host.open(profile: nil)
        try requireSurfaces(first.panes + second.panes)
        for controller in [first, second] {
            recorder.show(try XCTUnwrap(controller.window))
            controller.windowBlurRadius = 20
            controller.windowBlurRadius = 30
            try assertRadius(controller, false, "no call: the material")
            XCTAssertEqual(controller.focusedPane.view.backdrop?.isHidden, false)
        }
        XCTAssertEqual(WindowBlurRadius.fallbackLogCount, 1, "logged once for every window and radius")
    }

    func testAFailingCallFallsBackToTheMaterialAndLogsOnce() throws {
        recorder.result = -1
        let host = try startHost()
        let first = try host.open(profile: nil)
        let second = try host.open(profile: nil)
        try requireSurfaces(first.panes + second.panes)
        for controller in [first, second] {
            let window = try XCTUnwrap(controller.window)
            recorder.show(window)
            controller.windowBlurRadius = 20
            try assertRadius(controller, false, "a failed call: the material")
            try content(controller).updateWindowBlur()
            XCTAssertEqual(recorder.calls(for: window), [20], "not retried until something changes")
        }
        XCTAssertEqual(WindowBlurRadius.fallbackLogCount, 1, "logged once")

        recorder.result = 0
        first.windowBlurRadius = 25
        try assertRadius(first, true, "a later call that succeeds")
    }

    // MARK: Lifetime

    /// The window's observer keeps nothing alive: a closed tab blurred at
    /// its radius releases its content view and controller.
    func testClosingABlurredTabReleasesItsViews() async throws {
        let host = try startHost()
        weak var weakController: TerminalWindowController?
        weak var weakContent: TerminalWindowContentView?
        try autoreleasepool {
            let controller = try host.open(profile: nil)
            try requireSurfaces(controller.panes)
            recorder.show(try XCTUnwrap(controller.window))
            controller.windowBlurRadius = 20
            XCTAssertTrue(try content(controller).usesBlurRadius)
            weakController = controller
            weakContent = try content(controller)
            controller.focusedPane.view.inputContext?.deactivate()
        }
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the tab is freed") { weakController == nil && weakContent == nil }
    }
}

/// The app's own delegate reads `blur_radius` and gives it to every window:
/// at start, on every reload, and 0 while Reduce Transparency is on.
@MainActor
final class BlurRadiusReloadTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?
    private var reduces = false
    private var recorder: BlurRecorder!
    private var savedBackend: WindowBlurRadius.Backend?
    private var savedWindowNumber: (@MainActor (NSWindow) -> Int?)!

    private static func config(radius: Int) -> String {
        TransparencyTests.config.replacingOccurrences(of: "blur = true", with: "blur = true\nblur_radius = \(radius)")
    }

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { [unowned self] in reduces }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-blur-reload-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Self.config(radius: 0).write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        recorder = BlurRecorder()
        savedBackend = WindowBlurRadius.backend
        savedWindowNumber = WindowBlurRadius.windowNumber
        WindowBlurRadius.backend = recorder.backend
        WindowBlurRadius.windowNumber = { [recorder] in recorder?.number(of: $0) }
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        try appDelegate.replaceCore(start: false)
        appDelegate.spawnOptions = .init()
        HostDisplaySettings.pin(appDelegate)
        appDelegate.windowBlurRadius = 0
        appDelegate = nil
        WindowBlurRadius.backend = savedBackend
        WindowBlurRadius.windowNumber = savedWindowNumber
        recorder = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                throw BlurTimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    private func write(radius: Int) throws {
        try Self.config(radius: radius)
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
    }

    /// B1: a window and a tab opened with a radius already in the file get
    /// it as they come on screen, through the real window-number guard and
    /// with no notification posted by the test.
    func testAWindowOpenedWithARadiusGetsItWhenShown() async throws {
        WindowBlurRadius.windowNumber = savedWindowNumber
        try write(radius: 20)
        try appDelegate.replaceCore(start: true)
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        guard MTLCreateSystemDefaultDevice() != nil, controller.focusedPane.view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        guard window.isVisible, window.windowNumber > 0 else {
            throw XCTSkip("the host puts no window on screen")
        }
        let calls = { (window: NSWindow) in
            self.recorder.calls.filter { $0.windowNumber == window.windowNumber }.map(\.radius)
        }
        XCTAssertEqual(calls(window), [20], "set once, as the window is shown")
        XCTAssertTrue(try XCTUnwrap(window.contentView as? TerminalWindowContentView).usesBlurRadius)

        let tab = try appDelegate.newTab(in: window)
        let tabWindow = try XCTUnwrap(tab.window)
        guard tab.focusedPane.view.surface.isAttached, tabWindow.windowNumber > 0 else {
            throw XCTSkip("the tab has no surface or number")
        }
        XCTAssertEqual(calls(tabWindow), [20], "a new tab too")
        XCTAssertEqual(calls(window), [20], "the first window is not set again")
    }

    func testAReloadReachesEveryWindowAndReduceTransparencyTurnsItOff() async throws {
        try write(radius: 15)
        try appDelegate.replaceCore(start: true)
        XCTAssertEqual(appDelegate.windowBlurRadius, 15, "read at start")
        let first = try appDelegate.newWindow()
        let second = try appDelegate.newWindow()
        let windows = try [first, second].map { try XCTUnwrap($0.window) }
        guard MTLCreateSystemDefaultDevice() != nil, (first.panes + second.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        for window in windows {
            recorder.show(window)
            XCTAssertEqual(recorder.calls(for: window), [15], "each window at the radius read at start")
        }

        try write(radius: 40)
        try await waitUntil("the reload reaches every window") {
            windows.allSatisfy { self.recorder.calls(for: $0).last == 40 }
        }
        XCTAssertEqual([first, second].map(\.windowBlurRadius), [40, 40])

        try write(radius: 99)
        try await waitUntil("clamped by the core") { windows.allSatisfy { self.recorder.calls(for: $0).last == 64 } }

        reduces = true
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
        try await waitUntil("Reduce Transparency: radius 0") {
            windows.allSatisfy { self.recorder.calls(for: $0).last == 0 && $0.isOpaque }
        }
        XCTAssertEqual(appDelegate.windowBlurRadius, 0, "the host sets no radius under Reduce Transparency")
        XCTAssertEqual([first, second].map(\.windowBlurRadius), [0, 0])
        let third = try appDelegate.newWindow()
        XCTAssertEqual(third.windowBlurRadius, 0, "a window opened meanwhile")
        third.window?.close()

        reduces = false
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
        try await waitUntil("turned off: the radius is back") {
            windows.allSatisfy { self.recorder.calls(for: $0).last == 64 }
        }
    }
}

/// A tab window shows the state its panes ask for whenever it comes on
/// screen (#409, #405 n1): opaque or transparent, its title bar, the strip
/// under the title bar and tab bar, and its blur radius, by every route that
/// selects a tab or brings a window back, as a fresh attach of the tab gives.
/// Through the app's own windows and tabs and `config.toml`, posting nothing:
/// a radius changed while a tab is off screen reaches it as it is shown. The
/// window-number seam is the real guard, and treats an unselected tab as off
/// screen (as WindowServer does; this host reports such a tab visible).
@MainActor
final class TabVisibilityTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?
    private var recorder: BlurRecorder!
    private var savedBackend: WindowBlurRadius.Backend?
    private var savedWindowNumber: (@MainActor (NSWindow) -> Int?)!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { false }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-tab-blur-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try write(radius: 20)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        recorder = BlurRecorder()
        savedBackend = WindowBlurRadius.backend
        savedWindowNumber = WindowBlurRadius.windowNumber
        WindowBlurRadius.backend = recorder.backend
        let real = savedWindowNumber!
        WindowBlurRadius.windowNumber = { window in
            (window.tabGroup?.selectedWindow ?? window) === window ? real(window) : nil
        }
        try appDelegate.replaceCore(start: true)
    }

    override func tearDown() async throws {
        if NSApp.isHidden { NSApp.unhide(nil) }
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        try appDelegate.replaceCore(start: false)
        appDelegate.spawnOptions = .init()
        HostDisplaySettings.pin(appDelegate)
        appDelegate.windowBlurRadius = 0
        appDelegate = nil
        WindowBlurRadius.backend = savedBackend
        WindowBlurRadius.windowNumber = savedWindowNumber
        recorder = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func write(radius: Int) throws {
        let config = TransparencyTests.config.replacingOccurrences(
            of: "blur = true", with: "blur = true\nblur_radius = \(radius)")
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
    }

    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                throw BlurTimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// Two tabs of one window, the first (default profile: translucent,
    /// blurred) selected, the second of `profile`; the radius then changed
    /// to 40 in the file, which the unselected second tab cannot take yet.
    private func openTabs(
        second profile: String?
    ) async throws -> (TerminalWindowController, TerminalWindowController) {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window, profile: profile)
        let tab = try XCTUnwrap(second.window)
        guard MTLCreateSystemDefaultDevice() != nil, (first.panes + second.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        guard window.windowNumber > 0, tab.windowNumber > 0, savedWindowNumber(tab) != nil else {
            throw XCTSkip("the host puts no window on screen")
        }
        XCTAssertTrue(window.tabGroup === tab.tabGroup, "one tab group")
        window.tabGroup?.selectedWindow = window
        XCTAssertNil(WindowBlurRadius.windowNumber(tab), "the second tab is off screen")
        try write(radius: 40)
        try await waitUntil("the reload reaches both tabs") {
            appDelegate.windowBlurRadius == 40 && [first, second].allSatisfy { $0.windowBlurRadius == 40 }
        }
        XCTAssertEqual(recorder.calls.filter { $0.windowNumber == window.windowNumber }.last?.radius, 40)
        if profile == nil {
            XCTAssertEqual(
                try XCTUnwrap(tab.contentView as? TerminalWindowContentView).appliedBlur?.radius, 20,
                "the unselected tab keeps the radius it had")
        }
        return (first, second)
    }

    /// The state a fresh attach of `controller`'s tab gives its window, on
    /// screen: transparent with a transparent title bar over a tinted strip
    /// while `translucent`, which fills exactly what is above the panes, under
    /// the panes and the title bar's views; blurred at `radius` (the panes'
    /// own material hidden) while above 0; a standard opaque window otherwise.
    private func assertShown(
        _ controller: TerminalWindowController, translucent: Bool, radius: UInt32, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let window = controller.window, let content = window.contentView as? TerminalWindowContentView else {
            return XCTFail("\(what): a tab window's content", file: file, line: line)
        }
        XCTAssertTrue(window.tabGroup?.selectedWindow ?? window === window, "\(what): selected", file: file, line: line)
        XCTAssertEqual(window.isOpaque, !translucent, "\(what): opaque", file: file, line: line)
        XCTAssertEqual(
            window.backgroundColor, translucent ? .clear : .windowBackgroundColor, "\(what): background", file: file,
            line: line)
        XCTAssertEqual(
            window.styleMask.contains(.fullSizeContentView), translucent, "\(what): full-size content", file: file,
            line: line)
        XCTAssertEqual(window.titlebarAppearsTransparent, translucent, "\(what): title bar", file: file, line: line)
        let area = content.convert(window.contentLayoutRect, from: nil)
        XCTAssertEqual(controller.paneContainer.frame, area, "\(what): the panes' frame", file: file, line: line)
        if translucent {
            let strip = NSRect(
                x: content.bounds.minX, y: area.maxY, width: content.bounds.width,
                height: content.bounds.maxY - area.maxY)
            XCTAssertGreaterThan(strip.height, 0, "\(what): a strip", file: file, line: line)
            XCTAssertEqual(content.titlebarBackdrop?.frame, strip, "\(what): the strip's blur", file: file, line: line)
            XCTAssertEqual(content.titlebarTint?.frame, strip, "\(what): the strip's tint", file: file, line: line)
            XCTAssertEqual(
                content.subviews.map { ObjectIdentifier($0) },
                [content.titlebarBackdrop, content.titlebarTint, controller.paneContainer].compactMap {
                    $0.map { ObjectIdentifier($0) }
                }, "\(what): blur, tint, panes, bottom up", file: file, line: line)
            XCTAssertEqual(
                content.titlebarTint?.segments.first?.minX, 0, "\(what): the tint spans the strip", file: file,
                line: line)
            XCTAssertEqual(
                content.titlebarTint?.segments.last?.maxX, strip.width, "\(what): to its end", file: file, line: line)
        } else {
            XCTAssertNil(content.titlebarBackdrop, "\(what): no strip", file: file, line: line)
            XCTAssertNil(content.titlebarTint, "\(what): no tint", file: file, line: line)
        }
        // The title bar's views (its buttons, title and the tab bar) stay
        // over the content, which would cover them with the strip (#409).
        if let close = window.standardWindowButton(.closeButton), let frame = content.superview {
            let above = frame.subviews.drop { $0 !== content }.dropFirst()
            XCTAssertTrue(
                above.contains { close.isDescendant(of: $0) }, "\(what): the title bar over the content", file: file,
                line: line)
            XCTAssertFalse(close.isHiddenOrHasHiddenAncestor, "\(what): the title bar shown", file: file, line: line)
        } else {
            XCTFail("\(what): a title bar", file: file, line: line)
        }
        XCTAssertEqual(content.usesBlurRadius, radius > 0, "\(what): blurred at the radius", file: file, line: line)
        if radius > 0 {
            XCTAssertEqual(
                content.appliedBlur?.windowNumber, window.windowNumber, "\(what): set on this window", file: file,
                line: line)
            XCTAssertEqual(content.appliedBlur?.radius, radius, "\(what): the radius", file: file, line: line)
        } else {
            XCTAssertEqual(content.appliedBlur?.radius ?? 0, 0, "\(what): no radius", file: file, line: line)
        }
        for pane in controller.panes {
            if let backdrop = pane.view.backdrop {
                XCTAssertEqual(backdrop.isHidden, radius > 0, "\(what): the pane's material", file: file, line: line)
            }
        }
    }

    /// Each route that selects the second tab, with that tab translucent
    /// and blurred like the first, then opaque.
    private func checkRoute(_ name: String, _ select: (NSWindow, NSWindow) -> Void) async throws {
        for profile in [nil, "solid"] {
            let (first, second) = try await openTabs(second: profile)
            let window = try XCTUnwrap(first.window)
            let tab = try XCTUnwrap(second.window)
            // #417 M1: the unselected tab's window is put in the other
            // opacity first, so the route has to re-derive all of its state
            // (opacity, background, style mask, title bar, strip), not only
            // the blur radius.
            TerminalSurfaceView.setWindow(tab, opaque: profile == nil)
            (tab.contentView as? TerminalWindowContentView)?.updateTitlebar(translucent: profile != nil)
            XCTAssertEqual(tab.isOpaque, profile == nil, "\(name): the unselected tab is in the other state")
            select(window, tab)
            assertShown(
                second, translucent: profile == nil, radius: profile == nil ? 40 : 0,
                "\(name), second tab \(profile ?? "translucent")")
            for controller in appDelegate.windowControllers {
                controller.window?.close()
            }
        }
    }

    func testSelectNextTabShowsTheTabsState() async throws {
        try await checkRoute("⇧⌘]") { window, _ in window.selectNextTab(nil) }
    }

    func testSelectingATabByNumberShowsItsState() async throws {
        try await checkRoute("⌘2") { window, _ in (window as? TerminalWindow)?.selectTab(at: 1) }
    }

    func testSettingTheSelectedWindowShowsItsState() async throws {
        try await checkRoute("selectedWindow") { window, tab in window.tabGroup?.selectedWindow = tab }
    }

    func testClosingTheSelectedTabShowsTheNextTabsState() async throws {
        try await checkRoute("closing the selected tab") { window, _ in window.close() }
    }

    func testMakeKeyAndOrderFrontShowsTheTabsState() async throws {
        try await checkRoute("makeKeyAndOrderFront") { _, tab in tab.makeKeyAndOrderFront(nil) }
    }

    /// #405 n1: the radius changed in the file while the window's tabs are
    /// minimized, then while the app is hidden, reaches every tab window as
    /// it comes back, through the real window-number guard (a minimized or
    /// hidden window has none), with no notification posted by the test.
    func testARadiusChangedWhileMinimizedOrHiddenReachesEveryTab() async throws {
        WindowBlurRadius.windowNumber = savedWindowNumber
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        let tab = try XCTUnwrap(second.window)
        let controllers = [first, second]
        guard MTLCreateSystemDefaultDevice() != nil, controllers.flatMap(\.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        guard savedWindowNumber(window) != nil || savedWindowNumber(tab) != nil else {
            throw XCTSkip("the host puts no window on screen")
        }
        let contents = try controllers.map { try XCTUnwrap($0.window?.contentView as? TerminalWindowContentView) }
        func radii() -> [UInt32?] { contents.map { $0.appliedBlur?.radius } }

        tab.miniaturize(nil)
        let deadline = ContinuousClock.now + .seconds(10)
        while !tab.isMiniaturized, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        guard tab.isMiniaturized else { throw XCTSkip("the host cannot minimize a window (no Dock)") }
        try await waitUntil("minimized") { [window, tab].allSatisfy { !$0.isVisible } }
        try write(radius: 40)
        try await waitUntil("the reload reaches both tabs") { controllers.allSatisfy { $0.windowBlurRadius == 40 } }
        XCTAssertFalse(radii().contains(40), "minimized: no window number to set it on")
        tab.deminiaturize(nil)
        try await waitUntil("restored, every tab at the new radius") { radii() == [40, 40] }
        XCTAssertEqual(contents.map(\.usesBlurRadius), [true, true])

        NSApp.hide(nil)
        try await waitUntil("hidden") { NSApp.isHidden }
        try write(radius: 30)
        try await waitUntil("the reload reaches both tabs") { controllers.allSatisfy { $0.windowBlurRadius == 30 } }
        NSApp.unhide(nil)
        try await waitUntil("shown again, every tab at the new radius") { radii() == [30, 30] }
        XCTAssertEqual(contents.map(\.usesBlurRadius), [true, true])
    }

    /// #417 m1: minimizing a window of three tabs and bringing it back
    /// gives each tab at most one update of its state from the restore,
    /// the selected tab exactly one, whichever tabs macOS posts the restore
    /// for (macOS 27.2 posts it for each), not one per tab for each of them.
    /// The app's own windows, minimized and restored for real, posting
    /// nothing.
    func testADeminiaturizeUpdatesEachTabOnce() async throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        let third = try appDelegate.newTab(in: try XCTUnwrap(second.window))
        let windows = try [first, second, third].map { try XCTUnwrap($0.window) }
        XCTAssertEqual(window.tabGroup?.windows.count, 3)
        let contents = try windows.map { try XCTUnwrap($0.contentView as? TerminalWindowContentView) }
        let selected = try XCTUnwrap(window.tabGroup?.selectedWindow)

        selected.miniaturize(nil)
        let deadline = ContinuousClock.now + .seconds(10)
        while !selected.isMiniaturized, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        guard selected.isMiniaturized else { throw XCTSkip("the host cannot minimize a window (no Dock)") }
        let before = contents.map(\.deminiaturizeCount)
        selected.deminiaturize(nil)
        try await waitUntil("restored") { !selected.isMiniaturized && selected.isVisible }
        // Every restore notification is posted by now; let any late one land.
        try await Task.sleep(for: .milliseconds(300))
        let counts = zip(contents, before).map { $0.deminiaturizeCount - $1 }
        let index = try XCTUnwrap(windows.firstIndex { $0 === selected })
        XCTAssertEqual(counts[index], 1, "the selected tab is updated once: \(counts)")
        XCTAssertTrue(counts.allSatisfy { $0 <= 1 }, "no tab is updated more than once: \(counts)")
    }

    /// #417 M6: a window that turns opaque forgets the height of the title
    /// bar and tab bar it had, so after its tab bar goes and it turns
    /// translucent again at a frame too small to show a pane, its smallest
    /// size takes the title bar alone, not the tab bar that is gone.
    func testAnOpaqueWindowForgetsTheTabBarItHad() throws {
        let first = try appDelegate.newWindow(profile: "solid")
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window, profile: "solid")
        let tab = try XCTUnwrap(second.window)
        window.tabGroup?.selectedWindow = tab
        let content = try XCTUnwrap(tab.contentView as? TerminalWindowContentView)
        let titlebar = TerminalWindowContentView.standardTitlebarHeight(of: tab)
        content.updateTitlebar(translucent: true)
        let strip = content.bounds.maxY - content.paneArea.maxY
        guard strip > titlebar + 1 else { throw XCTSkip("the host shows no tab bar") }
        XCTAssertEqual(content.titlebarInset, strip, "translucent with a tab bar: the strip")

        content.updateTitlebar(translucent: false)
        XCTAssertEqual(content.titlebarInset, 0, "opaque: no strip")
        window.close()
        XCTAssertNil(tab.tabGroup.flatMap { $0.windows.count > 1 ? $0 : nil }, "the tab bar went")

        var tiny = tab.frame
        tiny.size.height = 20
        tab.setFrame(tiny, display: false)
        content.updateTitlebar(translucent: true)
        XCTAssertEqual(content.paneArea.height, 0, "no pane shows")
        XCTAssertEqual(content.titlebarInset, titlebar, "the title bar alone, the tab bar it had forgotten")
    }

    /// #403 m2: the smallest frame of a translucent tabbed window, set by a
    /// program, leaves every pane its smallest size under the title bar and
    /// the tab bar, also after a frame too small to show any pane.
    func testTheSmallestTabbedWindowKeepsThePanesSmallestSize() throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        guard MTLCreateSystemDefaultDevice() != nil, (first.panes + second.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        _ = try first.splitFocusedPane(.below)
        let tab = try XCTUnwrap(second.window)
        let content = try XCTUnwrap(tab.contentView as? TerminalWindowContentView)
        let titlebar = TerminalWindowContentView.standardTitlebarHeight(of: tab)
        let strip = content.bounds.maxY - content.paneArea.maxY
        guard strip > titlebar + 1 else { throw XCTSkip("the host shows no tab bar") }

        for controller in [second, first] {
            let window = try XCTUnwrap(controller.window)
            window.tabGroup?.selectedWindow = window
            controller.paneContainer.layoutSubtreeIfNeeded()
            var tiny = window.frame
            tiny.size.height = 20
            window.setFrame(tiny, display: false)
            var smallest = window.frame
            smallest.size.height =
                window.frameRect(forContentRect: NSRect(origin: .zero, size: window.contentMinSize)).height
            window.setFrame(smallest, display: false)
            controller.paneContainer.layoutSubtreeIfNeeded()
            for pane in controller.panes {
                XCTAssertGreaterThanOrEqual(
                    pane.view.frame.height, PaneContainerView.minimumSize(of: pane.view).height - 0.5,
                    "\(controller.panes.count) panes: each keeps its smallest height")
            }
        }
    }
}
