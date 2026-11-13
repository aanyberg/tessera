import AppKit
import TesseraKit
import UniformTypeIdentifiers
import XCTest

@testable import Tessera

/// The link-opening follow-ups of #299 (ADR 0027 amendment): the files a
/// Command-click shows in Finder because opening them would install
/// something or start a system flow that does (D4-2), the beep for a click
/// that opens nothing (D4-3), and the Command-hover under an overlay scroll
/// bar.
///
/// The policy is judged by extension or by type, so each rule has a row
/// only it catches: a folder named like the file isolates the extension
/// rule (its type is a folder's), and ``LinkPolicy/isLaunchable(_:)`` the
/// type rule.
@MainActor
final class LinkOpeningTests: XCTestCase {
    private var app: TesseraApp!
    private let delegate = TerminalSelectionTests.TitleRecorder()

    override func setUp() async throws {
        app = try TesseraApp()
        app.delegate = delegate
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    private func rig(rows: Int, cols: Int, feed: String) async throws -> TerminalSelectionTests.Rig {
        var row = TerminalSelectionTests.Row(id: "links")
        row.rows = rows
        row.cols = cols
        row.steps = [.feed(Array(feed.utf8))]
        return try await .init(app: app, delegate: delegate, row: row)
    }

    private func makeDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "tessera-link-opening-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    /// The extensions D4-2 adds: configuration and provisioning profiles,
    /// certificates (Keychain Access's dynamic-typed ones too), shortcuts
    /// (both formats), a Java Web Start file, a flat Automator workflow and
    /// a Terminal session file.
    private static let installing = [
        "mobileconfig", "configprofile", "provisionprofile", "cer", "crt", "der", "pem", "cert", "p7c", "p7b",
        "shortcut", "wflow", "jnlp", "workflow", "term",
    ]

    // MARK: Policy (D4-2)

    func testFilesThatInstallOrStartASystemFlowAreRevealedAndArchivesOpen() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let manager = FileManager.default
        func action(_ name: String) -> LinkPolicy.Action? {
            LinkPolicy.action(for: directory.appendingPathComponent(name).absoluteString, localHostName: "mymac")
        }
        func url(_ name: String) -> URL { directory.appendingPathComponent(name) }
        var files = Self.installing.map { "file.\($0)" } + ["UPPER.CER"]
        files += ["a.zip", "b.tar.gz", "c.tgz", "d.tar", "e.xml", "f.plist", "g.txt"]
        for name in files {
            XCTAssertTrue(manager.createFile(atPath: url(name).path, contents: Data("x".utf8)), name)
        }
        for ext in Self.installing {
            try manager.createDirectory(at: url("folder.\(ext)"), withIntermediateDirectories: false)
        }
        for ext in Self.installing {
            XCTAssertEqual(action("file.\(ext)"), .reveal(url("file.\(ext)")), "a .\(ext) file is shown, not opened")
            // A folder's type is a folder's: only the extension rule shows it
            // (unless an app declares the name a package, as one does for
            // .pem here, so the set is checked as well).
            XCTAssertEqual(action("folder.\(ext)"), .reveal(url("folder.\(ext)")), "the .\(ext) extension rule")
            XCTAssertTrue(LinkPolicy.launchableExtensions.contains(ext), ext)
        }
        XCTAssertEqual(action("UPPER.CER"), .reveal(url("UPPER.CER")), "in any case")
        for opens in ["a.zip", "b.tar.gz", "c.tgz", "d.tar"] {
            XCTAssertEqual(action(opens), .open(url(opens)), "\(opens): Archive Utility extracts, runs nothing")
        }
        for opens in ["e.xml", "f.plist", "g.txt"] {
            XCTAssertEqual(action(opens), .open(url(opens)), "\(opens): a profile's supertypes are not swept up")
        }
    }

    func testTheTypeRulesCatchEachKindWhateverItsName() throws {
        for identifier in [
            "com.apple.mobileconfig", "com.apple.configprofile", "com.apple.provisionprofile",
            "public.x509-certificate", "com.apple.shortcut", "com.apple.shortcuts.workflow-file",
            "com.sun.java-web-start", "com.apple.automator-workflow", "com.apple.terminal.session",
        ] {
            let type = try XCTUnwrap(UTType(identifier), "\(identifier) is declared on this system")
            XCTAssertTrue(LinkPolicy.isLaunchable(type), identifier)
        }
        // Every extension the system maps to a certificate, by its type.
        for ext in ["cer", "crt", "der", "pem"] {
            let type = try XCTUnwrap(UTType(filenameExtension: ext))
            XCTAssertTrue(type.conforms(to: try XCTUnwrap(UTType("public.x509-certificate"))), ext)
            XCTAssertTrue(LinkPolicy.isLaunchable(type), ext)
        }
        // Keychain Access's other certificate extensions have no declared
        // type: only the extension rule shows them.
        for ext in ["cert", "p7c", "p7b"] {
            XCTAssertTrue(try XCTUnwrap(UTType(filenameExtension: ext)).isDynamic, ext)
            XCTAssertTrue(LinkPolicy.launchableExtensions.contains(ext), ext)
        }
        for opens: UTType in [.zip, .gzip, .archive, .xml, .propertyList, .plainText, .folder, .pdf] {
            XCTAssertFalse(LinkPolicy.isLaunchable(opens), opens.identifier)
        }
    }

    // MARK: The click (D4-3)

    /// A Command-click on a hovered `file:` link whose path does not exist
    /// beeps; one that opens or reveals does not, nor does one on a link
    /// the policy refuses whatever the disk holds (it is not underlined).
    func testAClickOnAMissingFileLinkBeepsAndOpensNothing() async throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let present = directory.appendingPathComponent("notes.txt")
        XCTAssertTrue(FileManager.default.createFile(atPath: present.path, contents: Data("x".utf8)))
        let missing = directory.appendingPathComponent("missing.txt")
        let links = [missing.absoluteString, present.absoluteString, "file://otherhost/etc/passwd"]
        let rig = try await rig(rows: 4, cols: 200, feed: links.joined(separator: "\r\n"))
        defer { rig.close() }
        var beeps = 0
        rig.view.linkRefusedBeep = { beeps += 1 }
        for (row, link) in links.enumerated() {
            XCTAssertEqual(try rig.session.link(col: 10, row: UInt16(row)), link, "the core detects row \(row)")
        }
        try rig.perform("down 0,10 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(beeps, 1, "the missing file")
        XCTAssertEqual(rig.opener.opened + rig.opener.revealed, [], "and nothing opened")
        try rig.perform("down 1,10 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened, [present])
        try rig.perform("down 2,10 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(beeps, 1, "neither an opened file nor a refused host beeps")
        XCTAssertEqual(rig.opener.opened, [present])
    }

    // MARK: Hover under an overlay scroll bar

    /// An overlay scroll bar floats over the text's last columns and takes
    /// the click there: the link under it is not underlined, the same link
    /// beside the bar is. A hidden bar (no history) covers nothing.
    func testCommandHoverSkipsTheTextUnderAShownOverlayScrollBar() async throws {
        let lines = (1...9).map { "l\($0)" }.joined(separator: "\r\n")
        let link = "https://a.bc/" + String(repeating: "x", count: 17)
        let rig = try await rig(rows: 3, cols: 30, feed: lines + "\r\n" + link)
        var open = true
        defer { if open { rig.close() } }
        let view = rig.view
        XCTAssertEqual(view.scroller.effectiveStyle, .overlay)
        XCTAssertEqual(view.renderView.frame.maxX, view.bounds.maxX, "the text runs under the overlay bar")
        let y = view.bounds.height - (2 * 20 + 10)
        XCTAssertEqual(try rig.session.link(col: 29, row: 2), link, "the link reaches the last column")
        func hover(atX x: CGFloat) throws -> String? {
            view.mouseMoved(
                with: try XCTUnwrap(
                    NSEvent.mouseEvent(
                        with: .mouseMoved, location: NSPoint(x: x, y: y), modifierFlags: .command, timestamp: 0,
                        windowNumber: 0, context: nil, eventNumber: 0, clickCount: 0, pressure: 0)))
            return view.hoveredLink
        }
        let bar = view.scroller.frame
        XCTAssertGreaterThan(bar.minX, view.bounds.maxX - 20, "the bar covers at most the last two columns")
        view.scroller.update(try rig.session.scrollPosition)
        XCTAssertFalse(view.scroller.isHidden, "with history the bar is shown")
        XCTAssertEqual(try hover(atX: 5), link, "beside the bar the link hovers")
        XCTAssertNil(try hover(atX: bar.midX), "under the bar it does not")
        XCTAssertEqual(try hover(atX: bar.minX - 1), link, "up to the bar's edge")
        rig.close()
        open = false
        // Without history the overlay bar is hidden and covers nothing.
        let bare = try await self.rig(rows: 3, cols: 30, feed: "l1\r\nl2\r\n" + link)
        defer { bare.close() }
        XCTAssertEqual(try bare.session.scrollPosition.history, 0)
        bare.view.scroller.update(try bare.session.scrollPosition)
        XCTAssertTrue(bare.view.scroller.isHidden, "no history: the overlay bar is hidden")
        XCTAssertEqual(bare.view.scroller.frame, bar)
        bare.view.mouseMoved(
            with: try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: .mouseMoved, location: NSPoint(x: bar.midX, y: y), modifierFlags: .command, timestamp: 0,
                    windowNumber: 0, context: nil, eventNumber: 0, clickCount: 0, pressure: 0)))
        XCTAssertEqual(bare.view.hoveredLink, link, "and the link under it hovers")
    }
}
