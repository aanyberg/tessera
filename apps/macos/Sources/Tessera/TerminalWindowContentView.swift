import AppKit

/// A terminal tab's window content (#363): the tab's ``PaneContainerView``
/// in the window's `contentLayoutRect`, and under the title bar and tab bar
/// of a translucent window what the panes show through.
///
/// An opaque window is a standard titled window: the title bar draws the
/// system's material and the panes fill this view. A translucent window
/// (ADR 0043, E7) has a transparent title bar over full-size content, so the
/// title bar and tab bar draw no band of their own: the desktop shows
/// through them, always blurred by a ``PaneBackdropView`` so the title, the
/// tab titles and the traffic lights stay legible over a busy desktop; each
/// pane still follows its own blur setting. The panes stay below the title
/// bar and tab bar, and a switch keeps the window's frame, so they keep their
/// size and the sessions are not resized.
@MainActor
final class TerminalWindowContentView: NSView {
    let paneContainer: PaneContainerView

    /// The blur under the title bar and tab bar of a translucent window,
    /// whether or not a pane of the tab is blurred; `nil` while opaque.
    private(set) var titlebarBackdrop: PaneBackdropView?

    /// The window's `contentLayoutRect`, which a tab bar coming or going
    /// changes without resizing a full-size content view.
    private var layoutRectObservation: NSKeyValueObservation?

    /// While ``updateTitlebar(translucent:)`` changes the style mask:
    /// the window passes through another frame, which the panes must not
    /// take.
    private var holdsPanes = false

    /// The title bar's height over the panes, which the window's smallest
    /// content size adds; 0 for an opaque window.
    private(set) var titlebarInset: CGFloat = 0

    init(paneContainer: PaneContainerView, frame: NSRect) {
        self.paneContainer = paneContainer
        super.init(frame: frame)
        paneContainer.frame = bounds
        paneContainer.autoresizingMask = []
        addSubview(paneContainer)
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
        } else if !translucent, let titlebarBackdrop {
            titlebarBackdrop.removeFromSuperview()
            self.titlebarBackdrop = nil
        }
        placeSubviews()
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        super.resizeSubviews(withOldSize: oldSize)
        placeSubviews()
    }

    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        layoutRectObservation = nil
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        // Called on the main thread, as AppKit lays the window out.
        layoutRectObservation = window.observe(\.contentLayoutRect) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.placeSubviews() }
        }
        placeSubviews()
    }

    /// Lays the panes out in ``paneArea`` and the blur above them; the
    /// window's smallest size follows a change of the title bar's height.
    private func placeSubviews() {
        guard !holdsPanes else { return }
        let area = paneArea
        if paneContainer.frame != area {
            paneContainer.frame = area
        }
        titlebarBackdrop?.frame = NSRect(
            x: bounds.minX, y: area.maxY, width: bounds.width, height: max(0, bounds.maxY - area.maxY))
        let inset = bounds.height - area.height
        if inset != titlebarInset {
            titlebarInset = inset
            paneContainer.updateWindowMinimumSize()
        }
    }
}
