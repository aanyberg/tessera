import AppKit
import TesseraKit
import UniformTypeIdentifiers
import XCTest

@testable import Tessera

/// The link-opening follow-ups of #299 and #437 (ADR 0027 amendments): the
/// files a Command-click shows in Finder because opening them would run,
/// install or import something (D4-2, the risk list, the handler and the
/// `OpenWith` rules), the beep for a click that opens nothing (D4-3), and
/// the Command-hover under an overlay scroll bar.
///
/// The policy has several rules, so each one has a row only it catches: a
/// folder named like a file isolates the extension rule (its type is a
/// folder's and Finder opens it), a fake app bundle passed as the handler
/// isolates the handler rule (the file itself is plain text), and
/// ``LinkPolicy/isLaunchable(_:)`` the type rule. The system's own risk list
/// is read at test time, so the assertions on it hold whatever it lists on
/// the macOS the test runs on.
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

    /// The extensions the hand-kept list of #299 named: every one is still
    /// shown, now through the type, risk-list and handler rules.
    private static let installing = [
        "command", "tool", "terminal", "term", "webloc", "inetloc", "fileloc", "pkg", "mpkg", "dmg", "jar",
        "mobileconfig", "configprofile", "provisionprofile", "cer", "crt", "der", "pem", "cert", "p7c", "p7b",
        "shortcut", "wflow", "jnlp", "workflow",
    ]

    /// Kinds the reviews of #437 found opening (and M5 of #430): Keychain
    /// Access imports, Safari's web archives, flat help books and menu
    /// extras, Xcode's provisioning profiles.
    private static let found437 = [
        "p12", "pfx", "keychain", "keychain-db", "webarchive", "help", "menu", "mobileprovision",
    ]

    /// Kinds whose handler depends on the Mac (what is installed, what the
    /// user bound): each is shown whenever its default handler is a risky
    /// one, whatever that is here.
    private static let dependsOnHandler = [
        "pkcs12", "p7m", "p7s", "p7r", "ics", "vcf", "scptd", "scpt", "networkconnect", "internetconnect",
        "safariextz", "prefPane", "saver", "action", "caction", "definition", "lplug4", "skill", "torrent",
    ]

    /// Kinds Keychain Access imports. `.p12` and `.pfx` are caught by the
    /// type rule where `com.rsa.pkcs-12` is declared (CoreTypes), the
    /// keychains where `com.apple.keychain` is (Security's QuickLook
    /// generator, not CoreTypes), and `.cert`, `.p7c` and `.p7b`, which no
    /// declared type names, only by the handler rule. A Mac that binds them
    /// to another app and lacks the declaration opens them, so their rows
    /// are skipped there (``importsIntoKeychain(_:)``).
    private static let keychainKinds: Set<String> = ["p12", "pfx", "cert", "p7c", "p7b", "keychain", "keychain-db"]

    /// Whether Keychain Access is `file`'s default handler on this Mac.
    private static func opensInKeychainAccess(_ file: URL) -> Bool {
        guard let handler = LinkPolicy.defaultHandler(for: file) else { return false }
        return Bundle(url: handler)?.bundleIdentifier?.lowercased() == "com.apple.keychainaccess"
    }

    /// Whether a rule on this Mac names `file` as something Keychain Access
    /// imports: its type is a declared PKCS #12 or keychain type, or
    /// Keychain Access is its handler.
    private static func importsIntoKeychain(_ file: URL) -> Bool {
        if let type = UTType(filenameExtension: file.pathExtension), type.isDeclared,
            ["com.rsa.pkcs-12", "com.apple.keychain"].contains(type.identifier)
        {
            return true
        }
        return opensInKeychainAccess(file)
    }

    private func action(_ url: URL, handler: ((URL) -> URL?)? = nil) -> LinkPolicy.Action? {
        if let handler {
            return LinkPolicy.action(for: url.absoluteString, localHostName: "mymac", handler: handler)
        }
        return LinkPolicy.action(for: url.absoluteString, localHostName: "mymac")
    }

    // MARK: Policy (D4-2)

    func testFilesThatInstallOrStartASystemFlowAreRevealedAndArchivesOpen() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let manager = FileManager.default
        func url(_ name: String) -> URL { directory.appendingPathComponent(name) }
        var files = (Self.installing + Self.found437).map { "file.\($0)" } + ["UPPER.CER", "UPPER.P12"]
        files += ["a.zip", "b.tar.gz", "c.tgz", "d.tar", "e.xml", "f.plist", "g.txt", "h.md", "i.json", "j.png"]
        for name in files {
            XCTAssertTrue(manager.createFile(atPath: url(name).path, contents: Data("x".utf8)), name)
        }
        for ext in Self.installing + Self.found437 {
            let file = url("file.\(ext)")
            if Self.keychainKinds.contains(ext), !Self.importsIntoKeychain(file) { continue }
            XCTAssertEqual(action(file), .reveal(file), "a .\(ext) file is shown, not opened")
        }
        XCTAssertEqual(action(url("UPPER.CER")), .reveal(url("UPPER.CER")), "in any case")
        if Self.importsIntoKeychain(url("UPPER.P12")) {
            XCTAssertEqual(action(url("UPPER.P12")), .reveal(url("UPPER.P12")), "in any case")
        }
        for opens in ["a.zip", "b.tar.gz", "c.tgz", "d.tar"] {
            XCTAssertEqual(action(url(opens)), .open(url(opens)), "\(opens): Archive Utility extracts, runs nothing")
        }
        for opens in ["e.xml", "f.plist", "g.txt", "h.md", "i.json", "j.png"] {
            XCTAssertEqual(action(url(opens)), .open(url(opens)), "\(opens): a profile's supertypes and documents open")
        }
    }

    /// Whatever the Mac binds these to, a file whose default handler is
    /// risky is shown; the rest open.
    func testAFileIsShownWheneverItsDefaultHandlerIsRisky() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        var risky: [String] = []
        for ext in Self.dependsOnHandler + Self.installing + Self.found437 {
            let file = directory.appendingPathComponent("f.\(ext)")
            XCTAssertTrue(FileManager.default.createFile(atPath: file.path, contents: Data("x".utf8)), ext)
            guard let handler = LinkPolicy.defaultHandler(for: file), LinkPolicy.isRiskyHandler(handler) else { continue }
            risky.append(ext)
            XCTAssertEqual(action(file), .reveal(file), ".\(ext) opens in \(handler.lastPathComponent)")
        }
        // Where Keychain Access is the handler of a certificate or identity,
        // the handler rule names it (a Mac that binds them elsewhere has no
        // row here).
        for ext in ["p12", "pfx", "cert", "p7c", "p7b"]
        where Self.opensInKeychainAccess(directory.appendingPathComponent("f.\(ext)")) {
            XCTAssertTrue(risky.contains(ext), ".\(ext) opens in Keychain Access")
        }
    }

    /// The handler rule on its own: a plain text file opens unless the app
    /// that would open it is one of the risky system apps (by bundle
    /// identifier, in any case) or an agent app.
    func testTheHandlerRuleShowsWhatARiskyOrAgentAppWouldOpen() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let notes = directory.appendingPathComponent("notes.txt")
        XCTAssertTrue(FileManager.default.createFile(atPath: notes.path, contents: Data("x".utf8)))
        func fakeApp(_ name: String, _ info: [String: Any]) throws -> URL {
            let app = directory.appendingPathComponent("\(name).app", isDirectory: true)
            let contents = app.appendingPathComponent("Contents", isDirectory: true)
            try FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
            let plist = try PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
            try plist.write(to: contents.appendingPathComponent("Info.plist"))
            return app
        }
        XCTAssertEqual(action(notes), .open(notes), "plain text opens with its own app")
        let viewer = try fakeApp("Viewer", ["CFBundleIdentifier": "com.example.viewer", "LSUIElement": false])
        XCTAssertEqual(action(notes) { _ in viewer }, .open(notes), "a document app opens it")
        XCTAssertEqual(action(notes) { _ in nil }, .open(notes), "no app known: opening launches nothing")
        XCTAssertEqual(
            action(notes) { _ in directory.appendingPathComponent("Missing.app") }, .open(notes), "no bundle to judge")
        for (index, identifier) in LinkPolicy.riskyHandlers.sorted().enumerated() {
            // Bundle identifiers are compared in any case: Launch Services
            // does.
            let mixed = index.isMultiple(of: 2) ? identifier.uppercased() : identifier
            let app = try fakeApp("Risky\(index)", ["CFBundleIdentifier": mixed])
            XCTAssertEqual(action(notes) { _ in app }, .reveal(notes), mixed)
        }
        let agents: [[String: Any]] = [
            ["LSUIElement": true], ["LSUIElement": "1"], ["LSUIElement": 1], ["LSUIElement": "YES"],
            ["LSBackgroundOnly": true], ["LSBackgroundOnly": "true"],
        ]
        for (index, info) in agents.enumerated() {
            var plist: [String: Any] = ["CFBundleIdentifier": "com.example.agent\(index)"]
            plist.merge(info) { $1 }
            let agent = try fakeApp("Agent\(index)", plist)
            XCTAssertEqual(action(notes) { _ in agent }, .reveal(notes), "an agent app: \(info)")
        }
        let windowed: [[String: Any]] = [["LSUIElement": false], ["LSUIElement": "0"], ["LSBackgroundOnly": 0]]
        for (index, info) in windowed.enumerated() {
            var plist: [String: Any] = ["CFBundleIdentifier": "com.example.window\(index)"]
            plist.merge(info) { $1 }
            let app = try fakeApp("Window\(index)", plist)
            XCTAssertEqual(action(notes) { _ in app }, .open(notes), "not an agent: \(info)")
        }
    }

    /// Launch Services' own list, read from this Mac: every kind on it is
    /// shown (by extension, also for a folder, whose type is a folder's; by
    /// type for every declared type), and the built-in copy covers it, so a
    /// macOS that adds a kind fails here until the copy has it too.
    func testEveryKindOnTheSystemRiskListIsShownAndTheCopyCoversIt() throws {
        let system = try XCTUnwrap(LinkPolicy.systemRiskList(), "the system's risk list is readable")
        XCTAssertFalse(system.types.isEmpty)
        XCTAssertFalse(system.extensions.isEmpty)
        XCTAssertFalse(system.types.contains("public.archive"), "archives keep opening")
        XCTAssertTrue(system.types.isSubset(of: LinkPolicy.riskList.types))
        XCTAssertTrue(system.extensions.isSubset(of: LinkPolicy.riskList.extensions))
        XCTAssertEqual(
            system.types.subtracting(LinkPolicy.builtInRiskList.types), [], "types the built-in copy lacks")
        XCTAssertEqual(
            system.extensions.subtracting(LinkPolicy.builtInRiskList.extensions), [],
            "extensions the built-in copy lacks")
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let finder = URL(fileURLWithPath: "/System/Library/CoreServices/Finder.app")
        for ext in system.extensions.sorted() {
            let file = directory.appendingPathComponent("f.\(ext)")
            XCTAssertTrue(FileManager.default.createFile(atPath: file.path, contents: Data("x".utf8)), ext)
            XCTAssertEqual(action(file), .reveal(file), "a .\(ext) file")
            let folder = directory.appendingPathComponent("d.\(ext)", isDirectory: true)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
            XCTAssertEqual(action(folder) { _ in finder }, .reveal(folder), "a folder named .\(ext)")
        }
        for identifier in system.types.sorted() {
            guard let type = UTType(identifier) else { continue }  // not declared on this Mac: no file has it
            XCTAssertTrue(LinkPolicy.isLaunchable(type), identifier)
        }
        // A web archive opens in Safari, a document app: only the risk
        // list's type shows it.
        let archive = directory.appendingPathComponent("page.webarchive")
        XCTAssertTrue(FileManager.default.createFile(atPath: archive.path, contents: Data("x".utf8)))
        XCTAssertEqual(action(archive) { _ in finder }, .reveal(archive))
    }

    /// The reading of a risk-list property list: both categories, the
    /// archive type left out, extensions lower-cased, other entries ignored,
    /// and anything else, or too much of it, read as no list.
    func testTheRiskListIsReadFromBothCategoriesButTheArchiveType() throws {
        func data(_ plist: Any) throws -> Data {
            try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
        }
        let plist: [String: Any] = [
            "LSRiskCategoryUnsafeExecutable": [
                "LSRiskCategoryContentTypes": ["com.example.run", 7], "LSRiskCategoryExtensions": ["RUN", "x"],
                "LSRiskCategoryOSTypes": ["abcd"],
            ],
            "LSRiskCategoryMayContainUnsafeExecutable": [
                "LSRiskCategoryContentTypes": ["public.archive", "com.example.installer"],
                "LSRiskCategoryExtensions": ["inst"],
            ],
            "LSRiskCategorySafe": ["LSRiskCategoryContentTypes": ["public.plain-text"], "LSRiskCategoryExtensions": ["txt"]],
        ]
        XCTAssertEqual(
            LinkPolicy.riskList(fromPropertyList: try data(plist)),
            LinkPolicy.RiskList(types: ["com.example.run", "com.example.installer"], extensions: ["run", "x", "inst"]))
        XCTAssertEqual(
            LinkPolicy.riskList(fromPropertyList: try data(["LSRiskCategoryUnsafeExecutable": [String: Any]()])),
            LinkPolicy.RiskList(types: [], extensions: []), "a category with nothing in it")
        XCTAssertNil(LinkPolicy.riskList(fromPropertyList: try data(["LSRiskCategorySafe": [String: Any]()])))
        XCTAssertNil(LinkPolicy.riskList(fromPropertyList: try data(["a", "b"])), "not a dictionary")
        XCTAssertNil(LinkPolicy.riskList(fromPropertyList: Data("not a plist".utf8)))
        var big = plist
        big["padding"] = String(repeating: "x", count: LinkPolicy.maxRiskListBytes)
        XCTAssertNil(LinkPolicy.riskList(fromPropertyList: try data(big)), "more than the cap is not read")
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = directory.appendingPathComponent("System")
        try data(plist).write(to: file)
        XCTAssertEqual(LinkPolicy.systemRiskList(at: file)?.extensions, ["run", "x", "inst"])
        XCTAssertNil(LinkPolicy.systemRiskList(at: directory.appendingPathComponent("missing")))
        try data(big).write(to: file)
        XCTAssertNil(LinkPolicy.systemRiskList(at: file), "a file over the cap is not read")
    }

    /// A file that carries Launch Services' per-file `OpenWith` attribute
    /// (it hands the file to whatever app it names) is shown; the same file
    /// without it opens. The attribute names Preview here, which is no risky
    /// handler, so only the attribute rule shows it (Launch Services honours
    /// it when naming the handler, so one naming Terminal would be caught by
    /// the handler rule too). A symbolic link is shown whatever it leads to:
    /// Foundation counts it as an alias.
    func testAFileBoundToAnAppByItsOpenWithAttributeIsShown() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let plain = directory.appendingPathComponent("plain.txt")
        let bound = directory.appendingPathComponent("bound.txt")
        for file in [plain, bound] {
            XCTAssertTrue(FileManager.default.createFile(atPath: file.path, contents: Data("x".utf8)))
        }
        let value = try PropertyListSerialization.data(
            fromPropertyList: [
                "version": 0, "bundleidentifier": "com.apple.Preview", "path": "/System/Applications/Preview.app",
            ], format: .binary, options: 0)
        let result = value.withUnsafeBytes {
            setxattr(bound.path, LinkPolicy.openWithAttribute, $0.baseAddress, value.count, 0, 0)
        }
        XCTAssertEqual(result, 0, "setxattr: errno \(errno)")
        if let handler = LinkPolicy.defaultHandler(for: bound) {
            XCTAssertFalse(LinkPolicy.isRiskyHandler(handler), handler.path)
        }
        XCTAssertEqual(action(plain), .open(plain))
        XCTAssertEqual(action(bound), .reveal(bound), "the attribute names the app, so the file is shown")
        let link = directory.appendingPathComponent("link.txt")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: plain)
        XCTAssertEqual(action(link), .reveal(link), "a symbolic link, as before")
    }

    func testTheTypeRulesCatchEachKindWhateverItsName() throws {
        for identifier in [
            "com.apple.mobileconfig", "com.apple.configprofile", "com.apple.provisionprofile",
            "public.x509-certificate", "com.apple.shortcut", "com.apple.shortcuts.workflow-file",
            "com.sun.java-web-start", "com.apple.automator-workflow", "com.apple.terminal.session",
            "com.apple.webarchive", "com.apple.installer-package-archive",
        ] {
            let type = try XCTUnwrap(UTType(identifier), "\(identifier) is declared on this system")
            XCTAssertTrue(LinkPolicy.isLaunchable(type), identifier)
        }
        // Declared only where Xcode, Safari's extension support or Security's
        // QuickLook generator is (and the PKCS #12 type by CoreTypes, kept
        // here so a macOS that drops it skips the row): judged by type where
        // they are.
        for identifier in [
            "com.apple.mobileprovision", "com.apple.safari.extension", "com.rsa.pkcs-12", "com.apple.keychain",
        ] {
            guard let type = UTType(identifier) else { continue }
            XCTAssertTrue(LinkPolicy.isLaunchable(type), identifier)
        }
        // Every extension the system maps to a certificate, by its type.
        for ext in ["cer", "crt", "der", "pem"] {
            let type = try XCTUnwrap(UTType(filenameExtension: ext))
            XCTAssertTrue(type.conforms(to: try XCTUnwrap(UTType("public.x509-certificate"))), ext)
            XCTAssertTrue(LinkPolicy.isLaunchable(type), ext)
        }
        // Keychain Access's other certificate extensions have no declared
        // type: the handler rule shows them (Keychain Access opens them).
        for ext in ["cert", "p7c", "p7b"] {
            let type = try XCTUnwrap(UTType(filenameExtension: ext))
            XCTAssertTrue(type.isDynamic, ext)
            XCTAssertFalse(LinkPolicy.isLaunchable(type), ext)
        }
        for opens: UTType in [.zip, .gzip, .archive, .xml, .propertyList, .plainText, .folder, .pdf, .png, .json] {
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

    /// The click goes through the same policy with Launch Services' real
    /// handler: a PKCS #7 certificate bundle (Keychain Access imports it; no
    /// declared type, risk-list or extension rule names it, so only the
    /// handler rule shows it) and a text file bound to Terminal by its
    /// `OpenWith` attribute are shown, a plain text file opens. Skipped on a
    /// Mac that binds `.p7b` to another app.
    func testACommandClickShowsWhatTheHandlerOrOpenWithRuleCatches() async throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let certificates = directory.appendingPathComponent("certificates.p7b")
        let bound = directory.appendingPathComponent("bound.txt")
        let plain = directory.appendingPathComponent("plain.txt")
        for file in [certificates, bound, plain] {
            XCTAssertTrue(FileManager.default.createFile(atPath: file.path, contents: Data("x".utf8)))
        }
        guard Self.opensInKeychainAccess(certificates) else {
            throw XCTSkip(".p7b does not open in Keychain Access on this Mac")
        }
        XCTAssertFalse(UTType(filenameExtension: "p7b")?.isDeclared ?? false, "no declared type names .p7b")
        let value = try PropertyListSerialization.data(
            fromPropertyList: ["version": 0, "bundleidentifier": "com.apple.Terminal"], format: .binary, options: 0)
        let result = value.withUnsafeBytes {
            setxattr(bound.path, LinkPolicy.openWithAttribute, $0.baseAddress, value.count, 0, 0)
        }
        XCTAssertEqual(result, 0)
        let links = [certificates, bound, plain]
        let rig = try await rig(rows: 4, cols: 200, feed: links.map(\.absoluteString).joined(separator: "\r\n"))
        defer { rig.close() }
        for row in 0..<links.count {
            try rig.perform("down \(row),10 mods=cmd")
            try rig.perform("up mods=cmd")
        }
        XCTAssertEqual(rig.opener.revealed, [certificates, bound])
        XCTAssertEqual(rig.opener.opened, [plain])
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
