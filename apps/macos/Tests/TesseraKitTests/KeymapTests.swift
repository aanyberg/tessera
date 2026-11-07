import AppKit
@testable import TesseraKit
import XCTest

/// The key map (#43, ADR 0036) through the wrapper: reading it, resolving
/// keys with ``TesseraSession/handle(key:bindingsOnly:)``, and a
/// `config.toml` edit reaching ``TesseraAppDelegate/configurationDidApply(generation:)``.
@MainActor
final class KeymapTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var app: TesseraApp?
    private var recorder: Recorder!

    override func setUp() async throws {
        // A directory of this test's own, so the configuration watch runs on
        // it: the scheme's scratch directory stays absent for the others.
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-keymap-\(UUID().uuidString)")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        recorder = Recorder()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    /// Starts an app on this test's directory, with `config` as its
    /// `config.toml` when given.
    private func start(config: String? = nil) throws -> TesseraApp {
        if let config {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        }
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let started = try TesseraApp()
        started.delegate = recorder
        app = started
        return started
    }

    private func until(
        _ description: String, timeout: Duration = .seconds(10), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(description)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    private static func cmd(_ character: Unicode.Scalar) -> TesseraSession.KeyEvent {
        TesseraSession.KeyEvent(key: .char, codepoint: character, modifiers: .super, text: String(character))
    }

    func testTheDefaultMapCarriesTheMenuChords() throws {
        let keymap = try start().keymap()
        XCTAssertEqual(keymap.generation, 0)
        XCTAssertEqual(keymap.warnings, [])
        XCTAssertEqual(keymap.commands.map(\.id), Array(0..<UInt32(keymap.commands.count)), "id order")
        let newTab = try XCTUnwrap(keymap.command(named: "new_tab"))
        XCTAssertEqual(newTab.owner, .app)
        XCTAssertEqual(newTab.chords, ["cmd+t"])
        XCTAssertEqual(newTab.menu, [.init(keyEquivalent: "t", modifiers: .command)])
        // Two chords: the visible item's and a hidden alternate.
        XCTAssertEqual(
            keymap.command(named: "font_bigger")?.menu,
            [.init(keyEquivalent: "+", modifiers: .command), .init(keyEquivalent: "=", modifiers: .command)])
        XCTAssertEqual(keymap.command(named: "tab_previous")?.menu, [.init(keyEquivalent: "{", modifiers: .command)])
        XCTAssertEqual(keymap.command(named: "quit")?.owner, .system)
        XCTAssertEqual(keymap.command(named: "select_tab:3")?.parameter, "3")
    }

    func testHandleKeyReportsCommandsAndSendsOnlyWithoutBindingsOnly() throws {
        let app = try start()
        let session = try app.spawn(.init(program: "/bin/cat"))
        XCTAssertEqual(try session.handle(key: Self.cmd("t"), bindingsOnly: true), .command("new_tab"))
        XCTAssertEqual(try session.handle(key: Self.cmd("t")), .command("new_tab"), "a binding runs either way")
        XCTAssertEqual(try session.handle(key: Self.cmd("3")), .command("select_tab:3"))
        let unbound = TesseraSession.KeyEvent(key: .char, codepoint: "x", text: "x")
        XCTAssertEqual(try session.handle(key: unbound, bindingsOnly: true), .ignored, "left to the menus")
        XCTAssertEqual(try session.handle(key: unbound), .consumed, "encoded as on main")
    }

    func testAConfigEditRebindsAndReachesTheDelegate() async throws {
        let app = try start(config: "[keys]\n")
        let session = try app.spawn(.init(program: "/bin/cat"))
        XCTAssertEqual(try app.keymap().command(named: "new_tab")?.chords, ["cmd+t"])
        try "[keys]\n'cmd+e' = 'new_tab'\n'cmd+q' = 'none'\n'cmd+x' = 'bogus'\n"
            .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await until("the edit applies") { !recorder.configurations.isEmpty }
        let generation = try XCTUnwrap(recorder.configurations.last)
        let keymap = try app.keymap()
        XCTAssertEqual(keymap.generation, generation, "the map the delegate heard about")
        XCTAssertEqual(keymap.command(named: "new_tab")?.chords.first, "cmd+e", "the user's first")
        XCTAssertEqual(keymap.command(named: "new_tab")?.menu.first, .init(keyEquivalent: "e", modifiers: .command))
        XCTAssertEqual(keymap.command(named: "quit")?.menu, [], "`none` unbinds the menu item")
        XCTAssertEqual(keymap.warnings.count, 2, "\(keymap.warnings)")
        XCTAssertTrue(keymap.warnings.contains { $0.contains("cmd+x") }, "\(keymap.warnings)")
        XCTAssertEqual(try session.handle(key: Self.cmd("e"), bindingsOnly: true), .command("new_tab"))
        XCTAssertEqual(try session.handle(key: Self.cmd("q"), bindingsOnly: true), .consumed, "`none` swallows")
    }

    func testSendTextBindingsWriteToTheSession() async throws {
        let app = try start(config: "[keys]\n'cmd+e' = 'send_text:hi\\n'\n")
        let output = dir.appendingPathComponent("out")
        let ready = dir.appendingPathComponent("ready")
        let session = try app.spawn(
            .init(
                program: "/bin/sh",
                arguments: ["-c", "stty raw -echo; : > '\(ready.path)'; head -c 3 > '\(output.path)'"]))
        // Once `stty` ran, the bytes reach `head` unchanged.
        try await until("the shell is ready") { FileManager.default.fileExists(atPath: ready.path) }
        XCTAssertEqual(try session.handle(key: Self.cmd("e"), bindingsOnly: true), .consumed)
        try await until("the text arrives") {
            (try? Data(contentsOf: output))?.count == 3
        }
        XCTAssertEqual(try? Data(contentsOf: output), Data("hi\n".utf8))
    }
}
