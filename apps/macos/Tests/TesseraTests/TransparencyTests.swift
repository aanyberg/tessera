import AppKit
import Metal
import TesseraKit
import XCTest

@testable import Tessera

/// Stands in for the app delegate of the tabs a ``TransparencyTests`` test
/// opens on its own core: a split's session starts with ``nextProfile``, so
/// one tab can hold panes of different profiles (the app's splits take the
/// focused pane's), and closes reach the test's ``SessionRouter``.
@MainActor
private final class TransparencyHost: TerminalWindowControllerDelegate {
    let core: TesseraApp
    let router = SessionRouter()
    /// The profile of the next split's session; `nil` for the default one.
    var nextProfile: String?
    private(set) var controllers: [TerminalWindowController] = []

    init(core: TesseraApp) {
        self.core = core
        core.delegate = router
    }

    private func spawn(_ profile: String?) throws -> TesseraSession {
        try core.spawn(.init(program: "/bin/cat", rows: 10, cols: 40, profile: profile))
    }

    /// A tab whose first pane has `profile`, its window `size` and laid out.
    func open(profile: String?, size: NSSize = NSSize(width: 1000, height: 600)) throws -> TerminalWindowController {
        let session = try spawn(profile)
        let controller = TerminalWindowController(session: session, title: "")
        controller.delegate = self
        router.register(controller, session: session)
        controllers.append(controller)
        controller.window?.setContentSize(size)
        controller.paneContainer.layoutSubtreeIfNeeded()
        return controller
    }

    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController) {}

    func terminalWindowControllerSpawnSession(_ controller: TerminalWindowController) throws -> TesseraSession {
        let session = try spawn(nextProfile)
        router.register(controller, session: session)
        return session
    }

    func terminalWindowController(_ controller: TerminalWindowController, didClosePaneWith session: TesseraSession) {
        router.unregister(session)
    }

    func terminalWindowControllerDidClose(_ controller: TerminalWindowController) {
        for session in controller.sessions {
            router.unregister(session)
        }
        controllers.removeAll { $0 === controller }
    }

    func terminalWindowControllerDidChangeLayout(_ controller: TerminalWindowController) {}

    func presentOpenError(_ error: any Error) {
        XCTFail("a pane could not be opened: \(error)")
    }

    var closeConfirmationPresenter: (any CloseConfirmationPresenter)? { nil }
}

private struct TimedOut: Error, CustomStringConvertible {
    var description: String
}

/// Translucency and background blur per pane (#42, ADR 0043): each pane
/// follows its session's profile, a blurred one has a ``PaneBackdropView``
/// under its text, the window is opaque only while every pane of the tab
/// is, through splits, closes, a maximized pane and the dimming shade, and
/// the system's Reduce Transparency makes every pane opaque and unblurred.
/// On a core of the test's own, its configuration in a temporary directory
/// (never the user's); the tabs' windows are never shown.
@MainActor
final class TransparencyTests: XCTestCase {
    /// `[appearance]` (the default profile) is translucent and blurred,
    /// `solid` opaque, `clear` translucent without blur.
    static let config = """
        [appearance]
        opacity = 0.6
        blur = true
        [[profiles]]
        name = "solid"
        [profiles.appearance]
        opacity = 1.0
        [[profiles]]
        name = "clear"
        [profiles.appearance]
        blur = false
        """

    private var dir: URL!
    private var previous: String?
    private var host: TransparencyHost?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-transparency-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute configuration directory")
        try Self.config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        if let host {
            for controller in host.controllers {
                controller.window?.close()
            }
            host.core.shutdown()
        }
        host = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func startHost() throws -> TransparencyHost {
        let host = TransparencyHost(core: try TesseraApp())
        self.host = host
        return host
    }

    /// Skips without a renderer: a pane without a surface counts as opaque.
    private func requireSurfaces(_ panes: [Pane]) throws {
        guard MTLCreateSystemDefaultDevice() != nil, panes.allSatisfy(\.view.surface.isAttached) else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
    }

    private func split(
        _ controller: TerminalWindowController, _ direction: PaneSplitDirection = .right, profile: String?
    ) throws -> Pane {
        host?.nextProfile = profile
        let pane = try XCTUnwrap(controller.splitFocusedPane(direction), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        return pane
    }

    private func isOpaque(_ controller: TerminalWindowController) throws -> Bool {
        let window = try XCTUnwrap(controller.window)
        XCTAssertEqual(
            window.backgroundColor, window.isOpaque ? .windowBackgroundColor : .clear,
            "a transparent window has a clear background")
        return window.isOpaque
    }

    /// Asserts that `pane` is blurred by a backdrop under all it draws,
    /// filling it, or has none.
    private func assertBackdrop(
        _ pane: Pane, _ expected: Bool, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        let view = pane.view
        let backdrops = view.subviews.filter { $0 is NSVisualEffectView }
        guard expected else {
            XCTAssertNil(view.backdrop, "\(what): no backdrop", file: file, line: line)
            XCTAssertTrue(backdrops.isEmpty, "\(what): no effect view left", file: file, line: line)
            return
        }
        guard let backdrop = view.backdrop else {
            return XCTFail("\(what): a backdrop", file: file, line: line)
        }
        XCTAssertEqual(backdrops.count, 1, "\(what): one effect view", file: file, line: line)
        XCTAssertTrue(view.subviews.first === backdrop, "\(what): under the render view and the rest", file: file, line: line)
        XCTAssertEqual(backdrop.frame, view.bounds, "\(what): filling the pane", file: file, line: line)
        XCTAssertEqual(backdrop.blendingMode, .behindWindow, "\(what)", file: file, line: line)
        XCTAssertEqual(backdrop.state, .active, "\(what): blurred in a window that is not key", file: file, line: line)
        XCTAssertNil(backdrop.hitTest(NSPoint(x: 5, y: 5)), "\(what): takes no clicks", file: file, line: line)
    }

    /// Waits for `condition`, the core's appearance change reaching the
    /// panes on a later turn of the main actor.
    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(10), _ condition: () throws -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while try !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out after \(timeout): \(description)")
                throw TimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    // MARK: Per pane

    func testEachPaneFollowsItsProfileAndTheWindowIsOpaqueOnlyWhileEveryPaneIs() throws {
        let host = try startHost()
        let controller = try host.open(profile: "solid")
        let solid = controller.focusedPane
        try requireSurfaces([solid])
        XCTAssertEqual(solid.view.backgroundAppearance, .init(opaque: true, blur: false))
        assertBackdrop(solid, false, "solid")
        XCTAssertTrue(try isOpaque(controller), "one opaque pane")

        let frosted = try split(controller, profile: nil)
        try requireSurfaces([frosted])
        XCTAssertEqual(frosted.view.backgroundAppearance, .init(opaque: false, blur: true))
        assertBackdrop(frosted, true, "the default profile")
        assertBackdrop(solid, false, "solid, after the split")
        XCTAssertFalse(try isOpaque(controller), "a translucent pane makes the window transparent")

        let clear = try split(controller, .below, profile: "clear")
        try requireSurfaces([clear])
        XCTAssertEqual(clear.view.backgroundAppearance, .init(opaque: false, blur: false))
        assertBackdrop(clear, false, "clear")
        XCTAssertFalse(try isOpaque(controller))

        // A maximized opaque pane still has translucent siblings (E7): an
        // appearance change while zoomed recomputes the window from every
        // pane, the hidden ones included. The window is made opaque first,
        // so only that recomputation can make it transparent again.
        controller.focus(solid)
        XCTAssertTrue(controller.toggleZoom())
        XCTAssertTrue(frosted.view.isHiddenOrHasHiddenAncestor, "zoomed: the siblings are hidden")
        let window = try XCTUnwrap(controller.window)
        TerminalSurfaceView.setWindow(window, opaque: true)
        solid.view.appearanceDidChange()
        XCTAssertFalse(try isOpaque(controller), "hidden panes count")
        controller.unzoom()

        // Closing the translucent panes leaves the opaque one: so is the window.
        controller.focus(clear)
        controller.closeFocusedPane(nil)
        XCTAssertFalse(try isOpaque(controller), "the frosted pane is left")
        controller.focus(frosted)
        controller.closeFocusedPane(nil)
        XCTAssertEqual(controller.panes, [solid])
        XCTAssertTrue(try isOpaque(controller), "only the opaque pane is left")
    }

    /// The core's appearance change (here a reload of `config.toml` that
    /// turns the default profile's blur off) reaches each pane through the
    /// router: the backdrop goes, the window stays transparent.
    func testAConfigurationReloadReachesEveryPane() async throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let frosted = controller.focusedPane
        let solid = try split(controller, profile: "solid")
        try requireSurfaces([frosted, solid])
        assertBackdrop(frosted, true, "before")
        try Self.config.replacingOccurrences(of: "blur = true", with: "blur = false")
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the reload turns the blur off") { frosted.view.backdrop == nil }
        assertBackdrop(frosted, false, "after")
        XCTAssertEqual(frosted.view.backgroundAppearance, .init(opaque: false, blur: false))
        XCTAssertFalse(try isOpaque(controller), "still translucent")
    }

    // MARK: Reduce Transparency

    func testReduceTransparencyMakesEveryPaneOpaqueAndUnblurredUntilTurnedOff() async throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let frosted = controller.focusedPane
        let clear = try split(controller, profile: "clear")
        let solid = try split(controller, .below, profile: "solid")
        try requireSurfaces([frosted, clear, solid])
        XCTAssertFalse(try isOpaque(controller))

        try host.core.setDisplayOptions(.init(reduceTransparency: true))
        try await waitUntil("every pane is opaque") { try self.isOpaque(controller) }
        for pane in [frosted, clear, solid] {
            XCTAssertEqual(pane.view.backgroundAppearance, .init(opaque: true, blur: false))
            assertBackdrop(pane, false, "reduced")
        }

        // A pane opened meanwhile starts opaque.
        let late = try split(controller, profile: nil)
        try requireSurfaces([late])
        assertBackdrop(late, false, "opened while reduced")
        XCTAssertTrue(try isOpaque(controller))

        try host.core.setDisplayOptions(.init(reduceTransparency: false))
        try await waitUntil("the panes are translucent again") { try !self.isOpaque(controller) }
        try await waitUntil("the blur is back") { frosted.view.backdrop != nil && late.view.backdrop != nil }
        assertBackdrop(frosted, true, "turned off")
        assertBackdrop(late, true, "turned off, the late pane")
        assertBackdrop(clear, false, "turned off, clear")
        assertBackdrop(solid, false, "turned off, solid")
    }

    /// The dimming shade stays over everything the pane draws, the backdrop
    /// under it, however often the backdrop goes and comes back.
    func testTheBackdropStaysUnderTheDimmingShade() async throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let dimmed = controller.focusedPane
        let focused = try split(controller, profile: nil)
        try requireSurfaces([dimmed, focused])
        controller.paneContainer.inactivePaneDimming = 0.5
        XCTAssertTrue(controller.paneContainer.activePane === focused)
        func assertOrder(_ what: String) {
            XCTAssertTrue(dimmed.view.subviews.last is PaneDimView, "\(what): the shade on top")
            XCTAssertTrue(dimmed.view.subviews.first === dimmed.view.backdrop, "\(what): the backdrop at the bottom")
        }
        assertOrder("dimmed")
        for round in 0..<2 {
            try host.core.setDisplayOptions(.init(reduceTransparency: true))
            try await waitUntil("the backdrop goes, round \(round)") { dimmed.view.backdrop == nil }
            XCTAssertTrue(dimmed.view.subviews.last is PaneDimView, "the shade stays")
            try host.core.setDisplayOptions(.init(reduceTransparency: false))
            try await waitUntil("the backdrop is back, round \(round)") { dimmed.view.backdrop != nil }
            assertOrder("round \(round)")
        }
    }

    // MARK: No surface

    /// A pane whose surface is gone draws nothing: it counts as opaque and
    /// loses its blur, so the window does not show the desktop through it.
    func testAPaneWithoutASurfaceCountsAsOpaque() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let pane = controller.focusedPane
        try requireSurfaces([pane])
        XCTAssertFalse(try isOpaque(controller))
        try pane.view.surface.detach()
        pane.view.appearanceDidChange()
        XCTAssertTrue(pane.view.isBackgroundOpaque)
        assertBackdrop(pane, false, "without a surface")
        XCTAssertTrue(try isOpaque(controller))
    }

    // MARK: Closing

    /// A closed blurred pane, and then its tab, release their views,
    /// backdrops, sessions and controller.
    func testClosingAPaneAndItsTabReleasesTheBackdrop() async throws {
        let host = try startHost()
        weak var weakController: TerminalWindowController?
        weak var weakPane: Pane?
        weak var weakView: TerminalSurfaceView?
        weak var weakBackdrop: PaneBackdropView?
        weak var weakSession: TesseraSession?
        weak var weakFirstBackdrop: PaneBackdropView?
        try autoreleasepool {
            let controller = try host.open(profile: nil)
            weakController = controller
            let pane = try split(controller, profile: nil)
            try requireSurfaces(controller.panes)
            weakPane = pane
            weakView = pane.view
            weakBackdrop = pane.view.backdrop
            weakSession = pane.session
            weakFirstBackdrop = controller.panes.first?.view.backdrop
            pane.view.inputContext?.deactivate()
        }
        XCTAssertNotNil(weakBackdrop, "the pane was blurred")
        autoreleasepool { weakController?.closeFocusedPane(nil) }
        try await waitUntil("the closed pane is freed") {
            weakPane == nil && weakView == nil && weakBackdrop == nil && weakSession == nil
        }
        XCTAssertNotNil(weakFirstBackdrop, "the other pane keeps its blur")
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the tab is freed") { weakController == nil && weakFirstBackdrop == nil }
    }
}

/// The app hands the core the system's Reduce Transparency (E8): at start
/// and on `NSWorkspace.accessibilityDisplayOptionsDidChangeNotification`,
/// read through ``AppDelegate/reducesTransparency`` (here the test's, not
/// the machine's setting). The app's own core runs on the test's
/// configuration (``AppDelegate/replaceCore(start:)``).
@MainActor
final class ReduceTransparencySettingTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?
    private var reduces = false

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { [unowned self] in reduces }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-reduce-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute configuration directory")
        try TransparencyTests.config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        // The next test's first session starts a core on its own configuration.
        try appDelegate.replaceCore(start: false)
        appDelegate.spawnOptions = .init()
        HostDisplaySettings.pin(appDelegate)
        appDelegate = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func postSettingChange() {
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
    }

    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                throw TimedOut(description: description)
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    func testTheAppFollowsTheSystemsReduceTransparency() async throws {
        reduces = true
        try appDelegate.replaceCore(start: true)
        let controller = try appDelegate.newWindow()
        let pane = controller.focusedPane
        let window = try XCTUnwrap(controller.window)
        guard MTLCreateSystemDefaultDevice() != nil, pane.view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the view (no GPU can present, or no font)")
        }
        XCTAssertTrue(window.isOpaque, "reduced from the start")
        XCTAssertNil(pane.view.backdrop)

        reduces = false
        postSettingChange()
        try await waitUntil("translucent and blurred") { !window.isOpaque && pane.view.backdrop != nil }

        reduces = true
        postSettingChange()
        try await waitUntil("opaque again") { window.isOpaque && pane.view.backdrop == nil }
    }
}
