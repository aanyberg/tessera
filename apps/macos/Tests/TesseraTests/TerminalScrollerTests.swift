import AppKit
import Metal
import QuartzCore
import TesseraKit
import XCTest

@testable import Tessera

/// The overlay scroll bar's knob (#495, ADR 0032): AppKit draws the knob
/// of an overlay `NSScroller` that is not inside an `NSScrollView` at
/// alpha 0 and never widens it, so `TerminalScroller` draws its own. With
/// the pointer over the scroller the knob is wide, over its slot, at the
/// view's place in the scrollback; a scroll shows the narrow knob alone.
///
/// The oracle is what the scroller's layer tree draws, rendered into an
/// sRGB bitmap in light appearance, not `alphaValue` or the scroller's own
/// state: a knob pixel is dark and at least a third opaque. The pointer
/// enters and leaves through the scroller's handlers with a synthesised
/// event (lead decision D2: a window that is never on screen gets no
/// tracking-area events), and the tracking area those handlers belong to
/// is checked beside them. Real pointer dispatch is eye check E1.
@MainActor
final class TerminalScrollerTests: XCTestCase {
    /// 10 000 lines of history, the view at the middle (knob value 0.5).
    private static let middle = TesseraSession.ScrollPosition(offset: 5000, history: 10_000, rows: 24)
    private static let height: CGFloat = 400

    private var clock: CFTimeInterval = 1000
    private var hides: [(delay: CFTimeInterval, hide: @MainActor () -> Void)] = []

    // MARK: Helpers

    /// A scroller at the right edge of a 480 x 400 host in a window that is
    /// never on screen, in light appearance, on the test's clock, with the
    /// fade on whatever the machine's Reduce Motion (a CI runner may
    /// report it on). With `realClock` it keeps its own clock and hide
    /// scheduling, as the app's scrollers do.
    private func makeScroller(
        _ position: TesseraSession.ScrollPosition = middle, style: NSScroller.Style = .overlay,
        in provided: NSWindow? = nil, realClock: Bool = false
    ) -> (scroller: TerminalScroller, window: NSWindow) {
        let window = provided ?? NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: Self.height), styleMask: [.titled],
            backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.appearance = NSAppearance(named: .aqua)
        let host = NSView(frame: NSRect(x: 0, y: 0, width: 480, height: Self.height))
        host.wantsLayer = true
        window.contentView = host
        let scroller = TerminalScroller()
        scroller.reducesMotion = false
        if !realClock {
            scroller.now = { [unowned self] in self.clock }
            scroller.scheduleHide = { [unowned self] delay, hide in self.hides.append((delay, hide)) }
        }
        scroller.preferredStyleOverride = style
        scroller.frame = NSRect(
            x: 480 - TerminalScroller.width(for: style), y: 0, width: TerminalScroller.width(for: style),
            height: Self.height)
        host.addSubview(scroller)
        scroller.update(position)
        return (scroller, window)
    }

    /// The test point of the brief: 12 pt from the scroller's right edge,
    /// in AppKit's own hover strip (x 1 to 16 of 17 on macOS 27) and left
    /// of the narrow knob (x 8 to 14), at the knob's height.
    private func hoverPoint(_ scroller: TerminalScroller) -> NSPoint {
        NSPoint(x: scroller.bounds.maxX - 12, y: scroller.rect(for: .knob).midY)
    }

    private func crossing(
        _ type: NSEvent.EventType, _ scroller: TerminalScroller, at point: NSPoint
    ) throws -> NSEvent {
        let window = try XCTUnwrap(scroller.window)
        return try XCTUnwrap(
            NSEvent.enterExitEvent(
                with: type, location: scroller.convert(point, to: nil), modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, trackingNumber: 0, userData: nil))
    }

    private func enter(_ scroller: TerminalScroller) throws {
        scroller.mouseEntered(with: try crossing(.mouseEntered, scroller, at: hoverPoint(scroller)))
    }

    private func exit(_ scroller: TerminalScroller) throws {
        let point = NSPoint(x: -20, y: scroller.bounds.midY)
        scroller.mouseExited(with: try crossing(.mouseExited, scroller, at: point))
    }

    /// Runs the hides the scroller scheduled, each once its delay has
    /// passed on the test's clock, and any a hide schedules again.
    private func runScheduledHides() {
        while !hides.isEmpty {
            let next = hides.removeFirst()
            clock += next.delay
            next.hide()
        }
    }

    /// What the scroller's layer tree draws (model values, so not a fade in
    /// flight), as unpremultiplied sRGB pixels addressed in the scroller's
    /// own flipped coordinates. With `ignoringOpacity` the scroller's own
    /// layer counts as opaque: what a flash shows at its full opacity.
    private struct Rendering {
        let width: Int
        let height: Int
        let bytes: [UInt8]

        /// Red, green, blue and alpha (0...1) of the point's pixel; y from
        /// the top. Core Animation renders a geometry-flipped layer with
        /// its top at the bitmap's last row.
        func pixel(_ x: CGFloat, _ y: CGFloat) -> (red: Double, green: Double, blue: Double, alpha: Double) {
            let column = Int(x.rounded(.down)), row = height - 1 - Int(y.rounded(.down))
            guard (0..<width).contains(column), (0..<height).contains(row) else { return (0, 0, 0, 0) }
            let index = (row * width + column) * 4
            let alpha = Double(bytes[index + 3]) / 255
            guard alpha > 0 else { return (0, 0, 0, 0) }
            return (
                Double(bytes[index]) / 255 / alpha, Double(bytes[index + 1]) / 255 / alpha,
                Double(bytes[index + 2]) / 255 / alpha, alpha
            )
        }

        /// A knob pixel in light appearance: dark, at least a third opaque.
        /// AppKit's knob is black at half opacity; over its slot (white at
        /// three quarters) that gives grey 0.42 at 0.88, as in an
        /// `NSScrollView`.
        func isKnob(_ x: CGFloat, _ y: CGFloat) -> Bool {
            let p = pixel(x, y)
            return p.alpha >= 0.33 && max(p.red, p.green, p.blue) < 0.6
        }

        /// A light knob pixel (a dark theme's, #558): light, at least a
        /// third opaque. Not to be told from the slot in light
        /// appearance, so only where no slot is drawn.
        func isLightKnob(_ x: CGFloat, _ y: CGFloat) -> Bool {
            let p = pixel(x, y)
            return p.alpha >= 0.33 && min(p.red, p.green, p.blue) > 0.6
        }

        /// Something drawn that is not the knob (the slot).
        func isSlot(_ x: CGFloat, _ y: CGFloat) -> Bool {
            pixel(x, y).alpha > 0.05 && !isKnob(x, y)
        }

        /// The knob's extent down column `x`: the top of the first and the
        /// bottom of the last pixel that is a knob pixel, or `nil` without
        /// one. Off the knob's middle column its rounded ends are shorter.
        func knob(atX x: CGFloat, light: Bool = false) -> (top: CGFloat, bottom: CGFloat)? {
            let rows = (0..<height).map(CGFloat.init).filter { light ? isLightKnob(x, $0 + 0.5) : isKnob(x, $0 + 0.5) }
            guard let first = rows.first, let last = rows.last else { return nil }
            return (first, last + 1)
        }

        /// The knob's extent across row `y`: the left of the first and the
        /// right of the last knob pixel, or `nil` without one.
        func knob(atY y: CGFloat) -> (left: CGFloat, right: CGFloat)? {
            let columns = (0..<width).map(CGFloat.init).filter { isKnob($0 + 0.5, y) }
            guard let first = columns.first, let last = columns.last else { return nil }
            return (first, last + 1)
        }

        var maxAlpha: UInt8 { stride(from: 3, to: bytes.count, by: 4).map { bytes[$0] }.max() ?? 0 }
    }

    private func render(_ scroller: TerminalScroller, ignoringOpacity: Bool = false) throws -> Rendering {
        let layer = try XCTUnwrap(scroller.layer)
        func display(_ layer: CALayer) {
            layer.displayIfNeeded()
            layer.sublayers?.forEach(display)
        }
        scroller.displayIfNeeded()
        display(layer)
        let width = Int(scroller.bounds.width), height = Int(scroller.bounds.height)
        let context = try XCTUnwrap(
            CGContext(
                data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                space: try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB)),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        let opacity = layer.opacity
        if ignoringOpacity {
            layer.opacity = 1
        }
        // `render(in:)` draws sublayers in array order, the screen by
        // `zPosition`: AppKit re-adds its own layers on a restyle, after
        // the overlay ones, which are above them on screen.
        let sublayers = layer.sublayers ?? []
        let stacked = sublayers.enumerated()
            .sorted { ($0.element.zPosition, $0.offset) < ($1.element.zPosition, $1.offset) }
            .map(\.element)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.sublayers = stacked
        layer.render(in: context)
        layer.sublayers = sublayers
        CATransaction.commit()
        layer.opacity = opacity
        let data = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
        return Rendering(
            width: width, height: height, bytes: Array(UnsafeBufferPointer(start: data, count: width * height * 4)))
    }

    /// AppKit's minimum knob length for this scroller: its knob at a tiny
    /// proportion (not a size of the test's own).
    private func minimumKnobLength(_ scroller: TerminalScroller) -> CGFloat {
        let proportion = scroller.knobProportion
        scroller.knobProportion = 0.0001
        let length = scroller.rect(for: .knob).height
        scroller.knobProportion = proportion
        return length
    }

    /// The opacity the flash animation on the scroller's layer gives at
    /// `elapsed` seconds after it started (linear between its key frames),
    /// or `nil` without one.
    private func flashOpacity(_ scroller: TerminalScroller, after elapsed: CFTimeInterval) throws -> Double? {
        let layer = try XCTUnwrap(scroller.layer)
        guard
            let animation = layer.animationKeys()?.lazy
                .compactMap({ layer.animation(forKey: $0) as? CAKeyframeAnimation })
                .first(where: { $0.keyPath == "opacity" }),
            let values = animation.values?.compactMap({ ($0 as? NSNumber)?.doubleValue }),
            let times = animation.keyTimes?.map(\.doubleValue), values.count == times.count
        else { return nil }
        let t = min(max(elapsed / animation.duration, 0), 1)
        for index in 1..<times.count where t <= times[index] {
            let span = times[index] - times[index - 1]
            let f = span > 0 ? (t - times[index - 1]) / span : 1
            return values[index - 1] + (values[index] - values[index - 1]) * f
        }
        return values.last
    }

    private func assertWideKnobAtTheMiddle(
        _ scroller: TerminalScroller, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let drawn = try render(scroller, ignoringOpacity: true)
        let x = hoverPoint(scroller).x
        let knob = try XCTUnwrap(drawn.knob(atX: x), "\(message): a wide knob at x \(x)", file: file, line: line)
        let slot = scroller.rect(for: .knobSlot)
        XCTAssertEqual(
            (knob.top + knob.bottom) / 2, slot.midY, accuracy: 2, "\(message): the knob at the middle", file: file,
            line: line)
        // As wide as AppKit's own wide knob: from the hover point to the
        // narrow knob's right edge.
        let across = try XCTUnwrap(drawn.knob(atY: slot.midY), message, file: file, line: line)
        let narrow = scroller.rect(for: .knob)
        XCTAssertLessThanOrEqual(across.left, x, "\(message): wide", file: file, line: line)
        XCTAssertEqual(across.right, narrow.maxX, accuracy: 1, "\(message): wide", file: file, line: line)
        let middle = try XCTUnwrap(drawn.knob(atX: (across.left + across.right) / 2), message, file: file, line: line)
        XCTAssertGreaterThanOrEqual(
            middle.bottom - middle.top + 1, minimumKnobLength(scroller), "\(message): AppKit's minimum length",
            file: file, line: line)
        XCTAssertTrue(
            drawn.isSlot(x, slot.minY + 4) && drawn.isSlot(x, slot.maxY - 4),
            "\(message): the slot behind the knob, at both ends", file: file, line: line)
    }

    /// Asserts the knob is drawn narrow at its height: at AppKit's knob's
    /// right edge, at most half the strip wide, clear of the hover point.
    /// Measured from the pixels, not from AppKit's knob rect: macOS 15
    /// gives a wider rect for a scroller outside a scroll view than 27.
    @discardableResult
    private func assertNarrowKnob(
        _ drawn: Rendering, _ scroller: TerminalScroller, _ message: String, file: StaticString = #filePath,
        line: UInt = #line
    ) throws -> (left: CGFloat, right: CGFloat) {
        let knob = scroller.rect(for: .knob)
        let geometry = "bounds \(scroller.bounds), AppKit's knob \(knob), slot \(scroller.rect(for: .knobSlot))"
        let across = try XCTUnwrap(
            drawn.knob(atY: knob.midY), "\(message): a knob; \(geometry)", file: file, line: line)
        XCTAssertEqual(
            across.right, knob.maxX, accuracy: 1, "\(message): at the right edge; \(geometry)", file: file, line: line)
        let width = across.right - across.left
        XCTAssertTrue(
            width >= 3 && width <= scroller.bounds.width / 2, "\(message): narrow, \(width) pt; \(geometry)",
            file: file, line: line)
        XCTAssertNil(
            drawn.knob(atX: hoverPoint(scroller).x), "\(message): clear of the hover point; \(geometry)", file: file,
            line: line)
        return across
    }

    // MARK: The pointer over the scroller

    func testThePointerOverTheScrollerShowsAWideKnobAtTheViewsPlaceOverItsSlot() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        // The handlers below belong to a tracking area over the whole
        // strip, kept up to date by AppKit (D2).
        scroller.updateTrackingAreas()
        let areas = scroller.trackingAreas.filter { $0.owner === scroller }
        XCTAssertEqual(areas.count, 1)
        let area = try XCTUnwrap(areas.first)
        XCTAssertTrue(area.options.isSuperset(of: [.mouseEnteredAndExited, .inVisibleRect]), "\(area.options)")
        XCTAssertTrue(scroller.visibleRect.contains(hoverPoint(scroller)), "the test point is in the strip")

        XCTAssertEqual(try render(scroller).maxAlpha, 0, "nothing shown before the pointer comes")
        try enter(scroller)
        XCTAssertFalse(scroller.isHidden)
        XCTAssertEqual(scroller.alphaValue, 1)
        XCTAssertEqual(minimumKnobLength(scroller), scroller.rect(for: .knob).height, "a knob of the minimum length")
        // The scroller's own opacity, not ignored: shown without a fade.
        let drawn = try render(scroller)
        XCTAssertNotNil(drawn.knob(atX: hoverPoint(scroller).x), "drawn at full opacity")
        try assertWideKnobAtTheMiddle(scroller, "under the pointer")
    }

    func testTheWideKnobIsWhereTheViewIsAtTheTopAndAtTheBottom() throws {
        let (scroller, window) = makeScroller(.init(offset: 10_000, history: 10_000, rows: 24))
        defer { window.close() }
        try enter(scroller)
        let slot = scroller.rect(for: .knobSlot)
        let length = minimumKnobLength(scroller)
        let x = hoverPoint(scroller).x
        let middle = scroller.rect(for: .knob).midX - 3
        let top = try XCTUnwrap(try render(scroller).knob(atX: middle), "the oldest line")
        XCTAssertEqual(top.top, slot.minY, accuracy: 2, "the oldest line: the knob at the top")
        XCTAssertGreaterThanOrEqual(top.bottom - top.top + 1, length)
        XCTAssertNotNil(try render(scroller).knob(atX: x), "wide")
        scroller.update(.init(offset: 0, history: 10_000, rows: 24))
        let bottom = try XCTUnwrap(try render(scroller).knob(atX: middle), "the live screen")
        XCTAssertEqual(bottom.bottom, slot.maxY, accuracy: 2, "the live screen: the knob at the bottom")
    }

    func testAScrollShowsTheNarrowKnobWithoutItsSlot() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        scroller.flash()
        let drawn = try render(scroller, ignoringOpacity: true)
        let across = try assertNarrowKnob(drawn, scroller, "a scroll")
        let middle = (across.left + across.right) / 2, y = scroller.rect(for: .knob).midY
        let knob = try XCTUnwrap(drawn.knob(atX: middle), "the knob is drawn")
        XCTAssertEqual((knob.top + knob.bottom) / 2, scroller.rect(for: .knobSlot).midY, accuracy: 2)
        // AppKit's light rim, 1 pt outside the knob, keeps a dark knob
        // visible over dark output.
        let rim = drawn.pixel(across.left - 0.5, y)
        XCTAssertGreaterThan(rim.alpha, 0.05, "the rim: \(rim)")
        XCTAssertGreaterThan(min(rim.red, rim.green, rim.blue), 0.9, "a light rim: \(rim)")
        XCTAssertFalse(drawn.isSlot(hoverPoint(scroller).x, knob.top - 20), "no slot")
        XCTAssertFalse(drawn.isSlot(middle, knob.top - 20), "no slot")
    }

    func testTheKnobStaysWideWhileThePointerIsInsideAndFadesAfterItLeaves() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        clock += 30
        XCTAssertTrue(scroller.isRevealed)
        XCTAssertNil(try flashOpacity(scroller, after: 0), "no fade while the pointer is inside")
        try assertWideKnobAtTheMiddle(scroller, "30 s later")

        try exit(scroller)
        let hold = TerminalScroller.holdDuration, fade = TerminalScroller.fadeDuration
        // D3: held 1 s, then faded over 0.3 s.
        XCTAssertEqual(hold, 1.0)
        XCTAssertEqual(fade, 0.3)
        XCTAssertEqual(try XCTUnwrap(try flashOpacity(scroller, after: hold - 0.01)), 1, accuracy: 1e-9)
        let fading = try XCTUnwrap(try flashOpacity(scroller, after: hold + fade / 2))
        XCTAssertTrue(fading > 0 && fading < 1, "fading: \(fading)")
        XCTAssertEqual(try XCTUnwrap(try flashOpacity(scroller, after: hold + fade)), 0, accuracy: 1e-9)
        // What the fade shows is the wide knob it had under the pointer,
        // and a scroll before it ends keeps it.
        try assertWideKnobAtTheMiddle(scroller, "during the fade")
        scroller.flash()
        try assertWideKnobAtTheMiddle(scroller, "a scroll during the fade")
        clock += hold + fade - 0.01
        XCTAssertTrue(scroller.isRevealed, "still fading")
        clock += 0.02
        XCTAssertFalse(scroller.isRevealed, "faded out")

        // The next scroll shows the narrow knob again.
        scroller.flash()
        let drawn = try render(scroller, ignoringOpacity: true)
        try assertNarrowKnob(drawn, scroller, "after a fade-out")
    }

    func testUnderReduceMotionTheKnobHidesAtOnceAfterThePointerLeaves() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        scroller.reducesMotion = true
        try enter(scroller)
        try assertWideKnobAtTheMiddle(scroller, "under the pointer")
        try exit(scroller)
        XCTAssertNil(try flashOpacity(scroller, after: 0), "no fade")
        XCTAssertNotNil(try render(scroller).knob(atX: hoverPoint(scroller).x), "held at full opacity")
        let scheduled = try XCTUnwrap(hides.first)
        XCTAssertEqual(scheduled.delay, TerminalScroller.holdDuration, accuracy: 1e-9)
        clock += scheduled.delay
        hides.removeAll()
        scheduled.hide()
        XCTAssertEqual(try render(scroller).maxAlpha, 0, "gone at once")
        XCTAssertFalse(scroller.isRevealed)
    }

    func testAKnobPressedWithoutThePointerEnteringIsWide() throws {
        // The tracking area can miss a pointer already over the strip when
        // the scroller appears; pressing the narrow knob widens it.
        let (scroller, window) = makeScroller()
        defer { window.close() }
        scroller.flash()
        scroller.beginTracking()
        try assertWideKnobAtTheMiddle(scroller, "pressed")
        scroller.endTracking()
        try assertWideKnobAtTheMiddle(scroller, "released: fading out as it was")
    }

    func testAScrollerHiddenUnderThePointerShowsTheNarrowKnobAgain() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        scroller.update(.init(offset: 0, history: 0, rows: 24))
        XCTAssertTrue(scroller.isHidden, "no history")
        XCTAssertEqual(try render(scroller, ignoringOpacity: true).maxAlpha, 0, "nothing drawn without history")
        scroller.update(Self.middle)
        scroller.flash()
        let drawn = try render(scroller, ignoringOpacity: true)
        try assertNarrowKnob(drawn, scroller, "the pointer is not assumed to be there")
    }

    func testTheKnobFollowsTheAppearanceAndTheScrollersSize() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        try assertWideKnobAtTheMiddle(scroller, "light")
        // Dark appearance: AppKit's light knob, white at half opacity.
        window.appearance = NSAppearance(named: .darkAqua)
        let x = hoverPoint(scroller).x, y = scroller.rect(for: .knob).midY
        let dark = try render(scroller).pixel(x, y)
        XCTAssertGreaterThan(min(dark.red, dark.green, dark.blue), 0.6, "a light knob: \(dark)")
        XCTAssertGreaterThan(dark.alpha, 0.33)
        window.appearance = NSAppearance(named: .aqua)
        XCTAssertTrue(try render(scroller).isKnob(x, y), "dark again in light appearance")

        // The terminal view sizes the scroller to its height.
        scroller.frame.size.height = 300
        let drawn = try render(scroller)
        let knob = try XCTUnwrap(drawn.knob(atX: x), "at the new size")
        XCTAssertEqual((knob.top + knob.bottom) / 2, scroller.rect(for: .knobSlot).midY, accuracy: 2)
        XCTAssertTrue(drawn.isSlot(x, scroller.rect(for: .knobSlot).maxY - 4), "the slot to the new bottom")
    }

    // MARK: The knob against the theme (#558)

    /// Dracula's and Solarized Light's backgrounds, as the core reports
    /// them.
    private static let darkTheme = TesseraSession.Appearance.Color(
        red: 0x28 / 255, green: 0x2a / 255, blue: 0x36 / 255, alpha: 1)
    private static let lightTheme = TesseraSession.Appearance.Color(
        red: 0xfd / 255, green: 0xf6 / 255, blue: 0xe3 / 255, alpha: 1)

    /// The knob's colour as drawn: `light` (white-ish, at least a third
    /// opaque), `dark` (``Rendering/isKnob(_:_:)``) or `nil` for neither.
    private func knobShade(_ drawn: Rendering, _ x: CGFloat, _ y: CGFloat) -> String? {
        if drawn.isLightKnob(x, y) { return "light" }
        return drawn.isKnob(x, y) ? "dark" : nil
    }

    /// AppKit's slot at its lower end, under no knob: `light` for its
    /// light-appearance slot (white at three quarters), `dark` for its
    /// dark-appearance one (light grey at 15 %, which a dark background
    /// shows through), `nil` where nothing is drawn.
    private func slotShade(_ drawn: Rendering, _ scroller: TerminalScroller) -> String? {
        let p = drawn.pixel(hoverPoint(scroller).x, scroller.rect(for: .knobSlot).maxY - 4)
        guard p.alpha > 0.05 else { return nil }
        return p.alpha > 0.5 ? "light" : "dark"
    }

    /// What AppKit itself draws for the scroller (its slot and knob, asked
    /// to draw as AppKit does), unlike ``render(_:ignoringOpacity:)``,
    /// whose layer rendering has no legacy knob.
    private func appKitsDrawing(_ scroller: TerminalScroller) throws -> Rendering {
        let width = Int(scroller.bounds.width), height = Int(scroller.bounds.height)
        let context = try XCTUnwrap(
            CGContext(
                data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                space: try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB)),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
        scroller.effectiveAppearance.performAsCurrentDrawingAppearance {
            scroller.drawKnobSlot(in: scroller.rect(for: .knobSlot), highlight: false)
            scroller.drawKnob()
        }
        NSGraphicsContext.restoreGraphicsState()
        let data = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
        return Rendering(
            width: width, height: height, bytes: Array(UnsafeBufferPointer(start: data, count: width * height * 4)))
    }

    func testTheKnobIsLightOnADarkThemeAndDarkOnALightOneWhateverTheAppearance() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        let x = hoverPoint(scroller).x, y = scroller.rect(for: .knob).midY
        let light = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(light, x, y), "dark", "no theme yet, light appearance: AppKit's dark knob")
        XCTAssertEqual(slotShade(light, scroller), "light", "and AppKit's light slot")

        // A dark theme in light appearance: the light knob, on a dark slot.
        scroller.themeBackground = Self.darkTheme
        var drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "light", "a dark theme in light appearance: \(drawn.pixel(x, y))")
        XCTAssertEqual(slotShade(drawn, scroller), "dark", "its slot")
        // Narrow, after the pointer left and a scroll showed it again.
        try exit(scroller)
        clock += 10
        scroller.flash()
        drawn = try render(scroller, ignoringOpacity: true)
        let narrow = scroller.rect(for: .knob)
        XCTAssertEqual(knobShade(drawn, narrow.maxX - 1.5, narrow.midY), "light", "the narrow knob")
        try enter(scroller)

        // A light theme in dark appearance: the dark knob, on a light slot.
        window.appearance = NSAppearance(named: .darkAqua)
        drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "light", "the appearance alone changes nothing")
        XCTAssertEqual(slotShade(drawn, scroller), "dark")
        scroller.themeBackground = Self.lightTheme
        drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "dark", "a light theme in dark appearance: \(drawn.pixel(x, y))")
        XCTAssertEqual(slotShade(drawn, scroller), "light", "its slot")
        window.appearance = NSAppearance(named: .aqua)
        drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "dark", "light appearance again: still dark")
        XCTAssertEqual(slotShade(drawn, scroller), "light")

        // Back to a dark theme: light again.
        scroller.themeBackground = Self.darkTheme
        drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "light", "a dark theme again")
        XCTAssertEqual(slotShade(drawn, scroller), "dark")

        // Legacy style is AppKit's knob on a strip of the appearance's
        // colour, whatever the theme, and overlay style again the theme's.
        let (plain, plainWindow) = makeScroller(style: .legacy)
        defer { plainWindow.close() }
        try exit(scroller)
        scroller.preferredStyleOverride = .legacy
        plain.frame = scroller.frame
        // AppKit's legacy knob on macOS 27 draws the same whatever the
        // style, so the style itself is pinned too.
        XCTAssertEqual(scroller.knobStyle, .default, "legacy: the appearance's knob style")
        let legacy = try appKitsDrawing(scroller)
        XCTAssertGreaterThan(legacy.maxAlpha, 0, "AppKit's legacy knob")
        XCTAssertEqual(legacy.bytes, try appKitsDrawing(plain).bytes, "legacy: AppKit's knob, as without a theme")
        scroller.preferredStyleOverride = .overlay
        try enter(scroller)
        drawn = try render(scroller, ignoringOpacity: true)
        XCTAssertEqual(knobShade(drawn, x, y), "light", "overlay again: the dark theme's")
        XCTAssertEqual(slotShade(drawn, scroller), "dark")
    }

    /// The knob is dark on a background whose WCAG relative luminance is
    /// at least 0.5 (sRGB grey 0.7354), light below. Each colour pins one
    /// part of the formula: the linearisation (grey 0.6, 0.32 linear), the
    /// green weight (pure green, 0.715, where an average gives 0.33), the
    /// red weight (red with green 0.7: 0.533, 0.393 with blue's weight) and
    /// the blue one (blue with green 0.75: 0.446, 0.586 with red's). Alpha
    /// is not looked at.
    func testTheKnobStyleComesFromTheBackgroundsRelativeLuminance() {
        typealias Color = TesseraSession.Appearance.Color
        let cases: [(Color, Bool, String)] = [
            (Self.darkTheme, false, "Dracula"),
            (Self.lightTheme, true, "Solarized Light"),
            (Color(red: 0, green: 0, blue: 0, alpha: 1), false, "black"),
            (Color(red: 1, green: 1, blue: 1, alpha: 1), true, "white"),
            (Color(red: 0.73, green: 0.73, blue: 0.73, alpha: 1), false, "grey 0.73: 0.492"),
            (Color(red: 0.74, green: 0.74, blue: 0.74, alpha: 1), true, "grey 0.74: 0.507"),
            (Color(red: 0.6, green: 0.6, blue: 0.6, alpha: 1), false, "grey 0.6, linearised"),
            (Color(red: 0, green: 1, blue: 0, alpha: 1), true, "green"),
            (Color(red: 1, green: 0.7, blue: 0, alpha: 1), true, "red's weight"),
            (Color(red: 0, green: 0.75, blue: 1, alpha: 1), false, "blue's weight"),
            (Color(red: 0x28 / 255, green: 0x2a / 255, blue: 0x36 / 255, alpha: 0.3), false, "translucent Dracula"),
            (Color(red: 1, green: 1, blue: 1, alpha: 0), true, "transparent white"),
        ]
        for (color, light, what) in cases {
            XCTAssertEqual(TerminalScroller.isLightBackground(color), light, what)
        }
    }

    func testTheKnobMovesWithoutAnAnimation() throws {
        // The knob tracks the view at once; the scroller's own opacity is
        // what fades, and the pointer's widening is the only animation of
        // the knob (#559), which the moves neither restart nor end.
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        let widening = try overlayAnimations(scroller)
        XCTAssertFalse(widening.isEmpty, "the pointer widens the knob")
        // Layers animate implicitly only once committed.
        CATransaction.flush()
        scroller.update(.init(offset: 2000, history: 10_000, rows: 24))
        scroller.frame.size.height = 300
        try exit(scroller)
        scroller.flash()
        let after = try overlayAnimations(scroller)
        XCTAssertEqual(after.keys.sorted(), widening.keys.sorted(), "only the scroller's fade and the widening animate")
        for (key, animation) in widening {
            XCTAssertTrue(after[key] === animation, "\(key): the widening goes on")
        }
    }

    // MARK: The widening (#559)

    /// The overlay layers' animations, keyed by layer (`knob`, `fill`,
    /// `rim`, `slot`) and animation key.
    private func overlayAnimations(_ scroller: TerminalScroller) throws -> [String: CAAnimation] {
        var found: [String: CAAnimation] = [:]
        let layers = try knobLayers(scroller)
        for (name, layer) in [("knob", layers.knob), ("fill", layers.fill), ("rim", layers.rim), ("slot", layers.slot)] {
            for key in layer.animationKeys() ?? [] {
                found["\(name) \(key)"] = layer.animation(forKey: key)
            }
        }
        return found
    }

    /// The overlay knob layer, its fill and its rim (a 1 pt border), and
    /// the slot layer.
    private func knobLayers(
        _ scroller: TerminalScroller
    ) throws -> (knob: CALayer, fill: CALayer, rim: CALayer, slot: CALayer) {
        let layers = try overlayLayers(scroller)
        let knob = try XCTUnwrap(layers.first { $0.zPosition == 2 }, "the knob layer")
        let slot = try XCTUnwrap(layers.first { $0.zPosition == 1 }, "the slot layer")
        let sublayers = knob.sublayers ?? []
        let rim = try XCTUnwrap(sublayers.first { $0.borderWidth > 0 }, "the rim")
        let fill = try XCTUnwrap(sublayers.first { $0.borderWidth == 0 }, "the fill")
        return (knob, fill, rim, slot)
    }

    /// AppKit's knob or slot rect `rect` wide or narrow, as the scroller
    /// draws it: at its right edge, wide from as far from the left edge as
    /// AppKit insets it from the right, narrow at most half that.
    private func drawn(_ rect: NSRect, wide: Bool, in scroller: TerminalScroller) -> NSRect {
        let left = min(max(scroller.bounds.maxX - rect.maxX, 0), rect.minX)
        let full = rect.maxX - left
        let width = wide ? full : min(rect.width, (full / 2).rounded(.up))
        return NSRect(x: rect.maxX - width, y: rect.minY, width: width, height: rect.height)
    }

    /// The additive widening animation of `keyPath` on `layer`.
    private func widening(_ layer: CALayer, _ keyPath: String) -> CABasicAnimation? {
        (layer.animationKeys() ?? []).lazy.compactMap { layer.animation(forKey: $0) as? CABasicAnimation }
            .first { $0.keyPath == keyPath }
    }

    /// How far the widening of `keyPath` starts from the model value (its
    /// additive `fromValue`), or 0 without one.
    private func startOffset(_ layer: CALayer, _ keyPath: String) -> CGFloat {
        (widening(layer, keyPath)?.fromValue as? NSNumber).map { CGFloat($0.doubleValue) } ?? 0
    }

    /// The frame `layer` shows on the widening's first frame: its model
    /// frame offset by the additive animations of x and width.
    private func startFrame(_ layer: CALayer) -> NSRect {
        let width = layer.bounds.width + startOffset(layer, "bounds.size.width")
        let midX = layer.position.x + startOffset(layer, "position.x")
        return NSRect(x: midX - width / 2, y: layer.frame.minY, width: width, height: layer.frame.height)
    }

    private func controlPoints(_ function: CAMediaTimingFunction?) -> [Float] {
        guard let function else { return [] }
        return (0..<4).flatMap { index -> [Float] in
            var point: [Float] = [0, 0]
            function.getControlPoint(at: index, values: &point)
            return point
        }
    }

    func testThePointerWidensTheKnobAndItsSlotOverAppKitsDurationAndCurve() throws {
        // AppKit's, measured on macOS 27.2 (build 26B123).
        XCTAssertEqual(TerminalScroller.knobExpandDuration, 0.125)
        let easeInEaseOut: [Float] = [0, 0, 0.42, 0, 0.58, 1, 1, 1]
        XCTAssertEqual(controlPoints(TerminalScroller.knobExpandTimingFunction), easeInEaseOut)

        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        let knob = scroller.rect(for: .knob), slot = scroller.rect(for: .knobSlot)
        XCTAssertEqual(narrowKnob, drawn(knob, wide: false, in: scroller).insetBy(dx: -1, dy: -1), "narrow")
        XCTAssertTrue(layers.slot.isHidden, "no slot behind the narrow knob")
        try enter(scroller)

        // The model frames are the end frames at once.
        let wideKnob = drawn(knob, wide: true, in: scroller)
        XCTAssertEqual(layers.knob.frame, wideKnob.insetBy(dx: -1, dy: -1), "wide at once")
        XCTAssertEqual(layers.slot.frame, drawn(slot, wide: true, in: scroller), "the wide slot at once")
        XCTAssertFalse(layers.slot.isHidden)
        XCTAssertEqual(layers.fill.frame, CGRect(origin: .zero, size: wideKnob.size).offsetBy(dx: 1, dy: 1))
        XCTAssertEqual(layers.fill.cornerRadius, wideKnob.width / 2)
        XCTAssertEqual(layers.rim.frame.size, layers.knob.bounds.size)
        XCTAssertEqual(layers.rim.cornerRadius, wideKnob.width / 2 + 1)

        // Every width animation is AppKit's, additive, ending at the model.
        var count = 0
        for layer in [layers.knob, layers.fill, layers.rim, layers.slot] {
            for keyPath in ["position.x", "bounds.size.width"] {
                let animation = try XCTUnwrap(widening(layer, keyPath), "\(keyPath) of \(layer) widens")
                XCTAssertEqual(animation.duration, TerminalScroller.knobExpandDuration)
                XCTAssertEqual(controlPoints(animation.timingFunction), easeInEaseOut, keyPath)
                XCTAssertTrue(animation.isAdditive, keyPath)
                XCTAssertEqual((animation.toValue as? NSNumber)?.doubleValue, 0, "\(keyPath) ends at the model")
                XCTAssertEqual(animation.beginTime, 0, "in lockstep: begun at the same commit")
                count += 1
            }
        }
        XCTAssertEqual(count, 8)
        // From the narrow frames: the knob as it was, the slot at its
        // width from the first frame, in the knob's column.
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "the knob starts narrow")
        let slotStart = startFrame(layers.slot)
        XCTAssertEqual(slotStart, drawn(slot, wide: false, in: scroller), "the slot starts narrow")
        XCTAssertEqual(slotStart.minX, narrowKnob.minX + 1, "in the knob's column")
        XCTAssertEqual(slotStart.width, narrowKnob.width - 2)
        // The fill and rim stay centred and round.
        for (layer, inset) in [(layers.fill, CGFloat(1)), (layers.rim, 0)] {
            let width = startOffset(layer, "bounds.size.width")
            XCTAssertEqual(width, narrowKnob.width - layers.knob.frame.width)
            XCTAssertEqual(startOffset(layer, "position.x"), width / 2, "centred")
            let radius = layer.cornerRadius + startOffset(layer, "cornerRadius")
            XCTAssertEqual(radius, (narrowKnob.width - 2 * inset) / 2, "round at the narrow width")
            XCTAssertEqual(widening(layer, "cornerRadius")?.duration, TerminalScroller.knobExpandDuration)
        }
        // The wide knob grabs from the first frame (G6).
        XCTAssertEqual(scroller.testPart(scroller.convert(hoverPoint(scroller), to: nil)), .knob)
    }

    func testAFlashFromFadedOutNarrowsAtOnceAndEndsAWideningInFlight() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        try enter(scroller)
        try exit(scroller)
        clock += 10
        XCTAssertFalse(scroller.isRevealed, "faded out")
        XCTAssertFalse(try overlayAnimations(scroller).isEmpty, "the widening is still on the layers")
        // Committed, so that an implicit animation would show.
        CATransaction.flush()
        scroller.flash()
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "narrowed without an animation")
        XCTAssertEqual(layers.knob.frame, narrowKnob, "narrow")
        XCTAssertTrue(layers.slot.isHidden)

        // Hidden without history while widening, then shown by a scroll.
        try enter(scroller)
        XCTAssertFalse(try overlayAnimations(scroller).isEmpty)
        scroller.update(.init(offset: 0, history: 0, rows: 24))
        scroller.update(Self.middle)
        scroller.flash()
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "narrowed without an animation")
        XCTAssertEqual(layers.knob.frame, narrowKnob)
        XCTAssertTrue(layers.slot.isHidden)

        // Widened again, it starts from the narrow knob on screen.
        try enter(scroller)
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "no jump")
        XCTAssertEqual(startFrame(layers.slot).width, narrowKnob.width - 2)
    }

    /// Hidden without history while wide, as when the alternate screen
    /// opens under the pointer, the knob is narrow once history is back
    /// (#634) with no scroll to narrow it, and the next hover widens it
    /// with its animation.
    func testHiddenWhileWideTheKnobComesBackNarrowAndTheNextHoverWidensIt() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        try enter(scroller)
        XCTAssertGreaterThan(layers.knob.frame.width, narrowKnob.width, "the pointer widened it")
        CATransaction.flush()

        scroller.update(.init(offset: 0, history: 0, rows: 24))
        XCTAssertTrue(scroller.isHidden)
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "the widening ended")
        scroller.update(Self.middle)
        XCTAssertFalse(scroller.isHidden)
        XCTAssertEqual(layers.knob.frame, narrowKnob, "narrow before any scroll")
        XCTAssertTrue(layers.slot.isHidden, "no slot behind it")
        XCTAssertNotEqual(
            scroller.testPart(scroller.convert(hoverPoint(scroller), to: nil)), .knob,
            "the narrow knob's hit area")
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [])

        // No scroll in between, which would narrow it on its own.
        try enter(scroller)
        XCTAssertEqual(
            layers.knob.frame, drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1),
            "the next hover widens it")
        XCTAssertFalse(layers.slot.isHidden)
        XCTAssertNotNil(widening(layers.knob, "bounds.size.width"), "with its animation")
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "from the narrow knob")
    }

    /// A knob held across a hide stays wide while held (ADR 0032): shown
    /// again with the button still down, it is the wide knob over its
    /// slot. Released while hidden, it comes back narrow, and the next
    /// hover widens it with its animation (#634).
    func testAKnobHeldAcrossAHideStaysWideUntilReleased() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        let wideKnob = drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1)
        scroller.beginTracking()
        XCTAssertEqual(layers.knob.frame, wideKnob, "held: wide")
        scroller.update(.init(offset: 0, history: 0, rows: 24))
        XCTAssertTrue(scroller.isHidden)
        scroller.update(Self.middle)
        XCTAssertEqual(layers.knob.frame, wideKnob, "still held: wide")
        XCTAssertFalse(layers.slot.isHidden, "over its slot")
        scroller.endTracking()
        XCTAssertEqual(layers.knob.frame, wideKnob, "released: wide through the fade")

        // Held into a hide and released while hidden.
        clock += 10
        scroller.flash()
        XCTAssertEqual(layers.knob.frame, narrowKnob)
        scroller.beginTracking()
        CATransaction.flush()
        scroller.update(.init(offset: 0, history: 0, rows: 24))
        scroller.endTracking()
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "the widening ended")
        scroller.update(Self.middle)
        XCTAssertEqual(layers.knob.frame, narrowKnob, "released while hidden: narrow")
        XCTAssertTrue(layers.slot.isHidden)
        try enter(scroller)
        XCTAssertEqual(layers.knob.frame, wideKnob, "the next hover widens it")
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "with its animation, from the narrow knob")
    }

    /// The pointer leaves the wide knob and its fade ends with no scroll
    /// after it (#695): the knob narrows then, without an animation, as
    /// AppKit's does, and the next hover widens it with a new animation
    /// from the narrow knob. A scroll during the fade moves its end, and a
    /// pointer back before the end keeps the knob wide.
    func testWhenTheFadeAfterTheHoverEndsTheKnobNarrowsAndTheNextHoverWidensIt() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        let wideKnob = drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1)
        let hold = TerminalScroller.holdDuration, fade = TerminalScroller.fadeDuration
        try enter(scroller)
        let first = try XCTUnwrap(widening(layers.knob, "bounds.size.width"), "the first hover's widening")
        CATransaction.flush()
        try exit(scroller)
        XCTAssertEqual(layers.knob.frame, wideKnob, "wide through the fade")
        let end = try XCTUnwrap(hides.first, "the fade's end is scheduled")
        XCTAssertEqual(end.delay, hold + fade, accuracy: 1e-9, "at the fade's end")

        // A scroll halfway through the fade moves its end.
        clock += (hold + fade) / 2
        scroller.flash()
        hides.removeFirst()
        clock += (hold + fade) / 2
        end.hide()
        XCTAssertTrue(scroller.isRevealed, "the scroll's flash is still on")
        XCTAssertEqual(layers.knob.frame, wideKnob, "wide until the moved end")
        // Committed, so that an implicit animation would show.
        CATransaction.flush()
        runScheduledHides()
        XCTAssertFalse(scroller.isRevealed, "faded out")
        XCTAssertEqual(scroller.alphaValue, 0)
        XCTAssertEqual(layers.knob.frame, narrowKnob, "narrow once the fade ended")
        XCTAssertTrue(layers.slot.isHidden, "no slot behind it")
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "narrowed without an animation")

        try enter(scroller)
        let next = try XCTUnwrap(widening(layers.knob, "bounds.size.width"), "the next hover animates")
        XCTAssertFalse(next === first, "a new widening")
        XCTAssertEqual(layers.knob.frame, wideKnob)
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "from the narrow knob")

        // Back under the pointer before the fade ends: the end it had
        // scheduled leaves the knob wide.
        try exit(scroller)
        clock += hold
        try enter(scroller)
        runScheduledHides()
        XCTAssertTrue(scroller.isRevealed)
        XCTAssertEqual(layers.knob.frame, wideKnob, "the pointer is back: wide")
        XCTAssertFalse(layers.slot.isHidden)
    }

    /// A knob dragged and released with the pointer off the strip fades
    /// wide, then narrows when the fade ends (#695), and the next hover
    /// widens it with a new animation. A knob held again before the fade
    /// ends stays wide while held.
    func testWhenTheFadeAfterADragReleasedOffTheStripEndsTheKnobNarrows() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        let wideKnob = drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1)
        scroller.beginTracking()
        let first = try XCTUnwrap(widening(layers.knob, "bounds.size.width"), "the press's widening")
        scroller.endTracking()
        XCTAssertEqual(layers.knob.frame, wideKnob, "released: wide through the fade")
        CATransaction.flush()
        runScheduledHides()
        XCTAssertFalse(scroller.isRevealed, "faded out")
        XCTAssertEqual(layers.knob.frame, narrowKnob, "narrow once the fade ended")
        XCTAssertTrue(layers.slot.isHidden)
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "narrowed without an animation")

        try enter(scroller)
        let next = try XCTUnwrap(widening(layers.knob, "bounds.size.width"), "the next hover animates")
        XCTAssertFalse(next === first, "a new widening")
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "from the narrow knob")
        try exit(scroller)
        runScheduledHides()
        XCTAssertEqual(layers.knob.frame, narrowKnob)

        // Pressed again from faded out, released, and held again during
        // the fade: held past the end it scheduled, it stays wide.
        scroller.beginTracking()
        scroller.endTracking()
        clock += TerminalScroller.holdDuration
        scroller.beginTracking()
        runScheduledHides()
        XCTAssertTrue(scroller.isRevealed, "held")
        XCTAssertEqual(layers.knob.frame, wideKnob, "held across the fade's end: wide")
        XCTAssertFalse(layers.slot.isHidden)
        scroller.endTracking()
        runScheduledHides()
        XCTAssertEqual(layers.knob.frame, narrowKnob, "released, the fade ended: narrow")
    }

    /// The #634 adversary's hover after a fade (ADV-B), verbatim: the
    /// pointer comes back once the fade has ended but before the hide
    /// scheduled for its end has run (the main queue runs it late), and
    /// the hover still widens the knob with a new animation (#695).
    func testAHoverAfterTheFadeEndedBeforeItsHideRanAnimatesAnew() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        try enter(scroller)
        let first = widening(layers.knob, "bounds.size.width")
        CATransaction.flush()
        try exit(scroller)
        clock += 5
        XCTAssertFalse(scroller.isRevealed)
        try enter(scroller)
        let next = widening(layers.knob, "bounds.size.width")
        XCTAssertTrue(next != nil && next !== first, "a hover after the fade animates anew")
    }

    /// The #634 adversary's drag released off the strip (ADV-G),
    /// verbatim: its fade ended, its hide not yet run, the next hover
    /// widens the knob with a new animation (#695).
    func testAHoverAfterADragsFadeEndedBeforeItsHideRanAnimatesAnew() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        scroller.beginTracking()
        let first = widening(layers.knob, "bounds.size.width")
        scroller.endTracking()
        clock += 5
        XCTAssertFalse(scroller.isRevealed)
        try enter(scroller)
        let next = widening(layers.knob, "bounds.size.width")
        XCTAssertTrue(next != nil && next !== first, "a hover after the drag's fade animates anew")
    }

    /// A press after the fade ended, before its hide ran, widens the knob
    /// with a new animation from the narrow knob, as a hover does (#695).
    func testAPressAfterTheFadeEndedBeforeItsHideRanAnimatesAnew() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        try enter(scroller)
        let first = widening(layers.knob, "bounds.size.width")
        CATransaction.flush()
        try exit(scroller)
        clock += 5
        XCTAssertFalse(scroller.isRevealed)
        scroller.beginTracking()
        let next = widening(layers.knob, "bounds.size.width")
        XCTAssertTrue(next != nil && next !== first, "a press after the fade animates anew")
        XCTAssertEqual(startFrame(layers.knob), narrowKnob, "from the narrow knob")
        scroller.endTracking()
    }

    /// On its own clock and main-queue hide, as in the app, the knob the
    /// pointer widened narrows once the fade after it left has ended
    /// (#695), and not before.
    func testOnItsOwnClockTheKnobNarrowsWhenTheFadeEnds() throws {
        let (scroller, window) = makeScroller(realClock: true)
        defer { window.close() }
        let layers = try knobLayers(scroller)
        let narrowKnob = layers.knob.frame
        try enter(scroller)
        XCTAssertGreaterThan(layers.knob.frame.width, narrowKnob.width, "the pointer widened it")
        let left = CACurrentMediaTime()
        try exit(scroller)
        let deadline = Date().addingTimeInterval(30)
        while layers.knob.frame != narrowKnob, Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.05))
        }
        let narrowed = CACurrentMediaTime() - left
        XCTAssertEqual(layers.knob.frame, narrowKnob, "narrow once the fade ended")
        XCTAssertGreaterThanOrEqual(
            narrowed, TerminalScroller.holdDuration + TerminalScroller.fadeDuration, "not before the fade ended")
        XCTAssertFalse(scroller.isRevealed)
        XCTAssertTrue(hides.isEmpty, "the scroller's own scheduling")
    }

    func testUnderReduceMotionThePointerWidensTheKnobAtOnce() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        scroller.reducesMotion = true
        try enter(scroller)
        let layers = try knobLayers(scroller)
        XCTAssertEqual(try overlayAnimations(scroller).keys.sorted(), [], "no animation")
        XCTAssertEqual(
            layers.knob.frame, drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1))
        XCTAssertEqual(layers.slot.frame, drawn(scroller.rect(for: .knobSlot), wide: true, in: scroller))
        XCTAssertFalse(layers.slot.isHidden)
    }

    func testAFadeAndADragDuringTheWideningLeaveItRunningToTheWideKnob() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        CATransaction.flush()
        let widening = try overlayAnimations(scroller)
        XCTAssertFalse(widening.isEmpty, "widening")
        let layers = try knobLayers(scroller)
        func wideKnob() -> NSRect { drawn(scroller.rect(for: .knob), wide: true, in: scroller).insetBy(dx: -1, dy: -1) }
        func assertRunning(_ message: String, line: UInt = #line) throws {
            let now = try overlayAnimations(scroller)
            XCTAssertEqual(now.keys.sorted(), widening.keys.sorted(), message, line: line)
            for (key, animation) in widening {
                XCTAssertTrue(now[key] === animation, "\(message): \(key) neither restarted nor ended", line: line)
            }
            XCTAssertEqual(layers.knob.frame, wideKnob(), "\(message): the wide knob", line: line)
        }

        // A knob drag (G2): pressed, moved by AppKit, which sends the action.
        let start = layers.knob.frame
        scroller.beginTracking()
        try assertRunning("pressed")
        scroller.doubleValue = 0.8
        XCTAssertTrue(scroller.sendAction(scroller.action, to: scroller.target))
        try assertRunning("dragged")
        XCTAssertGreaterThan(abs(layers.knob.frame.minY - start.minY), 20, "at the dragged position")
        XCTAssertEqual(layers.knob.frame.width, start.width, "as wide")
        scroller.endTracking()
        try assertRunning("released")

        // The fade after the pointer left (G4, D4) is the scroller's own.
        try exit(scroller)
        XCTAssertNotNil(try flashOpacity(scroller, after: 0), "fading")
        try assertRunning("fading")
    }

    // MARK: Dragging

    func testDraggingTheWideKnobMovesTheViewAndTheKnobFollowsThePointer() throws {
        // A short history: the knob is longer than AppKit's minimum, so
        // the output the driver adds while it is held shows in its length
        // once released. While held, AppKit's tracking loop keeps its knob
        // whatever value and proportion are set (#559: with `freezesKnob`
        // always false the drawn knob still stays put, so that rule is
        // pinned by `TerminalScrollTests`, not here).
        let history: UInt64 = 200
        let (scroller, window) = makeScroller(.init(offset: history / 2, history: history, rows: 24))
        defer { window.close() }
        var targets: [TesseraSession.ScrollTarget] = []
        scroller.onScroll = { targets.append($0) }
        try enter(scroller)
        let start = hoverPoint(scroller)
        let before = try XCTUnwrap(try render(scroller).knob(atX: start.x))
        XCTAssertGreaterThan(before.bottom - before.top, minimumKnobLength(scroller) + 8, "longer than the minimum")
        let driver = KnobDragDriver(scroller: scroller, window: window, start: start, distance: 40)
        driver.render = { [unowned self] in try? self.render(scroller).knob(atX: start.x) }
        let knobWidth: () -> CGFloat? = { [unowned self] in
            guard let knob = try? self.render(scroller).knob(atY: scroller.rect(for: .knob).midY) else { return nil }
            return knob.right - knob.left
        }
        let wideWidth = try XCTUnwrap(knobWidth())
        driver.knobWidth = knobWidth
        driver.onScroll = { targets }
        // AppKit's tracking loop runs until the release the driver posts.
        let timer = Timer(
            timeInterval: 0.05, target: driver, selector: #selector(KnobDragDriver.fire(_:)), userInfo: nil,
            repeats: true)
        RunLoop.main.add(timer, forMode: .common)
        RunLoop.main.add(timer, forMode: .eventTracking)
        scroller.mouseDown(with: try driver.event(.leftMouseDown, at: start))
        timer.invalidate()

        let moved = try XCTUnwrap(driver.moved, "the knob is drawn during the drag")
        XCTAssertEqual(moved.top - before.top, 40, accuracy: 1.5, "the knob follows the pointer")
        let value = try XCTUnwrap(driver.value)
        let expected = history - UInt64((value * Double(history)).rounded())
        XCTAssertGreaterThan(value, 0.55, "dragged towards the live screen")
        XCTAssertEqual(driver.targetsAfterDrag.last, .offset(expected), "the view goes to the knob's line")
        XCTAssertEqual(driver.afterOutput?.top, moved.top, "output during the drag does not move the drawn knob")
        XCTAssertEqual(driver.afterOutput?.bottom, moved.bottom)
        XCTAssertEqual(
            try XCTUnwrap(driver.widthAfterExit), wideWidth, accuracy: 1,
            "still wide with the pointer off the strip during the drag")
        XCTAssertFalse(scroller.isTracking)
        // Released, the knob shows the position last read: the output's.
        let last = try XCTUnwrap(scroller.position)
        XCTAssertEqual(last.history, history + 500)
        XCTAssertEqual(scroller.doubleValue, Double(last.history - last.offset) / Double(last.history), accuracy: 1e-9)
        let column = scroller.rect(for: .knob).midX - 3
        let released = try XCTUnwrap(try render(scroller, ignoringOpacity: true).knob(atX: column), "the released knob")
        XCTAssertEqual(released.top, scroller.rect(for: .knob).minY, accuracy: 2, "where the position puts it")
        XCTAssertLessThan(released.bottom - released.top, before.bottom - before.top - 8, "shorter: more history")
    }

    // MARK: Layout

    /// The overlay knob and slot layers (the ones above AppKit's own).
    private func overlayLayers(_ scroller: TerminalScroller) throws -> [CALayer] {
        try XCTUnwrap(scroller.layer).sublayers?.filter { $0.zPosition > 0 } ?? []
    }

    /// Too short for AppKit to give it a knob, the scroller shows neither
    /// knob nor slot, even expanded; given its height back, both return.
    func testAScrollerTooShortForAKnobShowsNeitherKnobNorSlot() throws {
        let (scroller, window) = makeScroller()
        defer { window.close() }
        try enter(scroller)
        let full = scroller.frame.size
        XCTAssertFalse(try overlayLayers(scroller).isEmpty)
        XCTAssertTrue(try overlayLayers(scroller).allSatisfy { !$0.isHidden }, "expanded: knob and slot")

        scroller.setFrameSize(NSSize(width: full.width, height: 12))
        print("SCROLLER short: parts \(scroller.usableParts.rawValue), knob \(scroller.rect(for: .knob))")
        XCTAssertNotEqual(scroller.usableParts, .allScrollerParts, "AppKit gives it no knob")
        XCTAssertTrue(try overlayLayers(scroller).allSatisfy(\.isHidden), "neither is shown")

        scroller.setFrameSize(full)
        XCTAssertEqual(scroller.usableParts, .allScrollerParts)
        XCTAssertTrue(try overlayLayers(scroller).allSatisfy { !$0.isHidden }, "both are back")

        // At AppKit's boundary: 32 pt on macOS 27 (#559, E1).
        let shortest = try XCTUnwrap(
            (1...100).map(CGFloat.init).first { height in
                scroller.setFrameSize(NSSize(width: full.width, height: height))
                return scroller.usableParts == .allScrollerParts
            }, "AppKit gives a knob from some height")
        if ProcessInfo.processInfo.operatingSystemVersion.majorVersion >= 27 {
            XCTAssertEqual(shortest, 32, "AppKit's shortest overlay scroller with a knob")
        }
        scroller.setFrameSize(NSSize(width: full.width, height: shortest - 1))
        XCTAssertTrue(try overlayLayers(scroller).allSatisfy(\.isHidden), "\(shortest - 1) pt: neither is shown")
        scroller.setFrameSize(NSSize(width: full.width, height: shortest))
        XCTAssertTrue(try overlayLayers(scroller).allSatisfy { !$0.isHidden }, "\(shortest) pt: both are shown")
    }

    /// Moved to a screen of another scale, the drawn knob goes where AppKit
    /// now puts its own: AppKit rounds the knob's rect to the backing scale.
    /// The change is called as AppKit calls it; the window reports the scale.
    /// A position whose knob AppKit rounds differently at 1x and 2x, and
    /// its knob rect at 2x; the window is left at 1x.
    private func positionRoundedDifferently(
        _ scroller: TerminalScroller, in window: ScaledWindow
    ) throws -> (position: TesseraSession.ScrollPosition, rect: NSRect) {
        var found: (position: TesseraSession.ScrollPosition, rect: NSRect)?
        for offset in stride(from: UInt64(3000), to: 7000, by: 13) {
            let position = TesseraSession.ScrollPosition(offset: offset, history: 10_000, rows: 24)
            window.scale = 2
            scroller.update(position)
            let fine = scroller.rect(for: .knob)
            window.scale = 1
            scroller.update(position)
            if scroller.rect(for: .knob) != fine {
                found = (position, fine)
                break
            }
        }
        return try XCTUnwrap(found, "AppKit rounds the knob to the backing scale")
    }

    func testAChangeOfBackingScaleMovesTheKnobToAppKitsRect() throws {
        let window = ScaledWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: Self.height), styleMask: [.titled],
            backing: .buffered, defer: false)
        let (scroller, _) = makeScroller(in: window)
        defer { window.close() }
        let (position, fine) = try positionRoundedDifferently(scroller, in: window)
        // Laid out at 1x: an unchanged position would lay nothing out.
        scroller.update(.init(offset: position.offset + 1, history: position.history, rows: position.rows))
        scroller.update(position)
        let knob = try XCTUnwrap(try overlayLayers(scroller).first { $0.zPosition == 2 })
        XCTAssertNotEqual(knob.frame.minY, fine.minY - 1, "at 1x, not yet where 2x puts it")

        window.scale = 2
        scroller.viewDidChangeBackingProperties()

        XCTAssertEqual(knob.frame.minY, fine.minY - 1, "the rim is 1 pt outside AppKit's knob")
        XCTAssertEqual(knob.frame.height, fine.height + 2)
    }

    /// A change of backing scale while the knob widens (E5): the layers
    /// are at once where AppKit puts its knob and slot at the new scale,
    /// and the widening, made of differences from the model, ends there.
    func testAChangeOfBackingScaleDuringTheWideningEndsAtAppKitsNewRect() throws {
        let window = ScaledWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: Self.height), styleMask: [.titled],
            backing: .buffered, defer: false)
        let (scroller, _) = makeScroller(in: window)
        defer { window.close() }
        let (position, fine) = try positionRoundedDifferently(scroller, in: window)
        scroller.update(.init(offset: position.offset + 1, history: position.history, rows: position.rows))
        scroller.update(position)
        try enter(scroller)
        let layers = try knobLayers(scroller)
        XCTAssertNotEqual(layers.knob.frame.minY, fine.minY - 1, "at 1x, not yet where 2x puts it")
        XCTAssertFalse(try overlayAnimations(scroller).isEmpty, "widening")
        CATransaction.flush()

        window.scale = 2
        scroller.viewDidChangeBackingProperties()

        XCTAssertEqual(scroller.rect(for: .knob), fine)
        XCTAssertEqual(layers.knob.frame, drawn(fine, wide: true, in: scroller).insetBy(dx: -1, dy: -1), "2x")
        XCTAssertEqual(layers.slot.frame, drawn(scroller.rect(for: .knobSlot), wide: true, in: scroller))
        let animations = try overlayAnimations(scroller)
        XCTAssertFalse(animations.isEmpty, "the widening goes on")
        for (key, animation) in animations {
            let basic = try XCTUnwrap(animation as? CABasicAnimation, key)
            XCTAssertTrue(basic.isAdditive, "\(key): relative to the model, so not stale")
            XCTAssertEqual((basic.toValue as? NSNumber)?.doubleValue, 0, "\(key): ends at the new frame")
        }
    }

    // MARK: Legacy style

    func testALegacyScrollerDoesNotChangeUnderThePointer() throws {
        let (scroller, window) = makeScroller(style: .legacy)
        defer { window.close() }
        let before = try render(scroller)
        XCTAssertGreaterThan(before.maxAlpha, 0, "AppKit draws the legacy scroller")
        try enter(scroller)
        XCTAssertEqual(try render(scroller).bytes, before.bytes, "the pointer changes nothing")
        try exit(scroller)
        scroller.flash()
        XCTAssertEqual(try render(scroller).bytes, before.bytes, "nor a scroll")
        XCTAssertEqual(scroller.alphaValue, 1)
    }

    func testRestylingToLegacyAndBackDrawsTheRightKnob() throws {
        let (scroller, window) = makeScroller(style: .legacy)
        defer { window.close() }
        let legacy = try render(scroller)
        scroller.preferredStyleOverride = .overlay
        try enter(scroller)
        try assertWideKnobAtTheMiddle(scroller, "overlay after legacy")
        scroller.preferredStyleOverride = .legacy
        XCTAssertEqual(try render(scroller).bytes, legacy.bytes, "legacy again: AppKit's knob only")
    }

    func testAScrollerThatLostItsHistoryShowsNoKnobInLegacyStyle() throws {
        // Hidden without history in overlay style, shown again by legacy
        // style: no overlay knob from before.
        let (scroller, window) = makeScroller()
        defer { window.close() }
        // At the restyled scroller's size: the terminal view resizes it
        // later, and on macOS 15 a legacy scroller is narrower (15 pt)
        // than an overlay one (16 pt).
        let (empty, emptyWindow) = makeScroller(.init(offset: 0, history: 0, rows: 24), style: .legacy)
        defer { emptyWindow.close() }
        empty.frame = scroller.frame
        let expected = try render(empty)
        try enter(scroller)
        scroller.update(.init(offset: 0, history: 0, rows: 24))
        scroller.preferredStyleOverride = .legacy
        XCTAssertFalse(scroller.isHidden)
        XCTAssertEqual(try render(scroller).bytes, expected.bytes, "AppKit's empty legacy scroller only")
    }

    func testAppKitsOwnKnobAndSlotAreDrawnInLegacyStyleOnly() throws {
        // Asked to draw directly, as AppKit does: in overlay style only
        // the overlay layers draw (AppKit's knob would draw at half
        // opacity here); legacy style is AppKit's.
        func drawn(_ style: NSScroller.Style) throws -> Rendering {
            let (scroller, window) = makeScroller(style: style)
            defer { window.close() }
            let width = Int(scroller.bounds.width), height = Int(scroller.bounds.height)
            let context = try XCTUnwrap(
                CGContext(
                    data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                    space: try XCTUnwrap(CGColorSpace(name: CGColorSpace.sRGB)),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
            NSGraphicsContext.saveGraphicsState()
            NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
            scroller.effectiveAppearance.performAsCurrentDrawingAppearance {
                scroller.drawKnobSlot(in: scroller.rect(for: .knobSlot), highlight: false)
                scroller.drawKnob()
            }
            NSGraphicsContext.restoreGraphicsState()
            let data = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
            return Rendering(
                width: width, height: height, bytes: Array(UnsafeBufferPointer(start: data, count: width * height * 4)))
        }
        XCTAssertGreaterThan(try drawn(.legacy).maxAlpha, 0, "AppKit's legacy knob and slot")
        XCTAssertEqual(try drawn(.overlay).maxAlpha, 0, "nothing from AppKit in overlay style")
    }

    // MARK: The app's terminal view

    func testATerminalWithHistoryShowsTheKnobWhereTheViewScrolled() async throws {
        // Opened as the app opens a window; the View menu's scroll path
        // shows the knob, and nothing is posted.
        let appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        let ready = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera-scroller-\(UUID().uuidString).ready")
        addTeardownBlock { try? FileManager.default.removeItem(at: ready) }
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", "seq 1 2000; touch '\(ready.path)'; exec cat"])
        let controller = try appDelegate.newWindow()
        defer {
            for controller in appDelegate.windowControllers {
                controller.window?.close()
            }
            appDelegate.spawnOptions = .init()
        }
        let view = controller.focusedPane.view
        let window = KeyControlledWindow(contentRect: view.frame)
        defer { window.close() }
        window.appearance = NSAppearance(named: .aqua)
        window.contentView = view
        view.frameStatusOverride = .presented
        view.occlusionOverride = false
        view.scroller.preferredStyleOverride = .overlay
        view.scroller.reducesMotion = false
        let deadline = ContinuousClock.now + .seconds(30)
        // 2000 lines and the cursor's.
        while !FileManager.default.fileExists(atPath: ready.path)
            || (try? view.session.scrollPosition).map({ $0.history + UInt64($0.rows) }) != 2001
        {
            guard ContinuousClock.now < deadline else { throw XCTSkip("the child's output did not arrive") }
            try await Task.sleep(for: .milliseconds(10))
        }
        guard MTLCreateSystemDefaultDevice() != nil, view.surface.isAttached else {
            throw XCTSkip("no renderer: the scroll bar follows rendered frames")
        }
        XCTAssertEqual(view.renderView.frame.width, view.bounds.width, "overlay style keeps the grid's width")
        // A frame shows the history to the scroller, then the menu's scroll.
        view.sessionDidOutput()
        view.displayLinkDidFire()
        view.scroll(to: .top)
        view.sessionDidOutput()
        view.displayLinkDidFire()
        let scroller = view.scroller
        XCTAssertTrue(scroller.isRevealed, "the scroll shows it")
        let drawn = try render(scroller, ignoringOpacity: true)
        // Dark or light for the default theme (#558).
        let column = scroller.rect(for: .knob).midX
        let knob = try XCTUnwrap(drawn.knob(atX: column) ?? drawn.knob(atX: column, light: true), "the knob is drawn")
        XCTAssertEqual(knob.top, scroller.rect(for: .knobSlot).minY, accuracy: 2, "at the top: the oldest line")
    }

    /// The app's path (#558): each pane's knob follows its session's
    /// profile's theme as the core reports it on attach, not the window's
    /// appearance, and a live reload of `config.toml` that swaps the themes
    /// swaps the knobs. On a core of the test's own, its configuration in a
    /// temporary directory; nothing is posted and no view is told its
    /// background by the test. The scroll position is the test's (the
    /// panes' `cat` has no history), set right before each drawing.
    func testEachPanesKnobFollowsItsThemeThroughALiveReload() async throws {
        func config(_ first: String, _ second: String) -> String {
            """
            [appearance]
            theme = "\(first)"
            [[profiles]]
            name = "second"
            [profiles.appearance]
            theme = "\(second)"
            """
        }
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera-scroller-theme-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let file = dir.appendingPathComponent("config.toml")
        try config("Dracula", "Solarized Light").write(to: file, atomically: true, encoding: .utf8)
        let previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let host = TransparencyHost(core: try TesseraApp())
        addTeardownBlock { @MainActor in
            for controller in host.controllers {
                controller.window?.close()
            }
            host.core.shutdown()
            if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
            try? FileManager.default.removeItem(at: dir)
        }
        let controller = try host.open(profile: nil)
        let window = try XCTUnwrap(controller.window)
        window.appearance = NSAppearance(named: .aqua)
        let first = controller.focusedPane
        host.nextProfile = "second"
        let second = try XCTUnwrap(controller.splitFocusedPane(.right), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        guard MTLCreateSystemDefaultDevice() != nil, first.view.surface.isAttached, second.view.surface.isAttached
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        func knob(_ pane: Pane) throws -> String? {
            let scroller = pane.view.scroller
            scroller.preferredStyleOverride = .overlay
            scroller.update(Self.middle)
            let rect = scroller.rect(for: .knob)
            return knobShade(try render(scroller, ignoringOpacity: true), rect.maxX - 1.5, rect.midY)
        }
        XCTAssertEqual(try knob(first), "light", "Dracula in light appearance")
        XCTAssertEqual(try knob(second), "dark", "Solarized Light, the second profile's")
        window.appearance = NSAppearance(named: .darkAqua)
        XCTAssertEqual(try knob(first), "light", "dark appearance changes nothing")
        XCTAssertEqual(try knob(second), "dark", "a light theme in dark appearance")

        try config("Solarized Light", "Dracula").write(to: file, atomically: true, encoding: .utf8)
        let deadline = ContinuousClock.now + .seconds(10)
        while try knob(first) != "dark" || knob(second) != "light" {
            guard ContinuousClock.now < deadline else {
                let shades = [try knob(first), try knob(second)].map { $0 ?? "none" }
                return XCTFail("the reload swaps the knobs: \(shades)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
        window.appearance = NSAppearance(named: .aqua)
        XCTAssertEqual(try knob(first), "dark", "Solarized Light in light appearance")
        XCTAssertEqual(try knob(second), "light", "Dracula in light appearance")
    }

    // MARK: Lifetime

    func testAClosedScrollerIsReleased() throws {
        weak var released: TerminalScroller?
        try autoreleasepool {
            let (scroller, window) = makeScroller()
            try enter(scroller)
            scroller.flash()
            _ = try render(scroller)
            released = scroller
            scroller.removeFromSuperview()
            window.close()
        }
        XCTAssertNil(released, "the scroller and its knob layers are released")
    }
}

/// Drags a scroller's knob for ``TerminalScrollerTests`` from inside
/// AppKit's tracking loop: every 50 ms one step. It moves the pointer
/// `distance` points down, records what is drawn, has output grow the
/// history and records again, moves the pointer off the strip, then posts
/// the release.
@MainActor
private final class KnobDragDriver: NSObject {
    let scroller: TerminalScroller
    let window: NSWindow
    let start: NSPoint
    let distance: CGFloat
    var render: () -> (top: CGFloat, bottom: CGFloat)? = { nil }
    var onScroll: () -> [TesseraSession.ScrollTarget] = { [] }
    private(set) var moved: (top: CGFloat, bottom: CGFloat)?
    private(set) var afterOutput: (top: CGFloat, bottom: CGFloat)?
    private(set) var value: Double?
    private(set) var targetsAfterDrag: [TesseraSession.ScrollTarget] = []
    /// How wide the knob is drawn now, and was once the pointer had left.
    var knobWidth: () -> CGFloat? = { nil }
    private(set) var widthAfterExit: CGFloat?
    private var step = 0

    init(scroller: TerminalScroller, window: NSWindow, start: NSPoint, distance: CGFloat) {
        self.scroller = scroller
        self.window = window
        self.start = start
        self.distance = distance
    }

    /// A mouse event at `point` in the scroller's coordinates.
    func event(_ type: NSEvent.EventType, at point: NSPoint) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.mouseEvent(
                with: type, location: scroller.convert(point, to: nil), modifierFlags: [],
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber, context: nil,
                eventNumber: 0, clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1))
    }

    private func post(_ type: NSEvent.EventType, at point: NSPoint) {
        if let event = try? event(type, at: point) {
            NSApp.postEvent(event, atStart: false)
        }
    }

    @objc func fire(_ timer: Timer) {
        step += 1
        let end = NSPoint(x: start.x, y: start.y + distance)
        switch step {
        case 1:
            post(.leftMouseDragged, at: NSPoint(x: start.x, y: start.y + distance / 2))
            post(.leftMouseDragged, at: end)
        case 3:
            moved = render()
            value = scroller.doubleValue
            targetsAfterDrag = onScroll()
            // Output pushes lines while the knob is held.
            if let position = scroller.position {
                scroller.update(.init(offset: position.offset + 500, history: position.history + 500, rows: 24))
            }
            afterOutput = render()
        case 4:
            let outside = NSPoint(x: -20, y: end.y)
            if let exit = NSEvent.enterExitEvent(
                with: .mouseExited, location: scroller.convert(outside, to: nil), modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0, trackingNumber: 0, userData: nil)
            {
                scroller.mouseExited(with: exit)
            }
            widthAfterExit = knobWidth()
            post(.leftMouseUp, at: end)
        case 30, 60:
            // A second release in case AppKit missed the first.
            post(.leftMouseUp, at: end)
        default:
            if step > 70 {
                timer.invalidate()
            }
        }
    }
}

/// A window reporting the backing scale a test gives it; AppKit rounds what
/// is in it to that scale.
private final class ScaledWindow: NSWindow {
    var scale: CGFloat = 1
    override var backingScaleFactor: CGFloat { scale }
}
