import AppKit
import Metal
import TesseraKit
import XCTest

@testable import Tessera

private struct BlurTimedOut: Error, CustomStringConvertible {
    var description: String
}

/// The effect views in `view` and under it, at any depth: the window's one
/// blur at most (#491), never one per pane.
@MainActor
func effectViews(in view: NSView) -> [NSVisualEffectView] {
    ((view as? NSVisualEffectView).map { [$0] } ?? []) + view.subviews.flatMap(effectViews(in:))
}

/// The alpha of the window's blur mask at `point` (points from the bottom
/// left of `backdrop`): its layer's mask as Core Animation draws it, at 1x
/// into an sRGB bitmap the mask's size.
@MainActor
func maskAlpha(_ backdrop: WindowBackdropView, at point: NSPoint) throws -> CGFloat {
    let mask = try XCTUnwrap(backdrop.layer?.mask, "the blur's mask is on its layer")
    let width = Int(mask.bounds.width.rounded()), height = Int(mask.bounds.height.rounded())
    let space = try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB))
    let context = try XCTUnwrap(
        CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0, space: space,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
    mask.render(in: context)
    let pixels = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
    let x = min(max(Int(point.x), 0), width - 1), y = min(max(height - 1 - Int(point.y), 0), height - 1)
    return CGFloat(pixels[y * context.bytesPerRow + x * 4 + 3]) / 255
}

/// The bytes of bitmap the masks in `layer`'s tree hold (#491 B3): a
/// layer's image contents, or its drawn backing at its scale.
@MainActor
func maskBackingBytes(_ layer: CALayer?, inMask: Bool = false) -> Int {
    guard let layer else { return 0 }
    var bytes = 0
    if inMask, let contents = layer.contents {
        if CFGetTypeID(contents as CFTypeRef) == CGImage.typeID {
            let image = contents as! CGImage  // swiftlint:disable:this force_cast
            bytes += image.bytesPerRow * image.height
        } else {
            bytes += Int(layer.bounds.width * layer.bounds.height * layer.contentsScale * layer.contentsScale * 4)
        }
    }
    bytes += maskBackingBytes(layer.mask, inMask: true)
    for sublayer in layer.sublayers ?? [] {
        bytes += maskBackingBytes(sublayer, inMask: inMask)
    }
    return bytes
}

/// The window's blur (#491, ADR 0045 amendment): one masked material per
/// window at the radius's strength, none at radius 0, inside the window's
/// rounded corners. Windows come from ``TransparencyHost`` with its
/// configuration (translucent, blurred; profiles `solid` and `clear`).
@MainActor
final class BlurRadiusTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var host: TransparencyHost?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-blur-radius-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try TransparencyTests.config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
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

    private func frame(of pane: Pane, in content: TerminalWindowContentView) -> NSRect {
        content.convert(pane.view.bounds, from: pane.view)
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

    func testRadiusZeroAttachesNoBlurOrEffectView() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        let window = try XCTUnwrap(controller.window)
        let content = try content(controller)
        XCTAssertFalse(window.isOpaque, "translucent and blurred")
        XCTAssertEqual(controller.windowBlurRadius, WindowBlurRadius.defaultRadius, "the default until one is set")
        XCTAssertNotNil(content.windowBackdrop, "the default blurs")

        controller.windowBlurRadius = 0
        XCTAssertNil(content.windowBackdrop, "radius 0: no blur")
        XCTAssertTrue(effectViews(in: content).isEmpty, "radius 0: no effect view, under the title bar neither")
        _ = try split(controller, profile: nil)
        content.updateWindowBlur()
        XCTAssertTrue(effectViews(in: content).isEmpty, "radius 0, a new blurred pane: still none")

        controller.windowBlurRadius = 1
        XCTAssertEqual(effectViews(in: content).count, 1, "radius 1: one")
        controller.windowBlurRadius = 0
        XCTAssertTrue(effectViews(in: content).isEmpty, "back to 0: removed")
    }

    func testOneEffectViewPerWindowCoversOnlyTheBlurredPanes() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let frosted = controller.focusedPane
        let clear = try split(controller, profile: "clear")
        let solid = try split(controller, profile: "solid")
        try requireSurfaces([frosted, clear, solid])
        let content = try content(controller)
        let backdrop = try XCTUnwrap(content.windowBackdrop)
        XCTAssertEqual(effectViews(in: content).map(ObjectIdentifier.init), [ObjectIdentifier(backdrop)], "one per window")
        XCTAssertTrue(content.subviews.first === backdrop, "under everything")
        XCTAssertEqual(backdrop.frame, content.bounds, "filling the window")
        XCTAssertEqual(backdrop.blendingMode, .behindWindow)
        XCTAssertEqual(backdrop.material, .fullScreenUI, "no grey cast (#363)")
        XCTAssertEqual(backdrop.state, .active, "blurred in a window that is not key")
        XCTAssertNil(backdrop.hitTest(NSPoint(x: 5, y: 5)), "takes no clicks")
        let strip = try XCTUnwrap(content.titlebarTint).frame
        XCTAssertGreaterThan(strip.height, 0)
        XCTAssertEqual(
            backdrop.maskedRegions, [strip, frame(of: frosted, in: content)],
            "the strip and the blurred pane; not the clear pane, not the opaque one")
        XCTAssertNotNil(backdrop.maskPath)

        controller.focus(clear)
        controller.closeFocusedPane(nil)
        controller.paneContainer.layoutSubtreeIfNeeded()
        XCTAssertEqual(effectViews(in: content).count, 1, "still one")
        XCTAssertEqual(backdrop.maskedRegions, [strip, frame(of: frosted, in: content)], "the blurred pane, resized")
    }

    func testTheBlurFollowsTheWindowsRoundedCorners() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        let window = try XCTUnwrap(controller.window)
        let content = try content(controller)
        let radius = WindowBlurRadius.cornerRadius(of: window)
        if window.responds(to: NSSelectorFromString("_cornerRadius")),
            let appKit = window.value(forKey: "_cornerRadius") as? NSNumber
        {
            XCTAssertEqual(radius, CGFloat(appKit.doubleValue), "AppKit's corner radius")
        }
        XCTAssertGreaterThanOrEqual(radius, 8, "a titled window has rounded corners")

        func assertShape(_ what: String) throws {
            let backdrop = try XCTUnwrap(content.windowBackdrop, what)
            XCTAssertEqual(backdrop.maskedCornerRadius, radius, what)
            let shape = try XCTUnwrap(backdrop.layer?.mask as? CAShapeLayer, what)
            XCTAssertNotNil(shape.path, what)
            let size = content.bounds.size
            XCTAssertEqual(shape.bounds.size, size, "\(what): the window's size")
            let inset = radius - radius / 2.squareRoot()  // where the arc crosses the diagonal
            for (x, y) in [(0.0, 0.0), (size.width - 1, 0), (0, size.height - 1), (size.width - 1, size.height - 1)] {
                let toward = (x: x == 0 ? 1.0 : -1.0, y: y == 0 ? 1.0 : -1.0)
                let outside = NSPoint(x: x + toward.x * (inset - 2), y: y + toward.y * (inset - 2))
                let inside = NSPoint(x: x + toward.x * (inset + 2), y: y + toward.y * (inset + 2))
                XCTAssertEqual(try maskAlpha(backdrop, at: NSPoint(x: x, y: y)), 0, "\(what): no blur at corner \(x),\(y)")
                XCTAssertLessThan(try maskAlpha(backdrop, at: outside), 0.1, "\(what): none outside the arc at \(outside)")
                XCTAssertGreaterThan(try maskAlpha(backdrop, at: inside), 0.9, "\(what): blur inside the arc at \(inside)")
            }
            XCTAssertEqual(try maskAlpha(backdrop, at: NSPoint(x: 0, y: size.height / 2)), 1, "\(what): to the left edge")
            XCTAssertEqual(try maskAlpha(backdrop, at: NSPoint(x: size.width / 2, y: size.height - 1)), 1, "\(what): top")
        }
        try assertShape("at the default")
        controller.windowBlurRadius = 64
        try assertShape("at 64")
        for pane in controller.panes {
            pane.view.appearanceDidChange()
        }
        try assertShape("after a theme change")
        window.setContentSize(NSSize(width: 500, height: 320))
        content.layoutSubtreeIfNeeded()
        try assertShape("resized")
    }

    /// #491 B1: a maximized pane's hidden siblings are not blurred, and
    /// they are again when it is restored.
    func testZoomingLeavesTheHiddenPanesOutOfTheBlur() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil)
        let frosted = controller.focusedPane
        let clear = try split(controller, profile: "clear")
        controller.paneContainer.layoutSubtreeIfNeeded()
        try requireSurfaces([frosted, clear])
        let content = try content(controller)
        let backdrop = try XCTUnwrap(content.windowBackdrop)
        let strip = try XCTUnwrap(content.titlebarTint).frame
        let frostedFrame = content.convert(frosted.view.bounds, from: frosted.view)
        XCTAssertEqual(backdrop.maskedRegions, [strip, frostedFrame], "split: the frosted pane")
        controller.focus(clear)
        XCTAssertTrue(controller.toggleZoom())
        XCTAssertTrue(frosted.view.isHiddenOrHasHiddenAncestor)
        XCTAssertEqual(backdrop.maskedRegions, [strip], "the clear pane maximized: only the strip")
        controller.unzoom()
        XCTAssertEqual(
            backdrop.maskedRegions, [strip, content.convert(frosted.view.bounds, from: frosted.view)],
            "restored: the frosted pane again")
        controller.focus(frosted)
        XCTAssertTrue(controller.toggleZoom())
        XCTAssertEqual(backdrop.maskedRegions, [strip, content.convert(frosted.view.bounds, from: frosted.view)])
        XCTAssertEqual(frosted.view.frame.size, controller.paneContainer.bounds.size, "the frosted pane maximized")
        controller.unzoom()
        XCTAssertEqual(backdrop.maskedRegions, [strip, content.convert(frosted.view.bounds, from: frosted.view)])
    }

    /// #491 B3: the mask is a path, not a bitmap the window's size: at most
    /// 256 KB of mask backing for a large translucent window at the
    /// default, also after resizing it; and a mask update per resize or
    /// divider step takes at most 1 ms (median).
    func testTheMaskIsSmallAndQuickToUpdate() throws {
        let host = try startHost()
        let controller = try host.open(profile: nil, size: NSSize(width: 1440, height: 900))
        try requireSurfaces(controller.panes)
        let window = try XCTUnwrap(controller.window)
        let content = try content(controller)
        func settle() {
            content.layoutSubtreeIfNeeded()
            content.displayIfNeeded()
            CATransaction.flush()
        }
        settle()
        let backdrop = try XCTUnwrap(content.windowBackdrop)
        XCTAssertEqual(controller.windowBlurRadius, WindowBlurRadius.defaultRadius)
        XCTAssertNil(backdrop.maskImage, "no mask image the window's size")
        XCTAssertLessThanOrEqual(maskBackingBytes(backdrop.layer), 256 * 1024, "at the default")
        for step in 0..<50 {
            window.setContentSize(NSSize(width: 1200 + CGFloat(step * 3), height: 800 + CGFloat(step % 7)))
            settle()
        }
        XCTAssertLessThanOrEqual(maskBackingBytes(backdrop.layer), 256 * 1024, "after resizing")
        XCTAssertEqual(backdrop.maskedRegions, content.blurRegions, "the mask follows the resize")

        let cornerRadius = WindowBlurRadius.cornerRadius(of: window)
        let clock = ContinuousClock()
        var times: [Duration] = []
        for step in 0..<200 {
            // A resize step and a divider step: a new size and a pane edge
            // one point over, as a live resize or a divider drag gives.
            let size = NSSize(width: 1440 - CGFloat(step), height: 900)
            backdrop.setFrameSize(size)
            let edge = 700 + CGFloat(step % 100)
            let regions = [
                NSRect(x: 0, y: 868, width: size.width, height: 32), NSRect(x: 0, y: 0, width: edge, height: 868),
                NSRect(x: edge + 1, y: 0, width: size.width - edge - 1, height: 868),
            ]
            let start = clock.now
            backdrop.setMask(regions: regions, cornerRadius: cornerRadius)
            times.append(clock.now - start)
            XCTAssertEqual(backdrop.maskedRegions, regions)
        }
        times.sort()
        let median = times[times.count / 2]
        var load = [0.0, 0.0, 0.0]
        _ = getloadavg(&load, 3)
        print("BLUR mask update median \(median), p95 \(times[times.count * 95 / 100]), load \(load)")
        XCTAssertLessThanOrEqual(median, .milliseconds(1), "a mask update per step")
        XCTAssertLessThanOrEqual(maskBackingBytes(backdrop.layer), 256 * 1024, "after the steps")
        content.updateWindowBlur()
    }

    func testTheStrengthFollowsTheRadius() throws {
        XCTAssertEqual(WindowBlurRadius.strength(0), 0)
        XCTAssertEqual(WindowBlurRadius.strength(16), 0.25)
        XCTAssertEqual(WindowBlurRadius.strength(48), 0.75)
        XCTAssertEqual(WindowBlurRadius.strength(64), 1)
        XCTAssertEqual(WindowBlurRadius.strength(UInt32.max), 1, "clamped to 64")
        let host = try startHost()
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        for (radius, alpha) in [(UInt32(16), CGFloat(0.25)), (32, 0.5), (64, 1), (500, 1)] {
            controller.windowBlurRadius = radius
            XCTAssertEqual(try content(controller).windowBackdrop?.alphaValue, alpha, "radius \(radius)")
        }
    }

    func testTheDefaultRadius() throws {
        XCTAssertEqual(WindowBlurRadius.defaultRadius, 48, "close to the old material blur (#491)")
        let host = try startHost()
        XCTAssertEqual(try host.core.blurRadius(), WindowBlurRadius.defaultRadius, "the core's default is the app's")
        let controller = try host.open(profile: nil)
        try requireSurfaces(controller.panes)
        XCTAssertEqual(try content(controller).windowBackdrop?.alphaValue, 0.75, "at the default's strength")
    }

    func testAnOpaqueWindowHasNoBlur() async throws {
        let host = try startHost()
        let solid = try host.open(profile: "solid")
        try requireSurfaces(solid.panes)
        XCTAssertTrue(try XCTUnwrap(solid.window).isOpaque)
        XCTAssertTrue(effectViews(in: try content(solid)).isEmpty, "opaque: none")
    }

    func testClosingABlurredTabReleasesItsViews() async throws {
        let host = try startHost()
        weak var weakController: TerminalWindowController?
        weak var weakContent: TerminalWindowContentView?
        weak var weakBackdrop: WindowBackdropView?
        try autoreleasepool {
            let controller = try host.open(profile: nil)
            try requireSurfaces(controller.panes)
            weakController = controller
            weakContent = try content(controller)
            weakBackdrop = try content(controller).windowBackdrop
            XCTAssertNotNil(weakBackdrop)
            controller.focusedPane.view.inputContext?.deactivate()
        }
        autoreleasepool { weakController?.window?.close() }
        try await waitUntil("the tab is freed") { weakController == nil && weakContent == nil && weakBackdrop == nil }
    }
}

/// The radius as the app reads it (#491): at start, on a reload and with
/// Reduce Transparency, for windows the app opens itself.
@MainActor
final class BlurRadiusReloadTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?
    private var reduces = false

    private static func config(radius: Int, dimming: Double = 0) -> String {
        TransparencyTests.config.replacingOccurrences(
            of: "blur = true", with: "blur = true\nblur_radius = \(radius)\ndim_inactive_panes = \(dimming)")
    }

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { [unowned self] in reduces }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-blur-reload-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try write(radius: 32)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        try appDelegate.replaceCore(start: false)
        appDelegate.spawnOptions = .init()
        HostDisplaySettings.pin(appDelegate)
        appDelegate.windowBlurRadius = WindowBlurRadius.defaultRadius
        appDelegate = nil
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

    private func write(radius: Int, dimming: Double = 0) throws {
        try Self.config(radius: radius, dimming: dimming)
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
    }

    private func strength(_ controller: TerminalWindowController) -> CGFloat? {
        (controller.window?.contentView as? TerminalWindowContentView)?.windowBackdrop?.alphaValue
    }

    private func requireSurfaces(_ controllers: [TerminalWindowController]) throws {
        guard MTLCreateSystemDefaultDevice() != nil, controllers.flatMap(\.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
    }

    /// Opens the window and the tab as the app does (⌘N, ⌘T) and posts
    /// nothing: the file's radius is read at start and reaches both.
    func testAWindowTheAppOpensIsBlurredAtTheFilesRadius() async throws {
        try appDelegate.replaceCore(start: true)
        XCTAssertEqual(appDelegate.windowBlurRadius, 32, "read at start")
        let controller = try appDelegate.newWindow()
        let tab = try appDelegate.newTab(in: try XCTUnwrap(controller.window))
        try requireSurfaces([controller, tab])
        XCTAssertEqual(strength(controller), 0.5, "the window")
        XCTAssertEqual(strength(tab), 0.5, "the tab")
    }

    func testAReloadOrReduceTransparencyReachesEveryWindow() async throws {
        try appDelegate.replaceCore(start: true)
        let first = try appDelegate.newWindow()
        let second = try appDelegate.newWindow()
        let windows = try [first, second].map { try XCTUnwrap($0.window) }
        try requireSurfaces([first, second])
        XCTAssertEqual([first, second].map(strength), [0.5, 0.5])

        try write(radius: 16, dimming: 0.3)
        try await waitUntil("the reload is applied") { [first, second].map(strength) == [0.25, 0.25] }

        try write(radius: 0, dimming: 0.3)
        try await waitUntil("radius 0: no blur") { [first, second].allSatisfy { strength($0) == nil } }
        for controller in [first, second] {
            XCTAssertTrue(
                effectViews(in: try XCTUnwrap(controller.window?.contentView)).isEmpty, "radius 0: no effect view")
        }

        try write(radius: 64)
        try await waitUntil("64: the full material") { [first, second].map(strength) == [1, 1] }
        reduces = true
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
        try await waitUntil("Reduce Transparency: opaque") { windows.allSatisfy(\.isOpaque) }
        XCTAssertEqual(appDelegate.windowBlurRadius, 0, "Reduce Transparency: radius 0")
        XCTAssertTrue([first, second].allSatisfy { strength($0) == nil }, "no blur")
        reduces = false
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: NSWorkspace.shared)
        try await waitUntil("turned off: blurred again") { [first, second].map(strength) == [1, 1] }
    }
}

@MainActor
final class TabVisibilityTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previous: String?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        appDelegate.reducesTransparency = { false }
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-tab-blur-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try TransparencyTests.config.write(
            to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        try appDelegate.replaceCore(start: true)
        appDelegate.windowBlurRadius = 20
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
        appDelegate.windowBlurRadius = WindowBlurRadius.defaultRadius
        appDelegate = nil
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

    /// Two tabs of one window, the first (default profile: translucent,
    /// blurred) selected, the second of `profile`; the radius then changed
    /// to 40, which the unselected second tab cannot take yet.
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
        guard window.windowNumber > 0, tab.windowNumber > 0 else {
            throw XCTSkip("the host puts no window on screen")
        }
        XCTAssertTrue(window.tabGroup === tab.tabGroup, "one tab group")
        window.tabGroup?.selectedWindow = window
        appDelegate.windowBlurRadius = 40
        XCTAssertEqual([first, second].map(\.windowBlurRadius), [40, 40], "both tabs")
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
            XCTAssertEqual(content.windowBackdrop?.frame, content.bounds, "\(what): the blur", file: file, line: line)
            XCTAssertEqual(
                content.windowBackdrop?.maskedRegions.first, strip, "\(what): the strip's blur", file: file, line: line)
            XCTAssertEqual(content.titlebarTint?.frame, strip, "\(what): the strip's tint", file: file, line: line)
            XCTAssertEqual(
                content.subviews.map { ObjectIdentifier($0) },
                [content.windowBackdrop, content.titlebarTint, controller.paneContainer].compactMap {
                    $0.map { ObjectIdentifier($0) }
                }, "\(what): blur, tint, panes, bottom up", file: file, line: line)
            XCTAssertEqual(
                content.titlebarTint?.segments.first?.minX, 0, "\(what): the tint spans the strip", file: file,
                line: line)
            XCTAssertEqual(
                content.titlebarTint?.segments.last?.maxX, strip.width, "\(what): to its end", file: file, line: line)
        } else {
            XCTAssertNil(content.windowBackdrop, "\(what): no blur", file: file, line: line)
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
        XCTAssertEqual(
            content.windowBackdrop?.alphaValue ?? 0, WindowBlurRadius.strength(radius), "\(what): the radius's strength",
            file: file, line: line)
    }

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

    /// #443: a window or tab the app opens leaves restoration to the window
    /// state store. AppKit's own restoration work takes a lock that a
    /// session spawn's fork waits for under TSan (#278).
    func testTheAppsWindowsAndTabsAreNotRestorableByAppKit() throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        XCTAssertFalse(window.isRestorable, "a new window")
        XCTAssertFalse(try XCTUnwrap(second.window).isRestorable, "a new tab")
    }

    /// #405 n1: the radius changed while the window's tabs are
    /// minimized, then while the app is hidden, reaches every tab window as
    /// it comes back, through the real window-number guard (a minimized or
    /// hidden window has none), with no notification posted by the test.
    func testARadiusChangedWhileMinimizedOrHiddenReachesEveryTab() async throws {
        let first = try appDelegate.newWindow()
        let window = try XCTUnwrap(first.window)
        let second = try appDelegate.newTab(in: window)
        let tab = try XCTUnwrap(second.window)
        let controllers = [first, second]
        guard MTLCreateSystemDefaultDevice() != nil, controllers.flatMap(\.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        guard window.windowNumber > 0 || tab.windowNumber > 0 else {
            throw XCTSkip("the host puts no window on screen")
        }
        let contents = try controllers.map { try XCTUnwrap($0.window?.contentView as? TerminalWindowContentView) }
        func strengths() -> [CGFloat?] { contents.map { $0.windowBackdrop?.alphaValue } }

        tab.miniaturize(nil)
        let deadline = ContinuousClock.now + .seconds(10)
        while !tab.isMiniaturized, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        guard tab.isMiniaturized else { throw XCTSkip("the host cannot minimize a window (no Dock)") }
        try await waitUntil("minimized") { [window, tab].allSatisfy { !$0.isVisible } }
        appDelegate.windowBlurRadius = 40
        XCTAssertEqual(controllers.map(\.windowBlurRadius), [40, 40], "both tabs")
        XCTAssertEqual(strengths(), [40.0 / 64, 40.0 / 64], "minimized: every tab at once")
        tab.deminiaturize(nil)
        try await waitUntil("restored, every tab at the new radius") { strengths() == [40.0 / 64, 40.0 / 64] }

        NSApp.hide(nil)
        try await waitUntil("hidden") { NSApp.isHidden }
        appDelegate.windowBlurRadius = 30
        XCTAssertEqual(controllers.map(\.windowBlurRadius), [30, 30], "both tabs")
        XCTAssertEqual(strengths(), [30.0 / 64, 30.0 / 64], "hidden: every tab at once")
        NSApp.unhide(nil)
        try await waitUntil("shown again, every tab at the new radius") { strengths() == [30.0 / 64, 30.0 / 64] }
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

extension TerminalSurfaceView {
    /// Whether its window's one blur covers this pane, all of it (#491).
    var isBlurredByWindow: Bool {
        guard let content = window?.contentView as? TerminalWindowContentView, let backdrop = content.windowBackdrop
        else { return false }
        return backdrop.maskedRegions.contains(content.convert(bounds, from: self))
    }
}
