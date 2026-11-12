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
