import AppKit

/// A terminal window or tab whose first session is starting off the main
/// thread (#722): its window, from ``TerminalWindowController/makeWindow()``,
/// shows ``SpawnPlaceholderView`` until the session runs, and then becomes a
/// ``TerminalWindowController``'s, where it already is in its tab bar. Until
/// then this is the window's delegate, so a close while the session starts
/// is seen (``isClosed``) and the session is closed when it arrives.
@MainActor
final class PendingTerminal: NSObject, NSWindowDelegate {
    let window: TerminalWindow
    /// The spawn returned (with a session or an error).
    var isSettled = false
    /// The window was shown (placed in its tab bar or on screen).
    var isPlaced = false
    /// The window closed before its session ran.
    private(set) var isClosed = false
    /// The pane layout the tab opens with, a restored tab's saved one, or
    /// `nil` for one pane: what the window state saves for the tab while
    /// its session starts (``AppDelegate/currentWindowState(orderedWindows:)``).
    var layout: PaneLayout?
    /// Called when the window closes before its session ran.
    var onClose: (@MainActor () -> Void)?
    /// Keys typed while the placeholder showed, oldest first, for the
    /// session's view to get once it runs (``takeTyped()``): at most
    /// ``maxTypedKeys`` (S5).
    private(set) var typed: [NSEvent] = []
    /// Keys typed past ``maxTypedKeys``, dropped (only their count is kept).
    private(set) var droppedKeys = 0
    /// How many typed keys are kept while a session starts: more than a
    /// fast typist enters in the longest spawn wait (about 10 s, S5); keys
    /// past it are dropped, newest first, and counted.
    static let maxTypedKeys = 256
    /// Clipboard writes a closed tab left unseen while this tab was the one
    /// its tab bar selected (#763): its pane gets them once its session
    /// runs. One notice, however many tabs close.
    private(set) var clipboardNotice: ClipboardNoticeThrottle.Notice?

    init(window: TerminalWindow) {
        self.window = window
        super.init()
        window.delegate = self
    }

    /// Puts the placeholder in the window, for the time it shows.
    func showPlaceholder() {
        let size = window.contentView?.bounds.size ?? NSSize(width: 480, height: 200)
        let placeholder = SpawnPlaceholderView(frame: NSRect(origin: .zero, size: size))
        placeholder.onKeyDown = { [weak self] event in self?.keep(event) }
        window.contentView = placeholder
        window.makeFirstResponder(placeholder)
    }

    /// Keeps a key typed while the session starts, or counts it past the cap.
    func keep(_ event: NSEvent) {
        guard typed.count < Self.maxTypedKeys else {
            droppedKeys += 1
            return
        }
        typed.append(event)
    }

    /// The keys typed while the session started, handed over once; none
    /// are kept after.
    func takeTyped() -> [NSEvent] {
        defer { dropTyped() }
        return typed
    }

    /// Counts a closed tab's unseen clipboard writes into ``clipboardNotice``.
    func holdClipboardNotice(_ notice: ClipboardNoticeThrottle.Notice) {
        clipboardNotice = clipboardNotice?.adding(notice) ?? notice
    }

    /// Takes ``clipboardNotice``.
    func takeClipboardNotice() -> ClipboardNoticeThrottle.Notice? {
        defer { clipboardNotice = nil }
        return clipboardNotice
    }

    /// Forgets the typed keys (the window closed or its session failed).
    func dropTyped() {
        typed.removeAll()
    }

    func windowWillClose(_ notification: Notification) {
        guard !isClosed else { return }
        isClosed = true
        dropTyped()
        onClose?()
    }
}

/// What a terminal window shows while its session starts: the window
/// background and a small spinner, so the window is there at once and reads
/// as busy rather than broken (#722). As the window's first responder it
/// passes the keys typed meanwhile to ``onKeyDown`` (its terminal), so they
/// reach the session once it runs; Command chords no menu took go up the
/// responder chain, as the terminal's own view sends them.
final class SpawnPlaceholderView: NSView {
    let spinner = NSProgressIndicator()
    var onKeyDown: (@MainActor (NSEvent) -> Void)?

    override var acceptsFirstResponder: Bool { true }

    override func keyDown(with event: NSEvent) {
        guard let onKeyDown, !event.modifierFlags.contains(.command) else {
            super.keyDown(with: event)
            return
        }
        onKeyDown(event)
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isIndeterminate = true
        spinner.translatesAutoresizingMaskIntoConstraints = false
        addSubview(spinner)
        NSLayoutConstraint.activate([
            spinner.centerXAnchor.constraint(equalTo: centerXAnchor),
            spinner.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        spinner.startAnimation(nil)
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel("Starting terminal")
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        effectiveAppearance.performAsCurrentDrawingAppearance {
            layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
        }
    }
}

/// One ``AppDelegate/restoreWindows(from:screens:)``: whether a tab of it
/// has failed, so only the first failure is shown.
@MainActor
final class RestoreRun {
    var failed = false
}
