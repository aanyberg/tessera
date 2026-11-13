import AppKit
import TesseraKit

/// A terminal tab's window content (#363): the tab's ``PaneContainerView``
/// in the window's `contentLayoutRect`, and, under the title bar and tab bar
/// of a translucent window, a blur tinted with the theme background of the
/// panes below (#388).
///
/// An opaque window is a standard titled window: the title bar draws the
/// system's material and the panes fill this view. A translucent window
/// (ADR 0043, E7) has a transparent title bar over full-size content, so the
/// title bar and tab bar draw no band of their own. Under them, the strip is
/// always blurred by a ``PaneBackdropView`` (whatever the panes' own blur
/// setting) so the title, the tab titles and the traffic lights stay legible
/// over a busy desktop, and a ``StripTintView`` over the blur gives each pane
/// that touches the top of the panes its session's configured background
/// across its own width, as the pane draws it over its own blur. The panes
/// stay below the title bar and tab bar, and a switch keeps the window's
/// frame, so they keep their size and the sessions are not resized.
///
/// With `[appearance] blur_radius` above 0 (ADR 0045), a translucent window
/// whose translucent panes all ask for blur is blurred whole at that radius
/// (``WindowBlurRadius``) and the panes' own blur is hidden; otherwise, or
/// if the call is not available, each pane keeps its material blur. The
/// strip keeps its material either way (D4): over a window blurred at a
/// radius it shows its own blur, not the radius.
@MainActor
final class TerminalWindowContentView: NSView {
    let paneContainer: PaneContainerView

    /// The blur under the title bar and tab bar of a translucent window,
    /// whether or not a pane of the tab is blurred; `nil` while opaque.
    private(set) var titlebarBackdrop: PaneBackdropView?

    /// The theme's tint over ``titlebarBackdrop``, the same size; `nil`
    /// while opaque.
    private(set) var titlebarTint: StripTintView?

    /// The window's `contentLayoutRect`, which a tab bar coming or going
    /// changes without resizing a full-size content view.
    private var layoutRectObservation: NSKeyValueObservation?

    /// The selected tab of the window's tab group, which AppKit changes
    /// with no window notification (``windowDidBecomeVisible()``).
    private var tabSelectionObservation: NSKeyValueObservation?

    /// While ``updateTitlebar(translucent:)`` changes the style mask:
    /// the window passes through another frame, which the panes must not
    /// take.
    private var holdsPanes = false

    /// The title bar's height over the panes, which the window's smallest
    /// content size adds; 0 for an opaque window. Never less than the title
    /// bar's own height (``standardTitlebarHeight``), even while the content
    /// is no taller than that and so all of it is under the title bar (#388
    /// m1).
    private(set) var titlebarInset: CGFloat = 0

    /// The height of the title bar and tab bar over the panes the last time
    /// the content was taller than both: the floor of ``titlebarInset``
    /// while it is not, so the smallest frame of a tabbed window leaves the
    /// panes their smallest size (#403 m2).
    private var unclippedStripHeight: CGFloat = 0

    /// `[appearance] blur_radius` for this window, 0 while Reduce
    /// Transparency is on (``AppDelegate`` sets it on every window);
    /// a change applies at once (``updateWindowBlur()``).
    var blurRadius: UInt32 = 0 {
        didSet {
            if blurRadius != oldValue { updateWindowBlur() }
        }
    }

    /// The radius last set on the window and the window number it was set
    /// on, and whether the call reported no failure; `nil` before the
    /// first. The call is made again only when one of them changes.
    private(set) var appliedBlur: (windowNumber: Int, radius: UInt32, succeeded: Bool)?

    /// Whether the window is blurred at ``blurRadius`` now, with the panes'
    /// own blur hidden.
    private(set) var usesBlurRadius = false

    /// A view for `paneContainer`, which ``install(in:)`` puts in it.
    init(paneContainer: PaneContainerView, frame: NSRect) {
        self.paneContainer = paneContainer
        super.init(frame: frame)
        paneContainer.frame = bounds
        paneContainer.autoresizingMask = []
    }

    /// Makes this view `window`'s content view, then puts the panes in it.
    /// In that order: a pane makes the window transparent as it attaches,
    /// when it moves into the window, and a window whose background changes
    /// while AppKit still sets its content view keeps its title bar's views
    /// (the title, the tab bar, the buttons) under the content view, where
    /// the strip covers them (#409).
    func install(in window: NSWindow) {
        window.contentView = self
        addSubview(paneContainer)
        placeSubviews()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// The panes' frame: the window's content layout rect, below its title
    /// bar and tab bar, in this view; all of it outside a window.
    var paneArea: NSRect {
        guard let window else { return bounds }
        let area = convert(window.contentLayoutRect, from: nil).intersection(bounds)
        return area.isNull ? bounds : area
    }

    /// Makes the window's title bar transparent over full-size content while
    /// `translucent`, standard otherwise, keeping the window's frame, and
    /// blurs under it while `translucent`, whatever the panes' blur.
    func updateTitlebar(translucent: Bool) {
        guard let window else { return }
        if window.styleMask.contains(.fullSizeContentView) != translucent {
            // AppKit keeps the content size across the change, so the frame
            // grows or shrinks by the title bar; it is put back.
            let frame = window.frame
            holdsPanes = true
            if translucent {
                window.styleMask.insert(.fullSizeContentView)
            } else {
                window.styleMask.remove(.fullSizeContentView)
            }
            window.titlebarAppearsTransparent = translucent
            if window.frame != frame {
                window.setFrame(frame, display: false)
            }
            holdsPanes = false
        }
        if translucent, titlebarBackdrop == nil {
            let view = PaneBackdropView(frame: .zero)
            view.autoresizingMask = []
            addSubview(view, positioned: .below, relativeTo: paneContainer)
            titlebarBackdrop = view
            let tint = StripTintView(frame: .zero)
            addSubview(tint, positioned: .above, relativeTo: view)
            titlebarTint = tint
        } else if !translucent, let titlebarBackdrop {
            titlebarBackdrop.removeFromSuperview()
            self.titlebarBackdrop = nil
            titlebarTint?.removeFromSuperview()
            titlebarTint = nil
        }
        placeSubviews()
        updateWindowBlur()
    }

    /// The radius the window asks for now: ``blurRadius`` while the window
    /// is translucent and every translucent pane of the tab, hidden ones
    /// included, asks for blur (ADR 0045, option ii); 0 otherwise, an opaque
    /// window included.
    var wantedBlurRadius: UInt32 {
        let panes = paneContainer.panes.map(\.view)
        let translucent = panes.filter { !$0.isBackgroundOpaque }
        guard !translucent.isEmpty, translucent.allSatisfy({ $0.backgroundAppearance?.blur == true }) else {
            return 0
        }
        return min(blurRadius, WindowBlurRadius.maximum)
    }

    /// Sets the window's blur radius to ``wantedBlurRadius`` once the window
    /// is on screen (before, the call has no effect), when the radius or the
    /// window number changed; then hides each pane's own blur while the
    /// radius blurs the window, and shows it otherwise. Radius 0 is set only
    /// on a window that had another, so a window that never asks for one
    /// makes no call. While the window is off screen (an unselected tab,
    /// minimized, the app hidden) it keeps the state it has, as the radius
    /// set on it stays (#405 m1): an update meanwhile (a theme reload) does
    /// not show the panes' material, which would flash as it comes back.
    func updateWindowBlur() {
        guard let window else { return }
        let radius = wantedBlurRadius
        var active = usesBlurRadius
        if let number = WindowBlurRadius.windowNumber(window) {
            active = false
            if let appliedBlur, appliedBlur.windowNumber == number, appliedBlur.radius == radius {
                active = appliedBlur.succeeded && radius > 0
            } else if radius == 0, appliedBlur?.windowNumber != number || appliedBlur?.radius == 0 {
                appliedBlur = (number, 0, true)
            } else {
                let succeeded = WindowBlurRadius.set(radius, windowNumber: number)
                appliedBlur = (number, radius, succeeded)
                active = succeeded && radius > 0
            }
        }
        usesBlurRadius = active
        for pane in paneContainer.panes {
            pane.view.backdrop?.isHidden = active
        }
    }

    /// The window came back on screen (unhidden, deminiaturized), or went
    /// off or under another: a radius waiting for a window number is set
    /// now. A first show is ``TerminalWindow``'s ordering in.
    @objc private func windowOcclusionDidChange(_ notification: Notification) {
        updateWindowBlur()
    }

    /// The window's tab may be shown now (#409, #405 n1): its tab was
    /// selected (by any route: ⇧⌘], ⌘1 to ⌘9, a click, `selectedWindow`, the
    /// selected tab closing), it was ordered in, or it, a window of its tab
    /// group, or the app came back from the Dock or from hiding. Makes the
    /// window's opacity, title bar, strip and blur radius what its panes ask
    /// for now, as on a pane's attach; what is already right is left alone.
    func windowDidBecomeVisible() {
        guard window != nil else { return }
        paneContainer.paneOpacityDidChange()
    }

    /// A window was deminiaturized: if it is a tab of this window's group
    /// (only the selected one is posted), this tab shows again.
    @objc private func anyWindowDidDeminiaturize(_ notification: Notification) {
        guard let window, let restored = notification.object as? NSWindow,
            (window.tabGroup?.windows ?? [window]).contains(where: { $0 === restored })
        else { return }
        windowDidBecomeVisible()
    }

    @objc private func applicationDidUnhide(_ notification: Notification) {
        windowDidBecomeVisible()
    }

    /// Tints the strip again from the panes now at the top (their frames or
    /// their appearance changed). Cheap: a few panes at most, each with its
    /// appearance as last read.
    func updateTitlebarTint() {
        guard !holdsPanes, let titlebarTint else { return }
        let segments = paneContainer.topEdgePanes.map { pane in
            (
                minX: titlebarTint.convert(NSPoint(x: pane.frame.minX, y: 0), from: paneContainer).x,
                maxX: titlebarTint.convert(NSPoint(x: pane.frame.maxX, y: 0), from: paneContainer).x,
                color: pane.leaf.view.stripTint
            )
        }
        let span = titlebarTint.bounds
        titlebarTint.segments = Self.tile(segments, across: span.minX..<span.maxX)
    }

    /// `segments`, sorted by their left edge, stretched to cover `span` with
    /// no gap: the first from its start, the last to its end, each boundary
    /// in the middle of the gap a divider leaves.
    static func tile(
        _ segments: [(minX: CGFloat, maxX: CGFloat, color: CGColor?)], across span: Range<CGFloat>
    ) -> [StripTintView.Segment] {
        let sorted = segments.sorted { $0.minX < $1.minX }
        return sorted.indices.map { index in
            let start = index == sorted.startIndex ? span.lowerBound : (sorted[index - 1].maxX + sorted[index].minX) / 2
            let end = index == sorted.index(before: sorted.endIndex)
                ? span.upperBound : (sorted[index].maxX + sorted[index + 1].minX) / 2
            return .init(minX: start, maxX: max(start, end), color: sorted[index].color)
        }
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        super.resizeSubviews(withOldSize: oldSize)
        placeSubviews()
    }

    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        layoutRectObservation = nil
        tabSelectionObservation = nil
        if let window {
            NotificationCenter.default.removeObserver(
                self, name: NSWindow.didChangeOcclusionStateNotification, object: window)
        }
        NotificationCenter.default.removeObserver(self, name: NSWindow.didDeminiaturizeNotification, object: nil)
        NotificationCenter.default.removeObserver(self, name: NSApplication.didUnhideNotification, object: nil)
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        // Called on the main thread, as AppKit lays the window out.
        layoutRectObservation = window.observe(\.contentLayoutRect) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.placeSubviews() }
        }
        // On the main thread, also for a window that joins a tab group after
        // this view moved into it.
        tabSelectionObservation = window.observe(\.tabGroup?.selectedWindow) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.windowDidBecomeVisible() }
        }
        // Posted on the main thread; the centre keeps no reference to this
        // view.
        NotificationCenter.default.addObserver(
            self, selector: #selector(windowOcclusionDidChange(_:)),
            name: NSWindow.didChangeOcclusionStateNotification, object: window)
        // Any window's: the one restored may be another tab of this group.
        NotificationCenter.default.addObserver(
            self, selector: #selector(anyWindowDidDeminiaturize(_:)), name: NSWindow.didDeminiaturizeNotification,
            object: nil)
        NotificationCenter.default.addObserver(
            self, selector: #selector(applicationDidUnhide(_:)), name: NSApplication.didUnhideNotification,
            object: nil)
        placeSubviews()
        updateWindowBlur()
    }

    /// Lays the panes out in ``paneArea`` and the blur above them; the
    /// window's smallest size follows a change of the title bar's height.
    private func placeSubviews() {
        guard !holdsPanes else { return }
        let area = paneArea
        if paneContainer.frame != area {
            paneContainer.frame = area
        }
        let strip = NSRect(
            x: bounds.minX, y: area.maxY, width: bounds.width, height: max(0, bounds.maxY - area.maxY))
        titlebarBackdrop?.frame = strip
        titlebarTint?.frame = strip
        updateTitlebarTint()
        var inset = bounds.height - area.height
        if inset > 0, let window {
            if area.height > 0 {
                // The panes have height, so all of the strip is above them.
                unclippedStripHeight = inset
            }
            inset = max(inset, unclippedStripHeight, Self.standardTitlebarHeight(of: window))
        }
        if inset != titlebarInset {
            titlebarInset = inset
            paneContainer.updateWindowMinimumSize()
        }
    }

    /// The height of `window`'s title bar alone, without a tab bar: the
    /// frame a standard (not full-size) window of its style puts round an
    /// empty content rect.
    static func standardTitlebarHeight(of window: NSWindow) -> CGFloat {
        NSWindow.frameRect(forContentRect: .zero, styleMask: window.styleMask.subtracting(.fullSizeContentView)).height
    }
}

/// The theme's colour over the strip's blur in a translucent window (#388,
/// ADR 0043 strip tint): one segment per pane at the top of the panes, each
/// across that pane's width (stretched over the dividers), filled with the
/// pane session's configured background at its alpha, or left clear for a
/// pane without a surface. Takes no clicks, the pointer or accessibility.
@MainActor
final class StripTintView: NSView {
    struct Segment: Equatable {
        var minX: CGFloat
        var maxX: CGFloat
        /// `nil`: nothing drawn, the blur alone shows.
        var color: CGColor?
    }

    var segments: [Segment] = [] {
        didSet {
            if segments != oldValue { needsDisplay = true }
        }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        autoresizingMask = []
        setAccessibilityElement(false)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isOpaque: Bool { false }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        for segment in segments {
            guard let color = segment.color else { continue }
            context.setFillColor(color)
            let rect = CGRect(
                x: segment.minX, y: bounds.minY, width: segment.maxX - segment.minX, height: bounds.height)
            context.fill(Self.pixelAligned(rect, in: context))
        }
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        nil
    }

    /// `rect` with its edges on the nearest device pixels of `context`: a
    /// boundary between two segments falls in the middle of a divider, on a
    /// half point, which at 1x both segments would cover half, anti-aliased,
    /// leaving a lighter line (#388). Both round the shared edge the same
    /// way, so they meet with no gap and no overlap.
    static func pixelAligned(_ rect: CGRect, in context: CGContext) -> CGRect {
        let device = context.convertToDeviceSpace(rect).standardized
        let minX = device.minX.rounded()
        let minY = device.minY.rounded()
        let aligned = CGRect(
            x: minX, y: minY, width: device.maxX.rounded() - minX, height: device.maxY.rounded() - minY)
        return context.convertToUserSpace(aligned)
    }
}
