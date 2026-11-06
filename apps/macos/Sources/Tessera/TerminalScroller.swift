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
///   the terminal.
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

    /// How long a scroll keeps it shown before it fades, and the fade.
    static let holdDuration: CFTimeInterval = 1.0
    static let fadeDuration: CFTimeInterval = 0.3
    private static let flashKey = "flash"

    /// The clock the flash is timed by (tests advance their own).
    var now: () -> CFTimeInterval = { CACurrentMediaTime() }
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
    /// while it stays shown or is in use.
    func flash() {
        guard !isHidden, !staysShown, !isTracking, !isPointerInside, let layer else { return }
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

    /// Hidden only in overlay style without history. Hiding forgets the
    /// pointer and any flash: shown again, it waits for a scroll or for
    /// the pointer to enter. Nothing is set unless the history's presence
    /// or the style changed since the last time, so a position read every
    /// frame while output streams sets no AppKit property; the pointer and
    /// tracking apply their own visibility.
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
            }
        }
        applyVisibility()
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
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        isRevealed ? super.hitTest(point) : nil
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
        applyVisibility()
    }

    override func mouseExited(with event: NSEvent) {
        super.mouseExited(with: event)
        isPointerInside = false
        applyVisibility()
        flash()
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
        applyVisibility()
    }

    /// The button went up: the knob returns to the position last read, and
    /// the scroller fades unless it stays shown.
    func endTracking() {
        isTracking = false
        lastSentKnobValue = nil
        if let position, position.history > 0 {
            applyKnob(position)
        }
        applyVisibility()
        flash()
    }
}
