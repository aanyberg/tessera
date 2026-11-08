import AppKit
import QuartzCore
import TesseraKit

// MARK: The text area VoiceOver reads (ADR 0041)

/// The terminal view is one AXTextArea, as in Terminal.app and iTerm2: its
/// value is the text the view shows (``TesseraSession/AccessibleText``),
/// line `n` is row `n`, the insertion point is the cursor and the selected
/// range the selection. The core builds the text and answers every lookup;
/// this side converts cells to screen rects. Nothing set through it takes
/// effect (D-6): a terminal is not edited through its text. The selected
/// range is never settable; the value is settable (a no-op) while VoiceOver
/// is off, as in iTerm2, because Full Keyboard Access sends Space only to
/// an element with a settable value, and not while VoiceOver is on, where
/// a settable value wedges QuickNav.
extension TerminalSurfaceView {
    /// The label when the pane has no title yet.
    static let accessibilityFallbackLabel = "terminal"

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .textArea }

    /// The pane's title, which the core sanitised (docs/security.md S3),
    /// never empty.
    override func accessibilityLabel() -> String? {
        let pane = (window?.contentView as? PaneContainerView)?.panes.first { $0.view === self }
        guard let title = pane?.title, !title.isEmpty else { return Self.accessibilityFallbackLabel }
        return title
    }

    override func accessibilityValue() -> Any? {
        accessibility.text()?.string ?? ""
    }

    override func isAccessibilitySelectorAllowed(_ selector: Selector) -> Bool {
        switch selector {
        case #selector(setAccessibilityValue(_:)): !accessibility.monitor.isAttached
        case #selector(setAccessibilitySelectedTextRange(_:)), #selector(setAccessibilitySelectedTextRanges(_:)),
            #selector(setAccessibilitySelectedText(_:)):
            false
        default: super.isAccessibilitySelectorAllowed(selector)
        }
    }

    /// Ignored (D-6): it exists so that Full Keyboard Access, with
    /// VoiceOver off, sends Space to the view as a key press.
    override func setAccessibilityValue(_ value: Any?) {}

    /// Ignored (D-6): it would move neither the cursor nor the selection.
    override func setAccessibilitySelectedTextRange(_ range: NSRange) {}

    override func isAccessibilityFocused() -> Bool {
        guard let window else { return false }
        return window.isKeyWindow && window.firstResponder === self
    }

    override func accessibilityNumberOfCharacters() -> Int {
        accessibility.text()?.length ?? 0
    }

    override func accessibilityVisibleCharacterRange() -> NSRange {
        NSRange(location: 0, length: accessibilityNumberOfCharacters())
    }

    override func accessibilityInsertionPointLineNumber() -> Int {
        guard let text = accessibility.text() else { return 0 }
        return text.line(forIndex: text.insertionPoint)
    }

    override func accessibilitySelectedTextRange() -> NSRange {
        accessibility.text()?.selectedRange ?? NSRange(location: 0, length: 0)
    }

    override func accessibilitySelectedTextRanges() -> [NSValue]? {
        [NSValue(range: accessibilitySelectedTextRange())]
    }

    override func accessibilitySelectedText() -> String? {
        guard let text = accessibility.text() else { return "" }
        return Self.substring(of: text, in: text.selectedRange) ?? ""
    }

    override func accessibilityLine(for index: Int) -> Int {
        accessibility.text()?.line(forIndex: index) ?? 0
    }

    override func accessibilityRange(forLine line: Int) -> NSRange {
        accessibility.text()?.range(forLine: line) ?? NSRange(location: NSNotFound, length: 0)
    }

    override func accessibilityString(for range: NSRange) -> String? {
        guard let text = accessibility.text() else { return range.location == NSNotFound ? nil : "" }
        return Self.substring(of: text, in: range)
    }

    override func accessibilityRange(for index: Int) -> NSRange {
        accessibility.text()?.characterRange(at: index) ?? NSRange(location: 0, length: 0)
    }

    /// The character under a point in screen coordinates; above or left of
    /// the text, the start.
    override func accessibilityRange(for point: NSPoint) -> NSRange {
        guard let window, let text = accessibility.text() else { return NSRange(location: 0, length: 0) }
        let inView = convert(window.convertPoint(fromScreen: point), from: nil)
        let fromTop = isFlipped ? inView.y : bounds.height - inView.y
        guard inView.x >= 0, fromTop >= 0 else { return NSRange(location: 0, length: 0) }
        let (cell, _) = cell(at: inView)
        return text.range(atCol: cell.col, row: cell.row)
    }

    /// The rect, in screen coordinates, enclosing the cells of `range`;
    /// `.zero` without a window or for an `NSNotFound` range.
    override func accessibilityFrame(for range: NSRange) -> NSRect {
        guard range.location != NSNotFound, let window, let text = accessibility.text() else { return .zero }
        let rect = accessibilityViewRect(for: text.cells(for: range))
        return window.convertToScreen(convert(rect, to: nil))
    }

    /// The rect of `cells` in view coordinates, from the metrics the view
    /// last read; `.zero` before the surface is attached.
    func accessibilityViewRect(for cells: TesseraSession.AccessibleText.Cells) -> NSRect {
        guard let metrics, metrics.scale > 0 else { return .zero }
        let scale = CGFloat(metrics.scale)
        let fromTop = Self.cellRect(
            cells, cellWidth: CGFloat(metrics.cellWidthPx) / scale, cellHeight: CGFloat(metrics.cellHeightPx) / scale,
            padding: CGFloat(metrics.paddingPx) / scale)
        let y = isFlipped ? fromTop.minY : bounds.height - fromTop.maxY
        return NSRect(x: fromTop.minX, y: y, width: fromTop.width, height: fromTop.height)
    }

    /// The rect of `cells` with the origin at the top-left corner of the
    /// view and y growing down.
    static func cellRect(
        _ cells: TesseraSession.AccessibleText.Cells, cellWidth: CGFloat, cellHeight: CGFloat, padding: CGFloat
    ) -> CGRect {
        CGRect(
            x: padding + CGFloat(cells.col) * cellWidth, y: padding + CGFloat(cells.row) * cellHeight,
            width: CGFloat(cells.cols) * cellWidth, height: CGFloat(max(cells.rows, 1)) * cellHeight)
    }

    /// `range` of the text, clamped to it; `nil` for an `NSNotFound` one.
    private static func substring(of text: TesseraSession.AccessibleText, in range: NSRange) -> String? {
        guard range.location != NSNotFound else { return nil }
        let string = text.string as NSString
        let start = min(max(range.location, 0), string.length)
        let length = min(max(range.length, 0), string.length - start)
        return string.substring(with: NSRange(location: start, length: length))
    }
}

// MARK: Telling VoiceOver about output

/// Posts accessibility notifications; tests record them instead.
@MainActor
protocol AccessibilityPosting {
    func post(
        element: Any, notification: NSAccessibility.Notification,
        userInfo: [NSAccessibility.NotificationUserInfoKey: Any]?)
}

/// Posts to AppKit, which hands them to the attached assistive clients.
struct SystemAccessibilityPoster: AccessibilityPosting {
    func post(
        element: Any, notification: NSAccessibility.Notification,
        userInfo: [NSAccessibility.NotificationUserInfoKey: Any]?
    ) {
        if let userInfo {
            NSAccessibility.post(element: element, notification: notification, userInfo: userInfo)
        } else {
            NSAccessibility.post(element: element, notification: notification)
        }
    }
}

/// Whether an assistive client that reads output is attached, and when
/// that changes (tests stand in for the machine's VoiceOver).
@MainActor
protocol AssistiveClientMonitoring {
    var isAttached: Bool { get }
    /// Calls `changed` on the main actor when ``isAttached`` may have
    /// changed, until the returned token is released.
    func observe(_ changed: @escaping @MainActor () -> Void) -> AnyObject
}

/// VoiceOver, the only client a public API reports (D-7): Switch Control,
/// Voice Control and Zoom still query the view, but are told about no
/// output.
struct VoiceOverMonitor: AssistiveClientMonitoring {
    var isAttached: Bool { NSWorkspace.shared.isVoiceOverEnabled }

    func observe(_ changed: @escaping @MainActor () -> Void) -> AnyObject {
        NSWorkspace.shared.observe(\.isVoiceOverEnabled) { _, _ in
            DispatchQueue.main.async { MainActor.assumeIsolated { changed() } }
        }
    }
}

/// The clock and the one-shot timer the notifications are timed by (tests
/// advance their own).
@MainActor
protocol AccessibilityScheduling {
    var now: CFTimeInterval { get }
    func schedule(after delay: CFTimeInterval, _ work: @escaping @MainActor () -> Void)
}

struct MainQueueAccessibilityScheduler: AccessibilityScheduling {
    var now: CFTimeInterval { CACurrentMediaTime() }

    func schedule(after delay: CFTimeInterval, _ work: @escaping @MainActor () -> Void) {
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { MainActor.assumeIsolated { work() } }
    }
}

/// Tells an attached VoiceOver about a terminal view's output (ADR 0041,
/// docs/security.md S6), and keeps the snapshot its queries read.
///
/// - Without a client nothing is posted and no snapshot is taken for
///   output: ``contentChanged()`` only drops the cached one.
/// - With one, output settles first: the notifications go out
///   ``settleDelay`` after the last change (cancel and re-arm), or
///   ``maxWait`` after the first while changes keep coming, never more
///   than ``postsPerSecond`` in any second. A settle takes one snapshot
///   and compares it with the last one told: new text is announced (to
///   `NSApp`, ``announcementPriority``) before `valueChanged`, and a moved
///   cursor or selection posts `selectedTextChanged`. A cursor that only
///   moved posts that alone. `layoutChanged` is never posted.
/// - Attaching posts one `valueChanged` (the same way, so within the
///   budget); detaching drops the snapshots. A view out of its window
///   posts nothing.
///
/// Per output callback the work is a few stores, whatever the rate; at
/// most one timer is pending and two snapshots are held. Text is never
/// logged (S8).
@MainActor
final class TerminalAccessibility {
    static let settleDelay: CFTimeInterval = 0.15
    static let maxWait: CFTimeInterval = 1.0
    static let postsPerSecond = 10
    /// The most a settle posts: an announcement, `valueChanged` and
    /// `selectedTextChanged`.
    static let postsPerSettle = 3
    /// D-4: queued after what VoiceOver is saying. iTerm2 uses `.high`.
    static let announcementPriority = NSAccessibilityPriorityLevel.medium

    /// The view the notifications are about.
    weak var element: NSView?
    /// Takes a snapshot of the view's text (tests count the calls).
    var textProvider: @MainActor () throws -> TesseraSession.AccessibleText
    var poster: any AccessibilityPosting = SystemAccessibilityPoster()
    var scheduler: any AccessibilityScheduling = MainQueueAccessibilityScheduler()
    var monitor: any AssistiveClientMonitoring {
        didSet { observeMonitor() }
    }

    private var monitorToken: AnyObject?
    private(set) var isClientAttached = false
    /// The snapshot queries read, until the content changes.
    private var cached: TesseraSession.AccessibleText?
    /// The snapshot the client was last told about.
    private var told: TesseraSession.AccessibleText?
    /// The first and the last change not yet told, while one is pending.
    private var firstChange: CFTimeInterval?
    private var lastChange: CFTimeInterval = 0
    private var settleScheduled = false
    /// When the posts of the last second went out, oldest first.
    private var postTimes: [CFTimeInterval] = []

    init(
        textProvider: @escaping @MainActor () throws -> TesseraSession.AccessibleText,
        monitor: any AssistiveClientMonitoring = VoiceOverMonitor()
    ) {
        self.textProvider = textProvider
        self.monitor = monitor
        observeMonitor()
    }

    /// The view's text now, for a query: the cached snapshot, or a new one;
    /// `nil` once the session is gone.
    func text() -> TesseraSession.AccessibleText? {
        if let cached { return cached }
        cached = try? textProvider()
        return cached
    }

    /// The view's content may have changed (output, a scroll, a selection,
    /// a resize): called per output callback, so it only stores.
    func contentChanged() {
        cached = nil
        guard isClientAttached else { return }
        let now = scheduler.now
        if firstChange == nil { firstChange = now }
        lastChange = now
        scheduleSettle()
    }

    private func observeMonitor() {
        monitorToken = monitor.observe { [weak self] in self?.clientMayHaveChanged() }
        clientMayHaveChanged(posting: false)
    }

    private func clientMayHaveChanged(posting: Bool = true) {
        let attached = monitor.isAttached
        guard attached != isClientAttached else { return }
        isClientAttached = attached
        told = nil
        cached = nil
        firstChange = nil
        guard attached, posting else { return }
        // Settles at once (within the budget): told is nil, so it posts
        // one `valueChanged` for the client to read the view.
        let now = scheduler.now
        firstChange = now
        lastChange = now - Self.settleDelay
        scheduleSettle()
    }

    /// When the pending change may be told: once it settled or waited
    /// long enough, and once the last second's posts leave room for a
    /// settle's.
    private func deadline(at now: CFTimeInterval) -> CFTimeInterval? {
        guard let firstChange else { return nil }
        let settled = min(lastChange + Self.settleDelay, firstChange + Self.maxWait)
        postTimes.removeAll { $0 <= now - 1 }
        let excess = postTimes.count + Self.postsPerSettle - Self.postsPerSecond
        let room = excess > 0 ? postTimes[excess - 1] + 1 : -CFTimeInterval.infinity
        return max(settled, room)
    }

    private func scheduleSettle() {
        guard !settleScheduled, let deadline = deadline(at: scheduler.now) else { return }
        settleScheduled = true
        scheduler.schedule(after: max(0, deadline - scheduler.now)) { [weak self] in
            guard let self else { return }
            settleScheduled = false
            settleIfDue()
        }
    }

    private func settleIfDue() {
        guard isClientAttached, let deadline = deadline(at: scheduler.now) else { return }
        guard scheduler.now >= deadline else {
            scheduleSettle()
            return
        }
        firstChange = nil
        settle()
    }

    private func settle() {
        guard let element, element.window != nil, let current = try? textProvider() else { return }
        cached = current
        let previous = told
        told = current
        guard let previous else {
            post(element, .valueChanged)
            return
        }
        if current.string != previous.string {
            let announcement = current.announcement(since: previous)
            if !announcement.allSatisfy(\.isWhitespace) {
                post(
                    NSApp as Any, .announcementRequested,
                    [.announcement: announcement, .priority: Self.announcementPriority.rawValue])
            }
            post(element, .valueChanged)
        }
        if current.selectedRange != previous.selectedRange || current.hasCursor != previous.hasCursor {
            post(element, .selectedTextChanged)
        }
    }

    private func post(
        _ element: Any, _ notification: NSAccessibility.Notification,
        _ userInfo: [NSAccessibility.NotificationUserInfoKey: Any]? = nil
    ) {
        postTimes.append(scheduler.now)
        poster.post(element: element, notification: notification, userInfo: userInfo)
    }
}
