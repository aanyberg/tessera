import Foundation
import Metal
import QuartzCore
import TesseraCore
@testable import TesseraKit
import XCTest

/// A session whose child copies its raw input to a file, so a test reads the
/// exact bytes the core wrote to the PTY. The terminal is in raw mode with
/// echo off: no CR/NL translation, no line editing.
@MainActor
struct PTYRecorder {
    let session: TesseraSession
    let file: URL

    /// Spawns the recorder and waits until its terminal is in raw mode.
    static func start(in app: TesseraApp, recorder: Recorder) async throws -> PTYRecorder {
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera-pty-\(UUID().uuidString).bin")
        let session = try app.spawn(Self.spawnOptions(writingTo: file))
        let ready = recorder.expect(session, "raw mode is set") { $0 == .output }
        let result = await XCTWaiter().fulfillment(of: [ready], timeout: 5)
        XCTAssertEqual(result, .completed, "the recorder never set raw mode")
        return PTYRecorder(session: session, file: file)
    }

    /// Options that run the recorder writing to `file`; it prints `R` once
    /// raw mode is on.
    static func spawnOptions(writingTo file: URL) -> TesseraSession.SpawnOptions {
        .init(
            program: "/bin/sh",
            arguments: ["-c", #"stty raw -echo; printf R; exec cat > "$0""#, file.path])
    }

    /// The bytes received so far.
    func bytes() -> [UInt8] {
        (try? Array(Data(contentsOf: file))) ?? []
    }

    /// Waits until `count` bytes have arrived (or a timeout) and returns
    /// everything received.
    func waitForBytes(count: Int, timeout: Duration = .seconds(5)) async throws -> [UInt8] {
        let deadline = ContinuousClock.now + timeout
        var received = bytes()
        while received.count < count, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
            received = bytes()
        }
        return received
    }

    /// Sends a sentinel key and returns everything received up to and
    /// including it: what a test sent before is then known to be complete,
    /// because the core writes a session's input in order.
    func bytesThroughSentinel() async throws -> [UInt8] {
        try session.send(key: .init(key: .char, codepoint: "~", text: "~"))
        let deadline = ContinuousClock.now + .seconds(5)
        var received = bytes()
        while received.last != UInt8(ascii: "~"), ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
            received = bytes()
        }
        XCTAssertEqual(received.last, UInt8(ascii: "~"), "sentinel never arrived; got \(hex(received))")
        return Array(received.dropLast())
    }

    func remove() {
        try? FileManager.default.removeItem(at: file)
    }
}

func hex(_ bytes: [UInt8]) -> String {
    bytes.map { String(format: "%02x", $0) }.joined(separator: " ")
}

/// The TesseraKit wrappers for the input calls (ADR 0011) and the surface
/// calls (ADR 0014), each round-tripped through the C API.
@MainActor
final class SurfaceAndInputTests: XCTestCase {
    private var app: TesseraApp!
    private var recorder: Recorder!
    private var pty: PTYRecorder?

    override func setUp() async throws {
        app = try TesseraApp()
        recorder = Recorder()
        app.delegate = recorder
    }

    override func tearDown() async throws {
        app?.shutdown()
        pty?.remove()
        app = nil
        recorder = nil
    }

    private func startRecorder() async throws -> PTYRecorder {
        let pty = try await PTYRecorder.start(in: app, recorder: recorder)
        self.pty = pty
        return pty
    }

    // MARK: Keys

    func testKeysReachThePTYEncodedByTheCore() async throws {
        let pty = try await startRecorder()
        let session = pty.session
        let cases: [(TesseraSession.KeyEvent, [UInt8])] = [
            (.init(key: .char, codepoint: "a", text: "a"), [0x61]),
            (.init(key: .char, codepoint: "a", shifted: "A", modifiers: .shift, text: "A"), [0x41]),
            (.init(key: .char, codepoint: "a", modifiers: .alt), [0x1b, 0x61]),
            (.init(key: .char, codepoint: "c", modifiers: .control), [0x03]),
            (.text("å"), [0xc3, 0xa5]),
            (.text("にほんご"), [0xe3, 0x81, 0xab, 0xe3, 0x81, 0xbb, 0xe3, 0x82, 0x93, 0xe3, 0x81, 0x94]),
            (.init(key: .enter), [0x0d]),
            (.init(key: .escape), [0x1b]),
            (.init(key: .backspace), [0x7f]),
            (.init(key: .tab, modifiers: .shift), Array("\u{1b}[Z".utf8)),
            (.init(key: .up), Array("\u{1b}[A".utf8)),
            (.init(key: .delete), Array("\u{1b}[3~".utf8)),
            (.init(key: .f1), Array("\u{1b}OP".utf8)),
        ]
        var expected: [UInt8] = []
        for (event, bytes) in cases {
            XCTAssertEqual(try session.send(key: event), .consumed, "\(event)")
            expected += bytes
        }
        let received = try await pty.bytesThroughSentinel()
        XCTAssertEqual(hex(received), hex(expected))
    }

    func testKeysThatSendNothingAreIgnored() async throws {
        let pty = try await startRecorder()
        let session = pty.session
        XCTAssertEqual(try session.send(key: .init(key: .leftShift, modifiers: .shift)), .ignored)
        XCTAssertEqual(try session.send(key: .init(key: .char, codepoint: "a", kind: .release)), .ignored)
        // A text-only event whose text is a control character sends nothing.
        XCTAssertEqual(try session.send(key: .text("\u{1b}[31m")), .ignored)
        let received = try await pty.bytesThroughSentinel()
        XCTAssertEqual(received, [])
    }

    func testInputToAClosedSessionThrowsSessionNotFound() async throws {
        let pty = try await startRecorder()
        try pty.session.close()
        XCTAssertThrowsError(try pty.session.send(key: .init(key: .enter))) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try pty.session.paste("x")) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
        XCTAssertThrowsError(try pty.session.setFocused(true)) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try pty.session.send(mouse: .init(kind: .press, button: .left, col: 0, row: 0, xPx: 0, yPx: 0)))
        {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try pty.session.send(scroll: .init(deltaLines: 1))) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
    }

    func testEveryKeyMirrorsTheCEnum() throws {
        XCTAssertEqual(TesseraSession.Key.allCases.count, 113)
        XCTAssertEqual(TesseraSession.Key.allCases.map(\.rawValue), Array(0..<113))
        let pairs: [(TesseraSession.Key, TesseraKey)] = [
            (.unidentified, TESSERA_KEY_UNIDENTIFIED), (.char, TESSERA_KEY_CHAR), (.escape, TESSERA_KEY_ESCAPE),
            (.enter, TESSERA_KEY_ENTER), (.pageDown, TESSERA_KEY_PAGE_DOWN), (.menu, TESSERA_KEY_MENU),
            (.f1, TESSERA_KEY_F1), (.f35, TESSERA_KEY_F35), (.kp0, TESSERA_KEY_KP0),
            (.kpBegin, TESSERA_KEY_KP_BEGIN), (.mediaPlay, TESSERA_KEY_MEDIA_PLAY),
            (.muteVolume, TESSERA_KEY_MUTE_VOLUME), (.leftShift, TESSERA_KEY_LEFT_SHIFT),
            (.rightMeta, TESSERA_KEY_RIGHT_META), (.isoLevel5Shift, TESSERA_KEY_ISO_LEVEL5_SHIFT),
        ]
        for (key, cKey) in pairs {
            XCTAssertEqual(key.rawValue, cKey.rawValue, "\(key)")
        }
        XCTAssertEqual(TesseraSession.KeyEventKind.release.rawValue, TESSERA_KEY_EVENT_KIND_RELEASE.rawValue)
        XCTAssertEqual(TesseraSession.Modifiers.super.rawValue, UInt8(TESSERA_MODIFIERS_SUPER.rawValue))
        XCTAssertEqual(TesseraSession.Modifiers.numLock.rawValue, UInt8(TESSERA_MODIFIERS_NUM_LOCK.rawValue))
        XCTAssertEqual(TesseraSession.MouseButton.forward.rawValue, TESSERA_MOUSE_BUTTON_FORWARD.rawValue)
        XCTAssertEqual(TesseraSession.MouseEventKind.motion.rawValue, TESSERA_MOUSE_EVENT_KIND_MOTION.rawValue)
    }

    func testEveryKeyIsAcceptedByTheCore() async throws {
        let pty = try await startRecorder()
        for key in TesseraSession.Key.allCases {
            let event = TesseraSession.KeyEvent(key: key, codepoint: key == .char ? "x" : nil, kind: .release)
            XCTAssertNoThrow(try pty.session.send(key: event), "\(key)")
        }
    }

    func testCEventCarriesEveryField() {
        let event = TesseraSession.KeyEvent(
            key: .char, codepoint: "с", shifted: "С", baseLayout: "c", modifiers: [.control, .alt],
            kind: .repeat, text: nil)
        let cEvent = event.cEvent(text: nil, length: 0)
        XCTAssertEqual(cEvent.key, TESSERA_KEY_CHAR.rawValue)
        XCTAssertEqual(cEvent.codepoint, 0x441)
        XCTAssertEqual(cEvent.shifted, 0x421)
        XCTAssertEqual(cEvent.base_layout, 0x63)
        XCTAssertEqual(cEvent.modifiers, 6)
        XCTAssertEqual(cEvent.kind, TESSERA_KEY_EVENT_KIND_REPEAT.rawValue)
        XCTAssertNil(cEvent.text)
        XCTAssertEqual(cEvent.text_len, 0)
        // A codepoint on a key that is not `char` is not sent.
        XCTAssertEqual(TesseraSession.KeyEvent(key: .enter, codepoint: "x").cEvent(text: nil, length: 0).codepoint, 0)
    }

    func testEveryInputOutcomeMaps() {
        XCTAssertEqual(TesseraSession.InputOutcome(TESSERA_INPUT_OUTCOME_IGNORED), .ignored)
        XCTAssertEqual(TesseraSession.InputOutcome(TESSERA_INPUT_OUTCOME_CONSUMED), .consumed)
        // A value from a newer header reads as ignored, like after a failure.
        XCTAssertEqual(TesseraSession.InputOutcome(TesseraInputOutcome(rawValue: 7)), .ignored)
    }

    // MARK: Paste, focus, mouse, scroll

    func testPasteIsSanitisedByTheCore() async throws {
        let pty = try await startRecorder()
        try pty.session.paste("a\nb\u{1b}[31mc\r\nd")
        try pty.session.paste("")
        let received = try await pty.bytesThroughSentinel()
        XCTAssertEqual(hex(received), hex(Array("a\rb[31mc\rd".utf8)))
    }

    func testFocusIsAcceptedAndSendsNothing() async throws {
        let pty = try await startRecorder()
        try pty.session.setFocused(true)
        try pty.session.setFocused(false)
        let received = try await pty.bytesThroughSentinel()
        XCTAssertEqual(received, [])
    }

    func testMouseIsRecordedAndIgnored() async throws {
        let pty = try await startRecorder()
        let session = pty.session
        for kind in [TesseraSession.MouseEventKind.press, .motion, .release] {
            let event = TesseraSession.MouseEvent(
                kind: kind, button: .left, col: 500, row: 500, xPx: -3, yPx: 12.5, modifiers: .shift)
            XCTAssertEqual(try session.send(mouse: event), .ignored)
        }
        XCTAssertThrowsError(
            try session.send(mouse: .init(kind: .press, button: .left, col: 0, row: 0, xPx: .nan, yPx: 0))
        ) { XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument) }
        let received = try await pty.bytesThroughSentinel()
        XCTAssertEqual(received, [], "mouse reporting is not on yet")
    }

    func testScrollMovesTheViewOnlyWhenThereIsScrollback() async throws {
        let session = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", "seq 1 200; exec sleep 30"]))
        // No scrollback yet, or not enough: wait for it.
        let deadline = ContinuousClock.now + .seconds(5)
        var outcome = TesseraSession.InputOutcome.ignored
        while outcome == .ignored, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
            outcome = try session.send(scroll: .init(deltaLines: 3))
        }
        XCTAssertEqual(outcome, .consumed)
        // Back to the live screen, then past it: nothing moves.
        XCTAssertEqual(try session.send(scroll: .init(deltaLines: -1000)), .consumed)
        XCTAssertEqual(try session.send(scroll: .init(deltaLines: -1)), .ignored)
        XCTAssertEqual(try session.send(scroll: .init(deltaLines: 0.25, precise: true)), .ignored, "fraction accumulates")
        XCTAssertThrowsError(try session.send(scroll: .init(deltaLines: .infinity))) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
    }

    // MARK: Surface

    private func metalLayer() throws -> CAMetalLayer {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device on this machine: the surface tests need a GPU")
        }
        return CAMetalLayer()
    }

    private func attached(_ session: TesseraSession, _ layer: CAMetalLayer) throws -> TesseraSurface {
        let surface = TesseraSurface(session: session)
        do {
            try surface.attach(layer: layer, widthPx: 800, heightPx: 600, scale: 2)
        } catch TesseraCoreError.gpu {
            throw XCTSkip("the core could not create a renderer (no GPU can present, or no font)")
        }
        return surface
    }

    func testSurfaceLifecycle() throws {
        let layer = try metalLayer()
        let session = try app.spawn(.init(program: "/bin/cat"))
        let surface = try attached(session, layer)

        XCTAssertTrue(surface.isAttached)
        XCTAssertTrue(surface.layer === layer)
        XCTAssertEqual(surface.size, .init(widthPx: 800, heightPx: 600, scale: 2))
        XCTAssertNotNil(layer.device, "the core configured the layer")

        try surface.resize(widthPx: 1000, heightPx: 700, scale: 2)
        XCTAssertEqual(surface.size, .init(widthPx: 1000, heightPx: 700, scale: 2))
        XCTAssertThrowsError(try surface.resize(widthPx: 10, heightPx: 10, scale: 0)) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
        XCTAssertEqual(surface.size?.widthPx, 1000, "a rejected size is not recorded")
        // Zero suspends drawing.
        try surface.resize(widthPx: 0, heightPx: 0, scale: 1)
        XCTAssertEqual(try surface.render(), .skipped)
        try surface.resize(widthPx: 1000, heightPx: 700, scale: 2)
        XCTAssertNoThrow(try surface.render())
        try surface.setFontSize(18)
        try surface.setFontSize(.nan)

        // A second attach for the same session is refused before the layer
        // is touched.
        XCTAssertThrowsError(try surface.attach(layer: CAMetalLayer(), widthPx: 1, heightPx: 1, scale: 1)) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
        XCTAssertTrue(surface.layer === layer)

        try surface.detach()
        XCTAssertFalse(surface.isAttached)
        XCTAssertNil(surface.layer)
        XCTAssertThrowsError(try surface.detach()) { XCTAssertEqual($0 as? TesseraCoreError, .noSurface) }
        XCTAssertThrowsError(try surface.render()) { XCTAssertEqual($0 as? TesseraCoreError, .noSurface) }
        XCTAssertThrowsError(try surface.setFontSize(12)) { XCTAssertEqual($0 as? TesseraCoreError, .noSurface) }

        // Attaching again works.
        try surface.attach(layer: layer, widthPx: 400, heightPx: 300, scale: 1)
        XCTAssertTrue(surface.isAttached)
    }

    func testReleasingAnAttachedSurfaceDetachesItAndReleasesTheLayer() throws {
        _ = try metalLayer()
        let session = try app.spawn(.init(program: "/bin/cat"))
        weak var weakLayer: CAMetalLayer?
        // Core Animation's implicit transaction and autorelease pools hold
        // layers they saw until they end: end both before checking who still
        // holds the layer.
        try autoreleasepool {
            let layer = CAMetalLayer()
            weakLayer = layer
            var surface: TesseraSurface? = try attached(session, layer)
            XCTAssertNotNil(surface)
            surface = nil
            CATransaction.flush()
        }
        XCTAssertNil(weakLayer, "neither the wrapper nor the core keeps the layer after the detach")
        // Detached: a new surface may attach.
        let again = try attached(session, CAMetalLayer())
        XCTAssertTrue(again.isAttached)
    }

    func testTheSurfaceKeepsTheLayerAliveWhileAttached() throws {
        _ = try metalLayer()
        let session = try app.spawn(.init(program: "/bin/cat"))
        weak var weakLayer: CAMetalLayer?
        let surface = try autoreleasepool {
            let layer = CAMetalLayer()
            weakLayer = layer
            defer { CATransaction.flush() }
            return try attached(session, layer)
        }
        XCTAssertNotNil(weakLayer, "the attached layer outlives the caller's reference")
        try autoreleasepool {
            try surface.detach()
            CATransaction.flush()
        }
        XCTAssertNil(weakLayer)
    }

    func testClosingTheSessionDetachesTheSurface() throws {
        let layer = try metalLayer()
        let session = try app.spawn(.init(program: "/bin/cat"))
        let surface = try attached(session, layer)
        try session.close()
        XCTAssertThrowsError(try surface.render()) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
        XCTAssertFalse(surface.isAttached, "the core let go of the layer, and so does the wrapper")
    }

    func testMetricsReportTheGridAndTheCursorAfterOutput() async throws {
        let layer = try metalLayer()
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", #"printf 'ab\r\ncd'; exec sleep 30"#]))
        let surface = try attached(session, layer)

        // The cursor ends after "cd" on the second row.
        let deadline = ContinuousClock.now + .seconds(5)
        while surface.cursorCell.map({ $0 != (1, 2) }) ?? true, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        let cursor = try XCTUnwrap(surface.cursorCell)
        XCTAssertEqual(cursor.row, 1)
        XCTAssertEqual(cursor.col, 2)

        let metrics = try surface.metrics()
        XCTAssertEqual(metrics.widthPx, 800)
        XCTAssertEqual(metrics.heightPx, 600)
        XCTAssertEqual(metrics.scale, 2)
        XCTAssertEqual(metrics.paddingPx, 8, "the default theme's 4 pt at 2x")
        XCTAssertGreaterThan(metrics.cellWidthPx, 0)
        XCTAssertGreaterThan(metrics.fontSizePt, 0)
        XCTAssertTrue(metrics.cursorOnScreen)
        XCTAssertFalse(metrics.cursorWide)
        XCTAssertEqual(
            surface.cellMetrics,
            .init(widthPx: metrics.cellWidthPx, heightPx: metrics.cellHeightPx, paddingPx: metrics.paddingPx))
        // The grid fits inside the padding, whole cells only.
        let padding: UInt32 = 2 * metrics.paddingPx
        let used: UInt32 = UInt32(metrics.cols) * metrics.cellWidthPx + padding
        let oneMore: UInt32 = used + metrics.cellWidthPx
        XCTAssertLessThanOrEqual(used, 800)
        XCTAssertGreaterThan(oneMore, 800)

        // `rows` and `cols` are the session's grid.
        try session.resize(rows: 20, cols: 70)
        let resized = try surface.metrics()
        XCTAssertEqual(resized.rows, 20)
        XCTAssertEqual(resized.cols, 70)
        try surface.setFontSize(26)
        let zoomed = try surface.metrics()
        XCTAssertEqual(zoomed.fontSizePt, 26)
        XCTAssertLessThan(zoomed.cols, metrics.cols, "the surface resized the grid again")

        try surface.detach()
        XCTAssertNil(surface.cellMetrics)
        XCTAssertNil(surface.cursorCell)
        XCTAssertThrowsError(try surface.metrics()) { XCTAssertEqual($0 as? TesseraCoreError, .noSurface) }
        try session.close()
        XCTAssertThrowsError(try surface.metrics()) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
    }

    func testMetricsMirrorEveryField() {
        let c = TesseraSurfaceMetrics(
            width_px: 1, height_px: 2, scale: 3, cell_width_px: 4, cell_height_px: 5, baseline_px: 6, padding_px: 7,
            rows: 8, cols: 9, font_size_pt: 10, cursor_row: 11, cursor_col: 12, cursor_on_screen: true,
            cursor_wide: true)
        XCTAssertEqual(
            TesseraSurface.Metrics(c),
            .init(
                widthPx: 1, heightPx: 2, scale: 3, cellWidthPx: 4, cellHeightPx: 5, baselinePx: 6, paddingPx: 7,
                rows: 8, cols: 9, fontSizePt: 10, cursorRow: 11, cursorCol: 12, cursorOnScreen: true,
                cursorWide: true))
        // The layout the Rust side pins (`surface_metrics_layout_is_stable`).
        XCTAssertEqual(MemoryLayout<TesseraSurfaceMetrics>.size, 44)
        XCTAssertEqual(MemoryLayout<TesseraSurfaceMetrics>.alignment, 4)
        XCTAssertEqual(MemoryLayout<TesseraSurfaceMetrics>.offset(of: \.font_size_pt), 32)
        XCTAssertEqual(MemoryLayout<TesseraSurfaceMetrics>.offset(of: \.cursor_wide), 41)
    }

    func testFrameStatusMapsEveryValue() {
        XCTAssertEqual(TesseraSurface.FrameStatus(TESSERA_FRAME_STATUS_PRESENTED), .presented)
        XCTAssertEqual(TesseraSurface.FrameStatus(TESSERA_FRAME_STATUS_SUBOPTIMAL), .suboptimal)
        XCTAssertEqual(TesseraSurface.FrameStatus(TESSERA_FRAME_STATUS_SKIPPED), .skipped)
        XCTAssertEqual(TesseraSurface.FrameStatus(TesseraFrameStatus(rawValue: 9)), .skipped)
    }
}
