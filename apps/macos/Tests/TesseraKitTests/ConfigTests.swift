import Foundation
import XCTest

@testable import TesseraKit

/// The configuration through the wrapper (ADR 0040, ADR 0044), on a core of
/// the test's own with a `config.toml` in a temporary directory (never the
/// user's): reading the configuration in effect and the themes, setting and
/// removing a key path from a background thread, the core's refusals and
/// their messages, importing a theme, and the shutdown waiting for a call
/// in progress.
@MainActor
final class ConfigTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var app: TesseraApp?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-config-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"))
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private var configFile: URL { dir.appendingPathComponent("config.toml") }

    private func start(config: String) throws -> TesseraApp {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try config.write(to: configFile, atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let started = try TesseraApp()
        app = started
        return started
    }

    private func file() throws -> String {
        try String(contentsOf: configFile, encoding: .utf8)
    }

    /// Runs `body` on a background queue, as the Settings window does.
    private nonisolated static func inBackground<T: Sendable>(_ body: @escaping @Sendable () -> T) async -> T {
        await withCheckedContinuation { continuation in
            DispatchQueue.global().async { continuation.resume(returning: body()) }
        }
    }

    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    // MARK: Values

    func testAFloatIsWrittenWithAFraction() {
        XCTAssertEqual(TesseraConfigValue.float(1).json, "1.0", "the core keeps a float key a float")
        XCTAssertEqual(TesseraConfigValue.float(0.85).json, "0.85")
        XCTAssertEqual(TesseraConfigValue.float(13).json, "13.0")
        XCTAssertEqual(TesseraConfigValue.integer(10_000).json, "10000")
        XCTAssertNil(TesseraConfigValue.float(.nan).json)
        XCTAssertNil(TesseraConfigValue.float(.infinity).json)
        XCTAssertEqual(TesseraConfigValue.bool(false).json, "false")
        XCTAssertEqual(
            TesseraConfigValue.string("a\"b\\c\n\u{1}\u{202E}").json, "\"a\\\"b\\\\c\\n\\u0001\u{202E}\"")
        XCTAssertEqual(TesseraConfigValue.array([.string("x"), .integer(2)]).json, "[\"x\",2]")
        XCTAssertNil(TesseraConfigValue.table([:]).json, "an edit never writes a table")
        XCTAssertNil(TesseraConfigValue.null.json)
        XCTAssertNil(TesseraConfigValue.array([.null]).json)
    }

    /// A float with no fraction stays a float (`JSONDecoder` would make it
    /// an integer, and writing it back would change the key's type).
    func testNumbersKeepTheirType() throws {
        let json = #"{"f":13.0,"g":1e2,"h":0.85,"i":13,"j":true,"k":1,"l":-9223372036854775808,"m":18446744073709551615}"#
        let value = try TesseraConfigValue.parse(json: Data(json.utf8))
        XCTAssertEqual(value["f"], .float(13))
        XCTAssertEqual(value["g"], .float(100))
        XCTAssertEqual(value["h"], .float(0.85))
        XCTAssertEqual(value["i"], .integer(13))
        XCTAssertEqual(value["j"], .bool(true))
        XCTAssertEqual(value["k"], .integer(1), "1 is not true")
        XCTAssertEqual(value["l"], .integer(.min))
        XCTAssertEqual(value["m"]?.double, 18446744073709551615.0)
        XCTAssertThrowsError(try TesseraConfigValue.parse(json: Data("{".utf8)))
    }

    func testAKeyPathReadsNestedTablesAndArrays() throws {
        let json = #"{"a":{"b":[{"c":1},{"c":"two","d":null}]},"e":true}"#
        let value = try TesseraConfigValue.parse(json: Data(json.utf8))
        XCTAssertEqual(value["a.b[0].c"], .integer(1))
        XCTAssertEqual(value["a.b[1].c"], .string("two"))
        XCTAssertNil(value["a.b[1].d"], "null is no value")
        XCTAssertNil(value["a.b[2].c"])
        XCTAssertNil(value["a.x"])
        XCTAssertNil(value["e.f"])
        XCTAssertEqual(value["e"]?.bool, true)
        XCTAssertNil(value["a.b[x]"])
        XCTAssertEqual(TesseraConfigValue.float(13).integer, 13)
        XCTAssertNil(TesseraConfigValue.float(13.5).integer)
        XCTAssertEqual(TesseraConfigValue.integer(3).double, 3)
    }

    // MARK: Reading

    func testTheSnapshotIsTheConfigurationInEffect() throws {
        let app = try start(
            config: """
                [appearance]
                opacity = 0.85
                [[profiles]]
                name = "zsh"
                """)
        let snapshot = try app.configSnapshot()
        XCTAssertEqual(snapshot.generation, 0)
        XCTAssertEqual(try XCTUnwrap(snapshot["appearance.opacity"]?.double), 0.85, accuracy: 1e-6)
        XCTAssertEqual(snapshot["general.confirm_close"], .bool(true), "a default is filled in")
        XCTAssertEqual(snapshot["terminal.scrollback_lines"], .integer(10_000))
        XCTAssertEqual(snapshot["profiles[0].name"], .string("zsh"))
        XCTAssertNil(snapshot["profiles[0].appearance.theme"], "a profile key left out is no value")
        XCTAssertNil(snapshot["profiles[1]"])
    }

    func testThemesListTheBundledOnesFirst() throws {
        let themes = try start(config: "").themes()
        XCTAssertGreaterThanOrEqual(themes.count, 10)
        XCTAssertEqual(themes.first, TesseraTheme(name: "Tessera Dark", id: "Tessera Dark", source: .bundled))
        XCTAssertTrue(themes.allSatisfy { $0.source == .bundled })
    }

    // MARK: Writing

    func testASetFromABackgroundThreadIsSavedAndApplied() async throws {
        let app = try start(config: "[appearance]\nopacity = 0.85\n")
        let writer = app.configWriter
        let result = await Self.inBackground { () -> TesseraConfigEditError? in
            XCTAssertFalse(Thread.isMainThread)
            do throws(TesseraConfigEditError) {
                try writer.set("appearance.opacity", to: .float(0.5))
                try writer.set("terminal.scrollback_lines", to: .integer(2000))
                try writer.set("general.default_profile", to: .string("work \"one\""))
                return nil
            } catch {
                return error
            }
        }
        XCTAssertNil(result)
        let text = try file()
        XCTAssertTrue(text.contains("opacity = 0.5"), text)
        XCTAssertTrue(text.contains("scrollback_lines = 2000"), text)
        try await waitUntil("the watch applied the file") {
            (try? app.configSnapshot())?["terminal.scrollback_lines"] == .integer(2000)
        }
        let snapshot = try app.configSnapshot()
        XCTAssertGreaterThan(snapshot.generation, 0)
        XCTAssertEqual(try XCTUnwrap(snapshot["appearance.opacity"]?.double), 0.5, accuracy: 1e-6)
        XCTAssertEqual(snapshot["general.default_profile"], .string("work \"one\""), "the string's quotes")
    }

    func testARemovedKeyTakesItsDefaultAgain() async throws {
        let app = try start(config: "[terminal]\nscrollback_lines = 2000\n")
        try app.configWriter.remove("terminal.scrollback_lines")
        XCTAssertFalse(try file().contains("scrollback_lines"))
        try await waitUntil("the default applies") {
            (try? app.configSnapshot())?["terminal.scrollback_lines"] == .integer(10_000)
        }
        try app.configWriter.remove("terminal.scrollback_lines")
    }

    func testARefusedSetWritesNothingAndSaysWhy() throws {
        let original = "[appearance]\nopacity = 0.85\n"
        let app = try start(config: original)
        XCTAssertThrowsError(try app.configWriter.set("appearance.opacity", to: .float(2))) { error in
            let error = error as? TesseraConfigEditError
            XCTAssertEqual(error?.kind, .refused)
            XCTAssertFalse(error?.message.isEmpty ?? true, "the loader's warning")
        }
        XCTAssertThrowsError(try app.configWriter.set("appearance.cursor_shape", to: .string("bogus"))) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .refused)
        }
        XCTAssertThrowsError(try app.configWriter.set("appearance..x", to: .bool(true))) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .invalidArgument)
        }
        XCTAssertThrowsError(try app.configWriter.set("appearance.opacity", to: .table([:]))) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .invalidArgument)
        }
        XCTAssertEqual(try file(), original, "nothing was written")
    }

    // MARK: Themes

    /// An `.itermcolors` file with every colour the importer needs.
    private func itermcolors(at url: URL) throws {
        var dict: [String: Any] = [:]
        let colour: [String: Any] = [
            "Red Component": 0.2, "Green Component": 0.4, "Blue Component": 0.6, "Color Space": "sRGB",
        ]
        for index in 0..<16 { dict["Ansi \(index) Color"] = colour }
        dict["Foreground Color"] = colour
        dict["Background Color"] = colour
        let data = try PropertyListSerialization.data(fromPropertyList: dict, format: .xml, options: 0)
        try data.write(to: url)
    }

    func testAnImportedThemeIsListedAndNotSelected() throws {
        let app = try start(config: "")
        let source = dir.appendingPathComponent("Mine.itermcolors")
        try itermcolors(at: source)
        let id = try app.configWriter.importTheme(at: source)
        XCTAssertFalse(id.isEmpty)
        let themes = try app.themes()
        XCTAssertTrue(themes.contains { $0.id == id && $0.source == .user }, "\(themes)")
        XCTAssertEqual(try app.configSnapshot()["appearance.theme"], .string("Tessera Dark"))
        XCTAssertThrowsError(try app.configWriter.importTheme(at: source)) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .conflict, "the file is there already")
        }
    }

    func testAHostileSchemeIsRefusedAndWritesNoFile() throws {
        let app = try start(config: "")
        let themes = dir.appendingPathComponent("themes")
        let before = (try? FileManager.default.contentsOfDirectory(atPath: themes.path)) ?? []
        let hostile = dir.appendingPathComponent("evil\u{202E}.itermcolors")
        try Data("<?xml version=\"1.0\"?><plist><dict><key>Ansi 0 Color</key><string>\u{1B}]0;x\u{7}</string></dict></plist>".utf8)
            .write(to: hostile)
        XCTAssertThrowsError(try app.configWriter.importTheme(at: hostile)) { error in
            let error = error as? TesseraConfigEditError
            XCTAssertEqual(error?.kind, .invalidArgument)
            XCTAssertFalse(error?.message.isEmpty ?? true, "the importer's error")
        }
        let missing = dir.appendingPathComponent("missing.itermcolors")
        XCTAssertThrowsError(try app.configWriter.importTheme(at: missing)) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .io)
        }
        XCTAssertEqual((try? FileManager.default.contentsOfDirectory(atPath: themes.path)) ?? [], before)
    }

    // MARK: Shutdown

    func testCallsAfterShutdownFailAndShutdownWaitsForACallInProgress() async throws {
        let app = try start(config: "")
        let writer = app.configWriter
        // Many edits on a background queue while the core shuts down: each
        // either completes or fails with a null pointer, none touches a
        // freed core (TSan and the address checks would say).
        let edits = Task.detached { () -> [TesseraConfigEditError.Kind?] in
            (0..<50).map { index in
                do throws(TesseraConfigEditError) {
                    try writer.set("terminal.scrollback_lines", to: .integer(Int64(1000 + index)))
                    return nil
                } catch {
                    return error.kind
                }
            }
        }
        try await Task.sleep(for: .milliseconds(5))
        app.shutdown()
        let kinds = await edits.value
        XCTAssertTrue(kinds.allSatisfy { $0 == nil || $0 == .core(.nullPointer) }, "\(kinds)")
        XCTAssertThrowsError(try writer.set("terminal.scrollback_lines", to: .integer(1))) { error in
            XCTAssertEqual((error as? TesseraConfigEditError)?.kind, .core(.nullPointer))
        }
        XCTAssertThrowsError(try app.configSnapshot())
    }
}
