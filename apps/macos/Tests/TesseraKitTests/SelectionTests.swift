import Foundation
import TesseraCore
@testable import TesseraKit
import XCTest

/// The selection, Copy and link calls of `TesseraSession` round-tripped
/// through the C API (#30): a 4 x 40 session whose child prints three lines,
/// one of them a URL, and then copies its input to its output.
@MainActor
final class SelectionTests: XCTestCase {
    private var app: TesseraApp!

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    private func session(
        printing text: String = #"one two\r\nsee https://a.bc/x now\r\nlast"#, endsWith last: String? = "last"
    ) async throws -> TesseraSession {
        try await session(running: "printf '\(text)'", rows: 4, cols: 40, endsWith: last)
    }

    /// A session whose child runs `command` (a `/bin/sh` command line) and
    /// then copies its input to its output, returned once `last` ends its
    /// text.
    private func session(
        running command: String, rows: UInt16, cols: UInt16, endsWith last: String?
    ) async throws -> TesseraSession {
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "\(command); exec cat"], rows: rows, cols: cols))
        guard let last else { return session }
        let deadline = ContinuousClock.now + .seconds(20)
        while true {
            try session.selectAll()
            if try session.selectionText()?.hasSuffix(last) == true { break }
            guard ContinuousClock.now < deadline else {
                XCTFail("the child's text is not in the session")
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        try session.selectClear()
        return session
    }

    func testGranularitiesAreTheCValues() {
        XCTAssertEqual(TesseraSession.SelectionGranularity.character.rawValue, TESSERA_GRANULARITY_CHAR.rawValue)
        XCTAssertEqual(TesseraSession.SelectionGranularity.word.rawValue, TESSERA_GRANULARITY_WORD.rawValue)
        XCTAssertEqual(TesseraSession.SelectionGranularity.line.rawValue, TESSERA_GRANULARITY_LINE.rawValue)
    }

    func testAClickSelectsNothingAndADragSelectsCells() async throws {
        let session = try await session()
        XCTAssertFalse(try session.hasSelection)
        XCTAssertNil(try session.selectionText())
        try session.selectBegin(col: 4, row: 0, granularity: .character, block: false)
        XCTAssertFalse(try session.hasSelection, "a click is no selection")
        try session.selectExtend(col: 6, row: 0)
        XCTAssertTrue(try session.hasSelection)
        XCTAssertEqual(try session.selectionText(), "two")
        try session.selectClear()
        XCTAssertFalse(try session.hasSelection)
    }

    func testWordLineBlockAndAdjust() async throws {
        let session = try await session()
        try session.selectBegin(col: 5, row: 0, granularity: .word, block: false)
        XCTAssertEqual(try session.selectionText(), "two")
        try session.selectBegin(col: 5, row: 1, granularity: .line, block: false)
        XCTAssertEqual(try session.selectionText(), "see https://a.bc/x now")
        try session.selectBegin(col: 1, row: 0, granularity: .character, block: true)
        try session.selectExtend(col: 2, row: 2)
        XCTAssertEqual(try session.selectionText(), "ne\nee\nas", "a block is a rectangle")
        try session.selectBegin(col: 0, row: 0, granularity: .character, block: false)
        try session.selectExtend(col: 2, row: 0)
        try session.selectAdjust(col: 6, row: 0)
        XCTAssertEqual(try session.selectionText(), "one two", "the nearer end moved")
    }

    func testSelectAllHasTheTextWithoutTrailingBlanks() async throws {
        let session = try await session()
        try session.selectAll()
        XCTAssertEqual(try session.selectionText(), "one two\nsee https://a.bc/x now\nlast")
    }

    func testASelectionOfBlanksIsEmptyText() async throws {
        let session = try await session(printing: "", endsWith: nil)
        try session.selectAll()
        XCTAssertTrue(try session.hasSelection)
        XCTAssertEqual(try session.selectionText(), "", "selected, but nothing to copy")
    }

    func testTextLongerThanTheFirstBufferIsReadWhole() async throws {
        let long = String(repeating: "abcdefghij", count: 4)
        let session = try await session(printing: "\(long)\\r\\nlast")
        try session.selectAll()
        XCTAssertEqual(try session.selectionText(), "\(long)\nlast")
    }

    func testLinkAtFindsTheURLUnderACell() async throws {
        let session = try await session()
        XCTAssertEqual(try session.link(col: 10, row: 1), "https://a.bc/x")
        XCTAssertNil(try session.link(col: 1, row: 1), "the word before it")
        XCTAssertNil(try session.link(col: 1, row: 0))
    }

    func testHoverIsSetAndCleared() async throws {
        let session = try await session()
        try session.setHover((col: 10, row: 1))
        try session.setHover(nil)
    }

    func testADoubleClickSelectsTheSmartMatchElseTheWord() async throws {
        let session = try await session(printing: #"mail me@ex.com or 10.0.0.1:22\r\nlast"#)
        try session.selectBegin(col: 9, row: 0, granularity: .word, block: false)
        XCTAssertEqual(try session.selectionText(), "me@ex.com", "the email rule, not the word ex.com")
        try session.selectBegin(col: 20, row: 0, granularity: .word, block: false)
        XCTAssertEqual(try session.selectionText(), "10.0.0.1", "the IPv4 rule")
        try session.selectBegin(col: 1, row: 0, granularity: .word, block: false)
        XCTAssertEqual(try session.selectionText(), "mail", "no rule: the word")
    }

    func testSelectionWithFormattingHasTheEscapedRichTextAndThePlainText() async throws {
        let session = try await session(printing: #"\033[1;31mred\033[0m <b>&{}\r\nlast"#)
        XCTAssertNil(try session.selectionWithFormatting(), "no selection")
        try session.selectBegin(col: 0, row: 0, granularity: .line, block: false)
        let copy = try XCTUnwrap(try session.selectionWithFormatting())
        XCTAssertEqual(copy.plain, "red <b>&{}")
        XCTAssertEqual(copy.plain, try session.selectionText())
        let html = String(decoding: try XCTUnwrap(copy.html), as: UTF8.self)
        XCTAssertTrue(html.contains("font-weight:bold\">red</span>"), html)
        XCTAssertTrue(html.contains("&lt;b&gt;&amp;&#123;&#125;"), html)
        let rtf = String(decoding: try XCTUnwrap(copy.rtf), as: UTF8.self)
        XCTAssertTrue(rtf.hasPrefix(#"{\rtf1"#), rtf)
        XCTAssertTrue(rtf.contains(#"\{\}"#), rtf)
        // A selection of blanks only: nothing to copy, no formatting.
        try session.selectBegin(col: 0, row: 3, granularity: .line, block: false)
        XCTAssertEqual(try session.selectionWithFormatting(), .init(plain: "", html: nil, rtf: nil))
    }

    func testFormattedTextLongerThanTheFirstBuffersIsReadWhole() async throws {
        // 30 runs of two cells, each its own colour: more than the first
        // (empty) buffers, read on the second try.
        let colours = (0..<30).map { #"\033[3\#($0 % 8)mab"# }.joined()
        let session = try await session(printing: "\(colours)\\r\\nlast")
        try session.selectAll()
        let copy = try XCTUnwrap(try session.selectionWithFormatting())
        XCTAssertEqual(copy.plain, String(repeating: "ab", count: 30) + "\nlast")
        XCTAssertGreaterThan(try XCTUnwrap(copy.html).count, 30 * 20)
    }

    func testASelectionTooLargeToFormatIsThePlainTextAlone() async throws {
        // 1 100 lines of 250 cells, each bold, italic and underlined in
        // colours of its own: about 30 MB of HTML, past the 16 MiB cap, from
        // 276 kB of text. Too long for `printf`'s arguments, so from a file.
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera-too-large-\(UUID().uuidString).txt")
        try Self.tooLargeToFormat(lines: 1100, cols: 250).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        let session = try await session(running: "cat '\(file.path)'", rows: 50, cols: 250, endsWith: "last")
        try session.selectAll()
        let copy = try XCTUnwrap(try session.selectionWithFormatting())
        XCTAssertNil(copy.html, "refused by the core: no HTML")
        XCTAssertNil(copy.rtf, "refused by the core: no RTF")
        // Compared, not printed: a failure would print 276 kB.
        XCTAssertTrue(try copy.plain == session.selectionText(), "the plain text is the selection's")
        XCTAssertGreaterThan(copy.plain.utf8.count, 1100 * 250)
    }

    /// `lines` lines of `cols` letters, each cell bold, italic and
    /// underlined with a foreground and background unlike its neighbour's,
    /// then `last`.
    static func tooLargeToFormat(lines: Int, cols: Int) -> Data {
        var bytes = [UInt8]()
        bytes.reserveCapacity(lines * cols * 18)
        for line in 0..<lines {
            for col in 0..<cols {
                let colour = (line + col) % 8
                bytes += Array("\u{1B}[1;3;4;3\(colour);4\((colour + 1) % 8)m".utf8)
                bytes.append(UInt8(ascii: "a") + UInt8(col % 26))
            }
            bytes += Array("\u{1B}[0m\r\n".utf8)
        }
        bytes += Array("last".utf8)
        return Data(bytes)
    }

    func testCallsFailOnceTheSessionIsClosed() async throws {
        let session = try await session()
        try session.close()
        XCTAssertThrowsError(try session.selectAll()) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
        XCTAssertThrowsError(try session.selectionText())
        XCTAssertThrowsError(try session.selectionWithFormatting())
        XCTAssertThrowsError(try session.link(col: 0, row: 0))
        XCTAssertThrowsError(try session.hasSelection)
    }
}
