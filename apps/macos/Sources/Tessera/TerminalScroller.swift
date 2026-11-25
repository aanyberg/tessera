import AppKit
import QuartzCore
import TesseraKit

/// The scrollback's scroll bar along the terminal's right edge, in the
/// style the system's "Show scroll bars" setting asks for (ADR 0032).
///
/// - Style: overlay (drawn over the text, which keeps its width) unless
///   AppKit prefers legacy scrollers ("Always", or "Automatically" with a
///   mouse); then it is a legacy scroller beside the text, whose width the
///   terminal view takes off the grid. A change of the setting, or of
///   ``preferredStyleOverride``, restyles it and calls ``onStyleChange``.
/// - Knob: its proportion is rows / (history + rows), and its position the
///   view's offset: at the top for the oldest line, at the bottom for the
///   live screen. ``update(_:)`` sets both from a scroll position the
///   terminal view reads when it draws a frame, so output and scrolling
///   move it without a timer.
/// - Visibility: a legacy scroller is always shown (its width is the
///   grid's, whatever the history), with an empty track and no action
///   while there is no history (a new session, the alternate screen). An
///   overlay scroller is hidden then; otherwise it shows when the user
///   scrolls or points at it and fades out after a moment, like AppKit's
///   own overlay scrollers. A faded-out scroller lets clicks through to
///   the terminal. Under the system's Reduce Motion (``reducesMotion``) it
///   appears and disappears at once, with no fade (ADR 0041).
/// - Overlay knob: AppKit draws the knob of an overlay scroller outside an
///   `NSScrollView` at alpha 0 and never widens it (#495), so in overlay
///   style this scroller draws its own knob over AppKit's knob rect, and
///   AppKit's own knob and slot are not drawn. The knob is light on a
///   dark ``themeBackground`` and dark on a light one, whatever the app's
///   appearance (#558), and the slot is drawn to match. A scroll shows the
///   narrow knob alone; under the pointer, or while the knob is held, the
///   knob is wide, over AppKit's slot drawn as wide, and stays so until
///   the scroller has faded out after the pointer left. It widens as
///   AppKit's does (``knobExpandDuration``), the slot with it, and narrows
///   at once (it is faded out then); under Reduce Motion it widens at
///   once.
/// - Input: dragging the knob or clicking the track reports where to go
///   through ``onScroll``; the terminal view moves the session's view.
@MainActor
final class TerminalScroller: NSScroller {
    /// Called with where the user moved the knob or clicked the track.
    var onScroll: (@MainActor (TesseraSession.ScrollTarget) -> Void)?
    /// Called after the style changed (the system setting or
    /// ``preferredStyleOverride``): the terminal view lays out again.
    var onStyleChange: (@MainActor () -> Void)?

    /// The position the knob shows (the last one passed to ``update(_:)``),
    /// or `nil` before the first.
    private(set) var position: TesseraSession.ScrollPosition?
    /// The scroller style used instead of AppKit's preferred one (tests:
    /// the system setting is the user's).
    var preferredStyleOverride: NSScroller.Style? {
        didSet { styleChanged() }
    }
    /// The session's configured background (its profile's theme), which
    /// the overlay knob is drawn to show on (``themeKnobStyle``),
    /// as the terminal view last read it (on attach and on every appearance
    /// change of the core); `nil` before the first, when the knob follows
    /// the app's appearance.
    var themeBackground: TesseraSession.Appearance.Color? {
        didSet {
            guard themeBackground != oldValue else { return }
            applyOverlayColors()
        }
    }
    /// The knob is being dragged or the track held. While the knob is
    /// dragged, positions read meanwhile are applied once it is released,
    /// so the knob does not fight the pointer (``freezesKnob(tracking:part:)``).
    private(set) var isTracking = false
    /// The knob value of the last knob action sent in this tracking session.
    private var lastSentKnobValue: Double?
    private var isPointerInside = false
    private var hoverArea: NSTrackingArea?
    /// When the current flash has faded out, or `nil` without one.
    private var flashEnd: CFTimeInterval?
    /// The history and style ``applyHidden()`` last applied, or `nil`
    /// before the first: a position that changes neither sets nothing.
    private var applied: (hasHistory: Bool, style: NSScroller.Style)?
    /// The overlay knob is wide, over its slot (``layoutOverlayKnob()``).
    private var isExpanded = false
    /// The overlay knob (a rim around a fill) and the slot behind a wide
    /// one, drawn over AppKit's layers; hidden in legacy style.
    private let knobLayer = CALayer()
    private let knobFill = CALayer()
    private let knobRim = CALayer()
    private let slotLayer = CALayer()
    /// The overlay layers' delegate: no implicit animations, and the slot
    /// drawn by AppKit.
    private let painter = OverlayPainter()

    /// How long a scroll keeps it shown before it fades, and the fade.
    static let holdDuration: CFTimeInterval = 1.0
    static let fadeDuration: CFTimeInterval = 0.3
    private static let flashKey = "flash"

    /// How long the overlay knob and its slot take to widen, and over what
    /// curve: AppKit's own. Measured on macOS 27.2 (build 26B123, #559)
    /// with an overlay scroller in an `NSScrollView`: on hover its imp asks
    /// its delegate for `scrollerImp:animateExpansionTransitionWithDuration:`
    /// 0.125 s, and the knob's width over time fits ease-in-ease-out
    /// (rms 0.004, linear 0.045). AppKit narrows its knob without an
    /// animation, once its fade has ended, and so does this scroller.
    static let knobExpandDuration: CFTimeInterval = 0.125
    static let knobExpandTimingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
    /// The widening's animations, one per key path (``animateWidening(from:)``),
    /// on the knob, fill, rim and slot layers.
    private static let widenKeyPaths = ["position.x", "bounds.size.width", "cornerRadius"]
    private static let widenKeyPrefix = "widen."

    /// The clock the flash is timed by (tests advance their own).
    var now: () -> CFTimeInterval = { CACurrentMediaTime() }
    /// Runs the closure on the main actor after the delay: hides a flash
    /// shown without an animation (tests run it themselves).
    var scheduleHide: (CFTimeInterval, @escaping @MainActor () -> Void) -> Void = { delay, hide in
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { MainActor.assumeIsolated { hide() } }
    }
    /// A hide is scheduled (at most one at a time).
    private var hidePending = false
    /// The system's Reduce Motion (Accessibility, Display) as the app last
    /// read it, which new scrollers start with.
    static var systemReducesMotion = false
    /// No fade: a flash shows at full opacity at once and hides after
    /// ``holdDuration`` without an animation. Turning it on or off during a
    /// flash settles it: shown until the flash would have ended, then
    /// hidden.
    var reducesMotion = TerminalScroller.systemReducesMotion {
        didSet {
            guard reducesMotion != oldValue, flashEnd != nil else { return }
            layer?.removeAnimation(forKey: Self.flashKey)
            alphaValue = 1
            hideAfterFlash()
        }
    }
    /// AppKit's preferred scroller style, read when the system says it
    /// changed (tests stand in for the user's setting).
    static var systemStyle: @MainActor () -> NSScroller.Style = { NSScroller.preferredScrollerStyle }

    /// The width of an overlay scroller at the regular size.
    static var width: CGFloat { width(for: .overlay) }

    /// The width of a regular-size scroller in `style`.
    static func width(for style: NSScroller.Style) -> CGFloat {
        NSScroller.scrollerWidth(for: .regular, scrollerStyle: style)
    }

    /// A subclass gets overlay scrollers only if it says it draws them.
    override class var isCompatibleWithOverlayScrollers: Bool { true }

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: Self.width, height: 100))
        wantsLayer = true
        knobStyle = .default
        isEnabled = false
        target = self
        action = #selector(userScrolled(_:))
        NotificationCenter.default.addObserver(
            self, selector: #selector(preferredStyleChanged(_:)),
            name: NSScroller.preferredScrollerStyleDidChangeNotification, object: nil)
        scrollerStyle = effectiveStyle
        setUpOverlayLayers()
        applyStripBackground()
        applyHidden()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Position

    /// Shows `position`: the knob's size and place, or no knob without
    /// history. While the knob is dragged only the position is kept.
    func update(_ position: TesseraSession.ScrollPosition) {
        guard position != self.position else { return }
        self.position = position
        applyHidden()
        guard hasHistory, !Self.freezesKnob(tracking: isTracking, part: hitPart) else { return }
        applyKnob(position)
    }

    private var hasHistory: Bool { (position?.history ?? 0) > 0 }

    /// Whether a new position leaves the knob where it is: only while the
    /// knob itself is dragged, where it follows the pointer. A held click
    /// in the track pages until the knob reaches the pointer, so there the
    /// knob must follow the position, or AppKit pages on to the end.
    static func freezesKnob(tracking: Bool, part: NSScroller.Part) -> Bool {
        tracking && part == .knob
    }

    private func applyKnob(_ position: TesseraSession.ScrollPosition) {
        let history = Double(position.history)
        let lines = history + Double(position.rows)
        knobProportion = CGFloat(Double(position.rows) / lines)
        // Another thread's resize can shrink the history under a stored
        // offset between two reads; the core clamps, and so does this.
        doubleValue = (history - Double(min(position.offset, position.history))) / history
        layoutOverlayKnob()
    }

    /// Where the user's action on `part` moves the view, from the knob's
    /// value for the knob and a jump to a clicked spot, a page for a click
    /// on the track, a line for a legacy scroller's arrows. The line cases
    /// are also what VoiceOver's increment and decrement buttons reach.
    func target(for part: NSScroller.Part) -> TesseraSession.ScrollTarget? {
        guard let position, position.history > 0 else { return nil }
        switch part {
        case .knob, .knobSlot:
            let fromTop = UInt64((min(max(doubleValue, 0), 1) * Double(position.history)).rounded())
            return .offset(position.history - min(fromTop, position.history))
        case .decrementPage: return .pageUp
        case .incrementPage: return .pageDown
        case .decrementLine: return .offset(min(position.offset + 1, position.history))
        case .incrementLine: return .offset(position.offset - min(position.offset, 1))
        default: return nil
        }
    }

    /// The user acted on `part` (the action AppKit sends while the knob is
    /// dragged or the track held; tests call it with the part). Within one
    /// tracking session a knob action at the value already sent, or at the
    /// value the knob was pressed at, is not sent: AppKit sends one on
    /// release with the value the knob was left at, which applied to a
    /// history grown meanwhile would jump the view.
    func userMoved(part: NSScroller.Part) {
        guard let target = target(for: part) else { return }
        if isTracking, part == .knob || part == .knobSlot {
            guard doubleValue != lastSentKnobValue else { return }
            lastSentKnobValue = doubleValue
        }
        onScroll?(target)
    }

    @objc private func userScrolled(_ sender: Any?) {
        // AppKit moved the knob it tracks; the drawn knob follows.
        layoutOverlayKnob()
        userMoved(part: hitPart)
    }

    // MARK: Style and visibility

    /// The style it is drawn in: the override, else AppKit's preferred one.
    var effectiveStyle: NSScroller.Style { preferredStyleOverride ?? Self.systemStyle() }

    /// Whether it stays shown rather than appearing on a scroll.
    var staysShown: Bool { effectiveStyle == .legacy }

    /// Whether it is on screen and takes clicks: not hidden, and stays
    /// shown, is in use, or is showing after a scroll.
    var isRevealed: Bool {
        guard !isHidden else { return false }
        return staysShown || isTracking || isPointerInside || flashEnd.map { now() < $0 } ?? false
    }

    /// Shows it after a scroll and fades it out after a moment, from what
    /// is on screen now (a fade in progress turns back). Nothing to do
    /// while it stays shown or is in use. Shown from faded out, the
    /// overlay knob is narrow; a fade in progress keeps its width.
    func flash() {
        flash(keepingWidth: isRevealed)
    }

    /// ``flash()``, keeping the overlay knob's width when `keepingWidth`
    /// (the pointer just left, or the knob was just released).
    private func flash(keepingWidth: Bool) {
        guard !isHidden, !staysShown, !isTracking, !isPointerInside, let layer else { return }
        if !keepingWidth {
            setExpanded(false)
        }
        guard !reducesMotion else {
            layer.removeAnimation(forKey: Self.flashKey)
            alphaValue = 1
            flashEnd = now() + Self.holdDuration
            hideAfterFlash()
            return
        }
        let total = Self.holdDuration + Self.fadeDuration
        let appear = 0.1
        let animation = CAKeyframeAnimation(keyPath: "opacity")
        animation.values = [layer.presentation()?.opacity ?? layer.opacity, 1, 1, 0]
        animation.keyTimes = [0, NSNumber(value: appear / total), NSNumber(value: Self.holdDuration / total), 1]
        animation.duration = total
        layer.add(animation, forKey: Self.flashKey)
        // Core Animation removes a finished animation only at its next
        // commit, which an idle terminal never makes: the end time, not
        // the animation's presence, says whether it is still shown.
        flashEnd = now() + total
    }

    /// Hides a flash shown without an animation once it has ended: at once
    /// when it has, otherwise through one hide scheduled for its end, which
    /// looks again (a later flash moves the end).
    private func hideAfterFlash() {
        guard let flashEnd else { return }
        let remaining = flashEnd - now()
        guard remaining > 0 else {
            self.flashEnd = nil
            applyVisibility()
            return
        }
        guard !hidePending else { return }
        hidePending = true
        scheduleHide(remaining) { [weak self] in
            guard let self else { return }
            hidePending = false
            hideAfterFlash()
        }
    }

    /// Hidden only in overlay style without history. Hiding forgets the
    /// pointer and any flash, and narrows the knob unless it is held
    /// (#634; a knob released while hidden narrows then): shown again, it
    /// waits for a scroll or for the pointer to enter, which widens it
    /// again. Nothing is set unless the history's presence or the style
    /// changed since the last time, so a position read every frame while
    /// output streams sets no AppKit property; the pointer and tracking
    /// apply their own visibility.
    private func applyHidden() {
        let style = effectiveStyle
        if let applied, applied.hasHistory == hasHistory, applied.style == style { return }
        applied = (hasHistory, style)
        isEnabled = hasHistory
        let hidden = !hasHistory && !staysShown
        if hidden != isHidden {
            isHidden = hidden
            if hidden {
                isPointerInside = false
                flashEnd = nil
                layer?.removeAnimation(forKey: Self.flashKey)
                setExpanded(isTracking)
            }
        }
        applyVisibility()
        layoutOverlayKnob()
    }

    /// Shown for good while it stays shown, is in use or under the
    /// pointer; otherwise transparent between flashes.
    private func applyVisibility() {
        let shown = staysShown || isTracking || isPointerInside
        alphaValue = shown ? 1 : 0
        if shown {
            flashEnd = nil
            layer?.removeAnimation(forKey: Self.flashKey)
        }
    }

    /// Restyles it for the current style and tells the terminal view.
    private func styleChanged() {
        let style = effectiveStyle
        if scrollerStyle != style {
            scrollerStyle = style
        }
        applyStripBackground()
        applyOverlayColors()
        applyHidden()
        if hasHistory, let position {
            applyKnob(position)
        }
        onStyleChange?()
    }

    @objc private func preferredStyleChanged(_ notification: Notification) {
        styleChanged()
    }

    /// The fill of a legacy scroller's whole strip. AppKit draws only a
    /// track inset 3 pt into the strip, nearly transparent (about 7%
    /// alpha) in dark appearance, and leaves the rest of its layer empty,
    /// so on a translucent theme the desktop showed through (ADR 0032).
    static let legacyStripBackground = NSColor.controlBackgroundColor

    /// Fills the backing layer behind AppKit's track with
    /// ``legacyStripBackground``, resolved for the current appearance, in
    /// legacy style; nothing in overlay style, which is drawn over the
    /// text. A layer colour is not dynamic: a change of style or
    /// appearance applies it again.
    private func applyStripBackground() {
        guard let layer else { return }
        guard scrollerStyle == .legacy else {
            layer.backgroundColor = nil
            return
        }
        var color: CGColor?
        effectiveAppearance.performAsCurrentDrawingAppearance {
            color = Self.legacyStripBackground.cgColor
        }
        layer.backgroundColor = color
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        applyStripBackground()
        applyOverlayColors()
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        isRevealed ? super.hitTest(point) : nil
    }

    // MARK: Overlay knob

    private func setUpOverlayLayers() {
        guard let layer else { return }
        for sublayer in [slotLayer, knobLayer, knobFill, knobRim] {
            sublayer.delegate = painter
        }
        painter.drawSlot = { [weak self] context, size in self?.drawOverlaySlot(in: context, size: size) }
        slotLayer.needsDisplayOnBoundsChange = true
        knobRim.borderWidth = 1
        knobLayer.addSublayer(knobRim)
        knobLayer.addSublayer(knobFill)
        // Above AppKit's track and knob layers, whatever their order.
        slotLayer.zPosition = 1
        knobLayer.zPosition = 2
        layer.addSublayer(slotLayer)
        layer.addSublayer(knobLayer)
        applyOverlayColors()
        layoutOverlayKnob()
    }

    /// The knob style for the theme: in overlay style, `.dark` on a light
    /// ``themeBackground`` (``isLightBackground(_:)``) and `.light` on a
    /// dark one, which AppKit's slot follows too; `.default` (the app's
    /// appearance) before the first, and in legacy style, whose knob is
    /// AppKit's on a strip of the appearance's colour.
    private var themeKnobStyle: NSScroller.KnobStyle {
        guard scrollerStyle == .overlay, let themeBackground else { return .default }
        return Self.isLightBackground(themeBackground) ? .dark : .light
    }

    /// Whether a dark knob shows on `color`: its WCAG relative luminance
    /// (sRGB components linearised, weighted 0.2126, 0.7152, 0.0722) is at
    /// least 0.5; below, the knob is light. Alpha is not looked at: a
    /// translucent background shows the theme's colour over what is behind
    /// the window.
    static func isLightBackground(_ color: TesseraSession.Appearance.Color) -> Bool {
        func linear(_ component: Float) -> Double {
            let c = Double(min(max(component, 0), 1))
            return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        let luminance = 0.2126 * linear(color.red) + 0.7152 * linear(color.green) + 0.0722 * linear(color.blue)
        return luminance >= 0.5
    }

    /// Sets the knob style for the theme (``themeKnobStyle``) and AppKit's
    /// overlay knob colours for it, measured from its `drawKnob()` on
    /// macOS 27: a dark knob (`.dark`, or `.default` in light appearance)
    /// is black at half opacity with a 1 pt white rim at 15 % outside it, a
    /// light one white at half opacity with a black rim at 20 %. Layer
    /// colours are not dynamic: a change of ``themeBackground``, of style
    /// or of appearance applies them again, and redraws the slot.
    private func applyOverlayColors() {
        let style = themeKnobStyle
        if knobStyle != style {
            knobStyle = style
        }
        let darkKnob: Bool
        switch style {
        case .dark: darkKnob = true
        case .light: darkKnob = false
        default: darkKnob = effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) != .darkAqua
        }
        knobFill.backgroundColor = CGColor(gray: darkKnob ? 0 : 1, alpha: 0.5)
        knobRim.borderColor = CGColor(gray: darkKnob ? 1 : 0, alpha: darkKnob ? 0.15 : 0.2)
        slotLayer.setNeedsDisplay()
    }

    /// AppKit's slot, as wide as the layer it is drawn into, for the knob
    /// style.
    private func drawOverlaySlot(in context: CGContext, size: CGSize) {
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
        effectiveAppearance.performAsCurrentDrawingAppearance {
            super.drawKnobSlot(in: NSRect(origin: .zero, size: size), highlight: false)
        }
        NSGraphicsContext.restoreGraphicsState()
    }

    /// Widens or narrows the overlay knob and shows or hides its slot. The
    /// layers get the new frames at once; a widening then animates them
    /// from the narrow ones (``animateWidening(from:)``), unless under
    /// Reduce Motion. A narrowing, reached only while the scroller is faded
    /// out (``flash()``) or hidden (``applyHidden()``), is not animated, as
    /// AppKit's is not, and ends a widening still in flight.
    private func setExpanded(_ expanded: Bool) {
        guard expanded != isExpanded else { return }
        let before = overlayRects
        isExpanded = expanded
        removeWidening()
        layoutOverlayKnob()
        if expanded, !reducesMotion {
            animateWidening(from: before)
        }
    }

    /// Animates the knob and slot layers, laid out wide, from `narrow` (the
    /// ``overlayRects`` before the widening): additive animations of each
    /// layer's x, width and corner radius from the narrow value's
    /// difference to none, over ``knobExpandDuration``. Being differences
    /// from the model, they compose with whatever moves the knob meanwhile
    /// (a drag, output, a resize, a change of backing scale), which sets
    /// the model frames at once and is not animated; the slot is shown at
    /// the narrow width from the first frame and widens with the knob.
    private func animateWidening(from narrow: (knob: NSRect, slot: NSRect)) {
        let wide = overlayRects
        if !narrow.knob.isEmpty, !wide.knob.isEmpty {
            let from = narrow.knob.insetBy(dx: -1, dy: -1), to = wide.knob.insetBy(dx: -1, dy: -1)
            let width = from.width - to.width
            addWidening(knobLayer, "position.x", from.midX - to.midX)
            addWidening(knobLayer, "bounds.size.width", width)
            // The fill and rim are centred in the knob layer.
            func radius(_ size: CGSize) -> CGFloat { min(size.width, size.height) / 2 }
            for (sublayer, inset) in [(knobFill, CGFloat(1)), (knobRim, 0)] {
                addWidening(sublayer, "position.x", width / 2)
                addWidening(sublayer, "bounds.size.width", width)
                let radiusDelta = radius(from.insetBy(dx: inset, dy: inset).size)
                    - radius(to.insetBy(dx: inset, dy: inset).size)
                addWidening(sublayer, "cornerRadius", radiusDelta)
            }
        }
        if !narrow.slot.isEmpty, !wide.slot.isEmpty {
            addWidening(slotLayer, "position.x", narrow.slot.midX - wide.slot.midX)
            addWidening(slotLayer, "bounds.size.width", narrow.slot.width - wide.slot.width)
        }
    }

    private func addWidening(_ layer: CALayer, _ keyPath: String, _ delta: CGFloat) {
        guard delta != 0 else { return }
        let animation = CABasicAnimation(keyPath: keyPath)
        animation.isAdditive = true
        animation.fromValue = delta
        animation.toValue = 0
        animation.duration = Self.knobExpandDuration
        animation.timingFunction = Self.knobExpandTimingFunction
        layer.add(animation, forKey: Self.widenKeyPrefix + keyPath)
    }

    /// Ends a widening in flight: the layers show their model frames.
    private func removeWidening() {
        for layer in [knobLayer, knobFill, knobRim, slotLayer] {
            for keyPath in Self.widenKeyPaths {
                layer.removeAnimation(forKey: Self.widenKeyPrefix + keyPath)
            }
        }
    }

    /// `rect` (AppKit's knob or slot) in the overlay knob's column, at its
    /// right edge: while expanded, wide, from as far from the left edge as
    /// AppKit insets it from the right; otherwise narrow, at most half as
    /// wide. That is AppKit's own overlay knob, 6 or 11 pt of 17 on
    /// macOS 27, whose `rect(for:)` gives the narrow one; macOS 15 gives a
    /// wider rect for a scroller outside a scroll view.
    private func widened(_ rect: NSRect) -> NSRect {
        guard !rect.isEmpty else { return rect }
        let left = min(max(bounds.maxX - rect.maxX, 0), rect.minX)
        let wide = rect.maxX - left
        let width = isExpanded ? wide : min(rect.width, (wide / 2).rounded(.up))
        return NSRect(x: rect.maxX - width, y: rect.minY, width: width, height: rect.height)
    }

    /// The overlay knob's and slot's rects at the current width
    /// (``widened(_:)``), or empty in legacy style, without history, or
    /// when the scroller is too short for AppKit to give it a knob.
    private var overlayRects: (knob: NSRect, slot: NSRect) {
        guard scrollerStyle == .overlay, hasHistory, usableParts == .allScrollerParts else { return (.zero, .zero) }
        return (widened(rect(for: .knob)), widened(rect(for: .knobSlot)))
    }

    /// Places the overlay knob over AppKit's knob rect (its position and
    /// minimum length are AppKit's), narrow or wide, and the slot
    /// behind it while expanded; hides both where ``overlayRects`` are
    /// empty. Sets nothing that has not changed.
    private func layoutOverlayKnob() {
        let rects = overlayRects
        let knob = rects.knob
        let slot = isExpanded ? rects.slot : .zero
        let knobShown = !knob.isEmpty, slotShown = !slot.isEmpty
        if knobLayer.isHidden == knobShown {
            knobLayer.isHidden = !knobShown
        }
        if slotLayer.isHidden == slotShown {
            slotLayer.isHidden = !slotShown
        }
        if slotShown, slotLayer.frame != slot {
            slotLayer.frame = slot
        }
        // The rim is 1 pt outside the knob rect, as AppKit draws it.
        let outer = knob.insetBy(dx: -1, dy: -1)
        guard knobShown, knobLayer.frame != outer else { return }
        let resized = knobLayer.frame.size != outer.size
        knobLayer.frame = outer
        guard resized else { return }
        let bounds = CGRect(origin: .zero, size: outer.size)
        let fill = bounds.insetBy(dx: 1, dy: 1)
        knobFill.frame = fill
        knobFill.cornerRadius = min(fill.width, fill.height) / 2
        // A 1 pt border, whose inner edge is the fill's outline.
        knobRim.frame = bounds
        knobRim.cornerRadius = min(bounds.width, bounds.height) / 2
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        layoutOverlayKnob()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        applySlotScale()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        applySlotScale()
        // AppKit rounds the knob's rect to the new backing scale.
        layoutOverlayKnob()
    }

    /// The slot is drawn at the window's backing scale.
    private func applySlotScale() {
        let scale = window?.backingScaleFactor ?? 1
        guard slotLayer.contentsScale != scale else { return }
        slotLayer.contentsScale = scale
        slotLayer.setNeedsDisplay()
    }

    /// In overlay style AppKit's own knob (drawn at alpha 0 outside a
    /// scroll view) and narrow slot are not drawn: the overlay layers are.
    override func drawKnob() {
        if scrollerStyle == .legacy {
            super.drawKnob()
        }
    }

    override func drawKnobSlot(in slotRect: NSRect, highlight flag: Bool) {
        if scrollerStyle == .legacy {
            super.drawKnobSlot(in: slotRect, highlight: flag)
        }
    }

    /// While expanded, a point over the wide knob or slot is tested where
    /// AppKit's narrow ones are, at the same height: the wide knob grabs.
    override func testPart(_ point: NSPoint) -> NSScroller.Part {
        guard isExpanded, scrollerStyle == .overlay else { return super.testPart(point) }
        let narrow = super.rect(for: .knobSlot)
        let wide = widened(narrow)
        var local = convert(point, from: nil)
        guard local.x >= wide.minX, local.x < wide.maxX else { return super.testPart(point) }
        local.x = narrow.midX
        return super.testPart(convert(local, to: nil))
    }

    // MARK: Pointer

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let hoverArea {
            removeTrackingArea(hoverArea)
        }
        let area = NSTrackingArea(
            rect: .zero, options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect], owner: self,
            userInfo: nil)
        addTrackingArea(area)
        hoverArea = area
    }

    override func mouseEntered(with event: NSEvent) {
        super.mouseEntered(with: event)
        isPointerInside = true
        setExpanded(true)
        applyVisibility()
    }

    override func mouseExited(with event: NSEvent) {
        super.mouseExited(with: event)
        isPointerInside = false
        applyVisibility()
        flash(keepingWidth: true)
    }

    override func mouseDown(with event: NSEvent) {
        beginTracking()
        // A knob pressed and released in place gets one action on release
        // with the value it was pressed at; output meanwhile grew the
        // history, so that value is not sent.
        if testPart(event.locationInWindow) == .knob {  // window coordinates
            lastSentKnobValue = doubleValue
        }
        // Tracks the knob or the track until the button goes up, sending
        // the action as it goes.
        super.mouseDown(with: event)
        endTracking()
    }

    /// A press on the knob or the track starts tracking (`mouseDown`;
    /// tests call it directly instead of running AppKit's tracking loop).
    func beginTracking() {
        isTracking = true
        lastSentKnobValue = nil
        setExpanded(true)
        applyVisibility()
    }

    /// The button went up: the knob returns to the position last read, and
    /// the scroller fades unless it stays shown; hidden, the knob narrows
    /// (``applyHidden()``).
    func endTracking() {
        isTracking = false
        lastSentKnobValue = nil
        if let position, position.history > 0 {
            applyKnob(position)
        }
        if isHidden {
            setExpanded(false)
        }
        applyVisibility()
        flash(keepingWidth: true)
    }
}

/// The delegate of ``TerminalScroller``'s overlay layers: no implicit
/// animations (the knob moves with the view, the scroller's own opacity
/// fades it, and its widening is animated explicitly), and the slot drawn
/// by the scroller. Core Animation
/// asks on the thread that changes or displays the layers: the main one.
@MainActor
private final class OverlayPainter: NSObject, @preconcurrency CALayerDelegate {
    var drawSlot: ((CGContext, CGSize) -> Void)?

    nonisolated func action(for layer: CALayer, forKey event: String) -> (any CAAction)? {
        NSNull()
    }

    func draw(_ layer: CALayer, in ctx: CGContext) {
        drawSlot?(ctx, layer.bounds.size)
    }
}
