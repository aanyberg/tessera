import Foundation
import TesseraCore
import XCTest

@testable import TesseraKit

/// `TesseraSession.accessibleText()` and its lookups round-tripped through
/// the C API (#52, ADR 0041), on sessions whose child prints a fixture grid
/// (accessibility fixture rows B and C) and then copies its input; and the
/// display options of ADR 0043 with Increase Contrast, on a core of the
/// test's own whose `config.toml` (never the user's) is translucent,
/// blurred and dims inactive panes.
@MainActor
final class AccessibleTextTests: XCTestCase {
    private var app: TesseraApp!
    private var dir: URL?
    private var previous: String?

    override func setUp() async throws {
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
    }

    private func start(config: String? = nil) throws {
        if let config {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-a11y-\(UUID().uuidString)")
            XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute configuration directory")
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
            setenv("TESSERA_CONFIG_DIR", dir.path, 1)
            self.dir = dir
        }
        app = try TesseraApp()
    }

    /// A 6 x 20 session whose child printed `text` (printf syntax) with
    /// echo off; returns once the view's text is `expected`.
    private func session(printing text: String, reads expected: String) async throws -> TesseraSession {
        if app == nil { try start() }
        let session = try app.spawn(
            .init(
                program: "/bin/sh", arguments: ["-c", "stty -echo -icanon; printf '\(text)'; exec cat"], rows: 6, cols: 20))
        try await waitFor(session, reads: expected)
        return session
    }

    @discardableResult
    private func waitFor(_ session: TesseraSession, reads expected: String) async throws -> TesseraSession.AccessibleText {
        let deadline = ContinuousClock.now + .seconds(5)
        while true {
            let text = try session.accessibleText()
            if text.string == expected { return text }
            guard ContinuousClock.now < deadline else {
                XCTFail("the view reads \(text.string.debugDescription), not \(expected.debugDescription)")
                return text
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    // MARK: Rows B

    func testB1TheValueKeepsTheCursorRowUpToTheCursor() async throws {
        let session = try await session(printing: #"ls\r\na.txt  b.txt\r\n$ "#, reads: "ls\na.txt  b.txt\n$ ")
        let text = try session.accessibleText()
        XCTAssertEqual(text.length, 18)
        XCTAssertEqual((text.string as NSString).length, text.length, "A1: UTF-16 units")
        XCTAssertEqual(text.lineCount, 3)
        XCTAssertTrue(text.hasCursor)
        XCTAssertEqual(text.insertionPoint, 18)
        XCTAssertEqual(text.selectedRange, NSRange(location: 18, length: 0), "A10: empty at the cursor")
        XCTAssertFalse(text.hasSelection)
        XCTAssertEqual(text.line(forIndex: text.insertionPoint), 2, "A13")
    }

    func testB5ToB7LinesTileTheText() async throws {
        let session = try await session(printing: #"ab\r\ncd\r\n\r\nef"#, reads: "ab\ncd\n\nef")
        let text = try session.accessibleText()
        XCTAssertEqual(text.length, 9)
        let lines = (0..<5).map { text.range(forLine: $0) }
        XCTAssertEqual(
            lines,
            [NSRange(location: 0, length: 3), NSRange(location: 3, length: 3), NSRange(location: 6, length: 1),
                NSRange(location: 7, length: 2), nil])
        XCTAssertNil(text.range(forLine: -1))
        let byIndex = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 100, -3].map { text.line(forIndex: $0) }
        XCTAssertEqual(byIndex, [0, 0, 0, 1, 1, 1, 2, 3, 3, 3, 3, 0])
    }

    func testB9ToB14WideAndCombiningCharactersAreWholeClusters() async throws {
        // 日本語x / 😀a / e + U+0301 x: one character per cluster, a wide one
        // two cells.
        let session = try await session(
            printing: #"\346\227\245\346\234\254\350\252\236x\r\n\360\237\230\200a\r\ne\314\201x"#,
            reads: "日本語x\n😀a\ne\u{301}x")
        let text = try session.accessibleText()
        XCTAssertEqual(text.length, 5 + 4 + 3)
        XCTAssertEqual(text.characterRange(at: 1), NSRange(location: 1, length: 1), "本")
        XCTAssertEqual(text.characterRange(at: 6), NSRange(location: 5, length: 2), "B12: the low surrogate")
        XCTAssertEqual(text.characterRange(at: 10), NSRange(location: 9, length: 2), "B13: the combining mark")
        XCTAssertEqual(text.characterRange(at: 4), NSRange(location: 4, length: 1), "the newline alone")
        XCTAssertEqual(text.characterRange(at: 99), NSRange(location: 12, length: 0), "past the end")
        XCTAssertEqual(text.range(atCol: 3, row: 0), NSRange(location: 1, length: 1), "B11: the spacer half of 本")
        XCTAssertEqual(text.range(atCol: 6, row: 0), NSRange(location: 3, length: 1), "x")
        XCTAssertEqual(text.range(atCol: 2, row: 1), NSRange(location: 7, length: 1), "a after the emoji")
        XCTAssertEqual(text.range(atCol: 15, row: 1), NSRange(location: 7, length: 1), "right of the text: its last")
        XCTAssertEqual(text.range(atCol: 0, row: 5), NSRange(location: 12, length: 0), "below the text: the end")
        // C3 to C5 in cells.
        XCTAssertEqual(text.cells(for: NSRange(location: 1, length: 1)), .init(row: 0, col: 2, rows: 1, cols: 2))
        XCTAssertEqual(text.cells(for: NSRange(location: 3, length: 1)), .init(row: 0, col: 6, rows: 1, cols: 1))
        XCTAssertEqual(text.cells(for: NSRange(location: 7, length: 1)), .init(row: 1, col: 2, rows: 1, cols: 1))
    }

    func testC1C2C6CellsOfRanges() async throws {
        let session = try await session(printing: #"ls\r\na.txt  b.txt\r\n$ "#, reads: "ls\na.txt  b.txt\n$ ")
        let text = try session.accessibleText()
        XCTAssertEqual(text.cells(for: NSRange(location: 3, length: 5)), .init(row: 1, col: 0, rows: 1, cols: 5))
        XCTAssertEqual(text.cells(for: NSRange(location: 0, length: 5)), .init(row: 0, col: 0, rows: 2, cols: 2))
        XCTAssertEqual(text.cells(for: NSRange(location: 18, length: 0)), .init(row: 2, col: 2, rows: 1, cols: 0))
        XCTAssertEqual(text.cells(for: NSRange(location: 0, length: 0)), .init(row: 0, col: 0, rows: 1, cols: 0))
    }

    func testB19TheSelectionIsTheSelectedRange() async throws {
        let session = try await session(printing: #"ls\r\na.txt  b.txt\r\n$ "#, reads: "ls\na.txt  b.txt\n$ ")
        try session.selectBegin(col: 0, row: 1, granularity: .character, block: false)
        try session.selectExtend(col: 4, row: 1)
        let text = try session.accessibleText()
        XCTAssertTrue(text.hasSelection)
        XCTAssertEqual(text.selectedRange, NSRange(location: 3, length: 5))
        XCTAssertEqual(text.insertionPoint, 18, "the insertion point stays the cursor")
    }

    func testB18ControlAndBidiCharactersNeverReachTheText() async throws {
        // U+202E (right-to-left override) and U+2028 (line separator) are
        // printed into cells; neither is in the text.
        let session = try await session(printing: #"a\342\200\256b\342\200\250c"#, reads: "ab c")
        let text = try session.accessibleText()
        XCTAssertFalse(text.string.unicodeScalars.contains { $0.value == 0x202E || $0.value == 0x2028 })
    }

    // MARK: Announcements

    func testNewOutputIsAnnouncedAndTypingIsNot() async throws {
        let session = try await session(printing: #"$ "#, reads: "$ ")
        let prompt = try session.accessibleText()
        XCTAssertEqual(prompt.announcement(since: prompt), "", "nothing changed")
        try session.write(Data("one\ntwo\n".utf8))
        let output = try await waitFor(session, reads: "$ one\ntwo\n")
        XCTAssertEqual(output.announcement(since: prompt), "$ one\ntwo", "the old cursor line changed, so it is read")
        try session.write(Data("x".utf8))
        let typed = try await waitFor(session, reads: "$ one\ntwo\nx")
        XCTAssertEqual(typed.announcement(since: output), "", "the cursor stayed on its line")
    }

    func testAnAnnouncementOverTheCapKeepsTheNewestText() async throws {
        try start()
        let session = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", "stty -echo -icanon; exec cat"], rows: 60, cols: 40))
        let empty = try await waitFor(session, reads: "")
        // 59 lines of 39 characters: about 2 300 characters, more than the cap.
        let lines = (0..<59).map { String(format: "%02d", $0) + String(repeating: "y", count: 37) }
        try session.write(Data((lines.joined(separator: "\n") + "\n").utf8))
        let full = try await waitFor(session, reads: lines.joined(separator: "\n") + "\n")
        let announcement = full.announcement(since: empty)
        XCTAssertEqual(announcement.count, 1024, "TESSERA_MAX_ANNOUNCEMENT_CHARS")
        XCTAssertEqual(Int(TESSERA_MAX_ANNOUNCEMENT_CHARS), 1024)
        XCTAssertTrue(announcement.hasSuffix(lines[58]), "the newest line is kept")
        XCTAssertFalse(announcement.contains(lines[0]), "the oldest is cut")
    }

    func testASnapshotOutlivesItsSession() async throws {
        let session = try await session(printing: #"kept"#, reads: "kept")
        let text = try session.accessibleText()
        try session.close()
        XCTAssertEqual(text.string, "kept")
        XCTAssertEqual(text.range(forLine: 0), NSRange(location: 0, length: 4))
        XCTAssertThrowsError(try session.accessibleText()) { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
    }

    // MARK: Display options (ADR 0043, ADR 0041)

    func testTheDisplayOptionsAreOneBytePerFlag() {
        XCTAssertEqual(MemoryLayout<TesseraDisplayOptions>.size, 2, "the layout the core reads (F7)")
        XCTAssertEqual(MemoryLayout<TesseraDisplayOptions>.offset(of: \.reduce_transparency), 0)
        XCTAssertEqual(MemoryLayout<TesseraDisplayOptions>.offset(of: \.increase_contrast), 1)
        XCTAssertEqual(TesseraApp.DisplayOptions(), .init(reduceTransparency: false, increaseContrast: false))
    }

    private static let translucent = """
        [appearance]
        opacity = 0.5
        blur = true
        dim_inactive_panes = 0.4
        """

    func testAppearanceFollowsReduceTransparencyAndIncreaseContrastStopsDimming() throws {
        try start(config: Self.translucent)
        let session = try app.spawn(.init(program: "/bin/cat"))
        XCTAssertEqual(try session.appearance(), .init(opaque: false, blur: true), "the configuration's look")
        XCTAssertEqual(try app.dimInactivePanes(), 0.4, accuracy: 1e-6)

        try app.setDisplayOptions(.init(reduceTransparency: true))
        XCTAssertEqual(try session.appearance(), .init(opaque: true, blur: false), "reduced")
        XCTAssertEqual(try app.dimInactivePanes(), 0.4, accuracy: 1e-6, "Reduce Transparency keeps the dimming")

        try app.setDisplayOptions(.init(increaseContrast: true))
        XCTAssertEqual(try session.appearance(), .init(opaque: false, blur: true), "contrast keeps the translucency")
        XCTAssertEqual(try app.dimInactivePanes(), 0, "Increase Contrast turns the dimming off")

        try app.setDisplayOptions(.init(reduceTransparency: true, increaseContrast: true))
        XCTAssertEqual(try session.appearance(), .init(opaque: true, blur: false))
        XCTAssertEqual(try app.dimInactivePanes(), 0)

        try app.setDisplayOptions(.init())
        XCTAssertEqual(try session.appearance(), .init(opaque: false, blur: true), "both off again")
        XCTAssertEqual(try app.dimInactivePanes(), 0.4, accuracy: 1e-6)
        try session.close()
        XCTAssertThrowsError(try session.appearance()) { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
        app.shutdown()
        XCTAssertThrowsError(try app.setDisplayOptions(.init())) { error in
            XCTAssertEqual(error as? TesseraCoreError, .nullPointer)
        }
    }
}
