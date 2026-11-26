import AppKit
import TesseraKit

/// Translucency and background blur (#42, ADR 0043), per pane: the core
/// draws each session's background at its profile's opacity, and a
/// translucent pane whose session asks for blur (``wantsBlur``) is blurred
/// by its window's one ``WindowBackdropView`` (#491), at the window's
/// radius, where the pane is. A window is opaque
/// only while every pane of its tab is, a pane hidden behind a maximized
/// one included (E7): so a hidden translucent pane keeps the window, and
/// with it the blurred strip under the title bar, translucent, which keeps
/// a maximize from changing the window. The core forces every pane
/// opaque and unblurred while the system's Reduce Transparency is on (E8,
/// ``AppDelegate/applyDisplayOptions()``).
///
/// The scroll bar's overlay knob is drawn for the session's background too
/// (``TerminalScroller/themeBackground``).
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

    /// Whether the window blurs what is behind this pane: its session asks
    /// for blur and its background is translucent (an opaque one would hide
    /// the blur); `false` without a surface.
    var wantsBlur: Bool {
        surface.isAttached && backgroundAppearance?.blur == true && !isBackgroundOpaque
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

    /// Reads the session's appearance and has the window follow its panes
    /// (opacity, title bar, blur). Called on attach and on every appearance
    /// change. Without a surface the pane is left unblurred.
    func updateBackdrop() {
        let appearance: TesseraSession.Appearance?
        do {
            appearance = try session.appearance()
        } catch {
            // Closed meanwhile: nothing is drawn any more.
            appearance = nil
        }
        backgroundAppearance = appearance
        scroller.themeBackground = appearance?.background
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
