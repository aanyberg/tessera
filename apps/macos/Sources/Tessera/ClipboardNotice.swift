import AppKit
import TesseraKit

/// When a pane shows its notice of a program's clipboard write (#471, ADR
/// 0022 D-NOTICE52): at most one notice per ``minimumInterval``. A write
/// within the interval is not dropped: it is counted into the held notice,
/// which shows when the interval has passed, so a burst of writes ends with
/// one notice that counts them all and names the latest size.
///
/// Memory: one held ``Notice`` (two integers) per pane, whatever the
/// program sends; the counts saturate.
///
/// A pure value driven by explicit instants: ``TerminalSurfaceView`` owns
/// one and runs the timer that calls ``release(at:)`` at ``dueAt``.
struct ClipboardNoticeThrottle {
    /// The shortest time between two notices of one pane.
    static let minimumInterval: Duration = .seconds(1)

    /// What a notice says: how many writes it reports and the size of the
    /// latest.
    struct Notice: Equatable {
        var writes: Int
        var bytes: Int
    }

    /// The writes waiting for the interval to pass, or `nil`.
    private(set) var held: Notice?
    /// When the previous notice showed, or `nil` before the first.
    private var lastShown: ContinuousClock.Instant?

    /// When the held notice falls due, or `nil` if none is held or none
    /// showed before it (a notice ``hold(_:)`` kept from a pane's first
    /// writes is due at once).
    var dueAt: ContinuousClock.Instant? {
        guard held != nil else { return nil }
        return lastShown.map { $0 + Self.minimumInterval }
    }

    /// Offers `write` at `now`: the notice to show now, or `nil` when it is
    /// counted into the held notice until ``dueAt``.
    mutating func offer(_ write: TesseraClipboardWrite, at now: ContinuousClock.Instant) -> Notice? {
        let notice = Notice(writes: Self.sum(held?.writes ?? 0, max(1, write.writes)), bytes: write.bytes)
        if let lastShown, now - lastShown < Self.minimumInterval {
            held = notice
            return nil
        }
        // The interval has passed; a held notice whose release came late
        // is folded into this one.
        held = nil
        lastShown = now
        return notice
    }

    /// Counts `write` into the held notice without showing anything: its
    /// pane is not on screen (#747). The notice waits for ``release(at:)``,
    /// one held notice as ever, however long the pane stays hidden.
    mutating func hold(_ write: TesseraClipboardWrite) {
        held = Notice(writes: Self.sum(held?.writes ?? 0, max(1, write.writes)), bytes: write.bytes)
    }

    /// Takes the held notice if it is due at `now`: the notice to show now.
    mutating func release(at now: ContinuousClock.Instant) -> Notice? {
        guard let notice = held, dueAt.map({ now >= $0 }) ?? true else { return nil }
        held = nil
        lastShown = now
        return notice
    }

    private static func sum(_ a: Int, _ b: Int) -> Int {
        let (total, overflow) = a.addingReportingOverflow(b)
        return overflow ? Int.max : total
    }
}

extension ClipboardNoticeThrottle.Notice {
    /// The notice's text: fixed words and numbers, never anything the
    /// program sent (`docs/security.md` S1, S3). "At least": the core keeps
    /// the last write of each output chunk, so writes in one chunk count as
    /// one.
    var text: String {
        let size = Self.bytesText(bytes)
        return writes == 1
            ? "A program copied \(size) to the clipboard"
            : "A program copied to the clipboard at least \(Self.count(writes)) times, last \(size)"
    }

    static func bytesText(_ bytes: Int) -> String {
        bytes == 1 ? "1 byte" : "\(count(bytes)) bytes"
    }

    private static func count(_ value: Int) -> String {
        value.formatted(.number.grouping(.automatic))
    }
}

/// The notice itself: a small rounded label over the top of the pane. It
/// never takes the keyboard or a click (a click passes through to the
/// terminal), and it fades out after ``holdDuration``.
@MainActor
final class ClipboardNoticeView: NSView {
    /// How long a notice stays before it fades.
    static let holdDuration: Duration = .milliseconds(2500)

    let label = NSTextField(labelWithString: "")
    private let background = NSVisualEffectView()

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        background.material = .hudWindow
        background.blendingMode = .withinWindow
        background.state = .active
        background.wantsLayer = true
        background.layer?.cornerRadius = 8
        background.layer?.masksToBounds = true
        background.translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)
        label.font = .systemFont(ofSize: 12, weight: .medium)
        label.textColor = .labelColor
        label.lineBreakMode = .byTruncatingTail
        label.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(label)
        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),
            label.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 12),
            label.trailingAnchor.constraint(equalTo: background.trailingAnchor, constant: -12),
            label.topAnchor.constraint(equalTo: background.topAnchor, constant: 6),
            label.bottomAnchor.constraint(equalTo: background.bottomAnchor, constant: -6),
        ])
        // VoiceOver reads the notice once: the view is the element, its
        // label is not.
        label.setAccessibilityElement(false)
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// Clicks and drags reach the terminal under the notice.
    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    var text: String {
        get { label.stringValue }
        set {
            label.stringValue = newValue
            setAccessibilityLabel(newValue)
        }
    }
}

/// A pane's clipboard notices: its ``ClipboardNoticeThrottle``, the timer
/// that shows a held notice when it falls due, and the
/// ``ClipboardNoticeView`` it shows them in, made at the first notice and
/// kept. Each ``TerminalSurfaceView`` owns one.
///
/// A notice shows only while its pane is on screen (#747): a write in a
/// pane behind a maximized one, in a background tab or in a hidden window
/// is counted into the held notice, which shows once the pane is (its host
/// calls ``hostVisibilityChanged()``). It is held, not shown on the pane
/// the user sees: the notice belongs to the pane whose program wrote, and
/// over another pane it would name the wrong program. A notice on screen
/// when its pane hides shows its full time again once the pane is back.
@MainActor
final class PaneClipboardNotice {
    /// The fade in and out, skipped under Reduce Motion.
    static let fadeDuration: TimeInterval = 0.2

    /// The pane the notice shows over.
    weak var host: NSView?
    /// Whether to show and hide without the fade (the pane's scroll bar
    /// follows the system's Reduce Motion setting).
    var reducesMotion: @MainActor () -> Bool = { false }
    /// Whether the user can see the pane now; notices wait until they can.
    var isHostShown: @MainActor () -> Bool = { true }
    /// The find bar's frame in the host while it is open: the notice sits
    /// below it, never under it (#747).
    var obstacle: @MainActor () -> NSRect? = { nil }

    /// The pane's throttle; read by tests.
    private(set) var throttle = ClipboardNoticeThrottle()
    private(set) var view: ClipboardNoticeView?
    private var releaseTask: Task<Void, Never>?
    private var hideTask: Task<Void, Never>?
    /// The notice on screen when the pane hid: it shows again, its full
    /// time, once the pane is back.
    private var hidWhileShowing = false
    /// Notices shown (tests).
    private(set) var shownCount = 0
    /// The latest notice shown and when (tests).
    private(set) var lastShown: ClipboardNoticeThrottle.Notice?
    private(set) var lastShownAt: ContinuousClock.Instant?

    /// Whether a notice is on screen now.
    var isShowing: Bool { view.map { !$0.isHidden } ?? false }

    /// A program wrote the clipboard: show the notice now, or count the
    /// write into the one that shows when the interval has passed.
    func offer(_ write: TesseraClipboardWrite) {
        guard isHostShown() else {
            throttle.hold(write)
            return
        }
        if let notice = throttle.offer(write, at: .now) {
            show(notice)
        }
        scheduleRelease()
    }

    /// The pane was shown or hidden: a held notice shows now, or when the
    /// interval has passed, once the pane is on screen; a notice on screen
    /// as it hides waits for it to be back.
    func hostVisibilityChanged() {
        guard isHostShown() else {
            if isShowing, !hidWhileShowing {
                hidWhileShowing = true
                hideTask?.cancel()
                hideTask = nil
            }
            return
        }
        if hidWhileShowing, let view {
            hidWhileShowing = false
            present(view)
        }
        if let notice = throttle.release(at: .now) {
            show(notice)
        }
        scheduleRelease()
    }

    /// One timer at most, whatever the program sends. A timer that falls
    /// due while the pane is hidden leaves the notice held for
    /// ``hostVisibilityChanged()``.
    private func scheduleRelease() {
        guard releaseTask == nil, let due = throttle.dueAt else { return }
        releaseTask = Task { [weak self] in
            try? await Task.sleep(until: due, clock: .continuous)
            guard let self else { return }
            self.releaseTask = nil
            guard self.isHostShown() else { return }
            if let notice = self.throttle.release(at: .now) {
                self.show(notice)
            }
            self.scheduleRelease()
        }
    }

    private func show(_ notice: ClipboardNoticeThrottle.Notice) {
        guard let host else { return }
        shownCount += 1
        lastShown = notice
        lastShownAt = .now
        let view = self.view ?? makeView(in: host)
        view.text = notice.text
        hidWhileShowing = false
        layout()
        present(view)
        NSAccessibility.post(
            element: view, notification: .announcementRequested,
            userInfo: [
                .announcement: notice.text,
                .priority: NSAccessibilityPriorityLevel.medium.rawValue,
            ])
    }

    /// Shows the view, fading in, and hides it after ``holdDuration``.
    private func present(_ view: ClipboardNoticeView) {
        let reduced = reducesMotion()
        if view.isHidden {
            view.alphaValue = reduced ? 1 : 0
            view.isHidden = false
        }
        if !reduced {
            Self.fade(view, to: 1)
        } else {
            view.alphaValue = 1
        }
        hideTask?.cancel()
        hideTask = Task { [weak self] in
            try? await Task.sleep(for: ClipboardNoticeView.holdDuration)
            guard !Task.isCancelled, let self else { return }
            await self.hide()
        }
    }

    private func hide() async {
        guard let view else { return }
        if !reducesMotion() {
            Self.fade(view, to: 0)
            try? await Task.sleep(for: .seconds(Self.fadeDuration))
            // A newer notice cancelled this hide and shows instead.
            guard !Task.isCancelled else { return }
        }
        view.isHidden = true
    }

    /// An opacity fade, nothing that moves.
    private static func fade(_ view: NSView, to alpha: CGFloat) {
        NSAnimationContext.runAnimationGroup { context in
            context.duration = fadeDuration
            view.animator().alphaValue = alpha
        }
    }

    /// The space between the notice and the pane's edges or the find bar.
    static let margin: CGFloat = 8

    /// Places the notice centred at the top of the pane, as wide as its
    /// text up to the pane's width less the margins, and while the find bar
    /// is open just below the bar, whatever the width: never under it
    /// (#747). A pane too short for both shows the notice as low as it
    /// fits, over the bar (it is the higher subview). The host calls it as
    /// it lays the find bar out.
    func layout() {
        guard let host, let view else { return }
        let bounds = host.bounds
        let size = view.fittingSize
        let width = max(0, min(size.width, bounds.width - 2 * Self.margin))
        var fromTop = Self.margin
        if let bar = obstacle() {
            let below = fromTop + (host.isFlipped ? bar.maxY : bounds.height - bar.minY)
            fromTop = max(Self.margin, min(below, bounds.height - Self.margin - size.height))
        }
        let y = host.isFlipped ? fromTop : bounds.height - fromTop - size.height
        let frame = NSRect(x: (bounds.width - width) / 2, y: y, width: width, height: size.height).integral
        if view.frame != frame { view.frame = frame }
    }

    private func makeView(in host: NSView) -> ClipboardNoticeView {
        let view = ClipboardNoticeView()
        view.isHidden = true
        // Above the find bar, below a dimmed pane's shade.
        if let shade = host.subviews.last(where: { $0 is PaneDimView }) {
            host.addSubview(view, positioned: .below, relativeTo: shade)
        } else {
            host.addSubview(view)
        }
        self.view = view
        return view
    }
}
