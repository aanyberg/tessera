import AppKit
import QuartzCore

/// The part of a terminal the core draws into: a layer-backed view whose
/// backing layer is the `CAMetalLayer` (ADR 0017, ADR 0032).
///
/// ``TerminalSurfaceView`` keeps the session, the input and the scroll bar;
/// this subview only carries the layer, so AppKit sizes, moves and scales
/// it, and so it can be narrower than the terminal when the scroll bar sits
/// beside the text ("Show scroll bars: Always"). It takes no pointer
/// events: they all reach the terminal view.
@MainActor
final class TerminalRenderView: NSView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        // The core draws every frame; AppKit must neither redraw nor stretch
        // the layer's contents while a resize is coalesced.
        layerContentsRedrawPolicy = .never
        layerContentsPlacement = .topLeft
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

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
        // Drawn by the core through the terminal view's display link.
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }
}
