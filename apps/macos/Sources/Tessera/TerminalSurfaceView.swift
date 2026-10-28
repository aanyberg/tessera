import AppKit
import QuartzCore
import TesseraKit
import os

/// A tab's terminal: a layer-backed view whose `CAMetalLayer` the core draws
/// the session into (ADR 0017).
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
/// - Mouse and scrolling are forwarded to the core in cell coordinates.
@MainActor
final class TerminalSurfaceView: NSView, TerminalContentView {
    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "surface")

    /// The theme's default padding in points on every side of the grid. The
    /// core owns it (`Theme::padding`) and does not export it yet (#90), so
    /// the composition cell falls back to it.
    static let defaultPaddingPoints: CGFloat = 4

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
    /// Time between re-attach attempts (tests shorten it).
    static var reattachInterval: Duration = .seconds(1)
    /// Consecutive re-attach attempts that may fail before the view stops
    /// trying; a later size, scale or window change tries again.
    static let maxReattachFailures = 5
    /// Consecutive failed re-attach attempts.
    private(set) var reattachFailures = 0
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

    init(session: TesseraSession, frame: NSRect = NSRect(x: 0, y: 0, width: 480, height: 200)) {
        self.session = session
        surface = TesseraSurface(session: session)
        super.init(frame: frame)
        wantsLayer = true
        // The core draws every frame; AppKit must neither redraw nor stretch
        // the layer's contents while a resize is coalesced.
        layerContentsRedrawPolicy = .never
        layerContentsPlacement = .topLeft

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
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Layer

    /// The backing layer the core draws into.
    var metalLayer: CAMetalLayer {
        // `makeBackingLayer` makes it; `wantsLayer` is set in `init`.
        layer as! CAMetalLayer  // swiftlint:disable:this force_cast
    }

    override func makeBackingLayer() -> CALayer {
        let layer = CAMetalLayer()
        // The core reconfigures device, pixel format, drawable size and
        // opacity (from the theme's alpha) on attach.
        layer.framebufferOnly = true
        layer.isOpaque = true
        layer.contentsGravity = .topLeft
        layer.needsDisplayOnBoundsChange = false
        layer.contentsScale = window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 1
        return layer
    }

    override var isOpaque: Bool { metalLayer.isOpaque }

    override var wantsUpdateLayer: Bool { true }

    override func updateLayer() {
        // Drawn by the core through the display link.
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
                MainActor.assumeIsolated { self?.tearDown() }
            })
        for (name, isKey) in [(NSWindow.didBecomeKeyNotification, true), (NSWindow.didResignKeyNotification, false)] {
            windowObservers.append(
                center.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.windowKeyChanged(isKey: isKey) }
                })
        }
        metalLayer.contentsScale = window.backingScaleFactor
        markedTextLayer.contentsScale = window.backingScaleFactor
        attachSurface()
        let link = displayLink(target: DisplayLinkTarget(self), selector: #selector(DisplayLinkTarget.tick(_:)))
        link.add(to: .main, forMode: .common)
        displayLink = link
        updateDisplayLinkPause()
    }

    /// Stops the display link and detaches the surface; the window is going
    /// away (or the view leaves it).
    private func tearDown() {
        observedWindow = nil
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

    /// The view's size in physical pixels and its backing scale.
    var pixelSize: TesseraSurface.Size {
        let backing = convertToBacking(bounds).size
        let scale = window?.backingScaleFactor ?? metalLayer.contentsScale
        return TesseraSurface.Size(
            widthPx: UInt32(max(0, backing.width.rounded())), heightPx: UInt32(max(0, backing.height.rounded())),
            scale: Float(scale))
    }

    private func attachSurface() {
        guard !surface.isAttached, window != nil else { return }
        lastAttachAttempt = .now
        let size = pixelSize
        do {
            try surface.attach(layer: metalLayer, widthPx: size.widthPx, heightPx: size.heightPx, scale: size.scale)
            wantsReattach = false
            reattachFailures = 0
            reattachGaveUpAt = nil
            lostSurfaceAcrossMove = false
            needsRender = true
            applyLayerOpacity()
        } catch {
            // Without a renderer (no GPU, no font) the session still runs and
            // takes input; there is just nothing to see. A first attach that
            // fails is deliberately not retried: no GPU or font appears
            // later. Only a surface lost after attaching is (`wantsReattach`),
            // a few times, then not until the size, scale or window changes.
            if wantsReattach {
                reattachFailures += 1
                if reattachFailures >= Self.maxReattachFailures {
                    wantsReattach = false
                    reattachGaveUpAt = size
                    Self.log.error(
                        "re-attaching the surface failed \(self.reattachFailures) times; retrying after the next size change: \(String(describing: error), privacy: .public)")
                }
            } else {
                Self.log.error("attaching the surface failed: \(String(describing: error), privacy: .public)")
            }
        }
        // Whether a surface is attached changes what counts as work.
        updateDisplayLinkPause()
    }

    /// The core makes the layer non-opaque for a translucent theme; the
    /// window must then be transparent too, or it shows its own background
    /// through the layer.
    private func applyLayerOpacity() {
        guard let window, !metalLayer.isOpaque else { return }
        window.isOpaque = false
        window.backgroundColor = .clear
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        surfaceSizeChanged()
    }

    override func layout() {
        super.layout()
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
            coreResizeCount += 1
            suboptimalFrames = 0
            needsRender = true
            updateMarkedTextLayer()
        } catch {
            Self.log.error("resizing the surface failed: \(String(describing: error), privacy: .public)")
        }
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
        do {
            switch try surface.render() {
            case .presented, .skipped:
                suboptimalFrames = 0
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
            // surface detached after a lost swapchain.
            needsRender = true
        } catch TesseraCoreError.noSurface {
            // The core detached the surface (lost swapchain): attach again.
            wantsReattach = true
            needsRender = true
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
        }
        sendFocus(isKey && isFocused)
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
        addCursorRect(bounds, cursor: .iBeam)
    }

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
            return try session.send(key: event)
        } catch {
            Self.log.debug("key not sent: \(String(describing: error), privacy: .public)")
            return .ignored
        }
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

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        sendMouse(event, kind: .press, button: .left)
    }

    override func mouseUp(with event: NSEvent) { sendMouse(event, kind: .release, button: .left) }
    override func mouseDragged(with event: NSEvent) { sendMouse(event, kind: .motion, button: .left) }
    override func rightMouseDown(with event: NSEvent) { sendMouse(event, kind: .press, button: .right) }
    override func rightMouseUp(with event: NSEvent) { sendMouse(event, kind: .release, button: .right) }
    override func rightMouseDragged(with event: NSEvent) { sendMouse(event, kind: .motion, button: .right) }
    override func otherMouseDown(with event: NSEvent) { sendMouse(event, kind: .press, button: Self.button(event)) }
    override func otherMouseUp(with event: NSEvent) { sendMouse(event, kind: .release, button: Self.button(event)) }
    override func otherMouseDragged(with event: NSEvent) {
        sendMouse(event, kind: .motion, button: Self.button(event))
    }

    private static func button(_ event: NSEvent) -> TesseraSession.MouseButton {
        switch event.buttonNumber {
        case 2: .middle
        case 3: .back
        case 4: .forward
        default: .middle
        }
    }

    private func sendMouse(
        _ event: NSEvent, kind: TesseraSession.MouseEventKind, button: TesseraSession.MouseButton
    ) {
        let point = convert(event.locationInWindow, from: nil)
        let (cell, pixels) = cell(at: point)
        let mouse = TesseraSession.MouseEvent(
            kind: kind, button: button, col: cell.col, row: cell.row, xPx: Float(pixels.x), yPx: Float(pixels.y),
            modifiers: Self.modifiers(of: event))
        do {
            try session.send(mouse: mouse)
        } catch {
            Self.log.debug("mouse event not sent: \(String(describing: error), privacy: .public)")
        }
    }

    override func scrollWheel(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        let (cell, _) = cell(at: point)
        let scale = window?.backingScaleFactor ?? 1
        // A notched wheel reports lines; a trackpad reports points.
        let lines: CGFloat
        if event.hasPreciseScrollingDeltas {
            let lineHeight = (cellGeometry?.cellSize.height).map { $0 / scale } ?? Self.fallbackLineHeight
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
        do {
            if try session.send(scroll: scroll) == .consumed {
                // The view moved through the scrollback.
                needsRender = true
            }
        } catch {
            Self.log.debug("scroll not sent: \(String(describing: error), privacy: .public)")
        }
    }

    // MARK: Cell geometry

    /// Cell size and grid origin in physical pixels.
    struct CellGeometry: Equatable {
        var cellSize: CGSize
        var padding: CGFloat
    }

    /// The grid's cell geometry, when known: the surface's (``TesseraSurface/cellMetrics``,
    /// `nil` until #90 exports it) unless a test sets ``cellGeometryOverride``.
    /// While it is unknown, pointer events carry exact pixels and cell 0,0,
    /// and the candidate window opens at the first cell.
    var cellGeometry: CellGeometry? {
        if let cellGeometryOverride { return cellGeometryOverride }
        return surface.cellMetrics.map {
            CellGeometry(
                cellSize: CGSize(width: CGFloat($0.widthPx), height: CGFloat($0.heightPx)),
                padding: CGFloat($0.paddingPx))
        }
    }

    /// Replaces the surface's cell geometry (tests).
    var cellGeometryOverride: CellGeometry?

    /// Line height in points for trackpad scrolling while the cell size is
    /// unknown (#90; Menlo 13 pt, the default font).
    static let fallbackLineHeight: CGFloat = 16

    /// The cell under `point` (view coordinates) and the point in pixels from
    /// the view's top-left corner.
    func cell(at point: NSPoint) -> (cell: (col: UInt16, row: UInt16), pixels: CGPoint) {
        let scale = window?.backingScaleFactor ?? metalLayer.contentsScale
        let fromTop = CGPoint(x: point.x, y: isFlipped ? point.y : bounds.height - point.y)
        let pixels = CGPoint(x: fromTop.x * scale, y: fromTop.y * scale)
        guard let geometry = cellGeometry, geometry.cellSize.width > 0, geometry.cellSize.height > 0 else {
            return ((0, 0), pixels)
        }
        func index(_ value: CGFloat, _ size: CGFloat) -> UInt16 {
            UInt16(clamping: Int(max(0, ((value - geometry.padding) / size).rounded(.down))))
        }
        return ((index(pixels.x, geometry.cellSize.width), index(pixels.y, geometry.cellSize.height)), pixels)
    }

    /// The rect of the cell the composition is drawn in and the candidate
    /// window is placed under, in view coordinates: the cursor's cell when
    /// the surface reports it, else the grid's first cell (until #90 exports
    /// the cursor cell and the cell metrics).
    func compositionCellRect() -> NSRect {
        let scale = window?.backingScaleFactor ?? metalLayer.contentsScale
        let padding = (cellGeometry?.padding).map { $0 / scale } ?? Self.defaultPaddingPoints
        let size = cellGeometry.map { CGSize(width: $0.cellSize.width / scale, height: $0.cellSize.height / scale) }
            ?? CGSize(width: Self.fallbackLineHeight / 2, height: Self.fallbackLineHeight)
        let cursor = surface.cursorCell ?? (row: 0, col: 0)
        let x = padding + CGFloat(cursor.col) * size.width
        let fromTop = padding + CGFloat(cursor.row) * size.height
        let y = isFlipped ? fromTop : bounds.height - fromTop - size.height
        return NSRect(x: x, y: y, width: size.width, height: size.height)
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
        let available = max(cell.width, bounds.width - cell.minX)
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

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        let text = Self.plainString(string)
        markedText = NSMutableAttributedString(string: text)
        markedSelection = text.isEmpty ? NSRange(location: NSNotFound, length: 0) : selectedRange
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
