import AppKit
import TesseraKit
import XCTest

@testable import Tessera
@testable import TesseraKit

/// Copy with Formatting (#47, ADR 0042) through the real
/// `TerminalSurfaceView`: one pasteboard write of RTF, HTML and the plain
/// string, to a named pasteboard (never the user's), the menu item gated
/// like Copy, and a selection too large to format copied as plain text.
@MainActor
final class CopyWithFormattingTests: XCTestCase {
    private var app: TesseraApp!
    private var pasteboard: NSPasteboard!

    override func setUp() async throws {
        app = try TesseraApp()
        pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        pasteboard.clearContents()
        pasteboard.setString("sentinel", forType: .string)
    }

    override func tearDown() async throws {
        pasteboard?.releaseGlobally()
        pasteboard = nil
        app?.shutdown()
        app = nil
    }

    /// A view on a session printing `text` (a `printf` format), returned
    /// once `last` ends its text.
    private func view(printing text: String, endsWith last: String) async throws -> TerminalSurfaceView {
        try await view(running: "printf '\(text)'", rows: 4, cols: 40, endsWith: last)
    }

    /// A view on a session whose child runs `command` (a `/bin/sh` command
    /// line), returned once `last` ends its text.
    private func view(
        running command: String, rows: UInt16, cols: UInt16, endsWith last: String
    ) async throws -> TerminalSurfaceView {
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "\(command); exec cat"], rows: rows, cols: cols))
        let view = TerminalSurfaceView(session: session)
        view.pasteboard = pasteboard
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
        return view
    }

    private func enabled(_ view: TerminalSurfaceView) -> Bool {
        view.validateMenuItem(
            NSMenuItem(title: "", action: #selector(TerminalSurfaceView.copyWithFormatting(_:)), keyEquivalent: ""))
    }

    func testCopyWithFormattingWritesRTFHTMLAndThePlainString() async throws {
        let view = try await view(printing: #"\033[1;31mred\033[0m <b>&\r\nlast"#, endsWith: "last")
        XCTAssertFalse(enabled(view), "disabled without a selection")
        let before = pasteboard.changeCount
        view.copyWithFormatting(nil)
        XCTAssertEqual(pasteboard.changeCount, before, "nothing selected: the pasteboard is left as it was")
        XCTAssertEqual(pasteboard.string(forType: .string), "sentinel")

        try view.session.selectBegin(col: 0, row: 0, granularity: .line, block: false)
        XCTAssertTrue(enabled(view))
        view.copyWithFormatting(nil)
        XCTAssertEqual(pasteboard.pasteboardItems?.count, 1, "one item")
        XCTAssertEqual(pasteboard.string(forType: .string), "red <b>&")
        let html = String(decoding: try XCTUnwrap(pasteboard.data(forType: .html)), as: UTF8.self)
        XCTAssertTrue(html.contains("&lt;b&gt;&amp;"), html)
        XCTAssertFalse(html.contains("<b>"), html)
        // The RTF reads back as the text with a bold red run (AppKit's
        // reader, in the test only: the app never parses either format).
        let rtf = try XCTUnwrap(pasteboard.data(forType: .rtf))
        let styled = try XCTUnwrap(NSAttributedString(rtf: rtf, documentAttributes: nil))
        XCTAssertEqual(styled.string, "red <b>&")
        let font = try XCTUnwrap(styled.attribute(.font, at: 0, effectiveRange: nil) as? NSFont)
        XCTAssertTrue(font.fontDescriptor.symbolicTraits.contains(.bold), "\(font)")
        let colour = try XCTUnwrap(
            (styled.attribute(.foregroundColor, at: 0, effectiveRange: nil) as? NSColor)?.usingColorSpace(.sRGB))
        XCTAssertGreaterThan(colour.redComponent, 0.5)
        XCTAssertLessThan(colour.greenComponent, 0.5)
        XCTAssertNil(styled.attribute(.link, at: 4, effectiveRange: nil))
    }

    func testCopyStillWritesThePlainStringAlone() async throws {
        let view = try await view(printing: #"\033[31mred\033[0m\r\nlast"#, endsWith: "last")
        try view.session.selectBegin(col: 0, row: 0, granularity: .line, block: false)
        view.copy(nil)
        XCTAssertEqual(pasteboard.string(forType: .string), "red")
        XCTAssertNil(pasteboard.data(forType: .rtf))
        XCTAssertNil(pasteboard.data(forType: .html))
    }

    func testATooLargeSelectionIsWrittenAsThePlainStringAlone() {
        pasteboard.setData(Data("old".utf8), forType: .html)
        TerminalSurfaceView.write(.init(plain: "plain", html: nil, rtf: nil), to: pasteboard)
        XCTAssertNil(pasteboard.data(forType: .html), "the older HTML is gone")
        XCTAssertNil(pasteboard.data(forType: .rtf))
        XCTAssertEqual(pasteboard.pasteboardItems?.count, 1)
        XCTAssertEqual(pasteboard.string(forType: .string), "plain")
    }

    func testASelectionTheCoreRefusesToFormatIsCopiedAsThePlainString() async throws {
        // About 30 MB of HTML from 276 kB of text: refused by the core (the
        // 16 MiB cap), so Copy with Formatting writes the plain string alone.
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera-too-large-\(UUID().uuidString).txt")
        try Self.tooLargeToFormat(lines: 1100, cols: 250).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        let view = try await view(running: "cat '\(file.path)'", rows: 50, cols: 250, endsWith: "last")
        try view.session.selectAll()
        let plain = try XCTUnwrap(try view.session.selectionText())
        view.copyWithFormatting(nil)
        XCTAssertEqual(pasteboard.pasteboardItems?.count, 1, "one item")
        // Compared, not printed: a failure would print 276 kB.
        XCTAssertGreaterThan(plain.utf8.count, 1100 * 250)
        XCTAssertTrue(pasteboard.string(forType: .string) == plain, "the plain string is on the pasteboard")
        XCTAssertNil(pasteboard.data(forType: .rtf))
        XCTAssertNil(pasteboard.data(forType: .html))
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

    func testTheMenuItemSitsAfterCopy() throws {
        let bar = MainMenu.makeMenuBar().bar
        let item = try XCTUnwrap(MainMenu.item(forCommand: "copy_with_formatting", in: bar))
        let menu = try XCTUnwrap(item.menu)
        XCTAssertEqual(menu.items[menu.index(of: item) - 1].identifier?.rawValue, "copy")
        XCTAssertEqual(item.title, "Copy with Formatting")
        XCTAssertEqual(item.keyEquivalent, "", "no default chord")
    }
}
