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

    /// When the held notice falls due, or `nil` if none is held.
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

    /// Takes the held notice if it is due at `now`: the notice to show now.
    mutating func release(at now: ContinuousClock.Instant) -> Notice? {
        guard let due = dueAt, now >= due, let notice = held else { return nil }
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
@MainActor
final class PaneClipboardNotice {
    /// The fade in and out, skipped under Reduce Motion.
    static let fadeDuration: TimeInterval = 0.2

    /// The pane the notice shows over.
    weak var host: NSView?
    /// Whether to show and hide without the fade (the pane's scroll bar
    /// follows the system's Reduce Motion setting).
    var reducesMotion: @MainActor () -> Bool = { false }

    private var throttle = ClipboardNoticeThrottle()
    private(set) var view: ClipboardNoticeView?
    private var releaseTask: Task<Void, Never>?
    private var hideTask: Task<Void, Never>?
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
        if let notice = throttle.offer(write, at: .now) {
            show(notice)
        }
        scheduleRelease()
    }

    /// One timer at most, whatever the program sends.
    private func scheduleRelease() {
        guard releaseTask == nil, let due = throttle.dueAt else { return }
        releaseTask = Task { [weak self] in
            try? await Task.sleep(until: due, clock: .continuous)
            guard let self else { return }
            self.releaseTask = nil
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
        NSAccessibility.post(
            element: view, notification: .announcementRequested,
            userInfo: [
                .announcement: notice.text,
                .priority: NSAccessibilityPriorityLevel.medium.rawValue,
            ])
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

    private func makeView(in host: NSView) -> ClipboardNoticeView {
        let view = ClipboardNoticeView()
        view.isHidden = true
        view.translatesAutoresizingMaskIntoConstraints = false
        host.addSubview(view)
        NSLayoutConstraint.activate([
            view.centerXAnchor.constraint(equalTo: host.centerXAnchor),
            view.topAnchor.constraint(equalTo: host.topAnchor, constant: 8),
            view.widthAnchor.constraint(lessThanOrEqualTo: host.widthAnchor, constant: -16),
        ])
        self.view = view
        return view
    }
}
