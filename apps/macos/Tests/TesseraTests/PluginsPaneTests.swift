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
        appDelegate.settingsClock = SystemSettingsClock()
        appDelegate.settingsWaitLimit = .seconds(2)
        controllers.forEach { $0.close() }
        controllers = []
        // Whether or not the test got as far as replacing the core: a
        // stand-in model left here would be the next test's Settings.
        appDelegate.waitingSettingsModel = nil
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
        XCTAssertEqual(
            model.unknownEnabledPlugins, [#""gone\u{202e}x""#, #""\u{200b}""#],
            "each once, escaped, an id with nothing visible too")
        // A name with nothing visible shows the id.
        let nameless = TesseraPlugin(
            id: "dev.n", name: "\u{202E}", version: "1", capabilities: [], enabled: false, state: .disabled)
        XCTAssertEqual(PluginsPane.title(nameless), "dev.n")
    }

    /// Ids that differ by a character the window would not show apart (a
    /// zero-width or trailing space, NFC and NFD) show apart, escaped, and
    /// never as a plugin this version has (#489 48b adversarial m1).
    func testUnknownIdsShowEscapedAndApart() throws {
        let long = String(repeating: "\u{200B}", count: 300)
        let (model, _, _, _) = try fakeModel(
            enabled: [
                "dev.a", "dev.a\u{200B}", "dev.a ", "caf\u{E9}", "cafe\u{301}", "a\"b\\c", "dev.a\u{200B}", long,
            ],
            plugins: [Self.plugin("dev.a", enabled: true, state: .active)])
        let shown = model.unknownEnabledPlugins
        XCTAssertEqual(
            Array(shown.prefix(5)),
            [#""dev.a\u{200b}""#, #""dev.a ""#, #""caf\u{e9}""#, #""cafe\u{301}""#, #""a\"b\\c""#])
        // Checked before it is indexed: an index out of range ends the
        // test host, and xcodebuild with it hangs on the crashed host.
        guard shown.count == 6 else { return XCTFail("\(shown.count) shown: \(shown)") }
        XCTAssertLessThanOrEqual(shown[5].count, SettingsModel.maxPluginIDShown + 2, "capped, in quotes")
        XCTAssertTrue(shown[5].contains("…"), shown[5])
        XCTAssertTrue(shown[5].hasSuffix(#"\u{200b}""#), "its end is kept, no escape cut: \(shown[5])")
        XCTAssertEqual(SettingsModel.shownPluginID("dev.tessera.hello"), #""dev.tessera.hello""#)
        let exact = SettingsModel.shownPluginID(String(repeating: "x", count: SettingsModel.maxPluginIDShown))
        XCTAssertFalse(exact.contains("…"), "exactly at the cap: whole")
        for text in shown {
            XCTAssertTrue(text.unicodeScalars.allSatisfy { (0x20...0x7E).contains($0.value) || $0 == "…" }, text)
        }
        XCTAssertTrue(model.isPluginEnabled("dev.a"))
        // NFC listed, NFD asked: another id for the host, which compares bytes.
        let (nfc, _, _, _) = try fakeModel(enabled: ["caf\u{E9}"], plugins: [])
        XCTAssertTrue(nfc.isPluginEnabled("caf\u{E9}"))
        XCTAssertFalse(nfc.isPluginEnabled("cafe\u{301}"))
    }

    /// Turning a plugin off takes out its id byte for byte: another
    /// spelling of it (NFD for NFC), another id for the host, stays.
    func testTurningOffKeepsAnotherSpellingOfTheId() async throws {
        let writer = FakeWriter()
        let (model, _, _, _) = try fakeModel(
            enabled: ["caf\u{E9}", "cafe\u{301}", "caf\u{E9}"],
            plugins: [Self.plugin("caf\u{E9}", enabled: true, state: .active)], writer: writer)
        PluginsPane.enabledBinding(model, "caf\u{E9}").wrappedValue = false
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(writer.calls.count, 1)
        let written = try XCTUnwrap(writer.calls.first?.value?.array?.compactMap(\.string))
        XCTAssertEqual(written.map { Array($0.utf8) }, [Array("cafe\u{301}".utf8)])
    }

    /// A flood of `[plugins] enabled` ids (S5): the footer lists at most
    /// ``SettingsModel/maxUnknownPluginsShown`` of the ids the host reads
    /// and a line for the rest, and the pane's skip warning says how many
    /// the host ignores; the pane renders it in a bounded time.
    func testTenThousandIdsShowACappedFooterAndTheSkipWarning() throws {
        let ids = (0..<10_000).map { "flood.\($0)" }
        let (model, _, _, _) = try fakeModel(enabled: ids, plugins: [Self.plugin("dev.a")])
        let shown = model.unknownEnabledPlugins
        XCTAssertEqual(shown.count, SettingsModel.maxUnknownPluginsShown + 1, "\(shown)")
        XCTAssertEqual(shown.first, #""flood.0""#)
        XCTAssertEqual(
            shown.last, "and \(TesseraPlugin.maxEnabled - SettingsModel.maxUnknownPluginsShown) more",
            "of the ids the host reads")
        XCTAssertEqual(
            model.pluginsSkipWarning,
            "[plugins] enabled lists more than 64 ids; the rest are ignored (\(10_000 - 64) ids ignored).")
        // One id, ten thousand times: once.
        let (same, _, _, _) = try fakeModel(enabled: Array(repeating: "x", count: 10_000), plugins: [])
        XCTAssertEqual(same.unknownEnabledPlugins, [#""x""#])

        let controller = SettingsWindowController(
            model: model, actions: SettingsActions(openConfigFile: {}), autosaveName: nil)
        controllers.append(controller)
        let start = ContinuousClock.now
        controller.select(pane: "plugins")
        controller.window?.contentView?.layoutSubtreeIfNeeded()
        let elapsed = ContinuousClock.now - start
        // Recorded for the report; a loose bound (the uncapped footer took
        // 0.23 s at 10 000 ids, 1.57 s at 100 000).
        print("plugins pane with 10 000 ids rendered in \(elapsed)")
        XCTAssertLessThan(elapsed, .seconds(5))
    }

    /// Ids too long to show whole that share far more than the shown start
    /// show apart by their ends, within the cap, with no escape cut and
    /// nothing unescaped (S3, S5).
    func testLongIdsSharingTheirStartShowApart() {
        let cap = SettingsModel.maxPluginIDShown
        let start = String(repeating: "x", count: cap + 20)
        let one = SettingsModel.shownPluginID(start + "-one")
        let two = SettingsModel.shownPluginID(start + "-two")
        XCTAssertNotEqual(one, two)
        XCTAssertTrue(one.hasSuffix("-one\""), one)
        // A piece of every width on both sides of the cut: control, bidi
        // and quote characters, none of them whole-or-cut by accident.
        let pieces = ["\u{202E}", "\u{7}", "\"", "\\", "\u{10FFFF}", "e\u{301}", "a"]
        for piece in pieces {
            for filler in 40..<70 {
                let id = String(repeating: "y", count: filler) + String(repeating: piece, count: 30)
                let shown = SettingsModel.shownPluginID(id)
                XCTAssertLessThanOrEqual(shown.count, cap + 2, shown)
                XCTAssertTrue(
                    shown.unicodeScalars.allSatisfy { (0x20...0x7E).contains($0.value) || $0 == "…" }, shown)
                Self.assertWholeEscapes(String(shown.dropFirst().dropLast()), shown)
            }
        }
    }

    /// Every backslash in `text` starts a whole escape: a `\u{…}`, or an
    /// escaped quote or backslash; the ellipsis only follows or precedes one.
    private static func assertWholeEscapes(_ text: String, _ shown: String, line: UInt = #line) {
        let rest = text.replacing(/\\u\{[0-9a-f]+\}/, with: "").replacing(/\\["\\]/, with: "")
        XCTAssertFalse(rest.contains("\\"), "a cut escape in \(shown)", line: line)
    }

    /// The refusal is true at exactly the cap and past it, where the file
    /// lists more than Tessera reads, and it does not say how many are listed.
    func testTheFullRefusalHoldsAtTheCapAndPastIt() throws {
        let cap = TesseraPlugin.maxEnabled
        for count in [cap, cap + 1] {
            let listed = (0..<count).map { "x.\($0)" }
            let (model, _, writer, _) = try fakeModel(enabled: listed, plugins: [Self.plugin("dev.a")])
            PluginsPane.enabledBinding(model, "dev.a").wrappedValue = true
            XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal, "\(count)")
            XCTAssertEqual(writer.calls, [], "\(count)")
        }
        XCTAssertFalse(SettingsModel.pluginsFullRefusal.contains("lists \(cap)"), "true of any number listed")
        XCTAssertTrue(SettingsModel.pluginsFullRefusal.contains("first \(cap) ids"))
    }

    /// Turning on an id whose other spelling (NFD for NFC) is listed adds
    /// it: for the host, which compares bytes, it is another id.
    func testTurningOnAnIdListedOnlyInAnotherSpellingAddsIt() async throws {
        let nfd = "cafe\u{301}", nfc = "caf\u{E9}"
        let (model, _, writer, _) = try fakeModel(enabled: [nfd], plugins: [Self.plugin(nfc)])
        model.setPlugin(nfc, enabled: true)
        try await waitUntil("written") { !writer.calls.isEmpty }
        guard let written = writer.calls.first?.value?.array?.compactMap(\.string), writer.calls.count == 1 else {
            return XCTFail("one write, got \(writer.calls)")
        }
        XCTAssertEqual(written.map { Array($0.utf8) }, [Array(nfd.utf8), Array(nfc.utf8)])
    }

    /// At `TesseraPlugin.maxEnabled` ids a toggle turned on writes
    /// nothing and says why; turning one off still writes, and a hand edit
    /// that makes room clears the refusal (#489 48b B1, stand-ins).
    func testAtTheCapTurningOnIsRefusedAndOffStillWrites() async throws {
        let full = ["dev.b"] + (1..<TesseraPlugin.maxEnabled).map { "x.\($0)" }
        let (model, source, writer, _) = try fakeModel(enabled: full, plugins: [Self.plugin("dev.a")])
        let toggle = PluginsPane.enabledBinding(model, "dev.a")
        toggle.wrappedValue = true
        XCTAssertFalse(toggle.wrappedValue)
        XCTAssertFalse(model.pluginsChanging)
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal)
        XCTAssertNil(model.pluginsSkipWarning, "64 ids: none ignored")
        try Self.apply(source, model, generation: 1, enabled: full, plugins: [Self.plugin("dev.a")])
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal, "still full")
        try Self.apply(source, model, generation: 2, enabled: Array(full.dropLast()), plugins: [Self.plugin("dev.a")])
        XCTAssertNil(model.message(SettingsModel.pluginsKey), "room made by hand")
        XCTAssertEqual(writer.calls, [])

        // Full again, and one turned off: that writes.
        let plugins = [Self.plugin("dev.a"), Self.plugin("dev.b", enabled: true, state: .active)]
        try Self.apply(source, model, generation: 3, enabled: full, plugins: plugins)
        toggle.wrappedValue = true
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal)
        PluginsPane.enabledBinding(model, "dev.b").wrappedValue = false
        try await waitUntil("written") { !writer.calls.isEmpty }
        XCTAssertEqual(
            writer.calls, [.init(key: SettingsModel.pluginsKey, value: Self.listed(Array(full.dropFirst())))],
            "at the cap, turning one off writes the list without it")
    }

    /// 25.2 adversarial M1: at the cap, a hand edit that swaps ids keeps 64
    /// listed, and the refusal, about the list before it, goes.
    func testAHandEditOfAFullListTakesBackTheRefusal() throws {
        let full = (0..<TesseraPlugin.maxEnabled).map { "x.\($0)" }
        let plugin = Self.plugin("dev.a")
        let (model, source, writer, _) = try fakeModel(enabled: full, plugins: [plugin])
        let toggle = PluginsPane.enabledBinding(model, "dev.a")
        toggle.wrappedValue = true
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal)
        try Self.apply(source, model, generation: 1, enabled: full, plugins: [plugin])
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal, "the same list")
        let swapped = Array(full.dropLast()) + ["dev.a"]
        try Self.apply(source, model, generation: 2, enabled: swapped, plugins: [plugin])
        XCTAssertEqual(model.value(SettingsModel.pluginsKey)?.array?.count, TesseraPlugin.maxEnabled)
        XCTAssertNil(model.message(SettingsModel.pluginsKey), "another list, as full")
        XCTAssertEqual(writer.calls, [])
    }

    /// 25.2: reads that keep failing do not set ``pluginsUnreadable`` again
    /// (Swift 6.1's `@Observable` notifies on equal values).
    func testFailedReadsAfterTheFirstDoNotRedraw() throws {
        let (model, source, _, _) = try fakeModel(enabled: [], plugins: [])
        source.pluginList = nil
        model.readPluginsAgain()
        XCTAssertTrue(model.pluginsUnreadable)
        let changes = ChangeCount()
        withObservationTracking {
            _ = model.pluginsUnreadable
        } onChange: {
            changes.value += 1
        }
        model.readPluginsAgain()
        XCTAssertEqual(changes.value, 0)
    }

    /// The plugins read again while one settles and read the same are not
    /// set again, nor is whether they could be read: the pane, which reads
    /// both, is not redrawn for them (#489 48b m2).
    func testASettleReadThatChangesNothingDoesNotRedraw() throws {
        let stuck = [Self.plugin("dev.a", enabled: true, state: .disabled)]
        let (model, source, _, clock) = try fakeModel(enabled: ["dev.a"], plugins: stuck)
        let changes = ChangeCount()
        func observe() {
            withObservationTracking {
                _ = model.plugins
                _ = model.pluginsUnreadable
            } onChange: {
                changes.value += 1
            }
        }
        observe()
        clock.advance(by: SettingsModel.pluginSettleInterval * 5)
        model.readPluginsAgain()
        XCTAssertEqual(changes.value, 0, "the same plugins read again")
        source.pluginList = [Self.plugin("dev.a", enabled: true, state: .active)]
        clock.advance(by: SettingsModel.pluginSettleInterval)
        XCTAssertEqual(changes.value, 1, "a change is shown")
        XCTAssertEqual(model.plugins.first?.state, .active)
    }

    /// With no core the pane shows no plugins and says they could not be
    /// read, also from a read while one settles (#489 48b adversarial m4).
    func testWithNoCoreThePluginsCannotBeRead() throws {
        let (model, _, _, clock) = try fakeModel(
            enabled: ["dev.a"], plugins: [Self.plugin("dev.a", enabled: true, state: .disabled)])
        XCTAssertEqual(model.plugins.count, 1)
        XCTAssertFalse(model.pluginsUnreadable)
        sources = []
        clock.advance(by: SettingsModel.pluginSettleInterval)
        XCTAssertTrue(model.pluginsUnreadable, "a settle read with the core gone")
        XCTAssertEqual(model.plugins, [])
        let (other, _, _, _) = try fakeModel(enabled: [], plugins: [Self.plugin("dev.b", enabled: false)])
        sources = []
        other.reload()
        XCTAssertTrue(other.pluginsUnreadable, "a reload with the core gone")
        XCTAssertEqual(other.plugins, [])
        XCTAssertNotNil(other.loadError)
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

    private static let hello = "dev.tessera.hello"

    /// `[plugins] enabled` as TOML, one id per line.
    private static func enabledTOML(_ ids: [String]) -> String {
        "# mine\n[plugins]\nenabled = [\n" + ids.map { "  \"\($0)\",\n" }.joined() + "]\n"
    }

    /// At 64 ids Hello's toggle is refused with the pane's message, the
    /// file keeps its bytes and nothing waits; with 65 the pane's skip
    /// warning shows, and Hello, the 65th, shows off as the host has it
    /// (#489 48b B1). On the app's core.
    func testAtSixtyFourIdsTheToggleIsRefusedAndSixtyFiveShowTheSkipWarning() async throws {
        let unknown = (0..<TesseraPlugin.maxEnabled).map { "x.\($0)" }
        let config = Self.enabledTOML(unknown)
        let controller = try openPlugins(config: config)
        let model = controller.model
        let toggle = PluginsPane.enabledBinding(model, Self.hello)
        XCTAssertNil(model.pluginsSkipWarning)
        toggle.wrappedValue = true
        XCTAssertFalse(toggle.wrappedValue)
        XCTAssertFalse(model.pluginsChanging)
        XCTAssertFalse(model.isWriting)
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(try file(), config, "nothing written")

        // 65: Hello listed past the host's cap.
        try Self.enabledTOML(unknown + [Self.hello])
            .write(to: try XCTUnwrap(dir).appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the 65 ids applied") { model.pluginsSkipWarning != nil }
        XCTAssertEqual(
            model.pluginsSkipWarning, "[plugins] enabled lists more than 64 ids; the rest are ignored (1 id ignored).")
        let hello = try XCTUnwrap(model.plugins.first { $0.id == Self.hello })
        XCTAssertFalse(hello.enabled, "the host ignores it")
        XCTAssertFalse(toggle.wrappedValue, "shown as the host has it")
        XCTAssertEqual(PluginsPane.status(model, hello), "Off")
        toggle.wrappedValue = true
        XCTAssertEqual(model.message(SettingsModel.pluginsKey), SettingsModel.pluginsFullRefusal)
        XCTAssertFalse(model.pluginsChanging)
    }

    /// A wrong-typed `[plugins] enabled` (a string): the toggle shows the
    /// core's refusal (the loader's error, sanitised) and the file keeps
    /// what the user wrote (#489 48b adversarial m2, #449).
    func testAWrongTypedEnabledShowsTheCoresRefusal() async throws {
        let config = "# mine\n[plugins]\nenabled = \"\(Self.hello)\" # by hand\n"
        let controller = try openPlugins(config: config)
        let model = controller.model
        PluginsPane.enabledBinding(model, Self.hello).wrappedValue = true
        try await waitUntil("refused") { !model.isWriting && model.message(SettingsModel.pluginsKey) != nil }
        let message = try XCTUnwrap(model.message(SettingsModel.pluginsKey))
        print("wrong-typed enabled refusal: \(message)")
        XCTAssertTrue(message.contains("invalid type"), message)
        XCTAssertFalse(model.pluginsChanging)
        XCTAssertEqual(try file(), config, "what the user wrote is kept")
    }

    /// Settings closed while a toggle's edit waits to apply keeps its model
    /// (#489 48b m1): opened again, the app shows that model, still locked,
    /// not one of the configuration from before the edit; it goes once the
    /// edit applies. Opened and closed as the app does; on a manual clock,
    /// so only the apply can release it. Nothing awaited from the toggle to
    /// the reopen, so the watch's apply (on the main actor) cannot come
    /// first.
    func testClosingSettingsDuringAPluginChangeKeepsTheModelUntilItLands() async throws {
        let clock = ManualClock()
        appDelegate.settingsClock = clock
        let controller = try openPlugins(config: "[plugins]\nenabled = []\n")
        let model = controller.model
        model.setPlugin(Self.hello, enabled: true)
        XCTAssertTrue(model.pluginsChanging)
        controller.close()
        XCTAssertNil(appDelegate.settingsWindowController)
        XCTAssertTrue(appDelegate.waitingSettingsModel === model, "kept while the edit waits")
        MainMenu.run(command: "settings")
        let reopened = try XCTUnwrap(appDelegate.settingsWindowController)
        XCTAssertTrue(reopened.model === model, "the model that made the edit")
        XCTAssertTrue(reopened.model.pluginsChanging, "still locked")
        XCTAssertTrue(PluginsPane.enabledBinding(reopened.model, Self.hello).wrappedValue)
        reopened.close()
        XCTAssertTrue(appDelegate.waitingSettingsModel === model)
        try await waitUntil("applied and let go") { appDelegate.waitingSettingsModel == nil }
        XCTAssertFalse(model.pluginsChanging)
        let written = try file()
        XCTAssertTrue(written.contains("\"\(Self.hello)\""), written)
    }

    /// A configuration applied that does not yet hold the closed window's
    /// plugin edit keeps its model, and the one that holds it lets it go.
    /// The applies are real (the app's core, an edit of its file); the
    /// model is a stand-in, so the test decides what each reads.
    func testTheKeptModelStaysUntilAnApplyHoldsItsPluginEdit() async throws {
        try openPlugins(config: "[plugins]\nenabled = []\n").close()
        XCTAssertNil(appDelegate.waitingSettingsModel)
        let (model, source, writer, _) = try fakeModel(enabled: [], plugins: [Self.plugin("dev.a")])
        model.setPlugin("dev.a", enabled: true)
        try await waitUntil("written") { !writer.calls.isEmpty }
        XCTAssertTrue(model.pluginsChanging)
        appDelegate.waitingSettingsModel = model
        let config = try XCTUnwrap(dir).appendingPathComponent("config.toml")

        // The generation the edit was queued at: not an apply after it.
        let reads = source.pluginReads
        try "[appearance]\nfont_size = 15.0\n".write(to: config, atomically: true, encoding: .utf8)
        try await waitUntil("the apply reached the kept model") { source.pluginReads > reads }
        XCTAssertTrue(model.pluginsChanging, "not applied yet")
        XCTAssertTrue(appDelegate.waitingSettingsModel === model, "kept: its edit still waits")

        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.config(enabled: ["dev.a"]))
        source.pluginList = [Self.plugin("dev.a", enabled: true, state: .active)]
        try "[appearance]\nfont_size = 16.0\n".write(to: config, atomically: true, encoding: .utf8)
        try await waitUntil("let go") { appDelegate.waitingSettingsModel == nil }
        XCTAssertFalse(model.pluginsChanging)
    }

    /// The model a closed window left waiting is freed once its edit lands.
    /// As the app does it, on its core; on a manual clock, so only the
    /// apply can release it.
    ///
    /// Opened, shown for a pass of the run loop, then edited and closed,
    /// each step in a pool of its own (as
    /// ``testClosingTheWindowWhileHelloSettlesFreesTheControllerAndModel``).
    /// Opened and closed in one pass, the model sometimes outlived the
    /// test (#598, about one run in eight to ten under load): AppKit lets
    /// go of the panes' tab items on a later pass through an autoreleased
    /// binding, which in an async test can land in XCTest's wait's pool,
    /// kept until the test ends. Not the app's: ADR 0044, #604 amendment.
    func testTheKeptModelIsFreedOnceItsEditLands() async throws {
        appDelegate.settingsClock = ManualClock()
        let opened = try autoreleasepool {
            let controller = try openPlugins(config: "[plugins]\nenabled = []\n")
            controller.window?.contentView?.layoutSubtreeIfNeeded()
            return Opened(controller: controller, model: controller.model, window: nil)
        }
        try await Task.sleep(for: .milliseconds(100))
        try autoreleasepool {
            let controller = try XCTUnwrap(opened.controller)
            controller.model.setPlugin(Self.hello, enabled: true)
            XCTAssertTrue(controller.model.pluginsChanging)
            controller.close()
            XCTAssertTrue(appDelegate.waitingSettingsModel === controller.model, "kept while the edit waits")
        }
        XCTAssertNotNil(opened.model, "kept while the edit waits")
        try await waitUntil("applied and let go") { appDelegate.waitingSettingsModel == nil }
        try await waitUntil("the model is freed") { autoreleasepool { opened.model == nil } }
        XCTAssertNil(opened.model, "nothing keeps it once its edit landed")
    }

    /// An id starting with U+FEFF is another id for the host, and the pane
    /// reads it as one: the toggle follows the host, the footer shows it,
    /// and a toggle's write keeps it byte for byte (#489). On the app's core.
    func testAnIdStartingWithAByteOrderMarkStaysAnotherId() async throws {
        let marked = "\u{FEFF}\(Self.hello)"
        let controller = try openPlugins(config: "[plugins]\nenabled = [\"\(marked)\"]\n")
        let model = controller.model
        let toggle = PluginsPane.enabledBinding(model, Self.hello)
        let hello = try XCTUnwrap(model.plugins.first { $0.id == Self.hello })
        XCTAssertFalse(hello.enabled, "the host does not read it as Hello")
        XCTAssertFalse(toggle.wrappedValue)
        XCTAssertEqual(PluginsPane.status(model, hello), "Off")
        XCTAssertEqual(model.unknownEnabledPlugins, [#""\u{feff}dev.tessera.hello""#])

        toggle.wrappedValue = true
        try await waitUntil("applied") {
            !model.pluginsChanging && model.plugins.first { $0.id == Self.hello }?.enabled == true
        }
        let listed = try XCTUnwrap(model.value(SettingsModel.pluginsKey)?.array?.compactMap(\.string))
        XCTAssertEqual(listed.map { Array($0.utf8) }, [Array(marked.utf8), Array(Self.hello.utf8)])
        XCTAssertTrue(try file().contains(marked), "the id in the file keeps its mark")
    }

    /// ⌘, onto an open Settings window reads the plugins again: one that
    /// settled after the reads stopped shows where it stands (#489 48b
    /// m6). Opened and brought forward as the menu does, nothing posted;
    /// on a stand-in model the app opens (a plugin of the app's core
    /// settles before any read the test could race) on a manual clock, so
    /// no read while it settles runs.
    func testSettingsOpenedAgainReadsThePluginsAgain() async throws {
        try openPlugins(config: "[plugins]\nenabled = []\n").close()
        XCTAssertNil(appDelegate.waitingSettingsModel)
        let (model, source, _, _) = try fakeModel(
            enabled: ["dev.a"], plugins: [Self.plugin("dev.a", enabled: true, state: .disabled)])
        appDelegate.waitingSettingsModel = model
        MainMenu.run(command: "settings")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        XCTAssertTrue(controller.model === model)
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(model.plugins.first)), "Starting…")
        // Started once the reads stopped: nothing reads it on its own.
        source.pluginList = [Self.plugin("dev.a", enabled: true, state: .active)]
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(model.plugins.first)), "Starting…")
        MainMenu.run(command: "settings")
        XCTAssertTrue(appDelegate.settingsWindowController === controller, "the open window brought forward")
        XCTAssertEqual(PluginsPane.status(model, try XCTUnwrap(model.plugins.first)), "Running", "read again")
    }
}

/// Counts observation changes; the onChange closure runs on the main
/// actor here, where every change in these tests is made.
private final class ChangeCount: @unchecked Sendable {
    var value = 0
}
