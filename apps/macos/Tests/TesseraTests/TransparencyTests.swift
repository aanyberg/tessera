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
        XCTAssertEqual(backdrop.material, .fullScreenUI, "\(what): no grey cast (#363)", file: file, line: line)
        XCTAssertEqual(backdrop.state, .active, "\(what): blurred in a window that is not key", file: file, line: line)
        XCTAssertNil(backdrop.hitTest(NSPoint(x: 5, y: 5)), "\(what): takes no clicks", file: file, line: line)
    }

    /// Asserts the window's title bar of `controller`'s tab (#363): standard
    /// over panes filling the content of an opaque window; transparent over
    /// full-size content for a translucent one, the panes below the title bar
    /// and tab bar and a backdrop filling the rest above them, whether or not
    /// a pane is blurred. The window's smallest size keeps the panes at
    /// theirs either way.
    private func assertTitlebar(
        _ controller: TerminalWindowController, translucent: Bool, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let window = controller.window, let content = window.contentView as? TerminalWindowContentView else {
            return XCTFail("\(what): a tab window's content", file: file, line: line)
        }
        XCTAssertTrue(content.paneContainer === controller.paneContainer, "\(what)", file: file, line: line)
        XCTAssertEqual(
            window.styleMask.contains(.fullSizeContentView), translucent, "\(what): full-size content",
            file: file, line: line)
        XCTAssertEqual(
            window.titlebarAppearsTransparent, translucent, "\(what): a transparent title bar", file: file, line: line)
        let area = content.convert(window.contentLayoutRect, from: nil)
        XCTAssertEqual(
            controller.paneContainer.frame, area, "\(what): the panes are below the title bar and tab bar",
            file: file, line: line)
        if translucent {
            XCTAssertGreaterThan(
                content.bounds.maxY, area.maxY, "\(what): the content runs under the title bar", file: file, line: line)
        } else {
            XCTAssertEqual(area, content.bounds, "\(what): the panes fill the content", file: file, line: line)
        }
        XCTAssertEqual(
            window.contentMinSize.height,
            controller.paneContainer.minimumSize.height + content.bounds.height - area.height,
            "\(what): the smallest size keeps the panes at theirs", file: file, line: line)
        let effects = content.subviews.filter { $0 is NSVisualEffectView }
        guard translucent else {
            XCTAssertNil(content.titlebarBackdrop, "\(what): no blur under the title bar", file: file, line: line)
            XCTAssertNil(content.titlebarTint, "\(what): no tint under the title bar", file: file, line: line)
            XCTAssertFalse(
                content.subviews.contains { $0 is StripTintView }, "\(what): no tint view left", file: file, line: line)
            XCTAssertTrue(effects.isEmpty, "\(what): no effect view left", file: file, line: line)
            return
        }
        guard let backdrop = content.titlebarBackdrop else {
            return XCTFail("\(what): a blur under the title bar", file: file, line: line)
        }
        XCTAssertEqual(effects.count, 1, "\(what): one effect view", file: file, line: line)
        XCTAssertTrue(content.subviews.first === backdrop, "\(what): under the panes", file: file, line: line)
        XCTAssertEqual(
            backdrop.frame,
            NSRect(x: 0, y: area.maxY, width: content.bounds.width, height: content.bounds.maxY - area.maxY),
            "\(what): filling the title bar and tab bar", file: file, line: line)
        XCTAssertEqual(backdrop.blendingMode, .behindWindow, "\(what)", file: file, line: line)
        XCTAssertEqual(backdrop.material, .fullScreenUI, "\(what): the panes' material", file: file, line: line)
        XCTAssertEqual(backdrop.state, .active, "\(what): blurred in a window that is not key", file: file, line: line)
        XCTAssertNil(
            backdrop.hitTest(NSPoint(x: backdrop.frame.midX, y: backdrop.frame.midY)), "\(what): takes no clicks",
            file: file, line: line)
        guard let tint = content.titlebarTint else {
            return XCTFail("\(what): a tint over the blur", file: file, line: line)
        }
        XCTAssertEqual(content.subviews.filter { $0 is StripTintView }.count, 1, "\(what)", file: file, line: line)
        XCTAssertEqual(
            content.subviews.firstIndex(of: tint), content.subviews.firstIndex(of: backdrop).map { $0 + 1 },
            "\(what): the tint right over the blur", file: file, line: line)
        XCTAssertEqual(tint.frame, backdrop.frame, "\(what): the tint fills the strip", file: file, line: line)
        XCTAssertNil(tint.hitTest(NSPoint(x: tint.frame.midX, y: tint.frame.midY)), "\(what)", file: file, line: line)
        XCTAssertFalse(tint.isAccessibilityElement(), "\(what)", file: file, line: line)
    }

    /// The strip's tint segments of `controller`'s tab.
    private func tintSegments(_ controller: TerminalWindowController) throws -> [StripTintView.Segment] {
        let content = try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
        return try XCTUnwrap(content.titlebarTint, "a tint").segments
    }

    /// The colours `controller`'s strip tint draws, one per point of its
    /// width (1x, as on a 1x display), drawn into an sRGB bitmap and read
    /// back with straight alpha.
    private func drawnTintRow(_ controller: TerminalWindowController) throws -> [[CGFloat]] {
        let content = try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
        let tint = try XCTUnwrap(content.titlebarTint)
        let width = max(Int(tint.bounds.width.rounded(.up)), 1)
        let srgb = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
        let context = try XCTUnwrap(
            CGContext(
                data: nil, width: width, height: 4, bitsPerComponent: 8, bytesPerRow: 0, space: srgb,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
        tint.draw(NSRect(x: 0, y: 0, width: tint.bounds.width, height: 4))
        NSGraphicsContext.restoreGraphicsState()
        let pixels = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
        return (0..<width).map { column in
            let offset = context.bytesPerRow + column * 4
            let alpha = CGFloat(pixels[offset + 3]) / 255
            guard alpha > 0 else { return [0, 0, 0, 0] }
            return (0..<3).map { CGFloat(pixels[offset + $0]) / 255 / alpha } + [alpha]
        }
    }

    /// The colour `controller`'s strip tint draws at `x` (in the content
    /// view).
    private func drawnTint(_ controller: TerminalWindowController, atX x: CGFloat) throws -> [CGFloat] {
        let content = try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
        let tint = try XCTUnwrap(content.titlebarTint)
        let row = try drawnTintRow(controller)
        return row[min(max(Int(tint.convert(NSPoint(x: x, y: 0), from: content).x), 0), row.count - 1)]
    }

    /// Asserts that every pixel of `controller`'s strip from 2 pt before to
    /// 2 pt after each segment boundary is drawn in one of the two
    /// segments' colours: no lighter or blended line at 1x.
    private func assertNoSeam(
        _ controller: TerminalWindowController, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let segments = try tintSegments(controller)
        let row = try drawnTintRow(controller)
        for (left, right) in zip(segments, segments.dropFirst()) {
            let colours = [left, right].map { segment in
                row[min(max(Int((segment.minX + segment.maxX) / 2), 0), row.count - 1)]
            }
            let boundary = Int(left.maxX.rounded(.down))
            for column in max(boundary - 2, 0)...min(boundary + 2, row.count - 1) {
                let pixel = row[column]
                let matches = colours.contains { colour in
                    zip(pixel, colour).allSatisfy { abs($0 - $1) <= 1.5 / 255 }
                }
                XCTAssertTrue(
                    matches, "\(what): pixel \(column) at the boundary \(left.maxX) is \(pixel), not one of \(colours)",
                    file: file, line: line)
            }
        }
    }

    /// `pane`'s session's configured background as sRGB components and
    /// alpha.
    private func background(of pane: Pane) throws -> [CGFloat] {
        let color = try pane.session.appearance().background
        return [color.red, color.green, color.blue, color.alpha].map { CGFloat($0) }
    }

    /// Asserts that `controller`'s strip is tinted with `panes`' backgrounds,
    /// left to right, one segment each across the whole width, each drawn.
    private func assertTint(
        _ controller: TerminalWindowController, _ panes: [Pane], _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let content = try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
        let segments = try tintSegments(controller)
        XCTAssertEqual(segments.count, panes.count, "\(what): one segment per pane at the top", file: file, line: line)
        guard segments.count == panes.count, let first = segments.first, let last = segments.last else { return }
        XCTAssertEqual(first.minX, 0, "\(what): from the left edge", file: file, line: line)
        XCTAssertEqual(last.maxX, content.bounds.width, "\(what): to the right edge", file: file, line: line)
        for (segment, next) in zip(segments, segments.dropFirst()) {
            XCTAssertEqual(segment.maxX, next.minX, "\(what): no gap", file: file, line: line)
        }
        for (index, (segment, pane)) in zip(segments, panes).enumerated() {
            let expected = try background(of: pane)
            let frame = content.convert(pane.view.bounds, from: pane.view)
            XCTAssertLessThanOrEqual(
                segment.minX, frame.minX + 0.5, "\(what) \(index): over its pane", file: file, line: line)
            XCTAssertGreaterThanOrEqual(segment.maxX, frame.maxX - 0.5, "\(what) \(index)", file: file, line: line)
            let srgb = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
            let stored = segment.color?.converted(to: srgb, intent: .defaultIntent, options: nil)?.components ?? []
            XCTAssertEqual(stored.count, 4, "\(what) \(index): a colour", file: file, line: line)
            for (got, want) in zip(stored, expected) {
                XCTAssertEqual(
                    got, want, accuracy: 1e-6, "\(what) \(index): the session's background", file: file, line: line)
            }
            let drawn = try drawnTint(controller, atX: (segment.minX + segment.maxX) / 2)
            for (got, want) in zip(drawn, expected) {
                XCTAssertEqual(
                    got, want, accuracy: 2.5 / 255, "\(what) \(index): drawn \(drawn), want \(expected)",
                    file: file, line: line)
            }
        }
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
        XCTAssertEqual(solid.view.backgroundAppearance.map { [$0.opaque, $0.blur] }, [true, false])
        assertBackdrop(solid, false, "solid")
        XCTAssertTrue(try isOpaque(controller), "one opaque pane")
        assertTitlebar(controller, translucent: false, "one opaque pane")
        let frame = try XCTUnwrap(controller.window).frame

        let frosted = try split(controller, profile: nil)
        try requireSurfaces([frosted])
        XCTAssertEqual(frosted.view.backgroundAppearance.map { [$0.opaque, $0.blur] }, [false, true])
        assertBackdrop(frosted, true, "the default profile")
        assertBackdrop(solid, false, "solid, after the split")
        XCTAssertFalse(try isOpaque(controller), "a translucent pane makes the window transparent")
        assertTitlebar(controller, translucent: true, "a blurred pane")
        XCTAssertEqual(try XCTUnwrap(controller.window).frame, frame, "the window keeps its frame")

        let clear = try split(controller, .below, profile: "clear")
        try requireSurfaces([clear])
        XCTAssertEqual(clear.view.backgroundAppearance.map { [$0.opaque, $0.blur] }, [false, false])
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
        assertTitlebar(controller, translucent: true, "the frosted pane is left")
        controller.focus(frosted)
        controller.closeFocusedPane(nil)
        XCTAssertEqual(controller.panes, [solid])
        XCTAssertTrue(try isOpaque(controller), "only the opaque pane is left")
        assertTitlebar(controller, translucent: false, "only the opaque pane is left")
        XCTAssertEqual(try XCTUnwrap(controller.window).frame, frame, "the window has its frame back")
    }

    // MARK: Title bar (#363)

    /// The title bar and tab bar of a translucent window draw no band of
    /// their own: it gets a transparent title bar over full-size content,
    /// always blurred under it (here no pane is), the panes below it at the
    /// size they had, the window at its frame; a tab bar coming moves the
    /// panes below it; opaque again, the window is standard again. Driven
    /// directly, so it runs without a renderer too.
    func testATranslucentWindowsTitleBarShowsWhatThePanesShowThrough() throws {
        let host = try startHost()
        let controller = try host.open(profile: "solid")
        let window = try XCTUnwrap(controller.window)
        let content = try XCTUnwrap(window.contentView as? TerminalWindowContentView)
        assertTitlebar(controller, translucent: false, "opaque")
        let frame = window.frame
        let size = controller.paneContainer.frame.size
        // Not even for a moment: a pane resized mid-flip and back changes the
        // pty's window size twice and reflows its scrollback (with a renderer;
        // without one nothing reaches the core and this holds trivially).
        let pane = controller.focusedPane
        let resizes = pane.view.coreResizeCount

        content.updateTitlebar(translucent: true)
        assertTitlebar(controller, translucent: true, "translucent")
        XCTAssertNil(pane.view.backdrop, "translucent: the pane keeps its own setting, no blur")
        XCTAssertEqual(window.frame, frame, "translucent: the window keeps its frame")
        XCTAssertEqual(controller.paneContainer.frame.size, size, "translucent: the panes keep their size")
        XCTAssertEqual(pane.view.coreResizeCount, resizes, "translucent: the pane never resized")

        let backdrop = content.titlebarBackdrop
        content.updateTitlebar(translucent: true)
        assertTitlebar(controller, translucent: true, "translucent again")
        XCTAssertTrue(content.titlebarBackdrop === backdrop, "translucent again: the same blur is kept")
        XCTAssertEqual(window.frame, frame, "translucent again: the window keeps its frame")

        for _ in 0..<10 {
            content.updateTitlebar(translucent: false)
            content.updateTitlebar(translucent: true)
        }
        assertTitlebar(controller, translucent: true, "after 20 flips")
        XCTAssertEqual(window.frame, frame, "after 20 flips: the window keeps its frame")
        XCTAssertEqual(pane.view.coreResizeCount, resizes, "20 flips: the pane never resized")

        let other = try XCTUnwrap(host.open(profile: "solid").window)
        window.addTabbedWindow(other, ordered: .above)
        XCTAssertLessThan(controller.paneContainer.frame.height, size.height, "a tab bar takes the panes' top")
        assertTitlebar(controller, translucent: true, "blurred, with a tab bar")

        let tabbed = window.frame
        let tabbedResizes = pane.view.coreResizeCount
        content.updateTitlebar(translucent: false)
        assertTitlebar(controller, translucent: false, "opaque again, with a tab bar")
        XCTAssertEqual(window.frame, tabbed, "opaque again: the window keeps its frame")
        XCTAssertEqual(pane.view.coreResizeCount, tabbedResizes, "opaque again: the pane never resized")
    }

    // MARK: Strip tint (#388)

    /// Dracula for the default profile, Nord for `nord`, at `opacity`.
    private static func tintConfig(opacity: Double, blur: Bool, theme: String = "Dracula") -> String {
        """
        [appearance]
        theme = "\(theme)"
        opacity = \(opacity)
        blur = \(blur)
        [[profiles]]
        name = "nord"
        [profiles.appearance]
        theme = "Nord"
        """
    }

    /// At 0.9, 0.7 and 0.5 opacity, blurred or not, each pane at the top of
    /// a split tab tints the strip across its own width with its session's
    /// background (Dracula left, Nord right; the pane below Nord none), as
    /// the core reports it and as drawn; the tint changes neither the panes'
    /// frames nor their sessions' sizes. A maximized pane alone tints all of
    /// it, a pane below it never.
    func testTheStripIsTintedWithEachTopPanesBackground() throws {
        for opacity in [0.9, 0.7, 0.5] {
            for blur in [true, false] {
                let what = "opacity \(opacity), blur \(blur)"
                try Self.tintConfig(opacity: opacity, blur: blur)
                    .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
                let host = try startHost()
                let controller = try host.open(profile: nil)
                let left = controller.focusedPane
                let right = try split(controller, profile: "nord")
                let below = try split(controller, .below, profile: nil)
                try requireSurfaces([left, right, below])
                XCTAssertFalse(try isOpaque(controller), what)
                assertTitlebar(controller, translucent: true, what)
                XCTAssertEqual(try background(of: left)[3], CGFloat(Float(opacity)), what)
                let frames = controller.paneContainer.paneFrames.map(\.frame)
                let area = controller.paneContainer.frame
                let resizes = controller.panes.map(\.view.coreResizeCount)
                try assertTint(controller, [left, right], what)
                try assertNoSeam(controller, what)

                for pane in controller.panes {
                    pane.view.appearanceDidChange()
                }
                try assertTint(controller, [left, right], "\(what), read again")
                XCTAssertEqual(controller.paneContainer.paneFrames.map(\.frame), frames, "\(what): the panes stay")
                XCTAssertEqual(controller.paneContainer.frame, area, "\(what): below the strip")
                XCTAssertEqual(controller.panes.map(\.view.coreResizeCount), resizes, "\(what): no session resized")

                // A divider moved (as a drag does) moves the segments with
                // the panes.
                let root = try XCTUnwrap(controller.paneContainer.subviews.first as? NSSplitView, what)
                let content = try XCTUnwrap(controller.window?.contentView as? TerminalWindowContentView)
                let tint = try XCTUnwrap(content.titlebarTint)
                content.displayIfNeeded()
                XCTAssertFalse(tint.needsDisplay, "\(what): drawn")
                root.setPosition(root.bounds.width * 0.3, ofDividerAt: 0)
                XCTAssertTrue(tint.needsDisplay, "\(what): the moved segments are drawn on screen")
                XCTAssertLessThan(left.view.frame.width, frames[0].width - 10, "\(what): the divider moved")
                try assertTint(controller, [left, right], "\(what), divider moved")

                controller.focus(right)
                _ = controller.toggleZoom()
                XCTAssertTrue(controller.isZoomed, what)
                try assertTint(controller, [right], "\(what), maximized")
                _ = controller.toggleZoom()
                try assertTint(controller, [left, right], "\(what), back")

                for controller in host.controllers {
                    controller.window?.close()
                }
                host.core.shutdown()
                self.host = nil
            }
        }
    }

    /// Two panes of one profile tint the strip in one continuous colour
    /// at 1x: the boundary between their segments, in the middle of the
    /// divider on a half point, leaves no lighter line.
    func testASameProfileSplitTintsTheStripWithoutASeam() throws {
        try Self.tintConfig(opacity: 0.6, blur: true)
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let left = controller.focusedPane
        let right = try split(controller, profile: nil)
        try requireSurfaces([left, right])
        let segments = try tintSegments(controller)
        XCTAssertEqual(segments.count, 2)
        let boundary = try XCTUnwrap(segments.first?.maxX)
        XCTAssertNotEqual(boundary, boundary.rounded(), "the boundary is on a half point, \(boundary)")
        let row = try drawnTintRow(controller)
        let expected = try background(of: left)
        for column in 0..<row.count {
            let pixel = row[column]
            XCTAssertTrue(
                zip(pixel, expected).allSatisfy { abs($0 - $1) <= 2.5 / 255 },
                "pixel \(column) (boundary \(boundary)) is \(pixel), want \(expected)")
        }
    }

    /// A reload that changes the theme and the opacity re-tints the strip
    /// with the new background, and resizes no session.
    func testAReloadRetintsTheStripWithoutResizing() async throws {
        try Self.tintConfig(opacity: 0.7, blur: true)
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let left = controller.focusedPane
        let right = try split(controller, profile: "nord")
        try requireSurfaces([left, right])
        try assertTint(controller, [left, right], "before")
        let before = try background(of: left)
        let frames = controller.paneContainer.paneFrames.map(\.frame)
        let resizes = controller.panes.map(\.view.coreResizeCount)

        try Self.tintConfig(opacity: 0.5, blur: true, theme: "Tokyo Night")
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the reload reaches the panes") { try self.background(of: left)[3] == 0.5 }
        try await waitUntil("the strip follows") {
            try self.tintSegments(controller).first?.color?.alpha == 0.5
        }
        XCTAssertNotEqual(Array(try background(of: left)[0..<3]), Array(before[0..<3]), "another theme")
        try assertTint(controller, [left, right], "after")
        XCTAssertEqual(controller.paneContainer.paneFrames.map(\.frame), frames, "the panes stay")
        XCTAssertEqual(controller.panes.map(\.view.coreResizeCount), resizes, "no session resized")
    }

    /// m1: content no taller than the title bar (a programmatic frame) still
    /// keeps the window's smallest size at the panes' plus the whole title
    /// bar, and at that smallest size a flip to opaque keeps the frame with
    /// the panes at their smallest size. Driven directly, so it runs without
    /// a renderer too.
    func testTheSmallestTranslucentWindowKeepsTheWholeTitleBarAndItsFrame() throws {
        let host = try startHost()
        let controller = try host.open(profile: "solid")
        let window = try XCTUnwrap(controller.window)
        let content = try XCTUnwrap(window.contentView as? TerminalWindowContentView)
        content.updateTitlebar(translucent: true)
        let titlebar = TerminalWindowContentView.standardTitlebarHeight(of: window)
        XCTAssertGreaterThan(titlebar, 20, "a title bar")
        let pmin = controller.paneContainer.minimumSize.height

        var small = window.frame
        small.size.height = 20
        window.setFrame(small, display: false)
        XCTAssertLessThanOrEqual(content.bounds.height, titlebar, "content no taller than the title bar")
        XCTAssertEqual(window.contentMinSize.height, pmin + titlebar, "the panes' smallest size plus the title bar")

        var smallest = window.frame
        smallest.size.height =
            window.frameRect(forContentRect: NSRect(origin: .zero, size: window.contentMinSize)).height
        window.setFrame(smallest, display: false)
        XCTAssertEqual(window.contentMinSize.height, pmin + titlebar, "at the smallest size")
        let frame = window.frame
        content.updateTitlebar(translucent: false)
        XCTAssertEqual(window.frame, frame, "opaque: the frame is kept")
        XCTAssertGreaterThanOrEqual(controller.paneContainer.frame.height, pmin, "the panes keep their smallest size")
        assertTitlebar(controller, translucent: false, "opaque at the smallest size")
    }

    /// The core's appearance change (here a reload of `config.toml` that
    /// turns the default profile's blur off) reaches each pane through the
    /// router: the pane's backdrop goes, the window stays transparent and its
    /// title bar stays blurred.
    func testAConfigurationReloadReachesEveryPane() async throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let frosted = controller.focusedPane
        let solid = try split(controller, profile: "solid")
        try requireSurfaces([frosted, solid])
        assertBackdrop(frosted, true, "before")
        assertTitlebar(controller, translucent: true, "before")
        try Self.config.replacingOccurrences(of: "blur = true", with: "blur = false")
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the reload turns the blur off") { frosted.view.backdrop == nil }
        assertBackdrop(frosted, false, "after")
        XCTAssertEqual(frosted.view.backgroundAppearance?.opaque, false)
        XCTAssertEqual(frosted.view.backgroundAppearance?.blur, false)
        XCTAssertFalse(try isOpaque(controller), "still translucent")
        assertBackdrop(solid, false, "after, solid")
        assertTitlebar(controller, translucent: true, "after: no pane is blurred")
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
        let panes = [frosted, clear, solid]
        let resizes = panes.map(\.view.coreResizeCount)

        try host.core.setDisplayOptions(.init(reduceTransparency: true))
        try await waitUntil("every pane is opaque") { try self.isOpaque(controller) }
        for pane in panes {
            XCTAssertEqual(pane.view.backgroundAppearance?.opaque, true)
            XCTAssertEqual(pane.view.backgroundAppearance?.blur, false)
            assertBackdrop(pane, false, "reduced")
        }
        assertTitlebar(controller, translucent: false, "reduced")
        XCTAssertEqual(
            panes.map(\.view.coreResizeCount), resizes,
            "the title bar's change resizes no pane (no pty window-size change)")

        // A pane opened meanwhile starts opaque.
        let late = try split(controller, profile: nil)
        try requireSurfaces([late])
        assertBackdrop(late, false, "opened while reduced")
        XCTAssertTrue(try isOpaque(controller))
        let all = panes + [late]
        let lateResizes = all.map(\.view.coreResizeCount)

        try host.core.setDisplayOptions(.init(reduceTransparency: false))
        try await waitUntil("the panes are translucent again") { try !self.isOpaque(controller) }
        try await waitUntil("the blur is back") { frosted.view.backdrop != nil && late.view.backdrop != nil }
        assertTitlebar(controller, translucent: true, "turned off")
        XCTAssertEqual(
            all.map(\.view.coreResizeCount), lateResizes, "turned off: the title bar's change resizes no pane")
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
        weak var weakTint: StripTintView?
        weak var weakStripBlur: PaneBackdropView?
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
            let content = controller.window?.contentView as? TerminalWindowContentView
            weakTint = content?.titlebarTint
            weakStripBlur = content?.titlebarBackdrop
            pane.view.inputContext?.deactivate()
        }
        XCTAssertNotNil(weakBackdrop, "the pane was blurred")
        XCTAssertNotNil(weakStripBlur, "the strip was blurred")
        XCTAssertNotNil(weakTint, "the strip was tinted")
        autoreleasepool { weakController?.closeFocusedPane(nil) }
        try await waitUntil("the closed pane is freed") {
            weakPane == nil && weakView == nil && weakBackdrop == nil && weakSession == nil
        }
        XCTAssertNotNil(weakFirstBackdrop, "the other pane keeps its blur")
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the tab is freed") {
            weakController == nil && weakFirstBackdrop == nil && weakTint == nil && weakStripBlur == nil
        }
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
