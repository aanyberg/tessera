import AppKit
import QuartzCore
import TesseraKit
import os

/// A tab's terminal: the core draws the session into the `CAMetalLayer` of
/// its ``TerminalRenderView`` subview (ADR 0017), beside or under the
/// scroll bar (ADR 0032).
///
/// - Surface: attached when the view enters a window, detached when it
///   leaves or the window closes. Size changes reach the core through
///   ``TesseraSurface/resize(widthPx:heightPx:scale:)``; during a live
///   window resize at most one per display refresh does, and the final size
///   is sent when the live resize ends.
/// - Drawing: a display link (`NSView.displayLink(target:selector:)`) renders
///   once per refresh while something changed (output, focus, size), and is
///   paused while the window is occluded or minimised.
/// - Keyboard: `keyDown` goes through `interpretKeyEvents`, so dead keys and
///   input methods compose through `NSTextInputClient`. Committed text is
///   sent as typed text (never as a paste); keys are mapped from their key
///   code. Keys pressed while text is marked belong to the input method and
///   send nothing of their own.
/// - Mouse: presses, releases, drags and plain motion go to the core in cell
///   and pixel coordinates; the core reports them to a program that tracks
///   the mouse and says so (`.consumed`), and leaves the rest to the view.
///   The click that activates the window is not sent.
/// - Selection and links (#30): a press the program did not take selects
///   through the core (a drag, a double-click word, a triple-click line,
///   Option or ⌥⌘ for a block, Shift to adjust), scrolling the view while
///   the pointer is above or below it. Command-hover underlines a link under
///   the pointer and shows a pointing hand; a Command-click opens it through
///   ``linkOpener`` when ``LinkPolicy`` allows. Copy and Select All are the
///   Edit menu's; nothing is copied on select.
/// - Scrollback: the wheel and trackpad move the view through the core;
///   the View menu's ⌘↑/⌘↓ page and ⌘Home/⌘End jump (``scroll(to:)``);
///   ``TerminalScroller`` shows and moves the position. Typing, pasting and
///   starting a composition return to the live screen, and drop the rest
///   of a trackpad gesture's momentum. Nothing scrolls while text is
///   marked.
@MainActor
final class TerminalSurfaceView: NSView, TerminalContentView {
    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "surface")

    let session: TesseraSession
    let surface: TesseraSurface

    /// The status shown once the session's child has failed, or `nil` while
    /// it runs.
    private(set) var status: String? {
        didSet {
            statusLabel.stringValue = status ?? ""
            statusLabel.isHidden = status == nil
        }
    }

    // MARK: Test seams (internal)

    /// Resizes that reached the core (attach excluded).
    private(set) var coreResizeCount = 0
    /// `render` calls made.
    private(set) var renderCount = 0
    /// Display link callbacks, real or driven by a test.
    private(set) var tickCount = 0
    /// Attach attempts made, first attach and re-attaches.
    private(set) var attachAttemptCount = 0
    #if DEBUG
        /// Every key event handed to the session, in order. Debug builds
        /// only (tests): a release build keeps no per-key history.
        private(set) var sentKeys: [TesseraSession.KeyEvent] = []
    #endif
    /// The focus state last sent to the session.
    private(set) var sentFocus: Bool?
    /// Forces the occlusion state the display link follows (tests; the
    /// hosted test app's windows always report themselves occluded).
    var occlusionOverride: Bool? {
        didSet { updateDisplayLinkPause() }
    }
    /// The status ``render()`` acts on instead of the one the core reported
    /// for the frame it just rendered (tests; errors still come from the
    /// core): the hosted test app's windows are never on screen, so their
    /// layers vend no drawables and every real frame is `.notReady`, and
    /// whether one is `.paced` depends on the clock. `nil` uses the core's.
    var frameStatusOverride: TesseraSurface.FrameStatus?

    // MARK: State

    private(set) var displayLink: CADisplayLink?
    /// The window the link and observers are set up for.
    private weak var observedWindow: NSWindow?
    /// Something changed since the last frame. Setting it wakes the display
    /// link.
    private var needsRender = true {
        didSet { if needsRender != oldValue { updateDisplayLinkPause() } }
    }
    private var isLiveResizing = false
    private var pendingResize = false {
        didSet { if pendingResize != oldValue { updateDisplayLinkPause() } }
    }
    /// The surface was attached once and lost since (a lost swapchain); the
    /// display link re-attaches it, at most once a second.
    private var wantsReattach = false {
        didSet { if wantsReattach != oldValue { updateDisplayLinkPause() } }
    }
    /// Consecutive frames reported `SUBOPTIMAL`; the size is resent only a
    /// few times in a row.
    private var suboptimalFrames = 0
    private static let maxSuboptimalResends = 3
    /// The current tick already passed a resize on to the core.
    private var resizedThisTick = false
    private var lastAttachAttempt: ContinuousClock.Instant?
    /// When the surface was last attached, or `nil` once a call found it
    /// dropped since.
    private var lastAttach: ContinuousClock.Instant?
    /// Time between failed re-attach attempts, and how long an attached
    /// surface must last for its drop not to count as a failed attempt
    /// (tests change it).
    static var reattachInterval: Duration = .seconds(1)
    /// Consecutive re-attach attempts that may fail before the view stops
    /// trying; a later size, scale or window change tries again.
    static let maxReattachFailures = 5
    /// Consecutive failed re-attach attempts. A drop within
    /// ``reattachInterval`` of a successful attach counts as one, so an
    /// attach does not reset it: the next drop does, once the surface lasted
    /// the interval. A surface the core drops on every frame is then given
    /// up on as one that never attaches is.
    private(set) var reattachFailures = 0
    #if DEBUG
        /// Runs right after every successful attach, before the view reads
        /// the new surface's metrics (tests: a core that drops the surface
        /// again at once).
        var didAttachForTesting: (() -> Void)?
    #endif
    /// The size at which re-attaching was given up, or `nil`.
    private var reattachGaveUpAt: TesseraSurface.Size?
    /// The view left its window while its surface was lost: the next window
    /// retries it.
    private var lostSurfaceAcrossMove = false
    private var windowObservers: [any NSObjectProtocol] = []
    /// Observers registered on the window (tests).
    var windowObserverCount: Int { windowObservers.count }
    private var isFocused = false

    // Input method state. `markedText` is the composition the input method
    // is editing; nothing of it reaches the session until it is committed.
    private var markedText = NSMutableAttributedString()
    private var markedSelection = NSRange(location: NSNotFound, length: 0)
    /// Text committed by `insertText` during the current `keyDown`, or `nil`
    /// outside one.
    private var keyTextAccumulator: [String]?
    /// Selectors `doCommand(by:)` received during the current `keyDown`.
    private var keyCommands: [Selector] = []
    /// Set while a composition is being discarded: commits are dropped.
    private var isDiscardingMarkedText = false
    private let markedTextLayer = CATextLayer()
    private let statusLabel = NSTextField(labelWithString: "")

    // Scrollback state.

    /// The scroll bar along the right edge: over the text in overlay style,
    /// beside it in legacy style (ADR 0032).
    let scroller = TerminalScroller()
    /// Momentum events are dropped until the next gesture begins: a key, a
    /// paste or a keyboard scroll moved the view since the gesture whose
    /// momentum is still arriving, which would otherwise carry the view
    /// away from where the user just put it (the live screen, after
    /// typing). See ``scrollWheel(with:)`` for what counts as a new gesture.
    private(set) var ignoresMomentum = false {
        didSet { if ignoresMomentum && !oldValue { phasedEventSinceIgnoring = false } }
    }
    /// A scroll event with a gesture phase arrived since ``ignoresMomentum``
    /// was set (a later key while it is set does not reset this): momentum
    /// that begins now ends that gesture, not a new one.
    private var phasedEventSinceIgnoring = false
    /// Scroll events handed to the session (tests).
    private(set) var sentScrollCount = 0

    // Menu state (Edit and View menus).

    /// Whether the core holds a selection: Copy is enabled only then. Read
    /// from the core each time, since output that changes a selected line
    /// drops the selection there.
    var hasSelection: Bool { (try? session.hasSelection) ?? false }
    /// Where Paste reads from. Tests use a private pasteboard, so the user's
    /// clipboard is neither read nor changed.
    var pasteboard: NSPasteboard = .general
    /// Pastes handed to the session (tests).
    private(set) var pasteCount = 0

    // Selection and link state (#30).

    /// What a Command-click opens a link with. Tests record instead.
    var linkOpener: any LinkOpening = WorkspaceLinkOpener()
    /// Scrolls the view while a selection drag has the pointer outside it.
    let autoscroller = SelectionAutoscroller()
    /// The link under the pointer while Command is held (the string the core
    /// detected), or `nil`.
    private(set) var hoveredLink: String?
    private var hoveredCell: GridCell?
    /// Where the pointer last was, in view coordinates, or `nil` once it
    /// left: Command pressed or released later re-tests this spot.
    private var lastPointer: NSPoint?
    /// The left-button gesture the view handles itself (the core did not
    /// report the press to a program), from press to release.
    private var gesture: PointerGesture?
    #if DEBUG
        /// Every selection call the gestures made, in order. Debug builds
        /// only (tests).
        private(set) var selectionCalls: [SelectionCall] = []
    #endif
    /// The font size the surface was last attached with: the configured size,
    /// which Actual Size (⌘0) returns to. `nil` before the first attach.
    private(set) var attachedFontSizePt: Float?
    /// The size font zoom chose, or `nil` at the configured size. The core
    /// starts a re-attached surface (a lost swapchain, a tab moved to another
    /// window) at the configured size, so the zoom is applied again then.
    private(set) var zoomedFontSizePt: Float?

    init(session: TesseraSession, frame: NSRect = NSRect(x: 0, y: 0, width: 480, height: 200)) {
        self.session = session
        surface = TesseraSurface(session: session)
        renderView = TerminalRenderView(frame: NSRect(origin: .zero, size: frame.size))
        super.init(frame: frame)
        wantsLayer = true
        addSubview(renderView)

        markedTextLayer.isHidden = true
        markedTextLayer.contentsScale = metalLayer.contentsScale
        markedTextLayer.anchorPoint = .zero
        metalLayer.addSublayer(markedTextLayer)

        statusLabel.isHidden = true
        statusLabel.font = .monospacedSystemFont(ofSize: 12, weight: .medium)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        addSubview(statusLabel)
        NSLayoutConstraint.activate([
            statusLabel.centerXAnchor.constraint(equalTo: centerXAnchor),
            statusLabel.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -8),
        ])

        scroller.onScroll = { [weak self] target in self?.scrollerMoved(to: target) }
        scroller.onStyleChange = { [weak self] in self?.scrollerStyleChanged() }
        addSubview(scroller)
        layoutRenderViewAndScroller()

        // Plain motion (no button held) while the window is key, for
        // programs that track every motion (DECSET 1003). AppKit keeps the
        // area on the visible rect, so it is never replaced.
        offersMouseDownToInputContext = { [weak self] event in self?.inputContext?.handleEvent(event) ?? false }
        // Entering and leaving end a command-hover; `cursorUpdate` restores
        // the text cursor over a link's hand.
        addTrackingArea(
            NSTrackingArea(
                rect: .zero, options: [.mouseMoved, .mouseEnteredAndExited, .cursorUpdate, .activeInKeyWindow, .inVisibleRect],
                owner: self, userInfo: nil))
        autoscroller.onTick = { [weak self] direction, lines in self?.autoscrollTicked(direction, lines: lines) }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Layer

    /// The subview whose backing layer the core draws into: the whole view
    /// with an overlay scroll bar, the view less the scroll bar's width
    /// with a legacy one (ADR 0032).
    let renderView: TerminalRenderView

    /// The layer the core draws into.
    var metalLayer: CAMetalLayer { renderView.metalLayer }

    /// Lays out the render view and the scroll bar for the scroll bar's
    /// style: a legacy scroll bar sits beside the text and takes its width
    /// off the grid, an overlay one floats over the text. The width depends
    /// on the style only, never on the history or the screen shown, so the
    /// alternate screen never resizes the grid.
    private func layoutRenderViewAndScroller() {
        let style = scroller.effectiveStyle
        let reserved = style == .legacy ? TerminalScroller.width(for: .legacy) : 0
        let render = NSRect(x: 0, y: 0, width: max(0, bounds.width - reserved), height: bounds.height)
        let width = TerminalScroller.width(for: style)
        let bar = NSRect(x: bounds.width - width, y: 0, width: width, height: bounds.height)
        guard render != renderView.frame || bar != scroller.frame else { return }
        renderView.frame = render
        scroller.frame = bar
        window?.invalidateCursorRects(for: self)
    }

    /// The system's "Show scroll bars" setting (or the test override)
    /// changed: the grid takes the new width, through the same path as a
    /// window resize (one core resize, coalesced during a live resize).
    private func scrollerStyleChanged() {
        layoutRenderViewAndScroller()
        window?.invalidateCursorRects(for: self)
        surfaceSizeChanged()
    }

    // MARK: Window

    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        guard window !== newWindow, window != nil else { return }
        tearDown()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        // Moved to another superview within the same window: already set up.
        guard window !== observedWindow || displayLink == nil else { return }
        tearDown()
        observedWindow = window
        if lostSurfaceAcrossMove {
            // A surface lost before the move (being retried, or given up on)
            // is retried in the new window, not treated as a first attach.
            lostSurfaceAcrossMove = false
            reattachGaveUpAt = nil
            reattachFailures = 0
            lastAttachAttempt = nil
            wantsReattach = true
        }
        window.initialFirstResponder = self
        let center = NotificationCenter.default
        let names: [Notification.Name] = [
            NSWindow.didChangeOcclusionStateNotification, NSWindow.didMiniaturizeNotification,
            NSWindow.didDeminiaturizeNotification,
        ]
        for name in names {
            windowObservers.append(
                center.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.updateDisplayLinkPause() }
                })
        }
        windowObservers.append(
            center.addObserver(forName: NSWindow.willCloseNotification, object: window, queue: .main) {
                [weak self] _ in
                MainActor.assumeIsolated { self?.windowWillClose() }
            })
        for (name, isKey) in [(NSWindow.didBecomeKeyNotification, true), (NSWindow.didResignKeyNotification, false)] {
            windowObservers.append(
                center.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.windowKeyChanged(isKey: isKey) }
                })
        }
        // Called as the group's selection changes, on the main thread.
        tabSelectionObservation = window.observe(\.tabGroup?.selectedWindow) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.tabSelectionChanged() }
        }
        metalLayer.contentsScale = window.backingScaleFactor
        markedTextLayer.contentsScale = window.backingScaleFactor
        layoutRenderViewAndScroller()
        attachSurface()
        let link = displayLink(target: DisplayLinkTarget(self), selector: #selector(DisplayLinkTarget.tick(_:)))
        link.add(to: .main, forMode: .common)
        displayLink = link
        updateDisplayLinkPause()
    }

    /// The window closes: tears down, deactivates the view's input context
    /// and then takes the view out of the closed window. An active input
    /// context (AppKit activates the one of the key window's first
    /// responder) retains this view until it is deactivated, and AppKit may
    /// keep a closed window alive after its controller is gone (seen on the
    /// macOS 15 CI runner), the window holding its content view; either
    /// would keep a closed tab's view and session alive. The view leaves
    /// once the main actor is next free, so the closing window never draws
    /// without it. A closed window is never shown again.
    private func windowWillClose() {
        tearDown()
        inputContext?.deactivate()
        let closedWindow = window
        Task { @MainActor [weak self, weak closedWindow] in
            guard let self, let closedWindow, window === closedWindow else { return }
            removeFromSuperview()
        }
    }

    /// Stops the display link and detaches the surface; the window is going
    /// away (or the view leaves it).
    private func tearDown() {
        releaseHeldButtons()
        tabSelectionObservation?.invalidate()
        tabSelectionObservation = nil
        observedWindow = nil
        autoscroller.stop()
        gesture = nil
        displayLink?.invalidate()
        displayLink = nil
        for observer in windowObservers {
            NotificationCenter.default.removeObserver(observer)
        }
        windowObservers.removeAll()
        // Kept across repeated tear-downs (leaving the old window, then
        // setting up in the new one); a successful attach clears it.
        lostSurfaceAcrossMove = lostSurfaceAcrossMove || wantsReattach || reattachGaveUpAt != nil
        wantsReattach = false
        if surface.isAttached {
            do {
                try surface.detach()
            } catch TesseraCoreError.sessionNotFound, TesseraCoreError.noSurface {
                // The session closed first, which detaches.
            } catch {
                Self.log.error("detach failed: \(String(describing: error), privacy: .public)")
            }
        }
        metrics = nil
    }

    /// Whether the window is hidden from the user: minimised, occluded or
    /// gone. The display link is paused meanwhile.
    var isOccluded: Bool {
        if let occlusionOverride { return occlusionOverride }
        guard let window else { return true }
        return window.isMiniaturized || !window.occlusionState.contains(.visible)
    }

    /// Whether the display link is paused.
    var isDisplayLinkPaused: Bool { displayLink?.isPaused ?? true }

    /// Whether a refresh has work: a frame to draw, a resize to pass on or a
    /// surface to re-attach. With none, the link idles (paused), saving
    /// energy on a quiet terminal.
    /// A frame counts only while a surface is attached: without one (the
    /// first attach failed, and is not retried) the link would otherwise
    /// tick forever with nothing to draw.
    private var hasPendingWork: Bool { (surface.isAttached && needsRender) || pendingResize || wantsReattach }

    /// Pauses the link while the window is hidden or there is nothing to do,
    /// and runs it otherwise. A window that becomes visible again is redrawn:
    /// what it showed may be stale.
    private func updateDisplayLinkPause() {
        guard let displayLink else { return }
        let wasHidden = isHiddenForDisplayLink
        isHiddenForDisplayLink = isOccluded
        if wasHidden && !isHiddenForDisplayLink && !needsRender {
            needsRender = true  // calls back here
            return
        }
        displayLink.isPaused = isHiddenForDisplayLink || !hasPendingWork
    }

    /// The occlusion state the link last followed.
    private var isHiddenForDisplayLink = false

    // MARK: Surface size

    /// The render view's size in physical pixels and its backing scale.
    var pixelSize: TesseraSurface.Size {
        let backing = renderView.convertToBacking(renderView.bounds).size
        let scale = window?.backingScaleFactor ?? metalLayer.contentsScale
        return TesseraSurface.Size(
            widthPx: UInt32(max(0, backing.width.rounded())), heightPx: UInt32(max(0, backing.height.rounded())),
            scale: Float(scale))
    }

    private func attachSurface() {
        guard !surface.isAttached, window != nil else { return }
        attachAttemptCount += 1
        lastAttachAttempt = .now
        let size = pixelSize
        do {
            try surface.attach(layer: metalLayer, widthPx: size.widthPx, heightPx: size.heightPx, scale: size.scale)
            // Cleared before the reads below: one that finds the surface
            // dropped again asks for the next re-attach, which this must not
            // undo. Only failed attempts are spaced by `reattachInterval`, so
            // a drop right after this attach re-attaches on the next tick;
            // it counts as a failed attempt, though (`reattachIfDropped`), so
            // a re-attach keeps `reattachFailures`. Any other attach (a new
            // window, its surface not lost) starts the count afresh.
            if !wantsReattach {
                reattachFailures = 0
            }
            wantsReattach = false
            reattachGaveUpAt = nil
            lostSurfaceAcrossMove = false
            lastAttachAttempt = nil
            lastAttach = .now
            #if DEBUG
                didAttachForTesting?()
            #endif
            refreshMetrics()
            // Dropped again by that read: the surface's size is unknown, and
            // the zoom waits for the re-attach.
            if surface.isAttached {
                surfaceAttachedForZoom()
                needsRender = true
                applyLayerOpacity()
            }
        } catch {
            // Without a renderer (no GPU, no font) the session still runs and
            // takes input; there is just nothing to see. A first attach that
            // fails is deliberately not retried: no GPU or font appears
            // later. Only a surface lost after attaching is (`wantsReattach`),
            // a few times, then not until the size, scale or window changes.
            if wantsReattach {
                countReattachFailure(at: size, error)
            } else {
                Self.log.error("attaching the surface failed: \(String(describing: error), privacy: .public)")
            }
        }
        // Whether a surface is attached changes what counts as work.
        updateDisplayLinkPause()
    }

    /// The core makes the layer non-opaque for a translucent theme; the
    /// window must then be transparent too, or it shows its own background
    /// through the layer. On attach the window follows the layer the core
    /// has just configured.
    private func applyLayerOpacity() {
        setWindowOpaque(metalLayer.isOpaque)
    }

    /// The core re-themed every view (a live edit of the configuration or
    /// of the theme in use): the window follows `opaque`, and a frame is
    /// requested, since an idle view would otherwise keep its old colours
    /// until its next output. The layer is left to the core, which sets its
    /// `opaque` property when it reconfigures the surface at the start of
    /// that frame, the one that draws the new colours: made opaque here
    /// already, the layer would show its last translucent frame
    /// (premultiplied, so darkened) as opaque until then. A view without a
    /// surface (its first attach failed) is left alone: it draws nothing,
    /// so a transparent window would show only the desktop.
    func applyLayerOpacity(opaque: Bool) {
        configurationDidChangeFontSize()
        guard surface.isAttached else { return }
        setWindowOpaque(opaque)
        needsRender = true
    }

    /// Makes the window opaque, or transparent with a clear background,
    /// and has its shadow recomputed when that changes.
    private func setWindowOpaque(_ opaque: Bool) {
        guard let window, window.isOpaque != opaque else { return }
        window.isOpaque = opaque
        window.backgroundColor = opaque ? .windowBackgroundColor : .clear
        window.invalidateShadow()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        layoutRenderViewAndScroller()
        surfaceSizeChanged()
    }

    override func layout() {
        super.layout()
        layoutRenderViewAndScroller()
        surfaceSizeChanged()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        // The overlay's colours follow light and dark mode.
        updateMarkedTextLayer()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        if let scale = window?.backingScaleFactor {
            metalLayer.contentsScale = scale
            markedTextLayer.contentsScale = scale
        }
        surfaceSizeChanged()
    }

    override func viewWillStartLiveResize() {
        super.viewWillStartLiveResize()
        isLiveResizing = true
    }

    override func viewDidEndLiveResize() {
        super.viewDidEndLiveResize()
        isLiveResizing = false
        resizeSurface()
    }

    /// During a live resize every size change re-wraps the scrollback under
    /// the terminal lock (10-100 ms at 100k lines), so only the display link
    /// passes one on per refresh; otherwise it goes to the core at once.
    private func surfaceSizeChanged() {
        if let size = reattachGaveUpAt, size != pixelSize {
            resumeReattach()
        }
        if isLiveResizing {
            pendingResize = true
        } else {
            resizeSurface()
        }
    }

    private func resizeSurface() {
        pendingResize = false
        guard surface.isAttached else { return }
        let size = pixelSize
        guard size != surface.size else { return }
        do {
            try surface.resize(widthPx: size.widthPx, heightPx: size.heightPx, scale: size.scale)
            refreshMetrics()
            coreResizeCount += 1
            suboptimalFrames = 0
            needsRender = true
            updateMarkedTextLayer()
        } catch {
            guard !reattachIfDropped(error, wasAttached: true) else {
                Self.log.info("core dropped the surface; re-attaching")
                return
            }
            Self.log.error("resizing the surface failed: \(String(describing: error), privacy: .public)")
        }
    }

    /// Whether `error`, from a surface call made while the surface was
    /// attached (`wasAttached`), says the core dropped the surface since: a
    /// lost swapchain or GPU device, which the frame that found it reported
    /// as `.gpu`. `TesseraSurface` has then let go of it too, and the display
    /// link attaches it again, whichever call (a resize, a frame, a metrics
    /// read) noticed first. A view that had no surface (its first attach
    /// failed) is left alone: that attach is not retried.
    ///
    /// A drop within ``reattachInterval`` of the attach counts as a failed
    /// re-attach attempt (counted once, by the first call that finds it), so
    /// a core that drops every surface it is given is tried a few times and
    /// then left until the size, scale or window changes, as a refused
    /// attach is; a drop after that resets the count.
    @discardableResult
    private func reattachIfDropped(_ error: any Error, wasAttached: Bool) -> Bool {
        guard wasAttached, error as? TesseraCoreError == .noSurface else { return false }
        // Given up on by an earlier call that found the same drop.
        guard reattachGaveUpAt == nil else { return true }
        if !wantsReattach, let attachedAt = lastAttach {
            lastAttach = nil
            if ContinuousClock.now - attachedAt < Self.reattachInterval {
                guard !countReattachFailure(at: pixelSize, error) else { return true }
            } else {
                reattachFailures = 0
            }
        }
        wantsReattach = true
        needsRender = true
        return true
    }

    /// Counts a failed re-attach attempt and, after the last one allowed,
    /// stops re-attaching until the size differs from `size`. Returns
    /// whether it gave up.
    @discardableResult
    private func countReattachFailure(at size: TesseraSurface.Size, _ error: any Error) -> Bool {
        reattachFailures += 1
        guard reattachFailures >= Self.maxReattachFailures else { return false }
        wantsReattach = false
        reattachGaveUpAt = size
        // Found by a resize or a frame, with the link running: nothing is
        // left to do, so it idles.
        updateDisplayLinkPause()
        Self.log.error(
            "re-attaching the surface failed \(self.reattachFailures) times; retrying after the next size change: \(String(describing: error), privacy: .public)")
        return true
    }

    /// Tries re-attaching a lost surface again after giving up.
    private func resumeReattach() {
        guard reattachGaveUpAt != nil else { return }
        reattachGaveUpAt = nil
        reattachFailures = 0
        lastAttachAttempt = nil
        wantsReattach = true
    }

    // MARK: Display link

    /// One display refresh: applies a coalesced resize, re-attaches a lost
    /// surface and renders if anything changed.
    func displayLinkDidFire() {
        tickCount += 1
        let resizesBefore = coreResizeCount
        defer { resizedThisTick = false }
        if pendingResize {
            resizeSurface()
        }
        if !surface.isAttached {
            if wantsReattach, lastAttachAttempt.map({ ContinuousClock.now - $0 >= Self.reattachInterval }) ?? true {
                attachSurface()
            }
            guard surface.isAttached else { return }
        }
        // A refresh already queued when the window became hidden.
        guard needsRender, !isOccluded else { return }
        needsRender = false
        resizedThisTick = coreResizeCount != resizesBefore
        render()
    }

    private func render() {
        renderCount += 1
        refreshMetrics()
        updateScroller()
        do {
            let status = try surface.render()
            switch frameStatusOverride ?? status {
            case .presented, .skipped:
                // Drawn, or nothing changed: nothing is pending, and the link
                // idles unless something new arrived meanwhile.
                suboptimalFrames = 0
            case .paced:
                // Too soon after the previous frames: the newest content is
                // not drawn yet, so the next refresh draws it.
                needsRender = true
            case .notReady:
                // No drawable (none in time, or the window just became
                // hidden): what changed is still pending and the next refresh
                // retries, while visible (an occluded window pauses the link
                // and redraws when it shows again). A zero-sized surface
                // waits instead for the resize that gives it a size, which
                // asks for a frame itself.
                let size = surface.size
                needsRender = (size?.widthPx ?? 0) > 0 && (size?.heightPx ?? 0) > 0
            case .suboptimal:
                // The drawables no longer match the layer: resend the size,
                // a few times in a row at most, so a layer that stays
                // suboptimal does not resize the session every refresh.
                // A tick that already passed a resize on (a live resize) sends
                // no second one: the next frame tries again.
                needsRender = true
                guard !resizedThisTick else { return }
                suboptimalFrames += 1
                guard suboptimalFrames <= Self.maxSuboptimalResends else {
                    needsRender = false
                    return
                }
                let size = pixelSize
                try surface.resize(widthPx: size.widthPx, heightPx: size.heightPx, scale: size.scale)
                coreResizeCount += 1
            }
        } catch TesseraCoreError.gpu {
            // Drawing failed; the next frame may succeed, or report the
            // surface detached after a lost swapchain or GPU device.
            needsRender = true
        } catch TesseraCoreError.noSurface {
            // The core detached the surface (lost swapchain or GPU device):
            // attach again, on a new device after a device loss.
            reattachIfDropped(TesseraCoreError.noSurface, wasAttached: true)
        } catch {
            // The session is closed; the window closes with it.
            displayLink?.isPaused = true
        }
    }

    // MARK: TerminalContentView

    func sessionDidOutput() {
        needsRender = true
    }

    func sessionDidExit(code: Int32, signal: Int32) {
        status = signal == 0 ? "shell exited with code \(code)" : "shell killed by signal \(signal)"
        needsRender = true
    }

    // MARK: Focus

    override var acceptsFirstResponder: Bool { true }

    override func becomeFirstResponder() -> Bool {
        isFocused = true
        sendFocus(window?.isKeyWindow ?? false)
        return true
    }

    override func resignFirstResponder() -> Bool {
        isFocused = false
        discardComposition()
        sendFocus(false)
        return true
    }

    /// The window became or stopped being key: the session is focused while
    /// its window is key and the view is the first responder. Losing key
    /// status ends a composition like losing first responder does.
    func windowKeyChanged(isKey: Bool) {
        if !isKey {
            discardComposition()
            // A drag the window lost key status in goes on selecting only
            // while it is delivered; the timer must not keep scrolling
            // after it. The selection stays as it is.
            autoscroller.stop()
        }
        sendFocus(isKey && isFocused)
    }

    /// Another tab of the window's group was selected: AppKit sends a
    /// button held meanwhile no release here, so the core gets it now.
    private func tabSelectionChanged() {
        guard let window, let selected = window.tabGroup?.selectedWindow, selected !== window else { return }
        releaseHeldButtons()
    }

    /// Ends a composition without committing it (D5). The input context may
    /// answer the discard with `insertText`; nothing it commits meanwhile is
    /// sent.
    private func discardComposition() {
        guard hasMarkedText() else { return }
        clearMarkedText()
        isDiscardingMarkedText = true
        inputContext?.discardMarkedText()
        isDiscardingMarkedText = false
    }

    /// Tells the session whether it has keyboard focus.
    private func sendFocus(_ focused: Bool) {
        guard focused != sentFocus else { return }
        sentFocus = focused
        needsRender = true
        do {
            try session.setFocused(focused)
        } catch {
            Self.log.debug("focus not sent: \(String(describing: error), privacy: .public)")
        }
    }

    override func resetCursorRects() {
        for (rect, cursor) in cursorRects() {
            addCursorRect(rect, cursor: cursor)
        }
    }

    /// The pointer's cursor: an I-beam over the text, an arrow over the
    /// scroll bar while it is not hidden (an overlay one shows when the
    /// pointer reaches it). The rects do not overlap, since AppKit leaves
    /// overlapping cursor rects undefined: over an overlay scroll bar the
    /// I-beam stops at its edge.
    func cursorRects() -> [(NSRect, NSCursor)] {
        guard !scroller.isHidden else { return [(bounds, textCursor)] }
        let text = renderView.frame
        let bar = scroller.frame
        let beam = NSRect(
            x: text.minX, y: text.minY, width: max(0, min(text.maxX, bar.minX) - text.minX), height: text.height)
        return [(beam, textCursor), (bar, .arrow)]
    }

    /// The cursor over the text: the pointing hand over a link while Command
    /// is held, the I-beam otherwise.
    var textCursor: NSCursor { hoveredLink == nil ? .iBeam : .pointingHand }

    // MARK: Keyboard

    override func keyDown(with event: NSEvent) {
        processKeyDown(event) {
            interpretKeyEvents([event])
        }
    }

    /// Handles one key press. `interpret` runs the input system on the event
    /// (`interpretKeyEvents`), which calls back ``insertText(_:replacementRange:)``,
    /// ``setMarkedText(_:selectedRange:replacementRange:)`` and
    /// ``doCommand(by:)``; tests pass the calls AppKit would make instead,
    /// because a synthetic event goes through the machine's real input source.
    func processKeyDown(_ event: NSEvent, interpret: () -> Void) {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let markedBefore = hasMarkedText()
        // Command combinations that are not menu key equivalents go up the
        // responder chain (and end in a beep), not to the session: the core
        // has no Super bindings yet and would encode them as kitty `CSI u`
        // sequences the shell would print. Revisit when the core gains Super
        // bindings (#43).
        if !markedBefore && flags.contains(.command) {
            super.keyDown(with: event)
            return
        }
        // Control combinations are shortcuts, not text: they skip the input
        // system unless it is composing (Ctrl-J/K are Japanese input
        // shortcuts then).
        if !markedBefore && flags.contains(.control) {
            sendKey(for: event, selector: nil)
            return
        }

        keyTextAccumulator = []
        keyCommands = []
        interpret()
        let committed = (keyTextAccumulator ?? []).joined()
        let commands = keyCommands
        keyTextAccumulator = nil
        keyCommands = []

        if markedBefore || hasMarkedText() {
            // The input method owns this key (composition, its confirming
            // Return, cancelling Escape, Backspace in the preedit): only what
            // it committed is sent (D3).
            if let text = Self.typedText(committed) {
                sendCommittedText(text)
            }
            return
        }
        if let text = Self.typedText(committed) {
            // Text the input system committed always goes as text, never as
            // the key with Alt (D8).
            sendTyped(text, for: event)
        } else {
            // Nothing committed, or only a control or function-key character
            // standing for the key itself.
            sendKey(for: event, selector: commands.last)
        }
    }

    /// AppKit's private-use function-key characters (`NSUpArrowFunctionKey`
    /// U+F700 to `NSModeSwitchFunctionKey` U+F747, reserved to U+F8FE).
    /// U+F8FF is not among them: it is the Apple logo, ordinary text
    /// (Option-Shift-K on US, Option-a on Swedish).
    static let functionKeyCharacters: ClosedRange<UInt32> = 0xF700...0xF8FE

    /// Whether `scalar` is not text: a C0 or C1 control, DEL, or a function
    /// key's private-use character.
    static func isKeyCharacter(_ scalar: Unicode.Scalar) -> Bool {
        scalar.value < 0x20 || scalar.value == 0x7F || (0x80...0x9F).contains(scalar.value)
            || functionKeyCharacters.contains(scalar.value)
    }

    /// The text to send for `committed`: with controls (C0 including CR, LF
    /// and tab; DEL; C1) and function-key characters removed, as the core
    /// would ignore the whole text otherwise. `nil` when nothing is left:
    /// what was committed stood for the key itself.
    static func typedText(_ committed: String) -> String? {
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: committed.unicodeScalars.lazy.filter { !isKeyCharacter($0) })
        return scalars.isEmpty ? nil : String(scalars)
    }

    /// Sends text an input method or dead key committed: text only, with no
    /// key and no modifiers, so it is typed (never pasted) as is (D1).
    private func sendCommittedText(_ text: String) {
        send(.text(text))
    }

    /// Sends text a key produced through the layout. The key is named only
    /// when the text is the key's own character (`a` or `A` for the A key);
    /// otherwise (Option-a gives `å`) the event is text only, so the core
    /// sends the text and not the key (D2). Option's composition never sets
    /// the Alt bit (D8).
    private func sendTyped(_ text: String, for event: NSEvent) {
        let modifiers = Self.modifiers(of: event).subtracting([.alt, .control])
        let keys = KeyInfo(event)
        if let named = keys.named, named.isKeypad {
            send(TesseraSession.KeyEvent(key: named, modifiers: modifiers, kind: keys.kind, text: text))
        } else if let own = keys.unshifted, text == String(own) || keys.shifted.map({ text == String($0) }) ?? false {
            send(
                TesseraSession.KeyEvent(
                    key: .char, codepoint: own, shifted: keys.shifted, baseLayout: keys.baseLayout,
                    modifiers: modifiers, kind: keys.kind, text: text))
        } else {
            send(TesseraSession.KeyEvent(key: .unidentified, modifiers: modifiers, kind: keys.kind, text: text))
        }
    }

    /// Sends a key that produced no text: named from its key code first,
    /// then from the editing selector AppKit chose, else as its unshifted
    /// character with the modifiers held (D6).
    @discardableResult
    private func sendKey(for event: NSEvent, selector: Selector?) -> TesseraSession.InputOutcome {
        let keys = KeyInfo(event)
        let modifiers = Self.modifiers(of: event)
        if let named = keys.named {
            return send(TesseraSession.KeyEvent(key: named, modifiers: modifiers, kind: keys.kind))
        }
        if let selector, let (key, extra) = Self.key(for: selector) {
            return send(TesseraSession.KeyEvent(key: key, modifiers: modifiers.union(extra), kind: keys.kind))
        }
        guard let own = keys.unshifted else { return .ignored }
        return send(
            TesseraSession.KeyEvent(
                key: .char, codepoint: own, shifted: keys.shifted, baseLayout: keys.baseLayout,
                modifiers: modifiers, kind: keys.kind))
    }

    @discardableResult
    private func send(_ event: TesseraSession.KeyEvent) -> TesseraSession.InputOutcome {
        #if DEBUG
            sentKeys.append(event)
        #endif
        do {
            let outcome = try session.send(key: event)
            if outcome == .consumed {
                movedToLiveScreen()
            }
            return outcome
        } catch {
            Self.log.debug("key not sent: \(String(describing: error), privacy: .public)")
            return .ignored
        }
    }

    /// The core returned the view to the live screen (a key or a paste it
    /// sent on): redraw, since the child may echo nothing, and drop the
    /// momentum of the gesture before.
    private func movedToLiveScreen() {
        ignoresMomentum = true
        needsRender = true
    }

    static func modifiers(of event: NSEvent) -> TesseraSession.Modifiers {
        let flags = event.modifierFlags
        var modifiers: TesseraSession.Modifiers = []
        if flags.contains(.shift) { modifiers.insert(.shift) }
        if flags.contains(.option) { modifiers.insert(.alt) }
        if flags.contains(.control) { modifiers.insert(.control) }
        if flags.contains(.command) { modifiers.insert(.super) }
        if flags.contains(.capsLock) { modifiers.insert(.capsLock) }
        return modifiers
    }

    /// The key AppKit's standard editing selectors stand for, with the
    /// modifiers the selector implies. Used only for keys whose key code
    /// names nothing (custom key bindings, remapped keys).
    static func key(for selector: Selector) -> (TesseraSession.Key, TesseraSession.Modifiers)? {
        switch selector {
        case #selector(insertNewline(_:)), #selector(insertLineBreak(_:)): (.enter, [])
        case #selector(insertNewlineIgnoringFieldEditor(_:)): (.enter, .alt)
        case #selector(deleteBackward(_:)), #selector(deleteBackwardByDecomposingPreviousCharacter(_:)):
            (.backspace, [])
        case #selector(deleteForward(_:)): (.delete, [])
        case #selector(insertTab(_:)): (.tab, [])
        case #selector(insertBacktab(_:)): (.tab, .shift)
        case #selector(cancelOperation(_:)): (.escape, [])
        case #selector(moveUp(_:)): (.up, [])
        case #selector(moveDown(_:)): (.down, [])
        case #selector(moveLeft(_:)): (.left, [])
        case #selector(moveRight(_:)): (.right, [])
        case #selector(moveUpAndModifySelection(_:)): (.up, .shift)
        case #selector(moveDownAndModifySelection(_:)): (.down, .shift)
        case #selector(moveLeftAndModifySelection(_:)): (.left, .shift)
        case #selector(moveRightAndModifySelection(_:)): (.right, .shift)
        case #selector(moveWordLeft(_:)): (.left, .alt)
        case #selector(moveWordRight(_:)): (.right, .alt)
        case #selector(moveWordLeftAndModifySelection(_:)): (.left, [.alt, .shift])
        case #selector(moveWordRightAndModifySelection(_:)): (.right, [.alt, .shift])
        case #selector(moveToBeginningOfLine(_:)), #selector(moveToLeftEndOfLine(_:)): (.home, [])
        case #selector(moveToEndOfLine(_:)), #selector(moveToRightEndOfLine(_:)): (.end, [])
        case #selector(moveToBeginningOfLineAndModifySelection(_:)): (.home, .shift)
        case #selector(moveToEndOfLineAndModifySelection(_:)): (.end, .shift)
        case #selector(pageUp(_:)), #selector(scrollPageUp(_:)): (.pageUp, [])
        case #selector(pageDown(_:)), #selector(scrollPageDown(_:)): (.pageDown, [])
        default: nil
        }
    }

    // MARK: Mouse

    // Every pointer event over the text goes to the core, which decides who
    // has it (ADR 0027): `.consumed` means it wrote a mouse report, so the program
    // tracking the mouse has the event; `.ignored` leaves it to the view
    // (selection, #30). A press or motion with Shift (selection bypass) or
    // Command (links) held is always `.ignored`; the release of a reported
    // press is reported whatever the modifiers. Option is reported as Meta.

    /// The click that activates an inactive window only activates it: it
    /// reaches neither the program nor a selection, so clicking back into
    /// a terminal running vim does not move vim's cursor (D13). AppKit then
    /// sends neither the press nor its drags; ``pressOutcomes`` keeps a
    /// stray release out as well. AppKit has no first mouse for the other
    /// buttons: a right or other press reaches a window that is not key (a
    /// background window, an inactive app), so ``press(_:button:)`` drops
    /// it there itself (vim's `mousemodel=extend` acts on a right-click).
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { false }

    /// The core's outcome for the press of each button that is down, from
    /// its press until the core takes its release. A button missing here
    /// was pressed outside this view (or by the activating click), or its
    /// press was not offered to the core: its drags and its release are not
    /// sent either. A press the core refused (a closed session, a full
    /// input queue) is kept as `.ignored`; the core never saw it, so it
    /// ignores the drags and the release that follow. The selection (#30)
    /// starts only on an `.ignored` press.
    private(set) var pressOutcomes: [TesseraSession.MouseButton: TesseraSession.InputOutcome] = [:]
    /// The cell and pixel of the last pointer event the core took. While
    /// ``metrics`` is `nil` the pointer's cell is unknown, so the drags and
    /// the release of a press sent before go there, as do the releases
    /// ``releaseHeldButtons()`` sends without an event.
    private var lastMousePosition: (col: UInt16, row: UInt16, xPx: Float, yPx: Float)?
    /// Watches the selected tab of the window's tab group: a tab switched
    /// away from while a button is held gets no release from AppKit.
    private var tabSelectionObservation: NSKeyValueObservation?
    #if DEBUG
        /// Mouse events handed to the session (tests).
        private(set) var sentMouseCount = 0
        /// The last mouse event handed to the session (tests).
        private(set) var lastSentMouse: TesseraSession.MouseEvent?
    #endif

    /// Offers a press to the input method while text is marked (`NSTextView`
    /// does): it may use the click itself, for a candidate window. The test
    /// seam: the hosted tests have no input context.
    var offersMouseDownToInputContext: (NSEvent) -> Bool = { _ in false }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        if hasMarkedText() {
            // The input method uses the click, or the composition is
            // committed (as a commit by mouse is) and the click goes on.
            if offersMouseDownToInputContext(event) { return }
            insertText(markedText.string, replacementRange: NSRange(location: NSNotFound, length: 0))
            // The input method still holds the composition: tell it it ended,
            // or its next kana repeats the committed text. What it answers
            // with is dropped, as when a composition is discarded.
            isDiscardingMarkedText = true
            inputContext?.discardMarkedText()
            isDiscardingMarkedText = false
        }
        let outcome = press(event, button: .left)
        gesture = nil
        autoscroller.stop()
        lastPointer = convert(event.locationInWindow, from: nil)
        if outcome == .ignored {
            beginGesture(event)
        }
    }

    override func mouseUp(with event: NSEvent) {
        release(event, button: .left)
        endGesture(event)
    }

    override func mouseDragged(with event: NSEvent) {
        drag(event, button: .left)
        continueGesture(event)
    }
    override func rightMouseDown(with event: NSEvent) { press(event, button: .right) }
    override func rightMouseUp(with event: NSEvent) { release(event, button: .right) }
    override func rightMouseDragged(with event: NSEvent) { drag(event, button: .right) }

    override func otherMouseDown(with event: NSEvent) {
        guard let button = Self.otherButton(event) else { return }
        press(event, button: button)
    }

    override func otherMouseUp(with event: NSEvent) {
        guard let button = Self.otherButton(event) else { return }
        release(event, button: button)
    }

    override func otherMouseDragged(with event: NSEvent) {
        guard let button = Self.otherButton(event) else { return }
        drag(event, button: button)
    }

    override func mouseMoved(with event: NSEvent) {
        if reportsPointer(at: event) {
            sendMouse(event, kind: .motion, button: .none)
        }
        lastPointer = convert(event.locationInWindow, from: nil)
        updateHover(modifiers: event.modifierFlags)
    }

    override func mouseExited(with event: NSEvent) {
        lastPointer = nil
        setHover(link: nil, cell: nil)
    }

    /// Command pressed or released with the pointer still: the link under it
    /// is underlined, or not, without waiting for the pointer to move.
    override func flagsChanged(with event: NSEvent) {
        updateHover(modifiers: event.modifierFlags)
        super.flagsChanged(with: event)
    }

    override func cursorUpdate(with event: NSEvent) {
        textCursor.set()
    }

    /// The button of an `otherMouse` event: middle, back or forward. Buttons
    /// beyond forward have no report (xterm numbers none past 11), so they
    /// are not sent rather than sent as another button.
    static func otherButton(_ event: NSEvent) -> TesseraSession.MouseButton? {
        switch event.buttonNumber {
        case 2: .middle
        case 3: .back
        case 4: .forward
        default: nil
        }
    }

    /// Sends a press, unless no cell can be named for it: while ``metrics``
    /// is `nil` (no surface, or between a lost surface and its re-attach)
    /// or outside the render view (the legacy scroller's strip, which
    /// takes only left clicks). A right or other press in a window that is
    /// not key is dropped (see ``acceptsFirstMouse(for:)``). A press not
    /// offered is `.ignored` and leaves no record, so neither its drags nor
    /// its release are sent. A press the core refuses is recorded as
    /// `.ignored` (see ``pressOutcomes``).
    @discardableResult
    private func press(_ event: NSEvent, button: TesseraSession.MouseButton) -> TesseraSession.InputOutcome {
        if button != .left, window?.isKeyWindow != true { return .ignored }
        guard reportsPointer(at: event) else { return .ignored }
        let outcome = sendMouse(event, kind: .press, button: button) ?? .ignored
        pressOutcomes[button] = outcome
        return outcome
    }

    private func drag(_ event: NSEvent, button: TesseraSession.MouseButton) {
        guard pressOutcomes[button] != nil else { return }
        sendMouse(event, kind: .motion, button: button)
    }

    /// Sends the release of a press that was sent; its record goes only
    /// once the core takes it, so a release the core refuses (a position
    /// that is not a number) does not leave the program's button held.
    private func release(_ event: NSEvent, button: TesseraSession.MouseButton) {
        guard pressOutcomes[button] != nil else { return }
        guard sendMouse(event, kind: .release, button: button) != nil else { return }
        pressOutcomes[button] = nil
    }

    /// Releases every button whose press was sent and whose release has not
    /// reached the core, at the last position the core took and with no
    /// modifiers: the view leaves its window or its tab is switched away
    /// from, and AppKit sends the release, if at all, to another window.
    private func releaseHeldButtons() {
        guard let position = lastMousePosition else { return }
        for button in pressOutcomes.keys.sorted(by: { $0.rawValue < $1.rawValue }) {
            let mouse = TesseraSession.MouseEvent(
                kind: .release, button: button, col: position.col, row: position.row, xPx: position.xPx,
                yPx: position.yPx, modifiers: [])
            if send(mouse) != nil {
                pressOutcomes[button] = nil
            }
        }
    }

    /// Whether a press or a motion with no button held at `event` is sent:
    /// only over the render view while its cell size is known. The drags
    /// and the release of a press that was sent always are. The test is
    /// AppKit's hit test (`isMousePoint(_:in:)`): in this unflipped view
    /// a point on the top edge (y == maxY, the top pixel row) is inside and
    /// one on the bottom edge is not.
    private func reportsPointer(at event: NSEvent) -> Bool {
        metrics != nil && isMousePoint(convert(event.locationInWindow, from: nil), in: renderView.frame)
    }

    /// Sends a pointer event to the session and returns whether the core
    /// reported it to the program, or `nil` when it was not sent or the
    /// core refused it. While ``metrics`` is `nil` it goes to the last
    /// position the core took (``lastMousePosition``), and is not sent
    /// when there is none.
    @discardableResult
    private func sendMouse(
        _ event: NSEvent, kind: TesseraSession.MouseEventKind, button: TesseraSession.MouseButton
    ) -> TesseraSession.InputOutcome? {
        let position: (col: UInt16, row: UInt16, xPx: Float, yPx: Float)
        if metrics != nil {
            let (cell, pixels) = cell(at: convert(event.locationInWindow, from: nil))
            position = (cell.col, cell.row, Float(pixels.x), Float(pixels.y))
        } else if let lastMousePosition {
            position = lastMousePosition
        } else {
            return nil
        }
        let mouse = TesseraSession.MouseEvent(
            kind: kind, button: button, col: position.col, row: position.row, xPx: position.xPx,
            yPx: position.yPx, modifiers: Self.modifiers(of: event))
        return send(mouse)
    }

    /// Hands `mouse` to the session: its outcome, or `nil` when the core
    /// refused it. The position of one it took is ``lastMousePosition``.
    private func send(_ mouse: TesseraSession.MouseEvent) -> TesseraSession.InputOutcome? {
        #if DEBUG
            sentMouseCount += 1
            lastSentMouse = mouse
        #endif
        do {
            let outcome = try session.send(mouse: mouse)
            lastMousePosition = (mouse.col, mouse.row, mouse.xPx, mouse.yPx)
            return outcome
        } catch {
            Self.log.debug("mouse event not sent: \(String(describing: error), privacy: .public)")
            return nil
        }
    }

    /// Wheel and trackpad scrolling, momentum included, go to the core,
    /// which moves the view, sends cursor keys on the alternate screen or
    /// reports the wheel to a program tracking the mouse (ADR 0027).
    ///
    /// After a key, a paste or a keyboard scroll, momentum events are
    /// dropped until the next gesture begins (``ignoresMomentum``). A new
    /// gesture is an event whose gesture phase begins; a precise event with
    /// no phase at all (a device or utility that sends none); or momentum
    /// that begins with no gesture phase, unless a phased event arrived
    /// since the key (then it is the momentum of the gesture the key
    /// interrupted). A notched wheel sends no momentum, so it is never
    /// dropped (ADR 0032).
    ///
    /// While text is marked the event is dropped whole after that
    /// bookkeeping: the input method owns input during a composition, and
    /// the core cannot be asked to report the wheel without scrolling.
    override func scrollWheel(with event: NSEvent) {
        let gesture = event.phase
        let momentum = event.momentumPhase
        if !gesture.isEmpty {
            phasedEventSinceIgnoring = true
        }
        if gesture.contains(.began)
            || (gesture.isEmpty && momentum.isEmpty && event.hasPreciseScrollingDeltas)
            || (gesture.isEmpty && momentum.contains(.began) && !phasedEventSinceIgnoring)
        {
            ignoresMomentum = false
        }
        if ignoresMomentum, !momentum.isEmpty {
            return
        }
        guard !hasMarkedText() else { return }
        let point = convert(event.locationInWindow, from: nil)
        let (cell, _) = cell(at: point)
        let scale: CGFloat = metrics.map { CGFloat($0.scale) } ?? window?.backingScaleFactor ?? 1
        // A notched wheel reports lines; a trackpad reports points.
        let lines: CGFloat
        if event.hasPreciseScrollingDeltas {
            let lineHeight: CGFloat = metrics.map { CGFloat($0.cellHeightPx) / scale } ?? Self.fallbackLineHeight
            lines = event.scrollingDeltaY / lineHeight
        } else {
            lines = event.scrollingDeltaY
        }
        let scroll = TesseraSession.ScrollEvent(
            deltaLines: Float(lines),
            deltaXPx: Float(event.scrollingDeltaX * (event.hasPreciseScrollingDeltas ? scale : 1)),
            deltaYPx: Float(event.scrollingDeltaY * (event.hasPreciseScrollingDeltas ? scale : 1)),
            precise: event.hasPreciseScrollingDeltas, col: cell.col, row: cell.row,
            modifiers: Self.modifiers(of: event))
        sentScrollCount += 1
        do {
            // The core says `.consumed` for a wheel it reported to a program
            // as well as for one that moved the view: the scroll bar shows
            // only when the offset changed. Output that pushes lines while
            // the view is scrolled back between the two reads can show it
            // once for a reported wheel (accepted).
            let before = try? session.scrollPosition
            if try session.send(scroll: scroll) == .consumed {
                needsRender = true
                if let before, let after = try? session.scrollPosition, after.offset != before.offset {
                    scroller.flash()
                }
            }
        } catch {
            Self.log.debug("scroll not sent: \(String(describing: error), privacy: .public)")
        }
    }

    // MARK: Selection and links

    /// The modifiers a link gesture needs: Command alone.
    private static func isLinkModifier(_ flags: NSEvent.ModifierFlags) -> Bool {
        flags.intersection([.command, .option, .shift, .control]) == .command
    }

    private func gridCell(at point: NSPoint) -> GridCell {
        let (cell, _) = cell(at: point)
        return GridCell(col: cell.col, row: cell.row)
    }

    /// What a click of `count` selects: a cell, a word, a whole line (the
    /// fourth click and later too: no smart selection).
    private static func granularity(forClicks count: Int) -> TesseraSession.SelectionGranularity {
        switch count {
        case ...1: .character
        case 2: .word
        default: .line
        }
    }

    /// A press the program did not take. Command alone is a link click and
    /// never a selection; Command with Option, like Option alone, selects a
    /// block (iTerm2 uses ⌥⌘, Terminal.app and WezTerm ⌥: both work). Shift
    /// with a selection moves its nearer end. The mode is fixed here: later
    /// modifiers do not change it.
    private func beginGesture(_ event: NSEvent) {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let cell = gridCell(at: lastPointer ?? .zero)
        if flags.contains(.command), !flags.contains(.option) {
            if Self.isLinkModifier(flags), event.clickCount == 1 {
                gesture = .link(press: cell, moved: false)
            }
            return
        }
        if flags.contains(.shift), hasSelection {
            select(.adjust(col: cell.col, row: cell.row))
        } else {
            select(
                .begin(
                    col: cell.col, row: cell.row, granularity: Self.granularity(forClicks: event.clickCount),
                    block: flags.contains(.option)))
        }
        gesture = .selecting(last: cell)
    }

    /// A drag with the left button down.
    private func continueGesture(_ event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        lastPointer = point
        let cell = gridCell(at: point)
        switch gesture {
        case .selecting(let last)?:
            if cell != last {
                select(.extend(col: cell.col, row: cell.row))
                gesture = .selecting(last: cell)
            }
            autoscroller.update(direction: autoscrollDirection(of: point))
        case .link(let press, _)?:
            if cell != press {
                gesture = .link(press: press, moved: true)
            }
        case nil:
            break
        }
    }

    /// The left button went up: autoscroll stops, a Command-click that
    /// stayed in its cell opens the link there.
    private func endGesture(_ event: NSEvent) {
        autoscroller.stop()
        defer { gesture = nil }
        guard case .link(let press, let moved)? = gesture, !moved else { return }
        let point = convert(event.locationInWindow, from: nil)
        lastPointer = point
        guard gridCell(at: point) == press else { return }
        openLink(at: press)
    }

    /// Opens the link under `cell` if there is one and the policy allows
    /// (Finder shows a file that would launch something).
    private func openLink(at cell: GridCell) {
        guard let link = try? session.link(col: cell.col, row: cell.row),
            let action = LinkPolicy.action(for: link)
        else { return }
        switch action {
        case .open(let url): linkOpener.open(url)
        case .reveal(let url): linkOpener.reveal(url)
        }
    }

    /// Whether the pointer at `point` is above or below the view, so a
    /// selection drag scrolls.
    private func autoscrollDirection(of point: NSPoint) -> SelectionAutoscroller.Direction? {
        let fromTop = isFlipped ? point.y : bounds.height - point.y
        if fromTop < 0 { return .up }
        if fromTop > bounds.height { return .down }
        return nil
    }

    /// One autoscroll tick: the view moves `lines` towards older output
    /// (`.up`) or the live screen, and the selection's end follows to the
    /// row it revealed, at the pointer's column.
    private func autoscrollTicked(_ direction: SelectionAutoscroller.Direction, lines: Int) {
        guard case .selecting? = gesture, let point = lastPointer,
            let position = try? session.scrollPosition
        else {
            autoscroller.stop()
            return
        }
        let step = UInt64(lines)
        let target = direction == .up ? min(position.offset + step, position.history) : position.offset - min(position.offset, step)
        if target != position.offset {
            scroll(to: .offset(target))
        }
        let cell = GridCell(col: gridCell(at: point).col, row: direction == .up ? 0 : position.rows - 1)
        select(.extend(col: cell.col, row: cell.row))
        gesture = .selecting(last: cell)
    }

    private func select(_ call: SelectionCall) {
        #if DEBUG
            selectionCalls.append(call)
        #endif
        do {
            switch call {
            case .begin(let col, let row, let granularity, let block):
                try session.selectBegin(col: col, row: row, granularity: granularity, block: block)
            case .extend(let col, let row):
                try session.selectExtend(col: col, row: row)
            case .adjust(let col, let row):
                try session.selectAdjust(col: col, row: row)
            case .selectAll:
                try session.selectAll()
            }
            needsRender = true
        } catch {
            Self.log.debug("selection call failed: \(String(describing: error), privacy: .public)")
        }
    }

    /// Underlines the link under the pointer while Command is held, shows
    /// the pointing hand over it, and clears both when Command is released,
    /// the pointer leaves the link or a button is down.
    private func updateHover(modifiers: NSEvent.ModifierFlags) {
        guard Self.isLinkModifier(modifiers.intersection(.deviceIndependentFlagsMask)), gesture == nil,
            let point = lastPointer
        else {
            setHover(link: nil, cell: nil)
            return
        }
        let cell = gridCell(at: point)
        if cell == hoveredCell, hoveredLink != nil { return }
        guard let link = try? session.link(col: cell.col, row: cell.row), LinkPolicy.action(for: link) != nil else {
            setHover(link: nil, cell: nil)
            return
        }
        setHover(link: link, cell: cell)
    }

    private func setHover(link: String?, cell: GridCell?) {
        guard link != hoveredLink || cell != hoveredCell else { return }
        let linkChanged = link != hoveredLink
        hoveredLink = link
        hoveredCell = cell
        do {
            try session.setHover(cell.map { ($0.col, $0.row) })
        } catch {
            Self.log.debug("hover not sent: \(String(describing: error), privacy: .public)")
        }
        needsRender = true
        guard linkChanged else { return }
        window?.invalidateCursorRects(for: self)
        if lastPointer != nil, window != nil { textCursor.set() }
    }

    // MARK: Scrollback

    /// Moves the view in the scrollback (the menu's keys, the scroll bar,
    /// a composition starting) and redraws. Like a key, it ends the
    /// momentum of the gesture before: the view stays where it was put. The
    /// scroll bar shows even if the view was already there, as feedback for
    /// the command.
    func scroll(to target: TesseraSession.ScrollTarget) {
        do {
            try session.scroll(to: target)
        } catch {
            Self.log.debug("scroll not sent: \(String(describing: error), privacy: .public)")
            return
        }
        ignoresMomentum = true
        needsRender = true
        scroller.flash()
    }

    /// The user dragged the scroll bar's knob or clicked its track. While
    /// text is marked nothing moves (the input method owns input); the knob
    /// returns to the view's position when the button goes up.
    private func scrollerMoved(to target: TesseraSession.ScrollTarget) {
        guard !hasMarkedText() else { return }
        scroll(to: target)
    }

    /// Shows the session's scroll position in the scroll bar. Called with
    /// every frame, which output and scrolling ask for, so the bar follows
    /// both with one read of the core per frame at most.
    private func updateScroller() {
        let wasHidden = scroller.isHidden
        do {
            scroller.update(try session.scrollPosition)
        } catch {
            // The session closed; the window closes with it.
        }
        if scroller.isHidden != wasHidden {
            window?.invalidateCursorRects(for: self)
        }
    }

    // MARK: Cell geometry

    /// The surface's metrics as last read from the core: on attach, on every
    /// resize the core accepts and at every ``render()``, so pointer, scroll
    /// and input-method events use them without calling into the core.
    /// `nil` before the first attach and after `tearDown()`. When the core
    /// drops the surface (a lost swapchain, which that frame reports as
    /// `.gpu`, or a closed session), the next render's read finds no
    /// surface and clears it; it stays `nil` until a re-attach, since
    /// ``displayLinkDidFire()`` does not render while detached. While it is
    /// `nil` no press or plain motion is sent to the core (the drags and the
    /// release of a press sent before go to the last position it took), and
    /// the candidate window opens at the view's top-left corner. Tests set
    /// it to known metrics.
    var metrics: TesseraSurface.Metrics?

    /// Reads ``metrics`` from the core (`nil` once the surface is gone).
    /// While text is marked, a change moves the composition overlay and
    /// tells the input method the character rects moved, so its candidate
    /// window follows the cursor.
    private func refreshMetrics() {
        let previous = metrics
        let wasAttached = surface.isAttached
        do {
            metrics = try surface.metrics()
        } catch {
            metrics = nil
            reattachIfDropped(error, wasAttached: wasAttached)
        }
        guard metrics != previous, hasMarkedText() else { return }
        updateMarkedTextLayer()
        inputContext?.invalidateCharacterCoordinates()
    }

    /// Line height in points for trackpad scrolling, and the composition
    /// cell's height, while the cell size is unknown (Menlo 13 pt, the
    /// default font).
    static let fallbackLineHeight: CGFloat = 16

    /// The cell under `point` (view coordinates) and the point in pixels from
    /// the view's top-left corner. The padding band and anything outside the
    /// grid clamp to the nearest edge cell.
    func cell(at point: NSPoint) -> (cell: (col: UInt16, row: UInt16), pixels: CGPoint) {
        let scale: CGFloat = metrics.map { CGFloat($0.scale) } ?? window?.backingScaleFactor ?? metalLayer.contentsScale
        let fromTop = CGPoint(x: point.x, y: isFlipped ? point.y : bounds.height - point.y)
        let pixels = CGPoint(x: fromTop.x * scale, y: fromTop.y * scale)
        guard let metrics, metrics.cellWidthPx > 0, metrics.cellHeightPx > 0 else {
            return ((0, 0), pixels)
        }
        func index(_ value: CGFloat, _ size: UInt32, _ count: UInt16) -> UInt16 {
            let offset: CGFloat = value - CGFloat(metrics.paddingPx)
            let cell: CGFloat = (offset / CGFloat(size)).rounded(.down)
            guard cell.isFinite, cell > 0 else { return 0 }
            let last = CGFloat(max(count, 1) - 1)
            return UInt16(min(cell, last))
        }
        return (
            (index(pixels.x, metrics.cellWidthPx, metrics.cols), index(pixels.y, metrics.cellHeightPx, metrics.rows)),
            pixels
        )
    }

    /// The rect of the cell the composition is drawn in and the candidate
    /// window is placed under, in view coordinates: the cursor's cell (two
    /// columns wide on a wide character), or a default-sized cell at the
    /// view's top-left corner while the surface is not attached.
    func compositionCellRect() -> NSRect {
        guard let metrics, metrics.scale > 0 else {
            let size = CGSize(width: Self.fallbackLineHeight / 2, height: Self.fallbackLineHeight)
            let y: CGFloat = isFlipped ? 0 : bounds.height - size.height
            return NSRect(x: 0, y: y, width: size.width, height: size.height)
        }
        let scale = CGFloat(metrics.scale)
        let padding: CGFloat = CGFloat(metrics.paddingPx) / scale
        let cellWidth: CGFloat = CGFloat(metrics.cellWidthPx) / scale
        let cellHeight: CGFloat = CGFloat(metrics.cellHeightPx) / scale
        let cell = CGSize(width: cellWidth, height: cellHeight)
        let x: CGFloat = padding + CGFloat(metrics.cursorCol) * cellWidth
        let fromTop: CGFloat = padding + CGFloat(metrics.cursorRow) * cellHeight
        let y: CGFloat = isFlipped ? fromTop : bounds.height - fromTop - cell.height
        let columns: CGFloat = metrics.cursorWide ? 2 : 1
        return NSRect(x: x, y: y, width: cell.width * columns, height: cell.height)
    }

    // MARK: Marked text overlay

    private func clearMarkedText() {
        markedText = NSMutableAttributedString()
        markedSelection = NSRange(location: NSNotFound, length: 0)
        updateMarkedTextLayer()
    }

    /// Draws the composition over the cell it will be committed into,
    /// underlined, on the text background.
    private func updateMarkedTextLayer() {
        guard markedText.length > 0 else {
            markedTextLayer.isHidden = true
            markedTextLayer.string = nil
            return
        }
        let cell = compositionCellRect()
        let font = NSFont.monospacedSystemFont(ofSize: cell.height * 0.8, weight: .regular)
        // Only what can fit is drawn: a composition is short, but nothing
        // stops an input method from marking a huge string.
        let shown = markedText.string.prefix(Self.maxOverlayCharacters)
        let display = NSMutableAttributedString(string: String(shown))
        // Dynamic colours resolve against the view's current appearance; a
        // layer keeps only the resolved values.
        var text = NSColor.textColor.cgColor
        var background = NSColor.textBackgroundColor.cgColor
        effectiveAppearance.performAsCurrentDrawingAppearance {
            text = NSColor.textColor.cgColor
            background = NSColor.textBackgroundColor.cgColor
        }
        display.addAttributes(
            [
                .font: font, .foregroundColor: NSColor(cgColor: text) ?? NSColor.textColor,
                .underlineStyle: NSUnderlineStyle.single.rawValue,
            ], range: NSRange(location: 0, length: display.length))
        let width = ceil(display.size().width)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        markedTextLayer.backgroundColor = background
        markedTextLayer.string = display
        // The layer tree is not flipped: its origin is the bottom left.
        let layerY = isFlipped ? bounds.height - cell.maxY : cell.minY
        let available = max(cell.width, renderView.frame.width - cell.minX)
        markedTextLayer.frame = CGRect(
            x: cell.minX, y: layerY, width: min(max(width, cell.width), available), height: cell.height)
        markedTextLayer.isHidden = false
        CATransaction.commit()
    }

    /// The longest composition drawn, in characters (wider than any grid).
    static let maxOverlayCharacters = 2048

    /// The composition as drawn, for tests.
    var markedTextOverlay: (text: String, frame: CGRect)? {
        guard !markedTextLayer.isHidden, let string = markedTextLayer.string as? NSAttributedString else {
            return nil
        }
        return (string.string, markedTextLayer.frame)
    }
}

// MARK: Menu actions

/// The Shell, Edit and View items that act on the terminal. The menu sends
/// them up the responder chain, so they reach the key window's view.
extension TerminalSurfaceView: NSMenuItemValidation {
    /// The core's font size bounds (`tessera-render`'s `MIN_FONT_SIZE_PT` and
    /// `MAX_FONT_SIZE_PT`). The core clamps every size; these only disable
    /// Smaller and Bigger at either end.
    static let fontSizeRange: ClosedRange<Float> = 4...512
    /// One zoom step (D2).
    static let fontSizeStep: Float = 1
    /// The most UTF-8 a paste may have: the core's pending input (16 MiB),
    /// which refuses a larger paste whole.
    static let maxPasteBytes = 16 << 20

    /// Whether the session's child has exited: nothing more reaches it.
    private var hasExited: Bool { status != nil }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(copy(_:)):
            return hasSelection
        case #selector(selectAll(_:)):
            return true
        case #selector(paste(_:)), #selector(pasteAsPlainText(_:)):
            return !hasExited && pasteboard.availableType(from: [.string]) != nil
        case #selector(clearScrollback(_:)):
            return !hasExited
        case #selector(increaseFontSize(_:)):
            guard let size = metrics?.fontSizePt else { return false }
            return size < Self.fontSizeRange.upperBound
        case #selector(decreaseFontSize(_:)):
            guard let size = metrics?.fontSizePt else { return false }
            return size > Self.fontSizeRange.lowerBound
        case #selector(resetFontSize(_:)):
            guard let size = metrics?.fontSizePt, let attached = attachedFontSizePt else { return false }
            return size != attached
        case #selector(scrollToTop(_:)), #selector(scrollToBottom(_:)), #selector(scrollPageUpInHistory(_:)),
            #selector(scrollPageDownInHistory(_:)):
            // During a composition the items are disabled, so ⌘↑/⌘↓/⌘Home/⌘End
            // do not scroll; AppKit swallows them rather than handing them to
            // the input method (ADR 0032).
            return !hasMarkedText()
        default:
            return true
        }
    }

    /// Copy (⌘C): the selected text goes to the pasteboard as plain text
    /// and the selection stays. A selection of blanks only leaves the
    /// pasteboard as it was, rather than emptying the user's clipboard for
    /// nothing. Nothing is copied by selecting (copy-on-select is #47).
    @objc func copy(_ sender: Any?) {
        do {
            guard let text = try session.selectionText(), !text.isEmpty else { return }
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
        } catch {
            Self.log.debug("copy failed: \(String(describing: error), privacy: .public)")
        }
    }

    /// Select All (⌘A): the scrollback and the screen (the screen only on
    /// the alternate screen). Lines that arrive later are not added.
    @objc override func selectAll(_ sender: Any?) {
        select(.selectAll)
    }

    /// Paste (⌘V): the pasteboard's text goes to the session, which strips
    /// what a paste must not carry and brackets it when the terminal asked
    /// for bracketed paste (D5). No text on the pasteboard sends nothing.
    /// A composition in progress is kept, and the paste goes ahead of it.
    @objc func paste(_ sender: Any?) {
        guard let text = pasteboard.string(forType: .string), !text.isEmpty else { return }
        guard Self.fitsPasteLimit(text) else {
            Self.log.error("paste not sent: \(text.utf16.count) UTF-16 units is more than the core takes")
            NSSound.beep()
            return
        }
        pasteCount += 1
        do {
            try session.paste(text)
            movedToLiveScreen()
        } catch {
            // Too large for the pending input, or the shell exited.
            Self.log.error("paste not sent: \(String(describing: error), privacy: .public)")
            NSSound.beep()
        }
    }

    /// Whether `text` may fit the core's pending input. The core refuses a
    /// larger paste only after the wrapper has transcoded all of it to UTF-8
    /// (about 300 ms for 18 MiB on the main thread), so a paste that cannot
    /// fit is refused here first. UTF-8 is never shorter than UTF-16 and at
    /// most three times as long; the pasteboard's string is UTF-16, whose
    /// count is free, while counting its UTF-8 takes a scan (about 15 ms for
    /// 9 Mi characters), needed only when the text could be too long. The
    /// core still refuses a paste that fits the limit but not the pending
    /// input left (an earlier paste still being written).
    static func fitsPasteLimit(_ text: String) -> Bool {
        let units = text.utf16.count
        if units > maxPasteBytes { return false }
        if units <= maxPasteBytes / 3 { return true }
        return text.utf8.count <= maxPasteBytes
    }

    /// Paste as Plain Text (⌥⇧⌘V): the same as Paste, since the pasteboard's
    /// plain string is all a paste sends. It differs once copy modes (#47)
    /// put styled text on the pasteboard.
    @objc func pasteAsPlainText(_ sender: Any?) {
        paste(sender)
    }

    /// Clear to Start (⌘K): the scrollback and the screen above the prompt
    /// go, so the prompt becomes the top row: from the nearest prompt-start
    /// mark at or above the cursor (not past a row where command output
    /// starts; a row with both marks is a prompt start), else from the start
    /// of the cursor's logical line, never above that output-start row
    /// (ADR 0022 D-CLEAR-START, ``TesseraSession/clearScrollback()``). The core sends no
    /// output event for it, so the view redraws itself; an idle terminal's
    /// display link is paused and would otherwise keep the old screen.
    @objc func clearScrollback(_ sender: Any?) {
        do {
            try session.clearScrollback()
            needsRender = true
        } catch {
            Self.log.debug("clear not sent: \(String(describing: error), privacy: .public)")
        }
    }

    /// Scroll to Top (⌘Home): the oldest line of the scrollback.
    @objc func scrollToTop(_ sender: Any?) {
        scroll(to: .top)
    }

    /// Scroll to Bottom (⌘End): the live screen.
    @objc func scrollToBottom(_ sender: Any?) {
        scroll(to: .bottom)
    }

    /// Page Up (⌘↑): one screen less a line towards older output.
    @objc func scrollPageUpInHistory(_ sender: Any?) {
        scroll(to: .pageUp)
    }

    /// Page Down (⌘↓): one screen less a line towards the live screen.
    @objc func scrollPageDownInHistory(_ sender: Any?) {
        scroll(to: .pageDown)
    }

    /// Bigger (⌘+ and ⌘=): one step up from the surface's current size.
    @objc func increaseFontSize(_ sender: Any?) {
        stepFontSize(by: Self.fontSizeStep)
    }

    /// Smaller (⌘-): one step down from the surface's current size.
    @objc func decreaseFontSize(_ sender: Any?) {
        stepFontSize(by: -Self.fontSizeStep)
    }

    /// Actual Size (⌘0): back to the size the surface was attached with.
    @objc func resetFontSize(_ sender: Any?) {
        guard let attached = attachedFontSizePt else { return }
        setFontSize(attached)
    }

    /// Zoom applies to this view only (D2); it steps from what the core
    /// reports, so a size it clamped is where the next step starts.
    private func stepFontSize(by step: Float) {
        let wasAttached = surface.isAttached
        do {
            let current = try surface.metrics().fontSizePt
            setFontSize(current + step)
        } catch {
            reattachIfDropped(error, wasAttached: wasAttached)
            Self.log.debug("no surface to zoom: \(String(describing: error), privacy: .public)")
        }
    }

    private func setFontSize(_ points: Float) {
        let size: Float = min(max(points, Self.fontSizeRange.lowerBound), Self.fontSizeRange.upperBound)
        let wasAttached = surface.isAttached
        do {
            try surface.setFontSize(size)
        } catch {
            if !reattachIfDropped(error, wasAttached: wasAttached) {
                Self.log.error("font size not set: \(String(describing: error), privacy: .public)")
            }
            return
        }
        refreshMetrics()
        let applied = metrics?.fontSizePt ?? size
        zoomedFontSizePt = applied == attachedFontSizePt ? nil : applied
        // The cells changed size: redraw. `refreshMetrics` moved a
        // composition with them.
        needsRender = true
    }

    /// A configuration edit reached every view (#100). When it changed
    /// `font_size`, the core set every view to the new size, a zoomed one
    /// included, so the zoom ends and the new size is what Actual Size
    /// returns to; any other edit leaves each view's size alone, and a zoom
    /// with it. Which of the two happened shows only in the size: a view at
    /// its zoom is taken to have kept it, so a new configured size equal to
    /// the current zoom is indistinguishable from no font change, and Actual
    /// Size still returns to the old configured size then. A view without a
    /// surface learns the new size at its next attach (and keeps its zoom).
    private func configurationDidChangeFontSize() {
        refreshMetrics()
        guard let size = metrics?.fontSizePt else { return }
        if let zoom = zoomedFontSizePt {
            guard size != zoom else { return }
            zoomedFontSizePt = nil
        }
        attachedFontSizePt = size
    }

    /// Records the configured size a surface was attached with and applies
    /// the zoom again after a re-attach, which starts at the configured size.
    private func surfaceAttachedForZoom() {
        attachedFontSizePt = metrics?.fontSizePt
        guard let zoom = zoomedFontSizePt else { return }
        if zoom == attachedFontSizePt {
            zoomedFontSizePt = nil
        } else {
            setFontSize(zoom)
        }
    }
}

// MARK: NSTextInputClient

extension TerminalSurfaceView: @preconcurrency NSTextInputClient {
    func insertText(_ string: Any, replacementRange: NSRange) {
        guard !isDiscardingMarkedText else { return }
        let text = Self.plainString(string)
        clearMarkedText()
        guard !text.isEmpty else { return }
        if keyTextAccumulator != nil {
            keyTextAccumulator?.append(text)
        } else if let text = Self.typedText(text) {
            // Outside a key press: a commit by mouse, the character viewer,
            // dictation.
            sendCommittedText(text)
        }
    }

    /// A composition that starts while the view is scrolled back returns it
    /// to the live screen first (#134), as typing would: the composition is
    /// drawn, and the candidate window opens, at the live cursor.
    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        let text = Self.plainString(string)
        let starts = !hasMarkedText() && !text.isEmpty
        markedText = NSMutableAttributedString(string: text)
        markedSelection = text.isEmpty ? NSRange(location: NSNotFound, length: 0) : selectedRange
        if starts, let position = try? session.scrollPosition, position.offset > 0 {
            scroll(to: .bottom)
            // The cursor is on screen again: the metrics say so, and the
            // overlay and candidate window follow them.
            refreshMetrics()
        }
        updateMarkedTextLayer()
    }

    /// Ends the composition without committing it (D5): both reference
    /// terminals discard it, and so does a focus change.
    func unmarkText() {
        clearMarkedText()
    }

    func selectedRange() -> NSRange {
        hasMarkedText() ? markedSelection : NSRange(location: NSNotFound, length: 0)
    }

    func markedRange() -> NSRange {
        hasMarkedText()
            ? NSRange(location: 0, length: markedText.length) : NSRange(location: NSNotFound, length: 0)
    }

    func hasMarkedText() -> Bool {
        markedText.length > 0
    }

    func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        let whole = NSRange(location: 0, length: markedText.length)
        let clipped = NSIntersectionRange(range, whole)
        guard range.location != NSNotFound, clipped.length > 0 || range.length == 0, clipped.location <= whole.length
        else { return nil }
        actualRange?.pointee = clipped
        return markedText.attributedSubstring(from: clipped)
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.underlineStyle, .underlineColor, .markedClauseSegment]
    }

    /// The composition cell's rect in screen coordinates, so the candidate
    /// window opens just below it.
    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        actualRange?.pointee = range
        let inWindow = convert(compositionCellRect(), to: nil)
        return window?.convertToScreen(inWindow) ?? inWindow
    }

    func characterIndex(for point: NSPoint) -> Int {
        NSNotFound
    }

    override func doCommand(by selector: Selector) {
        if keyTextAccumulator != nil {
            keyCommands.append(selector)
        } else if let (key, modifiers) = Self.key(for: selector) {
            // Not from a key press (an input method or service asked).
            send(TesseraSession.KeyEvent(key: key, modifiers: modifiers))
        }
    }

    private static func plainString(_ string: Any) -> String {
        switch string {
        case let attributed as NSAttributedString: attributed.string
        case let plain as String: plain
        default: ""
        }
    }
}

// MARK: Key codes

/// What a key event says about its key, independent of the text it produced.
@MainActor
private struct KeyInfo {
    /// The non-character key its key code names, if any.
    let named: TesseraSession.Key?
    /// The key's character with no modifiers, and with Shift, in the active
    /// layout.
    let unshifted: Unicode.Scalar?
    let shifted: Unicode.Scalar?
    /// The US-layout character at the key's position, when it differs.
    let baseLayout: Unicode.Scalar?
    let kind: TesseraSession.KeyEventKind

    init(_ event: NSEvent) {
        named = TerminalSurfaceView.namedKey(event.keyCode)
        kind = event.isARepeat ? .repeat : .press
        func single(_ string: String?) -> Unicode.Scalar? {
            guard let scalars = string?.unicodeScalars, scalars.count == 1, let scalar = scalars.first,
                !TerminalSurfaceView.isKeyCharacter(scalar)
            else { return nil }
            return scalar
        }
        // Without Shift, the event's own characters-without-modifiers are the
        // unshifted character; with Shift they are the shifted one, so ask
        // the layout.
        let flags = event.modifierFlags
        unshifted =
            flags.contains(.shift)
            ? single(event.characters(byApplyingModifiers: [])) ?? single(event.charactersIgnoringModifiers)
            : single(event.charactersIgnoringModifiers) ?? single(event.characters(byApplyingModifiers: []))
        shifted = single(event.characters(byApplyingModifiers: .shift))
        let base = TerminalSurfaceView.usLayoutCharacter(event.keyCode)
        baseLayout = base == unshifted ? nil : base
    }
}

extension TerminalSurfaceView {
    /// The non-character key at a macOS virtual key code (`kVK_*` in
    /// HIToolbox's `Events.h`), or `nil` for a character key.
    static func namedKey(_ keyCode: UInt16) -> TesseraSession.Key? {
        switch keyCode {
        case 36: .enter  // kVK_Return
        case 48: .tab
        case 51: .backspace  // kVK_Delete
        case 53: .escape
        case 117: .delete  // kVK_ForwardDelete
        case 114: .insert  // kVK_Help, where PC keyboards have Insert
        case 123: .left
        case 124: .right
        case 125: .down
        case 126: .up
        case 115: .home
        case 119: .end
        case 116: .pageUp
        case 121: .pageDown
        case 122: .f1
        case 120: .f2
        case 99: .f3
        case 118: .f4
        case 96: .f5
        case 97: .f6
        case 98: .f7
        case 100: .f8
        case 101: .f9
        case 109: .f10
        case 103: .f11
        case 111: .f12
        case 105: .f13
        case 107: .f14
        case 113: .f15
        case 106: .f16
        case 64: .f17
        case 79: .f18
        case 80: .f19
        case 90: .f20
        case 82: .kp0
        case 83: .kp1
        case 84: .kp2
        case 85: .kp3
        case 86: .kp4
        case 87: .kp5
        case 88: .kp6
        case 89: .kp7
        case 91: .kp8
        case 92: .kp9
        case 65: .kpDecimal
        case 75: .kpDivide
        case 67: .kpMultiply
        case 78: .kpSubtract
        case 69: .kpAdd
        case 76: .kpEnter
        case 81: .kpEqual
        default: nil
        }
    }

    /// The US-layout character at an ANSI key position, for Ctrl and Alt
    /// combinations on non-Latin layouts.
    static func usLayoutCharacter(_ keyCode: UInt16) -> Unicode.Scalar? {
        // Indexed by kVK_ANSI_* code, 0 (A) to 50 (`); "\0" marks codes that
        // are not character keys.
        let table = Array(
            "asdfhgzxcv\0bqweryt123465=97-80]ou[ip\0lj'k;\\,/nm.\0 `".unicodeScalars)
        guard Int(keyCode) < table.count, table[Int(keyCode)] != "\0" else { return nil }
        return table[Int(keyCode)]
    }
}

extension TesseraSession.Key {
    /// A numeric keypad key.
    var isKeypad: Bool {
        (TesseraSession.Key.kp0.rawValue...TesseraSession.Key.kpBegin.rawValue).contains(rawValue)
    }
}

/// The display link's target. The link retains its target, so the view is
/// referenced weakly to keep the link from keeping the view alive.
@MainActor
private final class DisplayLinkTarget: NSObject {
    weak var view: TerminalSurfaceView?

    init(_ view: TerminalSurfaceView) {
        self.view = view
    }

    @objc func tick(_ link: CADisplayLink) {
        guard let view else {
            link.invalidate()
            return
        }
        view.displayLinkDidFire()
    }
}


/// A cell of the grid, view-relative: row 0 is the top row shown.
private struct GridCell: Equatable {
    var col: UInt16
    var row: UInt16
}

/// A left-button gesture the view handles: the core did not report the press
/// to a program.
private enum PointerGesture {
    /// A selection, begun or adjusted at the press; `last` is the cell the
    /// core was last told about. The core is asked to extend only when the
    /// pointer reaches another cell (a drag inside the cell pressed selects
    /// nothing).
    case selecting(last: GridCell)
    /// A Command-click: the link under `press` opens on release if the
    /// pointer did not leave the cell (a Command-drag does nothing).
    case link(press: GridCell, moved: Bool)
}

/// A call a gesture or the Edit menu makes on the core's selection.
enum SelectionCall: Equatable {
    case begin(col: UInt16, row: UInt16, granularity: TesseraSession.SelectionGranularity, block: Bool)
    case extend(col: UInt16, row: UInt16)
    case adjust(col: UInt16, row: UInt16)
    case selectAll
}
