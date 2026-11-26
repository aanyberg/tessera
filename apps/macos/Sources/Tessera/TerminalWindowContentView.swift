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
/// always blurred (whatever the panes' own blur setting, while
/// `blur_radius` is above 0) so the title, the tab titles and the traffic lights stay legible
/// over a busy desktop, and a ``StripTintView`` over the blur gives each pane
/// that touches the top of the panes its session's configured background
/// across its own width, as the pane draws it over its own blur. The panes
/// stay below the title bar and tab bar, and a switch keeps the window's
/// frame, so they keep their size and the sessions are not resized.
///
/// The blur is one ``WindowBackdropView`` per window (#491, ADR 0045
/// amendment), under everything, at the strength of `[appearance]
/// blur_radius` and masked to the strip and the translucent panes that ask
/// for blur, inside the window's rounded corners; radius 0 is no blur and
/// no effect view.
///
/// In native full screen (ADR 0043 D7) the title bar and tab bar move into
/// AppKit's full-screen toolbar window, where there is no desktop behind
/// them: they draw opaque there (the title bar is not transparent) and the
/// content has no strip under them, whatever the window's opacity and blur;
/// the panes keep theirs. There the title row slides away with the menu bar,
/// and the tab bar, which keeps its windowed space below the tabs and none
/// above (the title row is above it when windowed), would sit at the top
/// edge of the screen: a spacer over it gives the tabs the same space above
/// as below (#621). Leaving full screen brings the windowed look back.
@MainActor
final class TerminalWindowContentView: NSView {
    let paneContainer: PaneContainerView

    /// The window's one blur (``updateWindowBlur()``): `nil` while it
    /// blurs nothing (opaque, or radius 0).
    private(set) var windowBackdrop: WindowBackdropView?

    /// The theme's tint over the strip's blur, the strip's size; `nil`
    /// while opaque.
    private(set) var titlebarTint: StripTintView?

    /// The empty title bar accessory over the tab bar in native full
    /// screen (``TitlebarLayout/tabRowSpacerHeight``), added the first
    /// time one is needed and hidden (taking no height) while none is;
    /// `nil` before.
    private(set) var tabRowSpacer: NSTitlebarAccessoryViewController?

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
    /// Transparency is on (``AppDelegate`` sets it on every window; the
    /// default until then); a change applies at once (``updateWindowBlur()``).
    var blurRadius: UInt32 = WindowBlurRadius.defaultRadius {
        didSet {
            if blurRadius != oldValue { updateWindowBlur() }
        }
    }

    /// What the title bar and the strip under it are for a window
    /// (ADR 0043 D7).
    struct TitlebarLayout: Equatable {
        /// The window's `titlebarAppearsTransparent`.
        var titlebarTransparent: Bool
        /// The height of the blurred, tinted strip under the title bar and
        /// tab bar; 0 for none.
        var stripHeight: CGFloat
        /// The height of the spacer over the tab bar (``tabRowSpacer``);
        /// 0 for none.
        var tabRowSpacerHeight: CGFloat = 0
    }

    /// The space above and below the tab row (AppKit's tab bar) in the
    /// title bar accessory that holds it, as AppKit lays it out: on macOS
    /// 27, none above and 8 pt below, the title row being above it in a
    /// window.
    struct TabRowInsets: Equatable {
        var top: CGFloat
        var bottom: CGFloat
    }

    /// The title bar and strip of a window in native full screen or not,
    /// translucent or opaque, whose content runs `measuredStrip` above the
    /// panes (under the title bar and tab bar) and whose tab row has
    /// `tabRow` round it (`nil`: no tab bar): windowed and translucent,
    /// a transparent title bar over a strip of that height; otherwise
    /// (opaque, or in full screen, which has no desktop behind the bars)
    /// a title bar that draws its own material and no strip. In full
    /// screen, where the title row is hidden with the menu bar and the tab
    /// bar is the top of the screen, a spacer over the tab bar makes the
    /// space above the tabs what AppKit leaves below them (#621); windowed,
    /// none, and none where there is more space above than below.
    static func titlebarLayout(
        fullScreen: Bool, translucent: Bool, measuredStrip: CGFloat, tabRow: TabRowInsets?
    ) -> TitlebarLayout {
        guard translucent, !fullScreen else {
            let spacer = fullScreen ? max(0, (tabRow?.bottom ?? 0) - (tabRow?.top ?? 0)) : 0
            return TitlebarLayout(titlebarTransparent: false, stripHeight: 0, tabRowSpacerHeight: spacer)
        }
        return TitlebarLayout(titlebarTransparent: true, stripHeight: max(0, measuredStrip))
    }

    /// The insets of the tab row of `window`'s tab group
    /// (``TabRowInsets``): read from the accessory of whichever of its
    /// windows holds the tab bar now (the selected one, which a tab
    /// selection may not have moved it to yet); `nil` while the group shows
    /// no tab bar, or where AppKit has no `NSTabBar` to read.
    static func tabRowInsets(of window: NSWindow) -> TabRowInsets? {
        guard let tabBarClass = NSClassFromString("NSTabBar") else { return nil }
        func insets(in window: NSWindow) -> TabRowInsets? {
            for accessory in window.titlebarAccessoryViewControllers
            where !(accessory is TabRowSpacerController) && !accessory.isHidden {
                let bar = accessory.view
                guard let row = bar.subviews.first(where: { $0.isKind(of: tabBarClass) && !$0.isHidden }) else {
                    continue
                }
                let frame = row.frame
                let bounds = bar.bounds
                return bar.isFlipped
                    ? TabRowInsets(top: frame.minY - bounds.minY, bottom: bounds.maxY - frame.maxY)
                    : TabRowInsets(top: bounds.maxY - frame.maxY, bottom: frame.minY - bounds.minY)
            }
            return nil
        }
        if let own = insets(in: window) { return own }
        return (window.tabGroup?.windows ?? []).lazy.filter { $0 !== window }.compactMap(insets(in:)).first
    }

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
    /// blurs under it while `translucent`, whatever the panes' blur; in
    /// native full screen the title bar draws its own material over the
    /// full-size content and nothing is under it
    /// (``titlebarLayout(fullScreen:translucent:measuredStrip:tabRow:)``).
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
            // In full screen, ``placeSubviews()`` below makes it opaque again.
            window.titlebarAppearsTransparent = translucent
            if window.frame != frame {
                window.setFrame(frame, display: false)
            }
            holdsPanes = false
        }
        if !translucent {
            // No strip while opaque: a floor kept from before would outlive a
            // tab bar that goes meanwhile and keep the next translucent
            // window's smallest size too tall (#417 M6).
            unclippedStripHeight = 0
        }
        if translucent, titlebarTint == nil {
            let tint = StripTintView(frame: .zero)
            addSubview(tint, positioned: .below, relativeTo: paneContainer)
            titlebarTint = tint
        } else if !translucent, let titlebarTint {
            titlebarTint.removeFromSuperview()
            self.titlebarTint = nil
        }
        placeSubviews()
        updateWindowBlur()
    }

    /// The parts of the window its blur covers, in this view: the strip
    /// under the title bar and tab bar, and every shown pane that asks for
    /// blur (``TerminalSurfaceView/wantsBlur``); none while the window is
    /// opaque.
    var blurRegions: [NSRect] {
        guard let titlebarTint else { return [] }
        var regions: [NSRect] = []
        if !titlebarTint.frame.isEmpty { regions.append(titlebarTint.frame) }
        for pane in paneContainer.panes
        where pane.view.wantsBlur && pane.view.window === window && !pane.view.isHiddenOrHasHiddenAncestor {
            let frame = convert(pane.view.bounds, from: pane.view).intersection(bounds)
            if !frame.isEmpty { regions.append(frame) }
        }
        return regions
    }

    /// Makes the window's blur what it asks for now: with ``blurRadius``
    /// above 0 and ``blurRegions`` to cover, one ``windowBackdrop`` under
    /// everything, filling this view, at the radius's strength and masked
    /// to those regions inside the window's rounded corners; otherwise
    /// none. What is already right is left alone.
    func updateWindowBlur() {
        guard let window, !holdsPanes else { return }
        let radius = min(blurRadius, WindowBlurRadius.maximum)
        let regions = radius > 0 ? blurRegions : []
        guard !regions.isEmpty else {
            if let windowBackdrop {
                windowBackdrop.removeFromSuperview()
                self.windowBackdrop = nil
            }
            return
        }
        let view: WindowBackdropView
        if let windowBackdrop {
            view = windowBackdrop
        } else {
            view = WindowBackdropView(frame: bounds)
            // Under the strip's tint and the panes.
            addSubview(view, positioned: .below, relativeTo: subviews.first)
            windowBackdrop = view
        }
        if view.frame != bounds { view.frame = bounds }
        let strength = WindowBlurRadius.strength(radius)
        if view.alphaValue != strength { view.alphaValue = strength }
        view.setMask(regions: regions, cornerRadius: WindowBlurRadius.cornerRadius(of: window))
    }

    /// The window's tab may be shown now (#409, #405 n1): its tab was
    /// selected (by any route: ⇧⌘], ⌘1 to ⌘9, a click, `selectedWindow`, the
    /// selected tab closing), it was ordered in, or it or the app came back
    /// from the Dock or from hiding. Makes the window's opacity, title bar,
    /// strip and blur radius what its panes ask for now, as on a pane's
    /// attach; what is already right is left alone.
    func windowDidBecomeVisible() {
        guard window != nil else { return }
        paneContainer.paneOpacityDidChange()
    }

    /// This window was deminiaturized. macOS posts it for every window of
    /// a tab group that comes back (macOS 27.2), so each tab answers only
    /// its own: one ``windowDidBecomeVisible()`` per window, not one per
    /// window for each of them (#417). Where only the selected tab is
    /// posted, the others are brought up to date as they are selected.
    @objc private func windowDidDeminiaturize(_ notification: Notification) {
        guard let window, notification.object as? NSWindow === window else { return }
        deminiaturizeCount += 1
        windowDidBecomeVisible()
    }

    /// How many deminiaturizes this tab answered (tests).
    private(set) var deminiaturizeCount = 0

    @objc private func applicationDidUnhide(_ notification: Notification) {
        windowDidBecomeVisible()
    }

    /// Masks the blur and tints the strip again from the panes now (their
    /// frames or their appearance changed). Cheap: a few panes at most, each
    /// with its appearance as last read; the mask is drawn again only when
    /// it changes.
    func updateTitlebarTint() {
        guard !holdsPanes else { return }
        updateWindowBlur()
        guard let titlebarTint else { return }
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
        if let window, let tabRowSpacer {
            if let index = window.titlebarAccessoryViewControllers.firstIndex(of: tabRowSpacer) {
                window.removeTitlebarAccessoryViewController(at: index)
            }
            self.tabRowSpacer = nil
        }
        if let window {
            NotificationCenter.default.removeObserver(self, name: NSWindow.didDeminiaturizeNotification, object: window)
        }
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
            self, selector: #selector(windowDidDeminiaturize(_:)), name: NSWindow.didDeminiaturizeNotification,
            object: window)
        NotificationCenter.default.addObserver(
            self, selector: #selector(applicationDidUnhide(_:)), name: NSApplication.didUnhideNotification,
            object: nil)
        placeSubviews()
    }

    /// Lays the panes out in ``paneArea`` and the blur above them, and
    /// makes the title bar transparent or not, as
    /// ``titlebarLayout(fullScreen:translucent:measuredStrip:tabRow:)`` has it for
    /// the window now (in or out of native full screen, whose transition
    /// changes the layout rect); the window's smallest size follows a
    /// change of the title bar's height. The spacer over the tab bar comes
    /// first, as it moves the layout rect.
    private func placeSubviews() {
        guard !holdsPanes else { return }
        let style = window?.styleMask ?? []
        let fullScreen = style.contains(.fullScreen)
        let translucent = style.contains(.fullSizeContentView)
        // Read in full screen only: a window has no spacer, and reading
        // its tab group the first time creates it (#608), here while
        // AppKit still places a new window.
        let tabRow = fullScreen ? window.flatMap(Self.tabRowInsets(of:)) : nil
        placeTabRowSpacer(
            Self.titlebarLayout(fullScreen: fullScreen, translucent: translucent, measuredStrip: 0, tabRow: tabRow)
                .tabRowSpacerHeight)
        let area = paneArea
        if paneContainer.frame != area {
            paneContainer.frame = area
        }
        let layout = Self.titlebarLayout(
            fullScreen: fullScreen, translucent: translucent, measuredStrip: bounds.maxY - area.maxY, tabRow: tabRow)
        if let window, window.titlebarAppearsTransparent != layout.titlebarTransparent {
            window.titlebarAppearsTransparent = layout.titlebarTransparent
        }
        let strip = NSRect(x: bounds.minX, y: area.maxY, width: bounds.width, height: layout.stripHeight)
        titlebarTint?.frame = strip
        updateTitlebarTint()
        // Only a full-size (translucent) window has a strip: an opaque one
        // passes through a frame with its content over a tab bar that is
        // going, which is no strip and must not become the floor (#417 M6).
        // Content of no height (a program's frame) is all under the title
        // bar too (#417 T5 m2).
        var inset: CGFloat = 0
        if let window, window.styleMask.contains(.fullSizeContentView),
            bounds.height > area.height || bounds.height == 0
        {
            inset = bounds.height - area.height
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

    /// Shows ``tabRowSpacer`` `height` high over the tab bar, in full
    /// screen too (`fullScreenMinHeight`), adding it the first time; hides
    /// it for 0. AppKit sizes an accessory's view as it adds it, so the
    /// height is set after.
    private func placeTabRowSpacer(_ height: CGFloat) {
        guard let window else { return }
        guard height > 0 else {
            if let tabRowSpacer, !tabRowSpacer.isHidden { tabRowSpacer.isHidden = true }
            return
        }
        let spacer: NSTitlebarAccessoryViewController
        if let tabRowSpacer {
            spacer = tabRowSpacer
        } else {
            spacer = TabRowSpacerController()
            spacer.view = NSView(frame: NSRect(x: 0, y: 0, width: bounds.width, height: height))
            spacer.layoutAttribute = .bottom
            // Kept before it is added: adding it moves the layout rect,
            // whose observer lays out again from inside the call.
            tabRowSpacer = spacer
            window.addTitlebarAccessoryViewController(spacer)
        }
        if spacer.view.frame.height != height {
            spacer.view.setFrameSize(NSSize(width: spacer.view.frame.width, height: height))
        }
        if spacer.fullScreenMinHeight != height { spacer.fullScreenMinHeight = height }
        if spacer.isHidden { spacer.isHidden = false }
    }

    /// The height of `window`'s title bar alone, without a tab bar: the
    /// frame a standard (not full-size) window of its style puts round an
    /// empty content rect.
    static func standardTitlebarHeight(of window: NSWindow) -> CGFloat {
        NSWindow.frameRect(forContentRect: .zero, styleMask: window.styleMask.subtracting(.fullSizeContentView)).height
    }
}

/// ``TerminalWindowContentView/tabRowSpacer``'s controller: set apart from
/// AppKit's tab bar accessory, which ``TerminalWindowContentView/tabRowInsets(of:)``
/// reads, in every tab of a group.
final class TabRowSpacerController: NSTitlebarAccessoryViewController {}

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
