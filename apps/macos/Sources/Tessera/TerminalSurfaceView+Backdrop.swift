import AppKit
import TesseraKit

/// Translucency and background blur (#42, ADR 0043), per pane: the core
/// draws each session's background at its profile's opacity, and a pane
/// whose session asks for blur gets a ``PaneBackdropView`` under its render
/// view, which blurs what is behind the window there. A window is opaque
/// only while every pane of its tab is, a pane hidden behind a maximized
/// one included (E7): so a hidden translucent pane keeps the window, and
/// with it the blurred strip under the title bar, translucent, which keeps
/// a maximize from changing the window. The core forces every pane
/// opaque and unblurred while the system's Reduce Transparency is on (E8,
/// ``AppDelegate/applyDisplayOptions()``).
///
/// The appearance is the session's (``TesseraSession/appearance()``), read
/// when the surface attaches and again on every
/// ``TesseraAppDelegate/appearanceDidChange(opaque:)``; nothing here reads
/// the configuration.
extension TerminalSurfaceView {
    /// Whether this pane covers its frame with an opaque background: its
    /// session's is opaque, or it has no surface (it draws nothing, so a
    /// transparent window would show only the desktop through it).
    var isBackgroundOpaque: Bool {
        guard surface.isAttached, let backgroundAppearance else { return true }
        return backgroundAppearance.opaque
    }

    /// The tint of the strip above this pane in a translucent window
    /// (#388): its session's configured background at its alpha, as last
    /// read; `nil` without a surface (the pane draws nothing either).
    var stripTint: CGColor? {
        guard surface.isAttached, let color = backgroundAppearance?.background else { return nil }
        return CGColor(
            srgbRed: CGFloat(color.red), green: CGFloat(color.green), blue: CGFloat(color.blue),
            alpha: CGFloat(color.alpha))
    }

    /// Reads the session's appearance, shows or removes the blur under the
    /// render view to match, and has the window follow its panes. Called on
    /// attach and on every appearance change. Without a surface the pane is
    /// left unblurred.
    func updateBackdrop() {
        let appearance: TesseraSession.Appearance?
        do {
            appearance = try session.appearance()
        } catch {
            // Closed meanwhile: nothing is drawn any more.
            appearance = nil
        }
        backgroundAppearance = appearance
        let blur = surface.isAttached && (appearance?.blur ?? false)
        if blur, backdrop == nil {
            let view = PaneBackdropView(frame: bounds)
            // Under everything the pane draws: the text, the scroll bar, the
            // find bar and a dimming shade.
            addSubview(view, positioned: .below, relativeTo: renderView)
            backdrop = view
        } else if !blur, let backdrop {
            backdrop.removeFromSuperview()
            self.backdrop = nil
        }
        paneOpacityDidChange()
    }

    /// The window follows every pane of its tab when the view is in a
    /// ``PaneContainerView``, and this one pane otherwise.
    func paneOpacityDidChange() {
        guard let window else { return }
        if let container = enclosingPaneContainer {
            container.paneOpacityDidChange()
        } else {
            Self.setWindow(window, opaque: isBackgroundOpaque)
        }
    }

    /// The tab's pane container this view is laid out in, if any.
    var enclosingPaneContainer: PaneContainerView? {
        sequence(first: superview, next: { $0?.superview }).lazy.compactMap { $0 as? PaneContainerView }.first
    }

    /// Makes `window` opaque, or transparent with a clear background, and
    /// has its shadow recomputed when that changes. A translucent pane in an
    /// opaque window would show the window's background through it rather
    /// than the desktop.
    static func setWindow(_ window: NSWindow, opaque: Bool) {
        guard window.isOpaque != opaque else { return }
        window.isOpaque = opaque
        window.backgroundColor = opaque ? .windowBackgroundColor : .clear
        window.invalidateShadow()
    }
}

/// The blur under a translucent pane and under a translucent window's title
/// bar and tab bar (ADR 0043): what is behind the window, blurred through the
/// `.fullScreenUI` material (`.underWindowBackground` gave the theme's
/// colours a grey cast; `.popover` is the alternative); active whether or
/// not the window is key, so the pane does not change when the focus moves
/// to another window. It fills the pane it blurs, or the strip above the
/// panes, and takes no clicks, the pointer or accessibility. Its material is
/// a constant the tests pin; whether it looks right (dark and light system
/// appearance, a dark theme at 0.7 and 0.5 opacity) is an eye check.
@MainActor
final class PaneBackdropView: NSVisualEffectView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        blendingMode = .behindWindow
        material = .fullScreenUI
        state = .active
        autoresizingMask = [.width, .height]
        setAccessibilityElement(false)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        nil
    }
}
