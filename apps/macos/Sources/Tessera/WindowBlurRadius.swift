import AppKit

/// The window's blur (#491, ADR 0045 amendment): `[appearance]
/// blur_radius` as the strength of one ``WindowBackdropView`` per window,
/// masked to the window's rounded corners. No private call: the
/// WindowServer's radius call of 0.2.0 blurred the window's square
/// rectangle, whose sharp corners showed round the rounded window, and
/// nothing clips it. One private read stays: the corner radius AppKit
/// draws the window with (`_cornerRadius`, by key-value coding, see
/// ``cornerRadius(of:)``), with a fallback where the window does not
/// answer it.
///
/// AppKit has no blur of a chosen radius behind a window, so the radius is
/// approximated by how much of the system material shows: radius `r` draws
/// the material at an opacity of `r / 64` over the unblurred desktop. 0 is
/// no blur at all, with no effect view; 64 is the full material of ADR
/// 0043.
@MainActor
enum WindowBlurRadius {
    /// The largest radius (`Appearance::MAX_BLUR_RADIUS`).
    static let maximum: UInt32 = 64

    /// `[appearance] blur_radius`'s default (`Appearance::default()`), used
    /// by a window until the app delegate sets the configured one.
    static let defaultRadius: UInt32 = 48

    /// The opacity of the window's material at `radius`, clamped to
    /// 0...``maximum``: 0 for none, 1 for the full material.
    static func strength(_ radius: UInt32) -> CGFloat {
        CGFloat(min(radius, maximum)) / CGFloat(maximum)
    }

    /// The radius of `window`'s rounded corners: 0 in native full screen,
    /// whose window fills the screen with square corners; otherwise what
    /// AppKit draws the window with where it says (`_cornerRadius`, read
    /// only when the window answers it), else the standard titled window's
    /// (10 pt before macOS 26, 16 pt from it).
    static func cornerRadius(of window: NSWindow) -> CGFloat {
        if window.styleMask.contains(.fullScreen) { return 0 }
        if window.responds(to: NSSelectorFromString("_cornerRadius")),
            let value = window.value(forKey: "_cornerRadius") as? NSNumber,
            value.doubleValue.isFinite, value.doubleValue >= 0
        {
            return CGFloat(value.doubleValue)
        }
        return ProcessInfo.processInfo.isOperatingSystemAtLeast(
            OperatingSystemVersion(majorVersion: 26, minorVersion: 0, patchVersion: 0)) ? 16 : 10
    }

    /// The outline of a window's blur, `size` points: each of `regions`
    /// (the strip and the blurred panes, in the content view's coordinates)
    /// inside the window's outline, a rectangle of that size with
    /// `cornerRadius` corners, so no blur shows past a rounded corner. A
    /// path, not a bitmap: a mask image the window's size costs about
    /// 20 MB at 2x and is drawn again on every resize step (#491 B3).
    nonisolated static func maskPath(size: CGSize, cornerRadius: CGFloat, regions: [CGRect]) -> CGPath {
        let outline = CGRect(origin: .zero, size: size)
        let radius = min(max(cornerRadius, 0), min(size.width, size.height) / 2)
        let rounded = CGPath(roundedRect: outline, cornerWidth: radius, cornerHeight: radius, transform: nil)
        let covered = CGMutablePath()
        covered.addRects(regions)
        return covered.intersection(rounded)
    }
}

/// The one blur of a translucent window (#491): what is behind the window,
/// blurred through the `.fullScreenUI` material (ADR 0043:
/// `.underWindowBackground` gave the theme's colours a grey cast), at the
/// strength of the radius (``WindowBlurRadius/strength(_:)``), shown only
/// where its mask is: under the title bar and tab bar, and under each
/// translucent pane that asks for blur, within the window's rounded
/// corners. Active whether or not the window is key, so the blur does not
/// change when the focus moves to another window. It fills the content
/// view and takes no clicks, the pointer or accessibility. Its mask is a
/// shape layer on its own layer, as `maskImage` is a layer mask over the
/// material's backdrop layer, but drawn from a path with no bitmap. It sets no
/// appearance of its own, so it draws in the app's (`app_appearance`,
/// #365) as the window's effective appearance resolves it.
@MainActor
final class WindowBackdropView: NSVisualEffectView {
    /// The regions, corner radius and size the mask was last drawn for:
    /// it is drawn again only when one of them changes.
    private(set) var maskedRegions: [NSRect] = []
    private(set) var maskedCornerRadius: CGFloat = -1
    private var maskedSize: NSSize = .zero
    private let maskShape = CAShapeLayer()

    /// The outline the blur shows through, in the view's coordinates.
    var maskPath: CGPath? { maskShape.path }

    override init(frame: NSRect) {
        super.init(frame: frame)
        blendingMode = .behindWindow
        material = .fullScreenUI
        state = .active
        autoresizingMask = []
        wantsLayer = true
        setAccessibilityElement(false)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        nil
    }

    /// Masks the blur to `regions` inside a window outline with
    /// `cornerRadius` corners over the view's bounds.
    func setMask(regions: [NSRect], cornerRadius: CGFloat) {
        let size = bounds.size
        // On the view's layer as it is now, should AppKit make another.
        if let layer, layer.mask !== maskShape { layer.mask = maskShape }
        guard regions != maskedRegions || cornerRadius != maskedCornerRadius || size != maskedSize else { return }
        maskedRegions = regions
        maskedCornerRadius = cornerRadius
        maskedSize = size
        // At once, not animated as a shape layer's path otherwise is.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        maskShape.frame = CGRect(origin: .zero, size: size)
        maskShape.path = WindowBlurRadius.maskPath(size: size, cornerRadius: cornerRadius, regions: regions)
        CATransaction.commit()
    }
}
