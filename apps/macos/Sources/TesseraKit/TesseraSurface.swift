import Foundation
import QuartzCore
internal import TesseraCore

/// A session's on-screen surface: the `CAMetalLayer` the core draws the
/// session into, through `tessera_surface_attach`, `_resize`, `_render`,
/// `_set_font_size`, `_metrics` and `_detach` (ADR 0014).
///
/// Main actor only, like the C functions. Once attached, the session's grid
/// follows the surface: ``attach(layer:widthPx:heightPx:scale:)``,
/// ``resize(widthPx:heightPx:scale:)`` and ``setFontSize(_:)`` resize the
/// session to the cells that fit, so do not also call
/// ``TesseraSession/resize(rows:cols:widthPx:heightPx:)``.
///
/// Ownership: the surface holds a strong reference to the layer from attach
/// until the core has let go of it (``detach()`` returned, or a render found
/// the surface detached after a lost swapchain), so the layer always outlives
/// the core's use of it. Releasing an attached surface detaches it.
///
/// One surface object per session: the C calls are keyed by session, so
/// ``detach()`` or ``render()`` on a stale second `TesseraSurface` for the
/// same session would act on the newer one's attachment.
@MainActor
public final class TesseraSurface {
    /// A surface size in physical pixels at a backing scale.
    public struct Size: Sendable, Equatable {
        public var widthPx: UInt32
        public var heightPx: UInt32
        public var scale: Float

        public init(widthPx: UInt32, heightPx: UInt32, scale: Float) {
            self.widthPx = widthPx
            self.heightPx = heightPx
            self.scale = scale
        }
    }

    /// What ``render()`` did with a frame: `TesseraFrameStatus`.
    public enum FrameStatus: Sendable, Equatable {
        /// Drawn and queued for presentation.
        case presented
        /// Drawn and presented, but the layer's drawables no longer match its
        /// size: resize with the current size before the next frame.
        case suboptimal
        /// Nothing drawn: a zero-sized surface, an occluded window, or no
        /// drawable. Try again on the next display refresh.
        case skipped

        init(_ status: TesseraFrameStatus) {
            switch status {
            case TESSERA_FRAME_STATUS_PRESENTED: self = .presented
            case TESSERA_FRAME_STATUS_SUBOPTIMAL: self = .suboptimal
            default: self = .skipped
            }
        }
    }

    /// The session this surface draws.
    public let session: TesseraSession

    /// The attached layer, or `nil` while detached. Kept alive until the core
    /// no longer uses it.
    public var layer: CAMetalLayer? { attachment?.layer }

    /// Whether the core has a surface for the session.
    public var isAttached: Bool { attachment != nil }

    // The attached layer and what `deinit` needs to detach it. Sendable, so
    // the nonisolated `deinit` can read it.
    private var attachment: Detacher?

    /// The size the core last accepted from an attach or resize, or `nil`
    /// before the first attach. Kept across a detach. The core's own view of
    /// it (after clamping to the GPU's largest texture) is in ``metrics()``.
    public private(set) var size: Size?

    /// The surface's geometry, font size and cursor cell, as the core reports
    /// them (`TesseraSurfaceMetrics`). Lengths are physical pixels; divide by
    /// ``scale`` for points. Cell `(row, col)` covers ``cellWidthPx`` x
    /// ``cellHeightPx`` pixels from `(paddingPx + col * cellWidthPx,
    /// paddingPx + row * cellHeightPx)`, measured from the surface's top-left
    /// corner.
    public struct Metrics: Sendable, Equatable {
        /// The surface's size (after clamping to the GPU's largest texture).
        public var widthPx: UInt32
        public var heightPx: UInt32
        /// The backing scale factor the surface was last attached or resized
        /// with.
        public var scale: Float
        /// One cell at the current font size and scale.
        public var cellWidthPx: UInt32
        public var cellHeightPx: UInt32
        /// From a cell's top to the text baseline.
        public var baselinePx: Float
        /// The empty band between each edge of the surface and the grid.
        public var paddingPx: UInt32
        /// The session's grid. It follows the surface, except after a
        /// ``TesseraSession/resize(rows:cols:widthPx:heightPx:)`` until the
        /// next surface resize or font-size change.
        public var rows: UInt16
        public var cols: UInt16
        /// The font size in points (clamped by the core to 4...512), the
        /// value font zoom steps from.
        public var fontSizePt: Float
        /// The cursor's cell on the live screen (scroll offset 0), within
        /// ``rows`` and ``cols``.
        public var cursorRow: UInt16
        public var cursorCol: UInt16
        /// Whether the cursor is on the screen the surface shows: `false`
        /// while the view is scrolled back.
        public var cursorOnScreen: Bool
        /// Whether the cursor is on the left half of a two-column character,
        /// so the caret spans ``cursorCol`` and the column after it.
        public var cursorWide: Bool

        public init(
            widthPx: UInt32, heightPx: UInt32, scale: Float, cellWidthPx: UInt32, cellHeightPx: UInt32,
            baselinePx: Float, paddingPx: UInt32, rows: UInt16, cols: UInt16, fontSizePt: Float,
            cursorRow: UInt16, cursorCol: UInt16, cursorOnScreen: Bool, cursorWide: Bool
        ) {
            self.widthPx = widthPx
            self.heightPx = heightPx
            self.scale = scale
            self.cellWidthPx = cellWidthPx
            self.cellHeightPx = cellHeightPx
            self.baselinePx = baselinePx
            self.paddingPx = paddingPx
            self.rows = rows
            self.cols = cols
            self.fontSizePt = fontSizePt
            self.cursorRow = cursorRow
            self.cursorCol = cursorCol
            self.cursorOnScreen = cursorOnScreen
            self.cursorWide = cursorWide
        }

        init(_ m: TesseraSurfaceMetrics) {
            self.init(
                widthPx: m.width_px, heightPx: m.height_px, scale: m.scale, cellWidthPx: m.cell_width_px,
                cellHeightPx: m.cell_height_px, baselinePx: m.baseline_px, paddingPx: m.padding_px,
                rows: m.rows, cols: m.cols, fontSizePt: m.font_size_pt, cursorRow: m.cursor_row,
                cursorCol: m.cursor_col, cursorOnScreen: m.cursor_on_screen, cursorWide: m.cursor_wide)
        }
    }

    /// Reads the surface's geometry, font size and cursor cell from the core
    /// (`tessera_surface_metrics`). It locks the session's terminal briefly:
    /// read it once per frame or resize and keep it for the mouse and input
    /// method events in between.
    ///
    /// - Throws: ``TesseraCoreError/noSurface`` while detached,
    ///   ``TesseraCoreError/sessionNotFound`` once closed.
    public func metrics() throws -> Metrics {
        var metrics = TesseraSurfaceMetrics()
        try check(tessera_surface_metrics(session.core.pointer, session.id, &metrics))
        return Metrics(metrics)
    }

    /// A surface's cell geometry in physical pixels.
    public struct CellMetrics: Sendable, Equatable {
        /// Size of one cell.
        public var widthPx: UInt32
        public var heightPx: UInt32
        /// Space between the surface's edge and the grid, on every side.
        public var paddingPx: UInt32

        public init(widthPx: UInt32, heightPx: UInt32, paddingPx: UInt32) {
            self.widthPx = widthPx
            self.heightPx = heightPx
            self.paddingPx = paddingPx
        }
    }

    /// The attached surface's cell geometry from ``metrics()``, or `nil`
    /// while detached. Each read calls into the core.
    public var cellMetrics: CellMetrics? {
        (try? metrics()).map { CellMetrics(widthPx: $0.cellWidthPx, heightPx: $0.cellHeightPx, paddingPx: $0.paddingPx) }
    }

    /// The cursor's cell from ``metrics()``, or `nil` while detached or while
    /// the view is scrolled back so the cursor is not on the screen. Each
    /// read calls into the core.
    public var cursorCell: (row: UInt16, col: UInt16)? {
        guard let metrics = try? metrics(), metrics.cursorOnScreen else { return nil }
        return (metrics.cursorRow, metrics.cursorCol)
    }

    /// Creates a detached surface for `session`.
    public init(session: TesseraSession) {
        self.session = session
    }

    deinit {
        // `deinit` is nonisolated (isolated deinit needs Swift 6.2; CI runs
        // 6.1), so it may run off the main thread. Detach on the main thread,
        // keeping the layer alive until the core has let go of it.
        guard let detach = attachment else { return }
        if Thread.isMainThread {
            MainActor.assumeIsolated { detach.run() }
        } else {
            Task { @MainActor in detach.run() }
        }
    }

    /// Makes `layer` the session's surface, `widthPx` x `heightPx` physical
    /// pixels at backing `scale`, and resizes the session to the cells that
    /// fit. The core configures the layer (device, pixel format, drawable
    /// size, opacity). A zero size is allowed; nothing is drawn until a
    /// non-empty resize.
    ///
    /// - Throws: ``TesseraCoreError/invalidArgument`` if already attached
    ///   (detach first) or `scale` is not finite and positive,
    ///   ``TesseraCoreError/gpu`` if no renderer could be created (no GPU or
    ///   no usable font), ``TesseraCoreError/sessionNotFound`` once closed.
    public func attach(layer: CAMetalLayer, widthPx: UInt32, heightPx: UInt32, scale: Float) throws {
        let pointer = Unmanaged.passUnretained(layer).toOpaque()
        try TesseraCoreError.check(
            tessera_surface_attach(session.core.pointer, session.id, pointer, widthPx, heightPx, scale))
        attachment = Detacher(core: session.core, id: session.id, layer: layer, generation: Detacher.register(session))
        size = Size(widthPx: widthPx, heightPx: heightPx, scale: scale)
    }

    /// Tells the core the view is now `widthPx` x `heightPx` physical pixels
    /// at `scale`; the session is resized to the cells that fit. A zero size
    /// suspends drawing and keeps the session's size.
    ///
    /// - Throws: ``TesseraCoreError/noSurface`` while detached,
    ///   ``TesseraCoreError/invalidArgument`` for a bad scale.
    public func resize(widthPx: UInt32, heightPx: UInt32, scale: Float) throws {
        try check(tessera_surface_resize(session.core.pointer, session.id, widthPx, heightPx, scale))
        size = Size(widthPx: widthPx, heightPx: heightPx, scale: scale)
    }

    /// Draws the session's visible screen and presents it.
    ///
    /// - Throws: ``TesseraCoreError/noSurface`` while detached (also once the
    ///   core detached the surface after a lost swapchain or a lost GPU
    ///   device: ``isAttached`` is then `false` and the host attaches again,
    ///   on a new device after a device loss), ``TesseraCoreError/gpu`` if
    ///   drawing failed (the frame that found the swapchain or the device
    ///   lost reports this, and the next one ``TesseraCoreError/noSurface``).
    public func render() throws -> FrameStatus {
        var status = TESSERA_FRAME_STATUS_SKIPPED
        try check(tessera_surface_render(session.core.pointer, session.id, &status))
        return FrameStatus(status)
    }

    /// Draws text at `points` (clamped by the core to 4...512; a non-finite
    /// size becomes 13) and resizes the session to the cells that fit.
    ///
    /// - Throws: ``TesseraCoreError/noSurface`` while detached.
    public func setFontSize(_ points: Float) throws {
        try check(tessera_surface_set_font_size(session.core.pointer, session.id, points))
    }

    /// Detaches the surface; the core releases its reference to the layer,
    /// and so does this object. The session keeps running at its size.
    ///
    /// - Throws: ``TesseraCoreError/noSurface`` if not attached.
    public func detach() throws {
        try check(tessera_surface_detach(session.core.pointer, session.id))
        attachment?.forget()
        attachment = nil
    }

    /// Throws for a non-OK status. `NO_SURFACE` and `SESSION_NOT_FOUND`
    /// mean the core holds no surface for the session any more (detached
    /// after a lost swapchain, or the session or app closed, which detach
    /// first), so the layer is released too.
    private func check(_ status: TesseraError) throws(TesseraCoreError) {
        do {
            try TesseraCoreError.check(status)
        } catch {
            if error == .noSurface || error == .sessionNotFound || error == .nullPointer {
                attachment?.forget()
                attachment = nil
            }
            throw error
        }
    }
}

/// An attached layer and what a released surface needs to detach it on the
/// main thread. Off the main thread the layer is only kept alive, never used.
///
/// A detach deferred from a `deinit` off the main thread is keyed by session:
/// by the time it runs, the core may have dropped this attachment (a lost
/// swapchain) and another surface may have attached to the same session.
/// Each attach therefore records a generation per session, and a deferred
/// detach runs only while its generation is still the session's latest.
private struct Detacher: @unchecked Sendable {
    let core: CoreHandle
    let id: UInt64
    let layer: CAMetalLayer
    let generation: UInt64

    private struct Key: Hashable {
        let core: ObjectIdentifier
        let id: UInt64
    }

    @MainActor private static var latest: [Key: UInt64] = [:]
    @MainActor private static var nextGeneration: UInt64 = 0

    /// Records a new attachment for `session` and returns its generation.
    @MainActor
    static func register(_ session: TesseraSession) -> UInt64 {
        nextGeneration += 1
        latest[Key(core: ObjectIdentifier(session.core), id: session.id)] = nextGeneration
        return nextGeneration
    }

    /// Drops the record of this attachment: the core no longer has it.
    @MainActor
    func forget() {
        let key = Key(core: ObjectIdentifier(core), id: id)
        if Self.latest[key] == generation {
            Self.latest[key] = nil
        }
    }

    /// Detaches, unless a newer attachment for the session has replaced
    /// this one.
    @MainActor
    func run() {
        let key = Key(core: ObjectIdentifier(core), id: id)
        if Self.latest[key] == generation {
            Self.latest[key] = nil
            _ = tessera_surface_detach(core.pointer, id)
        }
        withExtendedLifetime(layer) {}
    }
}
