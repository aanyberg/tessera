import AppKit
@testable import TesseraKit
import XCTest

/// The built-in plugins (ADR 0047) through the wrapper: decoding the
/// core's JSON, and ``TesseraApp/plugins()`` on a core of the test's own.
@MainActor
final class PluginTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var app: TesseraApp?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-plugins-\(UUID().uuidString)")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    private func start(config: String) throws -> TesseraApp {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let started = try TesseraApp()
        app = started
        return started
    }

    private func until(_ description: String, _ condition: () throws -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while try !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(description)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    func testTheJSONDecodesEveryField() throws {
        let json = """
            [{"id": "dev.tessera.hello", "name": "Hello", "version": "0.1.0",
              "capabilities": ["sessions", "status_bar"], "enabled": true, "state": "active", "message": null,
              "status_items": [{"id": "sessions", "text": "2 sessions", "tooltip": null},
                               {"id": "t", "text": "x", "tooltip": "tip"}]},
             {"id": "b", "name": "B", "version": "1", "capabilities": ["output"], "enabled": true,
              "state": "refused", "message": "refused: needs the output capability, which this version does not grant",
              "status_items": []},
             {"id": "c", "name": "C", "version": "1", "capabilities": [], "enabled": false, "state": "sleeping",
              "message": null, "status_items": []}]
            """
        let plugins = try TesseraPlugin.decode(Array(json.utf8))
        XCTAssertEqual(
            plugins.first,
            TesseraPlugin(
                id: "dev.tessera.hello", name: "Hello", version: "0.1.0", capabilities: ["sessions", "status_bar"],
                enabled: true, state: .active, message: nil,
                statusItems: [
                    .init(id: "sessions", text: "2 sessions", tooltip: nil), .init(id: "t", text: "x", tooltip: "tip"),
                ]))
        XCTAssertEqual(plugins[1].state, .refused)
        XCTAssertEqual(plugins[1].message, "refused: needs the output capability, which this version does not grant")
        XCTAssertEqual(plugins[2].state, .failed, "a state a newer core adds reads as failed")
    }

    func testSettlingIsAPluginTheHostHasYetToStartOrStop() {
        func plugin(_ enabled: Bool, _ state: TesseraPlugin.State) -> TesseraPlugin {
            TesseraPlugin(id: "p", name: "P", version: "1", capabilities: [], enabled: enabled, state: state)
        }
        XCTAssertTrue(plugin(true, .disabled).isSettling, "listed, not yet started")
        XCTAssertTrue(plugin(false, .active).isSettling, "unlisted, not yet stopped")
        XCTAssertTrue(plugin(false, .failed).isSettling, "unlisted, its failure not yet cleared")
        for steady in [plugin(true, .active), plugin(true, .refused), plugin(true, .failed), plugin(false, .disabled)] {
            XCTAssertFalse(steady.isSettling, "\(steady.enabled) \(steady.state)")
        }
    }

    func testTheCoreReportsHelloAndStartsItWhenEnabled() async throws {
        let off = try start(config: "").plugins()
        let hello = try XCTUnwrap(off.first { $0.id == "dev.tessera.hello" }, "\(off)")
        XCTAssertEqual(hello.name, "Hello")
        XCTAssertEqual(hello.capabilities, ["sessions", "status_bar"])
        XCTAssertFalse(hello.version.isEmpty)
        XCTAssertFalse(hello.enabled)
        XCTAssertEqual(hello.state, .disabled)
        XCTAssertNil(hello.message)
        XCTAssertEqual(hello.statusItems, [])
        app?.shutdown()
        app = nil

        let on = try start(config: "[plugins]\nenabled = [\"dev.tessera.hello\"]\n")
        try await until("Hello is active") {
            try on.plugins().first { $0.id == "dev.tessera.hello" }?.state == .active
        }
        let active = try XCTUnwrap(on.plugins().first { $0.id == "dev.tessera.hello" })
        XCTAssertTrue(active.enabled)
        XCTAssertEqual(active.statusItems.map(\.id), ["sessions"])
        on.shutdown()
        app = nil
        XCTAssertThrowsError(try on.plugins(), "after shutdown")
    }
}
