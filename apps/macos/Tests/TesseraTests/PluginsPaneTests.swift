import AppKit
import SwiftUI
import XCTest

@testable import Tessera
import TesseraKit

/// The Plugins pane of the Settings window (ADR 0047; ADR 0044, Plugins
/// pane). What a toggle writes, the lock while it applies, refusals, the
/// reads while a plugin settles and the text shown run on stand-ins with a
/// manual clock; turning the built-in Hello plugin on and off, the reads on
/// apply and the window's release run against a core of the test's own on
/// a `config.toml` in a temporary directory (never the user's), opened the
/// way the app opens it.
@MainActor
final class PluginsPaneTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL?
    private var previous: String?
    private var replacedCore = false
    /// The stand-in sources of the test's models, which hold them weakly.
    private var sources: [FakeSource] = []
    private var controllers: [SettingsWindowController] = []

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertNil(appDelegate.settingsWindowController, "a previous test left Settings open")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        appDelegate.settingsWindowController?.close()
        controllers.forEach { $0.close() }
        controllers = []
        if replacedCore {
            try appDelegate.replaceCore(start: false)
        }
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        sources = []
        appDelegate = nil
    }

    // MARK: Helpers

    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    private func file() throws -> String {
        try String(contentsOf: try XCTUnwrap(dir).appendingPathComponent("config.toml"), encoding: .utf8)
    }

    /// Gives the app a core of its own on `config` in a fresh temporary
    /// directory and opens Settings on its Plugins pane, as the menu does.
    private func openPlugins(config: String) throws -> SettingsWindowController {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-plugins-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true
        try appDelegate.replaceCore(start: true)
        MainMenu.run(command: "settings")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        controller.select(pane: "plugins")
        return controller
    }

    private static func json(_ text: String) throws -> TesseraConfigValue {
        try TesseraConfigValue.parse(json: Data(text.utf8))
    }

    /// `[plugins] enabled` as the core's configuration JSON has it.
    private static func config(enabled: [String]) throws -> TesseraConfigValue {
        .table(["plugins": .table(["enabled": .array(enabled.map { .string($0) })])])
    }

    private static func plugin(
        _ id: String, enabled: Bool = false, state: TesseraPlugin.State = .disabled, message: String? = nil,
        capabilities: [String] = ["sessions"]
    ) -> TesseraPlugin {
        TesseraPlugin(
            id: id, name: id.uppercased(), version: "1.0", capabilities: capabilities, enabled: enabled, state: state,
            message: message)
    }

    /// The model on stand-ins with `enabled` applied and `plugins` built in.
    private func fakeModel(
        enabled: [String], plugins: [TesseraPlugin], writer: FakeWriter = FakeWriter(), clock: ManualClock = ManualClock()
    ) throws -> (SettingsModel, FakeSource, FakeWriter, ManualClock) {
        let source = FakeSource(writer: writer)
        source.snapshot = TesseraConfigSnapshot(generation: 0, config: try Self.config(enabled: enabled))
        source.pluginList = plugins
        sources.append(source)
        return (SettingsModel(source: source, clock: clock), source, writer, clock)
    }

    /// The core applies `enabled` at `generation`, with `plugins` as the
    /// host reports them then, and tells the window.
    private static func apply(
        _ source: FakeSource, _ model: SettingsModel, generation: UInt64, enabled: [String], plugins: [TesseraPlugin]
    ) throws {
        source.snapshot = TesseraConfigSnapshot(generation: generation, config: try config(enabled: enabled))
        source.pluginList = plugins
        model.reload()
    }

    private static func listed(_ ids: [String]) -> TesseraConfigValue {
        .array(ids.map { .string($0) })
    }

    // MARK: Toggles (stand-ins)

    func testATogglesEditWritesTheWholeListAndLocksTheTogglesUntilItApplies() async throws {
        let (model, source, writer, clock) = try fakeModel(
            enabled: ["hand.written", "dev.a"],
            plugins: [Self.plugin("dev.a", enabled: true, state: .active), Self.plugin("dev.b")])
        let b = PluginsPane.enabledBinding(model, "dev.b")
        let a = PluginsPane.enabledBinding(model, "dev.a")
        XCTAssertTrue(a.wrappedValue)
        XCTAssertFalse(b.wrappedValue)
        XCTAssertNil(model.pluginsStatus)

        b.wrappedValue = true
        // Shown at once, the toggles locked with why.
        XCTAssertTrue(b.wrappedValue)
        XCTAssertTrue(model.pluginsChanging)
        XCTAssertNotNil(model.pluginsStatus)
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(model.plugins.last)), "Starting…")
        a.wrappedValue = false
        XCTAssertTrue(a.wrappedValue, "locked: a second toggle does nothing")
        try await waitUntil("written") { !model.isWriting }
        // The ids this version does not have and the file's order stay.
        XCTAssertEqual(
            writer.calls, [.init(key: "plugins.enabled", value: Self.listed(["hand.written", "dev.a", "dev.b"]))])
        XCTAssertTrue(model.pluginsChanging, "written, not yet applied")

        // Applied: the host has yet to start it, then has.
        try Self.apply(
            source, model, generation: 1, enabled: ["hand.written", "dev.a", "dev.b"],
            plugins: [Self.plugin("dev.a", enabled: true, state: .active), Self.plugin("dev.b", enabled: true)])
        XCTAssertFalse(model.pluginsChanging)
        XCTAssertNil(model.pluginsStatus)
        XCTAssertEqual(PluginsPane.status(model, model.plugins[1]), "Starting…")
        source.pluginList = [
            Self.plugin("dev.a", enabled: true, state: .active), Self.plugin("dev.b", enabled: true, state: .active),
        ]
        clock.advance(by: SettingsModel.pluginSettleInterval)
        XCTAssertEqual(PluginsPane.status(model, model.plugins[1]), "Running", "read again while it settled")

        a.wrappedValue = false
        try await waitUntil("written") { writer.calls.count == 2 }
        XCTAssertEqual(writer.calls.last, .init(key: "plugins.enabled", value: Self.listed(["hand.written", "dev.b"])))
        XCTAssertEqual(PluginsPane.status(model, model.plugins[0]), "Stopping…")
        XCTAssertEqual(writer.mainThreadCalls, 0)
    }

    func testTurningAPluginOffRemovesEveryCopyAndOnDoesNotListItTwice() async throws {
        let (model, source, writer, _) = try fakeModel(
            enabled: ["dev.a", "x", "dev.a"], plugins: [Self.plugin("dev.a", enabled: true, state: .active)])
        model.setPlugin("dev.a", enabled: true)
        XCTAssertFalse(model.pluginsChanging, "listed already: nothing to write")
        model.setPlugin("dev.a", enabled: false)
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "plugins.enabled", value: Self.listed(["x"]))])
        try Self.apply(source, model, generation: 1, enabled: ["x"], plugins: [Self.plugin("dev.a")])
        // Off and back where the file is: nothing written.
        model.setPlugin("dev.a", enabled: false)
        XCTAssertEqual(writer.calls.count, 1)
    }

    func testARefusedEditShowsTheCoresMessageSanitisedAndTheToggleTheFileAgain() async throws {
        let writer = FakeWriter()
        writer.respond = { _, _ in TesseraConfigEditError(.refused, message: "plugins.enabled: no\u{202E}\u{1B}[31m way") }
        let (model, _, _, _) = try fakeModel(enabled: [], plugins: [Self.plugin("dev.a")], writer: writer)
        let toggle = PluginsPane.enabledBinding(model, "dev.a")
        toggle.wrappedValue = true
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertFalse(toggle.wrappedValue, "the file's value again")
        XCTAssertFalse(model.pluginsChanging, "a refusal unlocks at once")
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), "plugins.enabled: no[31m way")
        XCTAssertEqual(PluginsPane.status(model, model.plugins[0]), "Off")
    }

    func testAConflictIsRetriedOnceOnTopOfTheOtherEdit() async throws {
        let writer = FakeWriter()
        writer.respond = { _, attempt in attempt == 1 ? TesseraConfigEditError(.conflict, message: "changed") : nil }
        let (model, _, _, _) = try fakeModel(enabled: [], plugins: [Self.plugin("dev.a")], writer: writer)
        model.setPlugin("dev.a", enabled: true)
        try await waitUntil("retried") { !model.isWriting }
        XCTAssertEqual(writer.calls.count, 2)
        XCTAssertNil(model.message(SettingsModel.pluginsKey))
        XCTAssertTrue(model.isPluginEnabled("dev.a"), "shown until a configuration after the retry applies")
    }

    // MARK: Reading (stand-ins)

    func testSettlingPluginsAreReadAgainAtMostFortyTimesPerApply() throws {
        let stuck = [Self.plugin("dev.a", enabled: true, state: .disabled)]
        let (model, source, _, clock) = try fakeModel(enabled: ["dev.a"], plugins: stuck)
        XCTAssertEqual(source.pluginReads, 1, "read with the configuration")
        clock.advance(by: .seconds(10))
        XCTAssertEqual(source.pluginReads, 1 + SettingsModel.pluginSettleReads)
        // Each configuration applied reads them, and starts the reads again.
        model.reload()
        clock.advance(by: .seconds(10))
        XCTAssertEqual(source.pluginReads, 2 + 2 * SettingsModel.pluginSettleReads)
        // A settled plugin is not read again.
        source.pluginList = [Self.plugin("dev.a", enabled: true, state: .active)]
        model.reload()
        clock.advance(by: .seconds(10))
        XCTAssertEqual(source.pluginReads, 3 + 2 * SettingsModel.pluginSettleReads)
        // One applied while they settle starts the reads again, instead of
        // running a second set beside the first.
        source.pluginList = stuck
        model.reload()
        clock.advance(by: .milliseconds(525))
        XCTAssertEqual(source.pluginReads, 4 + 2 * SettingsModel.pluginSettleReads + 10)
        model.reload()
        clock.advance(by: .seconds(10))
        XCTAssertEqual(source.pluginReads, 5 + 3 * SettingsModel.pluginSettleReads + 10)
    }

    func testPluginsThatCannotBeReadSaySo() throws {
        let (model, source, _, _) = try fakeModel(enabled: [], plugins: [])
        XCTAssertFalse(model.pluginsUnreadable)
        source.pluginList = nil
        model.reload()
        XCTAssertTrue(model.pluginsUnreadable)
        source.pluginList = [Self.plugin("dev.a")]
        model.readPluginsAgain()
        XCTAssertFalse(model.pluginsUnreadable)
        XCTAssertEqual(model.plugins.map(\.id), ["dev.a"])
    }

    func testShowingThePaneReadsThePluginsAgain() async throws {
        let (model, source, _, _) = try fakeModel(enabled: [], plugins: [Self.plugin("dev.a")])
        let controller = SettingsWindowController(
            model: model, actions: SettingsActions(openConfigFile: {}), autosaveName: nil)
        controllers.append(controller)
        controller.select(pane: "general")
        controller.window?.contentView?.layoutSubtreeIfNeeded()
        let before = source.pluginReads
        controller.select(pane: "plugins")
        controller.window?.contentView?.layoutSubtreeIfNeeded()
        try await waitUntil("read when shown") { source.pluginReads > before }
    }

    // MARK: Text (stand-ins)

    func testEachStateShowsWhereThePluginStandsAndWhy() throws {
        let refused = Self.plugin(
            "dev.r", enabled: true, state: .refused,
            message: "refused: needs the output capability, which this version does not grant",
            capabilities: ["output", "status_bar"])
        let failed = Self.plugin("dev.f", enabled: true, state: .failed, message: "failed: panicked")
        let active = Self.plugin("dev.a", enabled: true, state: .active, capabilities: ["sessions", "storage"])
        let off = Self.plugin("dev.o", capabilities: [])
        let (model, _, _, _) = try fakeModel(enabled: ["dev.r", "dev.f", "dev.a"], plugins: [refused, failed, active, off])
        XCTAssertEqual(PluginsPane.status(model, refused), "Not started")
        XCTAssertEqual(
            PluginsPane.problem(model, refused),
            "Refused: needs the output capability, which this version does not grant.")
        XCTAssertEqual(PluginsPane.status(model, failed), "Failed")
        XCTAssertEqual(PluginsPane.problem(model, failed), "Failed: panicked. Turn it off and on again to try again.")
        XCTAssertEqual(PluginsPane.status(model, active), "Running")
        XCTAssertNil(PluginsPane.problem(model, active))
        XCTAssertEqual(PluginsPane.status(model, off), "Off")
        XCTAssertNil(PluginsPane.problem(model, off))
        XCTAssertEqual(PluginsPane.capabilities(refused), "Terminal output, Status bar items")
        XCTAssertEqual(PluginsPane.capabilities(active), "Sessions (titles and commands), Storage (in memory)")
        XCTAssertEqual(PluginsPane.capabilities(off), "None")
        XCTAssertEqual(PluginsPane.title(active), "DEV.A")
        XCTAssertEqual(PluginsPane.subtitle(active), "Version 1.0 · dev.a")

        // Turned off and not yet applied: no problem shown, it is stopping.
        model.setPlugin("dev.f", enabled: false)
        XCTAssertEqual(PluginsPane.status(model, failed), "Stopping…")
        XCTAssertNil(PluginsPane.problem(model, failed))
    }

    func testPluginTextIsShownSanitised() throws {
        let hostile = "evil\u{202E}txt\u{1B}]0;title\u{7}\u{200B}\n\u{E000}"
        let plugin = TesseraPlugin(
            id: "id\(hostile)", name: hostile, version: "1\(hostile)", capabilities: ["cap\(hostile)", "sessions"],
            enabled: true, state: .failed, message: "failed: \(hostile)")
        let (model, _, _, _) = try fakeModel(
            enabled: [plugin.id, "gone\u{202E}x", "gone\u{202E}x", "\u{200B}"], plugins: [plugin])
        let clean = { (text: String) in
            !text.unicodeScalars.contains {
                [.control, .format, .privateUse, .lineSeparator, .paragraphSeparator].contains($0.properties.generalCategory)
            }
        }
        let shown = [
            PluginsPane.title(plugin), PluginsPane.subtitle(plugin), PluginsPane.capabilities(plugin),
            try XCTUnwrap(PluginsPane.problem(model, plugin)),
        ] + model.unknownEnabledPlugins
        for text in shown {
            XCTAssertTrue(clean(text), text.debugDescription)
        }
        XCTAssertEqual(PluginsPane.title(plugin), "eviltxt]0;title")
        XCTAssertEqual(model.unknownEnabledPlugins, ["gonex"], "each once, nothing for an id with nothing visible")
        // A name with nothing visible shows the id.
        let nameless = TesseraPlugin(
            id: "dev.n", name: "\u{202E}", version: "1", capabilities: [], enabled: false, state: .disabled)
        XCTAssertEqual(PluginsPane.title(nameless), "dev.n")
    }

    // MARK: The window on a core

    func testTurningHelloOnAndOffInTheWindowWritesConfigTomlAndTheHostFollows() async throws {
        let controller = try openPlugins(config: "# mine\n[plugins]\nenabled = [] # none yet\n")
        let model = controller.model
        let hello = { model.plugins.first { $0.id == "dev.tessera.hello" } }
        XCTAssertEqual(hello()?.state, .disabled, "\(model.plugins)")
        let toggle = PluginsPane.enabledBinding(model, "dev.tessera.hello")

        toggle.wrappedValue = true
        // Nothing here reloads the model: the watch's apply does, through
        // the app's on_config_applied, and the reads while Hello settles.
        try await waitUntil("written") { (try? self.file())?.contains("enabled = [\"dev.tessera.hello\"]") ?? false }
        try await waitUntil("applied and running") { !model.pluginsChanging && hello()?.state == .active }
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(hello())), "Running")
        XCTAssertEqual(hello()?.statusItems.map(\.id), ["sessions"])
        let written = try file()
        XCTAssertTrue(written.hasPrefix("# mine\n"), written)

        toggle.wrappedValue = false
        try await waitUntil("applied and off") { !model.pluginsChanging && hello()?.state == .disabled }
        XCTAssertFalse(hello()?.enabled ?? true)
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(hello())), "Off")
        let off = try file()
        XCTAssertTrue(off.contains("enabled = []"), off)
        XCTAssertNil(model.message(SettingsModel.pluginsKey))
    }

    /// The Settings window's controller and model, held weakly, and its
    /// window, held as AppKit may hold a closed one.
    private struct Opened {
        weak var controller: SettingsWindowController?
        weak var model: SettingsModel?
        var window: NSWindow?
    }

    func testClosingTheWindowWhileHelloSettlesFreesTheControllerAndModel() async throws {
        let opened = try autoreleasepool {
            let controller = try openPlugins(config: "[plugins]\nenabled = [\"dev.tessera.hello\"]\n")
            controller.window?.contentView?.layoutSubtreeIfNeeded()
            return Opened(controller: controller, model: controller.model, window: controller.window)
        }
        try await Task.sleep(for: .milliseconds(100))
        try autoreleasepool {
            let controller = try XCTUnwrap(opened.controller)
            // An edit queued at close, and the reads of the plugins it
            // starts on apply.
            controller.model.setPlugin("dev.tessera.hello", enabled: false)
            controller.close()
        }
        XCTAssertNil(appDelegate.settingsWindowController)
        XCTAssertNotNil(opened.window)
        try await waitUntil("the controller and model are freed") { opened.controller == nil && opened.model == nil }
    }
}
