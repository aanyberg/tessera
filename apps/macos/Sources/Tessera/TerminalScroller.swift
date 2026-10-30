import AppKit
import QuartzCore
import TesseraKit

/// The scrollback's scroll bar: an overlay-style `NSScroller` along the
/// terminal's right edge, drawn over the text (the grid keeps its width).
///
/// - Knob: its proportion is rows / (history + rows), and its position the
///   view's offset: at the top for the oldest line, at the bottom for the
///   live screen. ``update(_:)`` sets both from a scroll position the
///   terminal view reads when it draws a frame, so output and scrolling
///   move it without a timer.
/// - Visibility: hidden while there is no history (a new session, the
///   alternate screen). It follows the system's "Show scroll bars"
///   setting: when AppKit prefers legacy scrollers ("Always", or
///   "Automatically" with a mouse) it stays shown; otherwise it shows when
///   the user scrolls or points at it and fades out after a moment, like
///   AppKit's own overlay scrollers. A faded-out scroller lets clicks
///   through to the terminal.
/// - Input: dragging the knob or clicking the track reports where to go
///   through ``onScroll``; the terminal view moves the session's view.
@MainActor
final class TerminalScroller: NSScroller {
    /// Called with where the user moved the knob or clicked the track.
    var onScroll: (@MainActor (TesseraSession.ScrollTarget) -> Void)?

    /// The position the knob shows (the last one passed to ``update(_:)``),
    /// or `nil` before the first.
    private(set) var position: TesseraSession.ScrollPosition?
    /// The scroller style the visibility follows instead of AppKit's
    /// preferred one (tests: the system setting is the user's).
    var preferredStyleOverride: NSScroller.Style? {
        didSet { applyVisibility() }
    }
    /// The knob is being dragged or the track held. While the knob is
    /// dragged, positions read meanwhile are applied once it is released,
    /// so the knob does not fight the pointer (``freezesKnob(tracking:part:)``).
    private(set) var isTracking = false
    private var isPointerInside = false
    private var hoverArea: NSTrackingArea?

    /// How long a scroll keeps it shown before it fades, and the fade.
    private static let holdDuration: CFTimeInterval = 1.0
    private static let fadeDuration: CFTimeInterval = 0.3
    private static let flashKey = "flash"

    /// The width of an overlay scroller at the regular size.
    static var width: CGFloat { NSScroller.scrollerWidth(for: .regular, scrollerStyle: .overlay) }

    /// A subclass gets overlay scrollers only if it says it draws them.
    override class var isCompatibleWithOverlayScrollers: Bool { true }

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: Self.width, height: 100))
        wantsLayer = true
        scrollerStyle = .overlay
        knobStyle = .default
        isEnabled = true
        isHidden = true
        target = self
        action = #selector(userScrolled(_:))
        NotificationCenter.default.addObserver(
            self, selector: #selector(preferredStyleChanged(_:)),
            name: NSScroller.preferredScrollerStyleDidChangeNotification, object: nil)
        applyVisibility()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Position

    /// Shows `position`: hidden without history, else the knob's size and
    /// place. While the knob is dragged only the position is kept.
    func update(_ position: TesseraSession.ScrollPosition) {
        guard position != self.position else { return }
        self.position = position
        let hasHistory = position.history > 0
        if isHidden == hasHistory {
            isHidden = !hasHistory
            applyVisibility()
        }
        guard hasHistory, !Self.freezesKnob(tracking: isTracking, part: hitPart) else { return }
        applyKnob(position)
    }

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
    /// on the track, a line for a legacy scroller's arrows.
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
    /// dragged or the track held; tests call it with the part).
    func userMoved(part: NSScroller.Part) {
        guard let target = target(for: part) else { return }
        onScroll?(target)
    }

    @objc private func userScrolled(_ sender: Any?) {
        userMoved(part: hitPart)
    }

    // MARK: Visibility

    /// Whether it stays shown rather than appearing on a scroll.
    var staysShown: Bool { (preferredStyleOverride ?? NSScroller.preferredScrollerStyle) == .legacy }

    /// Whether it is on screen and takes clicks: it has history, and stays
    /// shown, is in use, or is showing after a scroll.
    var isRevealed: Bool {
        guard !isHidden else { return false }
        return staysShown || isTracking || isPointerInside || layer?.animation(forKey: Self.flashKey) != nil
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
    }

    /// Shown for good while it stays shown, is in use or under the
    /// pointer; otherwise transparent between flashes.
    private func applyVisibility() {
        let shown = staysShown || isTracking || isPointerInside
        alphaValue = shown ? 1 : 0
        if shown {
            layer?.removeAnimation(forKey: Self.flashKey)
        }
    }

    @objc private func preferredStyleChanged(_ notification: Notification) {
        applyVisibility()
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
        isTracking = true
        applyVisibility()
        // Tracks the knob or the track until the button goes up, sending
        // the action as it goes.
        super.mouseDown(with: event)
        isTracking = false
        if let position, position.history > 0 {
            applyKnob(position)
        }
        applyVisibility()
        flash()
    }
}
