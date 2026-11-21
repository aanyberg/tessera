import AppKit
import OSLog
import SwiftUI
import XCTest

@testable import Tessera
import TesseraKit

/// The Settings window (ADR 0044). The model's write rules run against a
/// stand-in writer and a manual clock: writes never on the main thread,
/// sliders coalesced, refusals shown and reverted, a conflict retried once.
/// Every control's key, the profiles, theme import, the window's single
/// instance, its release on close and its opening time run against a core
/// of the test's own, on a `config.toml` in a temporary directory (never
/// the user's).
@MainActor
final class SettingsTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL? {
        willSet { if let dir, dir != newValue { earlierDirs.append(dir) } }
    }
    /// The directories of earlier attempts of a test that tries again.
    private var earlierDirs: [URL] = []
    private var previous: String?
    private var replacedCore = false
    /// The stand-in sources of the test's models, which hold them weakly.
    private var sources: [FakeSource] = []

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertNil(appDelegate.settingsWindowController, "a previous test left Settings open")
        XCTAssertNil(appDelegate.settingsAutosaveName, "tests never save the frame in the user's defaults")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        appDelegate.settingsWindowController?.close()
        XCTAssertNil(appDelegate.settingsWindowController)
        if replacedCore {
            try appDelegate.replaceCore(start: false)
        }
        // An edit of `app_appearance` made the app light; the next test
        // expects the app's default.
        AppDelegate.applyAppAppearance(.dark)
        if let bar = NSApp.mainMenu { MainMenu.setProfiles([], in: bar) }
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        removeTemporaryDirectories()
        sources = []
        appDelegate.opener = WorkspaceOpener()
        appDelegate.configFileCreator = nil
        appDelegate.settingsWaitLimit = .seconds(2)
        appDelegate.settingsClock = SystemSettingsClock()
        appDelegate = nil
    }

    // MARK: Helpers

    /// Removes the test's configuration directories, every attempt's.
    private func removeTemporaryDirectories() {
        for url in earlierDirs + (dir.map { [$0] } ?? []) { try? FileManager.default.removeItem(at: url) }
        earlierDirs = []
    }

    /// Waits up to 10 s for `condition`.
    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    private var configFile: URL {
        get throws { try XCTUnwrap(dir).appendingPathComponent("config.toml") }
    }

    private func file() throws -> String {
        try String(contentsOf: configFile, encoding: .utf8)
    }

    /// `config.toml`'s lines.
    private func lines() throws -> [String] {
        try file().split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    }

    /// Whether `value` is `expected`, a float to within the core's `f32`.
    private static func same(_ value: TesseraConfigValue?, _ expected: TesseraConfigValue) -> Bool {
        if case .float(let float) = expected, case .float(let read)? = value {
            return abs(float - read) < 1e-6
        }
        return value == expected
    }

    /// `config.toml`, for a failure's message.
    private var contents: String {
        (try? file()) ?? "(unreadable)"
    }

    /// Gives the app a core of its own on `config` (and `files`, by path
    /// in the configuration directory) in a fresh temporary directory and
    /// opens Settings on it; returns the window's model.
    @discardableResult
    private func openSettings(config: String, files: [String: String] = [:]) throws -> SettingsModel {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-settings-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        for (path, text) in files {
            let url = dir.appendingPathComponent(path)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try text.write(to: url, atomically: true, encoding: .utf8)
        }
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true
        try appDelegate.replaceCore(start: true)
        MainMenu.run(command: "settings")
        return try XCTUnwrap(appDelegate.settingsWindowController?.model)
    }

    /// The model on stand-ins: `config` is the configuration in effect (as
    /// the core's JSON).
    private func fakeModel(
        _ config: String, writer: FakeWriter = FakeWriter(), clock: any SettingsClock = SystemSettingsClock()
    ) throws -> (SettingsModel, FakeSource, FakeWriter) {
        let source = FakeSource(writer: writer)
        source.snapshot = TesseraConfigSnapshot(generation: 0, config: try Self.json(config))
        sources.append(source)
        let model = SettingsModel(source: source, clock: clock)
        // Real presses in the session must not count as a held mouse.
        model.leftMousePress = { nil }
        return (model, source, writer)
    }

    private static func json(_ text: String) throws -> TesseraConfigValue {
        try TesseraConfigValue.parse(json: Data(text.utf8))
    }

    private static let fakeConfig = """
        {"general": {"default_profile": "one", "confirm_close": true},
         "appearance": {"opacity": 0.85, "theme": "Tessera Dark", "font_size": 13.0},
         "terminal": {"scrollback_lines": 10000},
         "profiles": [{"name": "one", "appearance": null}, {"name": "two", "appearance": {"theme": "Nord"}}]}
        """

    // MARK: Writing (stand-ins)

    func testWritesNeverRunOnTheMainThread() async throws {
        let (model, _, writer) = try fakeModel(Self.fakeConfig)
        model.set("appearance.opacity", .float(0.5))
        model.remove("appearance.font_size")
        model.change("appearance.opacity", .float(0.6))
        model.endChange("appearance.opacity")
        model.addProfile()
        try await waitUntil("the edits ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.count, 4, "\(writer.calls)")
        XCTAssertEqual(writer.mainThreadCalls, 0, "an edit ran on the main thread")
    }

    func testAnEditShowsAtOnceAndTheFilesValueOnceApplied() async throws {
        let (model, source, writer) = try fakeModel(Self.fakeConfig)
        model.set("appearance.opacity", .float(0.5))
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.5), "shown before it is written")
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "appearance.opacity", value: .float(0.5))])
        model.reload()
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.5), "kept until a later configuration applies")
        // The watch applied a file that says otherwise (an editor saved it
        // in between): the file wins.
        source.snapshot = TesseraConfigSnapshot(
            generation: 1, config: try Self.json(Self.fakeConfig.replacingOccurrences(of: "0.85", with: "0.25")))
        model.reload()
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.25))
        model.set("appearance.opacity", .float(0.25))
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 1, "setting the value in effect writes nothing")
    }

    func testFiftySliderChangesInOneSecondWriteAtMostElevenTimesEndingWithTheLast() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.opacity" })
        let binding = field.doubleBinding(.init(model: model, path: field.key))
        // A drag from 0.10 to 0.59, a change every 20 ms.
        for step in 0..<50 {
            if step > 0 { clock.advance(by: .milliseconds(20)) }
            binding.wrappedValue = 0.10 + Double(step) / 100
            XCTAssertEqual(try XCTUnwrap(model.value(field.key)?.double), 0.10 + Double(step) / 100, accuracy: 1e-9)
        }
        field.endEditing(.init(model: model, path: field.key))
        try await waitUntil("the writes ran") { !model.isWriting }
        let writes = writer.calls.filter { $0.key == field.key }
        XCTAssertLessThanOrEqual(writes.count, 11, "\(writes)")
        XCTAssertGreaterThanOrEqual(writes.count, 5, "a drag writes as it goes: \(writes)")
        XCTAssertEqual(writes.last?.value, .float(0.59), "the final value is written")
        clock.advance(by: .seconds(1))
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.filter { $0.key == field.key }.count, writes.count, "nothing after the release")
    }

    /// The blur radius slider of #390, which no pane shows in 0.2.0 (#490):
    /// it returns in 0.3.0 (#491), and its control is kept and tested.
    static let integerSlider = SettingsField(
        key: "appearance.blur_radius", title: "Blur radius", control: .integerSlider(0...64, zeroTitle: "Off"))

    /// #490: Appearance has no blur radius slider, only the blur toggle.
    func testAppearanceHasTheBlurToggleAndNoBlurRadius() {
        let keys = AppearancePane.fields.map(\.key)
        XCTAssertTrue(keys.contains("appearance.blur"), "\(keys)")
        XCTAssertFalse(keys.contains("appearance.blur_radius"), "\(keys)")
        XCTAssertFalse(
            AppearancePane.fields.contains { if case .integerSlider = $0.control { true } else { false } },
            "no integer slider: \(keys)")
    }

    /// An integer slider writes whole numbers within its range, coalesced
    /// like any slider, never a float (which the core refuses for an
    /// integer key).
    func testAnIntegerSliderWritesWholeNumbersInItsRange() async throws {
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: ManualClock())
        let field = Self.integerSlider
        let context = SettingsFieldContext(model: model, path: field.key)
        let binding = field.integerSliderBinding(context)
        for (dragged, written) in [(20.4, 20), (20.6, 21), (90.0, 64), (-3.0, 0)] {
            binding.wrappedValue = dragged
            XCTAssertEqual(model.value(field.key), .integer(Int64(written)), "\(dragged)")
            XCTAssertEqual(binding.wrappedValue, Double(written))
        }
        binding.wrappedValue = 30
        field.endEditing(context)
        try await waitUntil("the writes ran") { !model.isWriting }
        let writes = writer.calls.filter { $0.key == field.key }
        XCTAssertEqual(writes.last?.value, .integer(30), "the final value is written")
        XCTAssertTrue(writes.allSatisfy { if case .integer = $0.value { true } else { false } }, "\(writes)")
    }

    /// #405 m3: a slider's and an integer slider's `onEditingChanged`
    /// (``SettingsField/editingChanged(_:)``, what the row hands SwiftUI)
    /// writes the value held back by the coalescing once the knob is let
    /// go, and nothing as it is taken.
    func testLettingGoOfASliderWritesItsLastValue() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        for (key, values) in [
            ("appearance.opacity", [TesseraConfigValue.float(0.4), .float(0.5), .float(0.6)]),
            ("appearance.blur_radius", [.integer(10), .integer(11), .integer(12)]),
        ] {
            let field = try XCTUnwrap((AppearancePane.fields + [Self.integerSlider]).first { $0.key == key })
            let context = SettingsFieldContext(model: model, path: field.key)
            let editingChanged = field.editingChanged(context)
            editingChanged(true)
            for value in values {
                switch value {
                case .float(let float): field.doubleBinding(context).wrappedValue = float
                case .integer(let integer): field.integerSliderBinding(context).wrappedValue = Double(integer)
                default: XCTFail("\(value)")
                }
            }
            try await waitUntil("the first write ran") { !model.isWriting }
            try await Task.sleep(for: .milliseconds(50))
            XCTAssertNotEqual(
                writer.calls.last { $0.key == key }?.value, values.last, "\(key): held back while dragged")
            editingChanged(true)
            try await Task.sleep(for: .milliseconds(50))
            XCTAssertNotEqual(
                writer.calls.last { $0.key == key }?.value, values.last, "\(key): taking it writes nothing")
            editingChanged(false)
            try await waitUntil("\(key): the last value written") {
                !model.isWriting && writer.calls.last { $0.key == key }?.value == values.last
            }
        }
    }

    func testARefusedEditShowsTheSanitisedWarningAndTheFilesValue() async throws {
        let writer = FakeWriter()
        writer.respond = { call, _ in
            call.key == "appearance.opacity"
                ? TesseraConfigEditError(
                    .refused, message: "appearance.opacity: 2 is out of\n range\u{1B}[31m\u{202E}\u{200B}\u{E000} (0..=1)")
                : nil
        }
        let (model, _, _) = try fakeModel(Self.fakeConfig, writer: writer)
        model.set("appearance.opacity", .float(2))
        XCTAssertEqual(model.value("appearance.opacity"), .float(2))
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.85), "the control shows the file's value again")
        XCTAssertEqual(model.message("appearance.opacity"), "appearance.opacity: 2 is out of range[31m (0..=1)")
        XCTAssertEqual(writer.calls.count, 1)
        // The next edit of the key clears it.
        model.set("appearance.opacity", .float(0.5))
        XCTAssertNil(model.message("appearance.opacity"))
        let long = TesseraConfigEditError(.refused, message: String(repeating: "x", count: 5000))
        XCTAssertEqual(SettingsModel.displayMessage(long).count, SettingsModel.maxMessageLength)
        XCTAssertFalse(SettingsModel.displayMessage(TesseraConfigEditError(.io, message: "\u{7}")).isEmpty)
    }

    func testAConflictReloadsAndRetriesOnceKeepingTheExternalEdit() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeModel(Self.fakeConfig, writer: writer)
        let external = try Self.json(Self.fakeConfig.replacingOccurrences(of: "10000", with: "500"))
        let applied = source.applied
        writer.respond = { call, attempt in
            guard call.key == "appearance.opacity", attempt == 1 else { return nil }
            // An editor saved the file in between: the core refuses to
            // overwrite it; its watch then applies the editor's version.
            applied.value = TesseraConfigSnapshot(generation: 1, config: external)
            return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
        }
        model.set("appearance.opacity", .float(0.5))
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.map(\.key), ["appearance.opacity", "appearance.opacity"], "retried once")
        XCTAssertNil(model.message("appearance.opacity"))
        XCTAssertEqual(model.value("terminal.scrollback_lines"), .integer(500), "reloaded: the external edit stays")
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.5))

        // A second conflict is shown, not retried again.
        writer.respond = { call, _ in
            call.key == "appearance.blur" ? TesseraConfigEditError(.conflict, message: "changed on disk") : nil
        }
        model.set("appearance.blur", .bool(true))
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.filter { $0.key == "appearance.blur" }.count, 2)
        XCTAssertEqual(model.message("appearance.blur"), "changed on disk")
        XCTAssertNil(model.value("appearance.blur"), "the file's value (none)")
    }

    func testDeletingTheDefaultProfileIsRefusedWithAMessage() async throws {
        let (model, _, writer) = try fakeModel(Self.fakeConfig)
        XCTAssertFalse(model.deleteProfile(0))
        XCTAssertEqual(
            model.message(SettingsModel.profilesKey),
            "“one” is the default profile. Choose another default profile in General first.")
        XCTAssertTrue(model.deleteProfile(1))
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "profiles[1]", value: nil)])
    }

    private static let threeProfiles = """
        {"general": {"default_profile": "one"}, "appearance": {"theme": "Tessera Dark"},
         "profiles": [{"name": "one"}, {"name": "two", "command": "/bin/cat"}, {"name": "three"}]}
        """

    /// Profile edits name the profile by its index, which an add or delete
    /// moves once the watch applies it: until then the next add, duplicate,
    /// delete or rename writes nothing (the stand-in's watch applies only
    /// when the test says).
    func testProfileEditsWaitUntilAnAddOrDeleteIsApplied() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles)
        XCTAssertTrue(model.deleteProfile(1))
        XCTAssertFalse(model.deleteProfile(1), "the second delete would remove “three”")
        XCTAssertFalse(model.addProfile())
        XCTAssertFalse(model.duplicateProfile(1))
        model.renameProfile(2, to: "renamed")
        model.setInherits(2, "appearance.theme", false)
        try await waitUntil("the delete ran") { !model.isWriting }
        XCTAssertFalse(model.deleteProfile(1), "written, not yet applied")
        XCTAssertEqual(writer.calls, [.init(key: "profiles[1]", value: nil)])

        // The watch applies the file: the indices are the file's again.
        let deleted = Self.threeProfiles.replacingOccurrences(of: #"{"name": "two", "command": "/bin/cat"}, "#, with: "")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(deleted))
        model.reload()
        XCTAssertEqual(model.profiles.map(\.name), ["one", "three"])
        XCTAssertTrue(model.duplicateProfile(1))
        XCTAssertFalse(model.addProfile(), "the copy is not applied yet")
        try await waitUntil("the copy ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.dropFirst(), [FakeWriter.added("three Copy")])
        XCTAssertEqual(model.takeSelectionRequest(), 2, "where the core put it")
        XCTAssertFalse(model.addProfile(), "written, not yet applied")
    }

    /// The list and the profile's controls are disabled while an add or
    /// delete waits for the watch; a failed one enables them again at once
    /// with its message by the list.
    func testTheProfileListIsLockedUntilAnAddOrDeleteIsAppliedOrFails() async throws {
        let writer = FakeWriter()
        writer.respond = { call, _ in
            call.key == FakeWriter.addKey ? TesseraConfigEditError(.io, message: "disk\u{202E} full") : nil
        }
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        XCTAssertFalse(model.profilesChanging)
        model.set("profiles[2].command", .string("/bin/sh"))
        XCTAssertFalse(model.profilesChanging, "an edit of one profile moves no index")
        XCTAssertTrue(model.addProfile())
        XCTAssertTrue(model.profilesChanging)
        try await waitUntil("the add failed") { !model.isWriting }
        XCTAssertFalse(model.profilesChanging, "a failed add unlocks the list")
        XCTAssertEqual(model.message(SettingsModel.profilesKey), "disk full")

        XCTAssertTrue(model.deleteProfile(2))
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        try await waitUntil("the delete ran") { !model.isWriting }
        model.reload()
        XCTAssertTrue(model.profilesChanging, "written, not yet applied")
        let deleted = Self.threeProfiles.replacingOccurrences(of: #", {"name": "three"}"#, with: "")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(deleted))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
    }

    /// A hand edit that removes a profile above the selected one moves
    /// another profile under the selection: a field that was focused then
    /// keeps nothing of the old profile, so leaving it writes nothing,
    /// whether text was typed (it would land on the next profile) or not
    /// (its empty value would remove the next profile's command).
    func testAFocusedProfileFieldWritesNothingToTheProfileAHandEditMovesUnderIt() async throws {
        let config = """
            {"general": {"default_profile": "A"},
             "profiles": [{"name": "A"}, {"name": "B"}, {"name": "C", "command": "keepme"}]}
            """
        for typed in ["echo FOR_B", nil] {
            let (model, source, writer) = try fakeModel(config)
            let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
            window.isKeyForTest = true
            window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
            defer {
                window.contentView = nil
                window.close()
            }
            window.layoutIfNeeded()
            try await waitUntil("the list shows the profiles") {
                Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
            }
            let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
            Self.click(table, row: 1)
            try await waitUntil("B's fields are shown") { Self.commandField(in: window) != nil }
            let field = try XCTUnwrap(Self.commandField(in: window))
            XCTAssertTrue(window.makeFirstResponder(field))
            if let typed {
                (field.currentEditor() as? NSTextView)?.insertText(
                    typed, replacementRange: NSRange(location: NSNotFound, length: 0))
            }
            try await Task.sleep(for: .milliseconds(50))

            // Another process saves the file without A; the watch applies it.
            let withoutA = config.replacingOccurrences(of: #"{"name": "A"}, "#, with: "")
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(withoutA))
            model.reload()
            try await waitUntil("C is shown at the selection") {
                Self.views(NSTextField.self, in: window.contentView).contains { $0.isEditable && $0.stringValue == "C" }
            }
            window.makeFirstResponder(nil)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(writer.calls, [], "typed \(typed ?? "nothing"): written to C")

            // C's own field still writes to C.
            let next = try XCTUnwrap(Self.commandField(in: window))
            XCTAssertTrue(window.makeFirstResponder(next))
            next.currentEditor()?.selectAll(nil)
            (next.currentEditor() as? NSTextView)?.insertText(
                "echo FOR_C", replacementRange: NSRange(location: NSNotFound, length: 0))
            try await Task.sleep(for: .milliseconds(50))
            window.makeFirstResponder(nil)
            let own = FakeWriter.Call(key: "profiles[1].command", value: .string("echo FOR_C"))
            try await waitUntil("C's edit ran") { !model.isWriting && writer.calls.last == own }
        }
    }

    /// Profiles that share a name, or have none, look alike by name: a
    /// hand edit that moves another of them under the selection still
    /// starts the focused field again, so leaving it (by a click away,
    /// Return or Tab) writes nothing to the profile now at that index.
    func testAFocusedProfileFieldWritesNothingToASameNamedProfileAHandEditMovesUnderIt() async throws {
        struct Case {
            var name: String
            var profiles: String
            var row: Int
            var typed: String?
            var after: String
            /// The selected profile's command after the hand edit.
            var shown: String
        }
        let cases = [
            Case(
                name: "two named alike, swapped",
                profiles: #"[{"name": "Q"}, {"name": "X", "command": "one"}, {"name": "X", "command": "two"}]"#,
                row: 2, typed: "FOR_TWO",
                after: #"[{"name": "Q"}, {"name": "X", "command": "two"}, {"name": "X", "command": "one"}]"#,
                shown: "one"),
            Case(
                name: "two named alike, swapped, nothing typed",
                profiles: #"[{"name": "Q"}, {"name": "X", "command": "one"}, {"name": "X"}]"#,
                row: 2, typed: nil,
                after: #"[{"name": "Q"}, {"name": "X"}, {"name": "X", "command": "one"}]"#, shown: "one"),
            Case(
                name: "the first of two named alike deleted, a third added",
                profiles: #"[{"name": "X", "command": "one"}, {"name": "X", "command": "two"}]"#,
                row: 1, typed: nil,
                after: #"[{"name": "X", "command": "two"}, {"name": "X", "command": "three"}]"#, shown: "three"),
            Case(
                name: "nameless",
                profiles: #"[{"command": "one"}, {"command": "two"}]"#,
                row: 1, typed: "FOR_TWO",
                after: #"[{"command": "two"}, {"command": "three"}]"#, shown: "three"),
        ]
        for test in cases {
            for end in ["click away", "Return", "Tab"] {
                let what = "\(test.name), left by \(end)"
                let (model, source, writer) = try fakeModel(#"{"profiles": \#(test.profiles)}"#)
                let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
                window.isKeyForTest = true
                window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
                defer {
                    window.contentView = nil
                    window.close()
                }
                window.layoutIfNeeded()
                try await waitUntil("the list shows the profiles") {
                    Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == model.profiles.count
                }
                let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
                Self.click(table, row: test.row)
                try await Task.sleep(for: .milliseconds(150))
                let field = try XCTUnwrap(Self.commandField(in: window), what)
                XCTAssertTrue(window.makeFirstResponder(field), what)
                if let typed = test.typed {
                    field.currentEditor()?.selectAll(nil)
                    (field.currentEditor() as? NSTextView)?.insertText(
                        typed, replacementRange: NSRange(location: NSNotFound, length: 0))
                }
                try await Task.sleep(for: .milliseconds(60))

                source.snapshot = TesseraConfigSnapshot(
                    generation: 1, config: try Self.json(#"{"profiles": \#(test.after)}"#))
                model.reload()
                try await Task.sleep(for: .milliseconds(250))
                switch end {
                case "Return": (window.firstResponder as? NSTextView)?.insertNewline(nil)
                case "Tab": (window.firstResponder as? NSTextView)?.insertTab(nil)
                default: window.makeFirstResponder(nil)
                }
                try await Task.sleep(for: .milliseconds(150))
                try await waitUntil("idle") { !model.isWriting }
                XCTAssertEqual(writer.calls, [], what)
                XCTAssertEqual(Self.commandField(in: window)?.stringValue, test.shown, what)
            }
        }
    }

    /// This window's own rename, applied while the next field is being
    /// typed in, leaves that field alone: Tab from Name into Command and
    /// both edits are written.
    func testARenameAppliedWhileTheNextProfileFieldIsTypedInKeepsThatField() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles)
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
        window.isKeyForTest = true
        window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
        defer {
            window.contentView = nil
            window.close()
        }
        window.layoutIfNeeded()
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: 1)
        try await waitUntil("two's fields are shown") { Self.commandField(in: window) != nil }
        let name = try XCTUnwrap(
            Self.views(NSTextField.self, in: window.contentView).first { $0.isEditable && $0.stringValue == "two" })
        XCTAssertTrue(window.makeFirstResponder(name))
        name.currentEditor()?.selectAll(nil)
        (name.currentEditor() as? NSTextView)?.insertText(
            "zwei", replacementRange: NSRange(location: NSNotFound, length: 0))
        try await Task.sleep(for: .milliseconds(50))
        (window.firstResponder as? NSTextView)?.insertTab(nil)
        try await Task.sleep(for: .milliseconds(50))
        let command = try XCTUnwrap(Self.commandField(in: window))
        XCTAssertNotNil(command.currentEditor(), "Tab moves into Command")
        command.currentEditor()?.selectAll(nil)
        (command.currentEditor() as? NSTextView)?.insertText(
            "/bin/sh", replacementRange: NSRange(location: NSNotFound, length: 0))
        let rename = FakeWriter.Call(key: "profiles[1].name", value: .string("zwei"))
        try await waitUntil("the rename ran") { !model.isWriting && writer.calls == [rename] }

        // The watch applies the rename while Command is still being typed in.
        let renamed = Self.threeProfiles.replacingOccurrences(of: #""two""#, with: #""zwei""#)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(renamed))
        model.reload()
        try await Task.sleep(for: .milliseconds(250))
        XCTAssertNotNil(command.currentEditor(), "Command keeps the focus")
        window.makeFirstResponder(nil)
        let both = [rename, FakeWriter.Call(key: "profiles[1].command", value: .string("/bin/sh"))]
        try await waitUntil("both edits ran") { !model.isWriting && writer.calls == both }
    }

    /// The profile controls start again when a hand edit changes a
    /// profile, whatever it is named, but not for this window's own edits
    /// of one (a rename, a command, an Inherit toggle, a slider, written or
    /// still moving: the field keeps the focus) nor for an edit outside
    /// the profiles.
    func testTheProfileListGenerationMovesOnlyForHandEditsOfTheProfiles() async throws {
        let (model, source, _) = try fakeModel(Self.threeProfiles, clock: ManualClock())
        let start = model.profileListGeneration
        var generation: UInt64 = 0
        var config = Self.threeProfiles
        let apply = { (edit: (String) -> String) in
            config = edit(config)
            generation += 1
            source.snapshot = TesseraConfigSnapshot(generation: generation, config: try Self.json(config))
            model.reload()
        }
        let written = { try await self.waitUntil("written") { !model.isWriting } }

        model.renameProfile(1, to: "zwei")
        try await written()
        try apply { $0.replacingOccurrences(of: #""two""#, with: #""zwei""#) }
        XCTAssertEqual(model.profileListGeneration, start, "this window's rename")
        model.set("profiles[1].command", .string("/bin/sh"))
        try await written()
        try apply { $0.replacingOccurrences(of: "/bin/cat", with: "/bin/sh") }
        XCTAssertEqual(model.profileListGeneration, start, "this window's command")
        model.setInherits(2, "appearance.theme", false)
        try await written()
        try apply {
            $0.replacingOccurrences(
                of: #"{"name": "three"}"#, with: #"{"name": "three", "appearance": {"theme": "Tessera Dark"}}"#)
        }
        XCTAssertEqual(model.profileListGeneration, start, "this window's Inherit toggle")
        // A slider: its first value written and applied while it still moves.
        model.change("profiles[0].appearance.opacity", .float(0.3))
        model.change("profiles[0].appearance.opacity", .float(0.5))
        try await written()
        try apply {
            $0.replacingOccurrences(
                of: #"{"name": "one"}"#, with: #"{"name": "one", "appearance": {"opacity": 0.30000001192092896}}"#)
        }
        XCTAssertEqual(model.profileListGeneration, start, "this window's slider, moving")
        model.endChange("profiles[0].appearance.opacity")
        try await written()
        try apply { $0.replacingOccurrences(of: "0.30000001192092896", with: "0.5") }
        XCTAssertEqual(model.profileListGeneration, start, "this window's slider, released")
        try apply { $0.replacingOccurrences(of: #""default_profile": "one""#, with: #""default_profile": "three""#) }
        XCTAssertEqual(model.profileListGeneration, start, "a key outside the profiles")

        try apply { $0.replacingOccurrences(of: "/bin/sh", with: "/bin/zsh") }
        XCTAssertEqual(model.profileListGeneration, start + 1, "a hand edit of a profile's command")
        try apply { $0.replacingOccurrences(of: #""zwei""#, with: #""drei""#) }
        XCTAssertEqual(model.profileListGeneration, start + 2, "a hand rename")
        try apply { $0.replacingOccurrences(of: #""drei""#, with: #""one""#) }
        XCTAssertEqual(model.profileListGeneration, start + 3, "a hand rename to a name taken")
        try apply { _ in
            #"{"profiles": [{"name": "one", "command": "/bin/zsh"}, "#
                + #"{"name": "one", "appearance": {"opacity": 0.5}}, "#
                + #"{"name": "three", "appearance": {"theme": "Tessera Dark"}}]}"#
        }
        XCTAssertEqual(model.profileListGeneration, start + 4, "a hand swap of two named alike")
        try apply { $0.replacingOccurrences(of: #"{"name": "three", "#, with: "{") }
        XCTAssertEqual(model.profileListGeneration, start + 5, "a hand edit that leaves a profile nameless")
        try apply { $0.replacingOccurrences(of: #", {"appearance": {"theme": "Tessera Dark"}}"#, with: "") }
        XCTAssertEqual(model.profileListGeneration, start + 6, "a hand delete")
    }

    /// On the core: two profiles named alike, swapped by hand, are another
    /// profile at each index.
    func testAHandSwapOfTwoProfilesNamedAlikeIsSeenOnTheCore() async throws {
        let profile = { (command: String) in "[[profiles]]\nname = \"zsh\"\ncommand = \"\(command)\"\n" }
        let model = try openSettings(config: profile("/bin/zsh") + "\n" + profile("/bin/fish"))
        XCTAssertEqual(model.profiles.map(\.name), ["zsh", "zsh"])
        let generation = model.snapshot.generation
        let moved = model.profileListGeneration
        try (profile("/bin/fish") + "\n" + profile("/bin/zsh")).write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("the watch applied the hand edit") { model.snapshot.generation > generation }
        XCTAssertEqual(model.profileListGeneration, moved + 1)
    }

    private static func views<T: NSView>(_ type: T.Type, in view: NSView?) -> [T] {
        guard let view else { return [] }
        return ((view as? T).map { [$0] } ?? []) + view.subviews.flatMap { views(type, in: $0) }
    }

    private static func commandField(in window: NSWindow) -> NSTextField? {
        views(NSTextField.self, in: window.contentView).first { $0.placeholderString == "Login shell" && $0.isEditable }
    }

    /// A click on `row` of the list, as the user makes it (the list is an
    /// outline view that selects in its own mouse tracking).
    private static func click(_ table: NSTableView, row: Int) {
        let rect = table.rect(ofRow: row)
        let location = table.convert(NSPoint(x: rect.midX, y: rect.midY), to: nil)
        let number = table.window?.windowNumber ?? 0
        let time = ProcessInfo.processInfo.systemUptime
        func event(_ type: NSEvent.EventType, _ offset: TimeInterval) -> NSEvent? {
            NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: [], timestamp: time + offset, windowNumber: number,
                context: nil, eventNumber: 0, clickCount: 1, pressure: type == .leftMouseDown ? 1 : 0)
        }
        // The mouse-down tracking loop ends at the mouse-up queued first.
        if let up = event(.leftMouseUp, 0.05) { NSApp.postEvent(up, atStart: false) }
        if let down = event(.leftMouseDown, 0) { table.mouseDown(with: down) }
    }

    func testProfileThemeAndPluginNamesAreShownSanitised() throws {
        let hostile = "evil\u{202E}txt.exe\u{1B}]0;title\u{7}\u{200B}\n\u{E000}"
        let (model, source, _) = try fakeModel(
            """
            {"general": {"default_profile": "x"}, "appearance": {"theme": "gone\\u202e"},
             "profiles": [{"name": "evil\\u202etxt.exe\\u001b]0;title\\u0007\\u200b\\n\\ue000"}, {"name": "\\u200b"}],
             "plugins": {"enabled": ["p\\u202e"]}}
            """)
        source.themeList = [TesseraTheme(name: hostile, id: "evil", source: .user)]
        model.reload()
        let clean = { (text: String) in
            !text.unicodeScalars.contains {
                [.control, .format, .privateUse].contains($0.properties.generalCategory)
            }
        }
        XCTAssertEqual(model.profiles.map(\.name).first, hostile, "the name as written, for the key paths")
        XCTAssertEqual(model.profiles.map(\.title), ["eviltxt.exe]0;title", "Unnamed Profile"])
        for field in GeneralPane.fields + AppearancePane.fields + AdvancedPane.fields {
            for choice in field.choices(model) ?? [] {
                XCTAssertTrue(clean(choice.title), "\(field.key): \(choice.title.debugDescription)")
            }
        }
        let themes = try XCTUnwrap(AppearancePane.theme.choices(model))
        XCTAssertEqual(themes, [.init(value: "evil", title: "eviltxt.exe]0;title")])
        // A value not among the choices shows as an extra item, sanitised.
        XCTAssertEqual(MainMenu.profileTitle("gone\u{202E}"), "gone")
    }

    // MARK: The window on a core

    /// One control's edit and what it writes.
    private struct Edit {
        var key: String
        var value: TesseraConfigValue
        /// The line `config.toml` has for it.
        var line: String
    }

    /// Sets `field` to `value` the way its control does.
    private func apply(_ field: SettingsField, _ value: TesseraConfigValue, _ context: SettingsFieldContext) {
        switch (field.control, value) {
        case (.toggle, .bool(let bool)):
            field.boolBinding(context).wrappedValue = bool
        case (.slider, .float(let float)), (.stepper, .float(let float)):
            field.doubleBinding(context).wrappedValue = float
            field.endEditing(context)
        case (.integerSlider, .integer(let integer)):
            field.integerSliderBinding(context).wrappedValue = Double(integer)
            field.endEditing(context)
        case (.integer, .integer(let integer)):
            field.integerBinding(context).wrappedValue = integer
        case (_, .string(let string)):
            field.stringBinding(context).wrappedValue = string
        default:
            XCTFail("no control sets \(field.key) to \(value)")
        }
    }

    func testEveryControlWritesItsKeyAndAnExternalEditIsShown() async throws {
        let model = try openSettings(
            config: """
                [general]
                default_profile = "one"
                [[profiles]]
                name = "one"
                [[profiles]]
                name = "two"
                """)
        let theme = try XCTUnwrap(model.themes.first { $0.name == "Dracula" }).id
        let font = try XCTUnwrap(FontFamilies.fixedPitch.first { $0 != "Menlo" })
        let edits = [
            Edit(key: "general.default_profile", value: .string("two"), line: #"default_profile = "two""#),
            Edit(key: "general.confirm_close", value: .bool(false), line: "confirm_close = false"),
            Edit(key: "general.remote_indicator", value: .string("off"), line: #"remote_indicator = "off""#),
            Edit(key: "appearance.app_appearance", value: .string("light"), line: #"app_appearance = "light""#),
            Edit(key: "appearance.theme", value: .string(theme), line: "theme = \"\(theme)\""),
            Edit(key: "appearance.font_family", value: .string(font), line: "font_family = \"\(font)\""),
            Edit(key: "appearance.font_size", value: .float(15), line: "font_size = 15.0"),
            Edit(key: "appearance.opacity", value: .float(0.5), line: "opacity = 0.5"),
            Edit(key: "appearance.blur", value: .bool(true), line: "blur = true"),
            Edit(key: "appearance.cursor_shape", value: .string("beam"), line: #"cursor_shape = "beam""#),
            Edit(key: "appearance.cursor_blink", value: .bool(true), line: "cursor_blink = true"),
            Edit(key: "appearance.dim_inactive_panes", value: .float(0.3), line: "dim_inactive_panes = 0.3"),
            Edit(key: "terminal.scrollback_lines", value: .integer(2000), line: "scrollback_lines = 2000"),
            Edit(key: "terminal.shell_integration", value: .string("off"), line: #"shell_integration = "off""#),
            Edit(key: "terminal.clipboard_write", value: .bool(false), line: "clipboard_write = false"),
        ]
        let fields = GeneralPane.fields + AppearancePane.fields + AdvancedPane.fields
        XCTAssertEqual(Set(edits.map(\.key)), Set(fields.map(\.key)), "every control of the panes is tested")
        for edit in edits {
            let field = try XCTUnwrap(fields.first { $0.key == edit.key })
            XCTAssertNotEqual(model.value(edit.key), edit.value, "\(edit.key) starts elsewhere")
            apply(field, edit.value, .init(model: model, path: edit.key))
            try await waitUntil("\(edit.key) written") { !model.isWriting }
            XCTAssertNil(model.message(edit.key), edit.key)
            XCTAssertTrue(try lines().contains(edit.line), "\(edit.line) in\n\(contents)")
        }
        try await waitUntil("the core applied every edit") {
            edits.allSatisfy { Self.same(model.snapshot[$0.key], $0.value) }
        }
        for edit in edits {
            XCTAssertTrue(Self.same(model.snapshot[edit.key], edit.value), "the core reads \(edit.key) back")
        }

        // An edit of the file itself (an editor) shows once applied.
        let edited = try file().replacingOccurrences(of: "opacity = 0.5", with: "opacity = 0.25")
        try edited.write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("the window shows the external edit") {
            model.value("appearance.opacity") == .float(0.25)
        }
    }

    func testARefusedValueWritesNothingAndTheControlShowsTheFilesValue() async throws {
        let model = try openSettings(config: "[terminal]\nscrollback_lines = 2000\n")
        let original = try file()
        let field = try XCTUnwrap(AdvancedPane.fields.first { $0.key == "terminal.scrollback_lines" })
        apply(field, .integer(-5), .init(model: model, path: field.key))
        try await waitUntil("the edit ran") { !model.isWriting }
        let message = try XCTUnwrap(model.message(field.key), "the loader's warning next to the control")
        XCTAssertFalse(message.isEmpty)
        XCTAssertEqual(model.value(field.key), .integer(2000))
        XCTAssertEqual(try file(), original, "nothing written")
    }

    func testProfilesAddDuplicateRenameDeleteAndInheritRoundTrip() async throws {
        let model = try openSettings(
            config: """
                [general]
                default_profile = "two"
                [[profiles]]
                name = "one"
                [[profiles]]
                name = "two"
                command = "/bin/cat"
                """)
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let submenu = {
            (MainMenu.item(forCommand: MainMenu.newTabWithProfile, in: bar)?.submenu?.items ?? []).map(\.title)
        }
        XCTAssertEqual(model.profiles.map(\.title), ["one", "two"])

        XCTAssertTrue(model.addProfile())
        try await waitUntil("added") { !model.isWriting }
        XCTAssertEqual(model.takeSelectionRequest(), 2)
        XCTAssertTrue(try file().contains(#"name = "New Profile""#), contents)
        try await waitUntil("the window lists it") { model.profiles.map(\.name) == ["one", "two", "New Profile"] }

        try await waitUntil("the add applied") { !model.profilesChanging }
        XCTAssertTrue(model.duplicateProfile(1))
        try await waitUntil("duplicated") { !model.isWriting }
        XCTAssertEqual(model.takeSelectionRequest(), 3)
        try await waitUntil("the copy is applied with the command") {
            !model.profilesChanging && model.profiles.map(\.name) == ["one", "two", "New Profile", "two Copy"]
                && model.value("profiles[3].command") == .string("/bin/cat")
        }

        // Renaming the default profile renames the default.
        let name = try XCTUnwrap(ProfilesPane.fields.first { $0.key == "name" })
        apply(name, .string("deux"), .init(model: model, path: "profiles[1].name", profile: 1))
        try await waitUntil("renamed") { !model.isWriting }
        XCTAssertTrue(try file().contains(#"default_profile = "deux""#), contents)

        // The submenu follows once the core applied the file.
        try await waitUntil("the submenu follows") { submenu() == ["one", "deux", "New Profile", "two Copy"] }

        // Its own value for each appearance key, then Inherit again.
        for field in ProfilesPane.appearanceFields {
            XCTAssertTrue(model.inherits(0, field.key), field.key)
            model.setInherits(0, field.key, false)
            XCTAssertFalse(model.inherits(0, field.key), field.key)
            try await waitUntil("\(field.key) set") { !model.isWriting }
            XCTAssertNil(model.message("profiles[0].\(field.key)"), field.key)
        }
        try await waitUntil("the core applied the profile's own keys") {
            ProfilesPane.appearanceFields.allSatisfy { model.snapshot["profiles[0].\($0.key)"] != nil }
        }
        XCTAssertTrue(try file().contains("[profiles.appearance]"), contents)
        for field in ProfilesPane.appearanceFields {
            model.setInherits(0, field.key, true)
            try await waitUntil("\(field.key) inherited") { !model.isWriting }
        }
        try await waitUntil("the core applied the inherited keys") {
            ProfilesPane.appearanceFields.allSatisfy { model.snapshot["profiles[0].\($0.key)"] == nil }
        }
        XCTAssertEqual(ProfilesPane.appearanceFields.map(\.key), SettingsField.profileAppearanceKeys)

        // Command and working directory: a value, then empty removes the key.
        let profileEdits = [
            Edit(key: "command", value: .string("/bin/sh -l"), line: #"command = "/bin/sh -l""#),
            Edit(key: "working_directory", value: .string("/tmp"), line: #"working_directory = "/tmp""#),
        ]
        for edit in profileEdits {
            let field = try XCTUnwrap(ProfilesPane.fields.first { $0.key == edit.key })
            let path = "profiles[0].\(edit.key)"
            apply(field, edit.value, .init(model: model, path: path, profile: 0))
            try await waitUntil("\(path) written") { !model.isWriting }
            XCTAssertNil(model.message(path), path)
            XCTAssertTrue(try lines().contains(edit.line), "\(edit.line) in\n\(contents)")
            try await waitUntil("the core applied \(path)") { model.snapshot[path] == edit.value }
            apply(field, .string(""), .init(model: model, path: path, profile: 0))
            try await waitUntil("\(path) removed") { !model.isWriting }
            XCTAssertNil(model.message(path), path)
            XCTAssertFalse(try lines().contains(edit.line), "\(edit.key) removed from\n\(contents)")
            try await waitUntil("the core applied the removal of \(path)") { model.snapshot[path] == nil }
        }
        XCTAssertEqual(Set(profileEdits.map(\.key) + ["name"]), Set(ProfilesPane.fields.map(\.key)))

        // The default profile stays; another goes.
        XCTAssertFalse(model.deleteProfile(1))
        XCTAssertNotNil(model.message(SettingsModel.profilesKey))
        XCTAssertTrue(model.deleteProfile(2))
        try await waitUntil("deleted") { !model.isWriting }
        XCTAssertFalse(try file().contains("New Profile"), contents)
        try await waitUntil("the window and the submenu follow") {
            model.profiles.map(\.name) == ["one", "deux", "two Copy"] && submenu() == ["one", "deux", "two Copy"]
        }
    }

    /// An `.itermcolors` file with every colour the importer needs.
    private func itermcolors(at url: URL) throws {
        var dict: [String: Any] = [:]
        let colour: [String: Any] = [
            "Red Component": 0.2, "Green Component": 0.4, "Blue Component": 0.6, "Color Space": "sRGB",
        ]
        for index in 0..<16 { dict["Ansi \(index) Color"] = colour }
        dict["Foreground Color"] = colour
        dict["Background Color"] = colour
        try PropertyListSerialization.data(fromPropertyList: dict, format: .xml, options: 0).write(to: url)
    }

    func testImportingAThemeSelectsItAndAHostileFileIsRefused() async throws {
        let model = try openSettings(config: "")
        let dir = try XCTUnwrap(dir)
        let themes = dir.appendingPathComponent("themes")

        let hostile = dir.appendingPathComponent("evil.itermcolors")
        try Data("<?xml version=\"1.0\"?><plist><dict><key>Ansi 0 Color</key><string>\u{1B}]0;x\u{7}\u{202E}</string></dict></plist>".utf8)
            .write(to: hostile)
        model.importTheme(at: hostile)
        try await waitUntil("refused") { !model.isWriting }
        let message = try XCTUnwrap(model.message(SettingsModel.importThemeKey), "the importer's error")
        XCTAssertFalse(message.unicodeScalars.contains { $0.properties.generalCategory == .control || $0 == "\u{202E}" })
        XCTAssertEqual((try? FileManager.default.contentsOfDirectory(atPath: themes.path)) ?? [], [], "no file")
        XCTAssertEqual(model.value("appearance.theme"), .string("Tessera Dark"))

        let good = dir.appendingPathComponent("Mine.itermcolors")
        try itermcolors(at: good)
        model.importTheme(at: good)
        try await waitUntil("imported and selected") {
            !model.isWriting && model.themes.contains { $0.source == .user }
        }
        let id = try XCTUnwrap(model.themes.first { $0.source == .user }).id
        XCTAssertNil(model.message(SettingsModel.importThemeKey), "cleared by the next import")
        XCTAssertEqual(model.value("appearance.theme"), .string(id))
        try await waitUntil("theme selected in the file") { (try? self.file())?.contains("theme = \"\(id)\"") ?? false }
    }

    func testSettingsOpensOrFocusesTheOneWindow() throws {
        try openSettings(config: "")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        let item = try XCTUnwrap(MainMenu.item(forCommand: "settings", in: try XCTUnwrap(NSApp.mainMenu)))
        XCTAssertEqual(item.keyEquivalent, ",")
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(item.action), to: nil, from: item))
        MainMenu.run(command: "settings")
        XCTAssertTrue(appDelegate.settingsWindowController === controller, "the same window")
        // A closed one may linger in the window list until it is freed; an
        // open one has its delegate.
        XCTAssertEqual(NSApp.windows.filter { $0.identifier?.rawValue == "settings" && $0.delegate != nil }.count, 1)
        XCTAssertEqual(
            controller.tabs?.tabViewItems.compactMap { $0.identifier as? String },
            ["general", "appearance", "profiles", "keys", "plugins", "advanced"])
        controller.select(pane: "profiles")
        XCTAssertEqual(controller.tabs?.selectedTabViewItemIndex, 2)
    }

    func testClosingTheWindowFreesItsControllerAndModel() async throws {
        // Only a model that waits for an add or delete is kept (#455).
        appDelegate.settingsWaitLimit = .seconds(60)
        try autoreleasepool { _ = try openSettings(config: "[[profiles]]\nname = \"one\"\n") }
        let closed = try await opened { controller in
            // Every pane's view, so each holds the model.
            for pane in SettingsWindowController.panes {
                controller.select(pane: pane.identifier)
                controller.window?.contentView?.layoutSubtreeIfNeeded()
            }
            controller.close()
        }
        XCTAssertNil(appDelegate.settingsWindowController)
        // AppKit may keep a closed window (the previous key window); the
        // test keeps it, and the model must go all the same.
        XCTAssertNotNil(closed.window)
        try await waitUntil("the controller and model are freed") { closed.controller == nil && closed.model == nil }

        // Replacing the core closes a window that edits through it.
        let replaced = try await opened { _ in try appDelegate.replaceCore(start: false) }
        XCTAssertNil(appDelegate.settingsWindowController)
        try await waitUntil("freed with the core") { replaced.controller == nil && replaced.model == nil }
    }

    /// The Settings window's controller and model, held weakly, and its
    /// window, held as AppKit may hold a closed one.
    private struct Opened {
        weak var controller: SettingsWindowController?
        weak var model: SettingsModel?
        var window: NSWindow?
    }

    /// Opens Settings (or focuses it), lets the run loop show it, and runs
    /// `close` on its controller. Each step is outside the async test's
    /// frame and autorelease pool, which would keep the objects alive; and
    /// AppKit keeps a window closed in the same pass of the run loop that
    /// made it key, which no click can do.
    private func opened(_ close: (SettingsWindowController) throws -> Void) async throws -> Opened {
        let opened = try autoreleasepool {
            MainMenu.run(command: "settings")
            let controller = try XCTUnwrap(appDelegate.settingsWindowController)
            return Opened(controller: controller, model: controller.model, window: controller.window)
        }
        try await Task.sleep(for: .milliseconds(100))
        try autoreleasepool {
            try close(XCTUnwrap(opened.controller))
        }
        return opened
    }

    /// What a 1 MiB configuration (256 profiles) adds to opening the window:
    /// the config read and the model and panes built from it. The product
    /// bar, the window open within 200 ms on such a config (#347), is a
    /// release-build figure; a Debug build under TSan on a shared CI runner
    /// cannot certify it (the runner alone moved the best open from 167 to
    /// 262 ms, and single opens up to 576 ms, between runs), so it goes to a
    /// release-build performance check (follow-up). This test times the same
    /// open on a minimal configuration (one profile) in the same host and
    /// bounds the ratio, which the runner's speed cancels out of, plus an
    /// absolute ceiling of 1 s for a gross regression.
    ///
    /// One untimed open first pays SwiftUI's and AppKit's first use in the
    /// host. Then the two configurations take turns, three opens each, and
    /// the best open of each is compared. Measured locally (Debug, TSan, M
    /// series, six runs alone, in the class and in the full suite), the best
    /// 1 MiB open took 1.29 to 1.38 times the best minimal one (about 125 ms
    /// against 95 ms); the bar, 2.75, is twice the highest ratio measured.
    /// A cost per profile shows in the ratio (1 ms per profile fails it); a
    /// fixed cost added to every open does not (250 ms passes it), which the
    /// release-build check covers.
    func testAOneMebibyteConfigOpensTheWindowWithinTwoAndThreeQuarterTimesAMinimalOneAndUnderASecond() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-settings-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true

        var large = "[general]\ndefault_profile = \"p0\"\n"
        for index in 0..<256 {
            large += "[[profiles]]\nname = \"p\(index)\"\ncommand = \"/bin/cat \(String(repeating: "x", count: 200))\"\n"
        }
        while large.utf8.count < 1024 * 1024 {
            large += "# " + String(repeating: "padding ", count: 12) + "\n"
        }
        let minimal = "[general]\ndefault_profile = \"p0\"\n[[profiles]]\nname = \"p0\"\ncommand = \"/bin/cat\"\n"

        // Starts a core on `config` (not timed), then opens the window,
        // shows Profiles and draws it (timed), and closes it, so the next
        // open builds a new window, model and panes.
        func open(_ config: String, profiles: Int) throws -> Duration {
            try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
            try appDelegate.replaceCore(start: true)
            return try autoreleasepool {
                let start = ContinuousClock.now
                MainMenu.run(command: "settings")
                let controller = try XCTUnwrap(appDelegate.settingsWindowController)
                controller.select(pane: "profiles")
                controller.window?.contentView?.layoutSubtreeIfNeeded()
                controller.window?.displayIfNeeded()
                let elapsed = ContinuousClock.now - start
                XCTAssertEqual(controller.model.profiles.count, profiles)
                controller.close()
                XCTAssertNil(appDelegate.settingsWindowController)
                // Lets AppKit finish the close, as the run loop does between
                // two opens by hand.
                RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.1))
                return elapsed
            }
        }
        let warmUp = try open(minimal, profiles: 1)
        var minimalOpens: [Duration] = []
        var largeOpens: [Duration] = []
        for _ in 0..<3 {
            minimalOpens.append(try open(minimal, profiles: 1))
            largeOpens.append(try open(large, profiles: 256))
        }
        let bestMinimal = try XCTUnwrap(minimalOpens.min())
        let bestLarge = try XCTUnwrap(largeOpens.min())
        let ratio = bestLarge / bestMinimal
        let timings = "1 MiB opens \(largeOpens.map { "\($0)" }.joined(separator: ", ")); "
            + "minimal opens \(minimalOpens.map { "\($0)" }.joined(separator: ", ")); ratio of the best \(ratio)"
        // On a pass too, so the CI log has the timings the release-build
        // check is to be calibrated on.
        print("SETTINGS-OPEN-TIMINGS: warm-up \(warmUp); \(timings); best 1 MiB \(bestLarge), best minimal \(bestMinimal)")
        XCTAssertLessThanOrEqual(ratio, 2.75, timings)
        XCTAssertLessThan(bestLarge, .seconds(1), timings)
    }

    // MARK: Open config.toml (Advanced)

    /// What the stand-in for the core's `createConfig()` returns; nothing
    /// is written there.
    private let createdConfigFile = URL(fileURLWithPath: "/nonexistent/tessera-tests/config.toml")

    /// Gives the app a recording opener and a stand-in `createConfig()` (it
    /// would create the configuration directory and watch it); `events`
    /// gets "create" and "open <path>", in order.
    private func record(_ opener: RecordingOpener, events: @escaping (String) -> Void) {
        appDelegate.opener = opener
        appDelegate.configFileCreator = { [createdConfigFile] in
            events("create")
            return createdConfigFile
        }
        opener.onOpen = { file in events("open \(file.path)") }
    }

    /// The actions the Settings window's Advanced pane was built with, read
    /// out of its hosted view: what its Open config.toml button calls.
    /// `nil` when SwiftUI's view storage hides them from `Mirror`.
    private func advancedPaneActions(in controller: SettingsWindowController) throws -> SettingsActions? {
        let tabs = try XCTUnwrap(controller.tabs)
        let item = try XCTUnwrap(tabs.tabViewItems.first { ($0.identifier as? String) == "advanced" })
        let hosting = try XCTUnwrap(item.viewController as? NSHostingController<AnyView>)
        // Depth-bounded: no cycle in a value's reflection can loop.
        func find(_ value: Any, depth: Int) -> SettingsActions? {
            if let actions = value as? SettingsActions { return actions }
            guard depth < 8 else { return nil }
            for child in Mirror(reflecting: value).children {
                if let found = find(child.value, depth: depth + 1) { return found }
            }
            return nil
        }
        return find(hosting.rootView, depth: 0)
    }

    func testTheAdvancedPanesOpenConfigFileIsTheAppsOpenConfigFile() throws {
        try openSettings(config: "")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        guard let actions = try advancedPaneActions(in: controller) else {
            throw XCTSkip("SwiftUI's AnyView storage is not reflectable here")
        }
        let opener = RecordingOpener()
        var events: [String] = []
        record(opener) { events.append($0) }
        actions.openConfigFile()
        XCTAssertEqual(events, ["create", "open \(createdConfigFile.path)"], "the app delegate's Open config.toml")
    }

    func testOpenConfigFileCreatesTheConfigFileThenOpensIt() throws {
        let opener = RecordingOpener()
        var events: [String] = []
        record(opener) { events.append($0) }
        appDelegate.openConfigFile(nil)
        XCTAssertEqual(events, ["create", "open \(createdConfigFile.path)"], "created (if missing) before it is opened")
        XCTAssertEqual(opener.opened, [createdConfigFile])
        XCTAssertEqual(opener.revealed, [])
        XCTAssertEqual(opener.urls, [])
        appDelegate.openConfigFile(nil)
        XCTAssertEqual(events.filter { $0 == "create" }.count, 2, "the core's createConfig() is idempotent")
    }

    func testOpenConfigFileShowsTheFileInFinderWhenNothingOpensIt() throws {
        let opener = RecordingOpener()
        opener.editor = nil
        var events: [String] = []
        record(opener) { events.append($0) }
        appDelegate.openConfigFile(nil)
        XCTAssertEqual(events, ["create"])
        XCTAssertEqual(opener.opened, [])
        XCTAssertEqual(opener.revealed, [createdConfigFile])
    }

    func testOpenConfigFileShowsTheFileInFinderWhenOpeningItFails() throws {
        let opener = RecordingOpener()
        opener.openError = CocoaError(.fileReadNoPermission)
        record(opener) { _ in }
        appDelegate.openConfigFile(nil)
        XCTAssertEqual(opener.opened, [createdConfigFile])
        XCTAssertEqual(opener.revealed, [createdConfigFile], "after the failed open")
    }
}

// MARK: Settings minors (#344)

extension SettingsTests {
    /// The General pane's default profile field.
    private static var defaultProfileField: SettingsField {
        get throws { try XCTUnwrap(GeneralPane.fields.first { $0.key == "general.default_profile" }) }
    }

    /// The default profile picker offers the built-in profile, first: the
    /// one new tabs start with while `default_profile` is not set, which is
    /// what choosing it makes it. The core reads an unset key as the
    /// built-in's name (which pins ``SettingsField/builtInProfileName``), so
    /// the picker then shows it and no extra item.
    func testTheDefaultProfilePickerOffersTheBuiltInProfileAndChoosingItRemovesTheKey() async throws {
        let model = try openSettings(
            config: """
                [general]
                default_profile = "one"
                [[profiles]]
                name = "one"
                [[profiles]]
                name = "two"
                """)
        let field = try Self.defaultProfileField
        let binding = field.stringBinding(.init(model: model, path: field.key))
        let choices = try XCTUnwrap(field.choices(model))
        XCTAssertEqual(choices.map(\.value), [SettingsField.builtInProfileName, "one", "two"])
        XCTAssertEqual(choices.first?.title, "Default (built-in)")

        binding.wrappedValue = "two"
        try await waitUntil("two written") { !model.isWriting }
        XCTAssertTrue(try lines().contains(#"default_profile = "two""#), contents)
        try await waitUntil("applied") { model.snapshot["general.default_profile"] == .string("two") }

        binding.wrappedValue = SettingsField.builtInProfileName
        try await waitUntil("the key removed") { !model.isWriting }
        XCTAssertFalse(try file().contains("default_profile"), contents)
        try await waitUntil("the core reads the built-in's name for the unset key") {
            model.snapshot["general.default_profile"] == .string(SettingsField.builtInProfileName)
        }
        XCTAssertEqual(binding.wrappedValue, SettingsField.builtInProfileName)
        XCTAssertEqual(
            SettingsPicker.items(choices, selection: binding.wrappedValue), choices, "no extra item for the unset key")
    }

    /// On stand-ins: an unset key shows the built-in choice, a profile of
    /// the built-in's name takes its place (it is the one used), and
    /// choosing that name removes the key either way.
    func testTheBuiltInChoiceGivesWayToAProfileOfItsName() async throws {
        let (model, source, writer) = try fakeModel(#"{"general": {}, "profiles": [{"name": "one"}]}"#)
        let field = try Self.defaultProfileField
        let binding = field.stringBinding(.init(model: model, path: field.key))
        XCTAssertEqual(binding.wrappedValue, SettingsField.builtInProfileName)
        XCTAssertEqual(field.choices(model), [SettingsField.builtInProfileChoice, .init(value: "one", title: "one")])

        source.snapshot = TesseraConfigSnapshot(
            generation: 1,
            config: try Self.json(
                #"{"general": {"default_profile": "one"}, "profiles": [{"name": "one"}, {"name": "Default"}]}"#))
        model.reload()
        XCTAssertEqual(field.choices(model)?.map(\.title), ["one", "Default"], "no built-in item beside the profile")
        binding.wrappedValue = SettingsField.builtInProfileName
        try await waitUntil("removed") { !model.isWriting && writer.calls.count == 1 }
        XCTAssertEqual(writer.calls, [.init(key: "general.default_profile", value: nil)])
    }

    /// The renames of the default profile: the name is written first and
    /// `default_profile` only once it was, so a refused rename leaves both
    /// (no `default_profile` naming no profile).
    func testARefusedRenameOfTheDefaultProfileLeavesTheDefault() async throws {
        let writer = FakeWriter()
        writer.respond = { call, _ in
            call.key == "profiles[0].name" ? TesseraConfigEditError(.refused, message: "too long") : nil
        }
        let (model, _, _) = try fakeModel(Self.threeProfiles, writer: writer)
        model.renameProfile(0, to: "uno")
        XCTAssertEqual(model.value("general.default_profile"), .string("uno"), "shown at once")
        try await waitUntil("the rename ran") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "profiles[0].name", value: .string("uno"))], "the default not written")
        XCTAssertEqual(model.value("general.default_profile"), .string("one"))
        XCTAssertEqual(model.value("profiles[0].name"), .string("one"))
        XCTAssertEqual(model.message("profiles[0].name"), "too long")

        // Accepted: both, the name first.
        writer.respond = { _, _ in nil }
        model.renameProfile(0, to: "uno")
        try await waitUntil("both ran") { !model.isWriting && writer.calls.count == 3 }
        XCTAssertEqual(
            writer.calls.dropFirst().map(\.key), ["profiles[0].name", "general.default_profile"])
        XCTAssertEqual(writer.calls.last?.value, .string("uno"))
    }

    /// On the core: a name it refuses (longer than it keeps) leaves the
    /// file as it was, `default_profile` included.
    func testARenameTheCoreRefusesLeavesTheFilesDefaultProfile() async throws {
        let model = try openSettings(
            config: "[general]\ndefault_profile = \"one\"\n\n[[profiles]]\nname = \"one\"\n")
        let original = try file()
        model.renameProfile(0, to: String(repeating: "n", count: 200))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertNotNil(model.message("profiles[0].name"), "the core's warning by the name")
        XCTAssertEqual(try file(), original, "nothing written")
        XCTAssertEqual(model.value("general.default_profile"), .string("one"))
    }

    /// `count` `[[profiles]]` entries named `p0`, `p1`…, the first the
    /// default.
    private static func profilesTOML(_ count: Int) -> String {
        "[general]\ndefault_profile = \"p0\"\n" + (0..<count).map { "\n[[profiles]]\nname = \"p\($0)\"\n" }.joined()
    }

    /// On the core: at its 256 profiles, the `[[profiles]]` entries and
    /// those of `profiles/` together, Add and Duplicate are disabled and the
    /// status line says why (21.6a m4, #344 m-f); they write nothing, and a
    /// delete still runs. One fewer, and Add adds the 256th. The app's
    /// mirror of the cap is the core's: one more add is refused there.
    func testAddAndDuplicateAreDisabledAtTheProfileCapWithTheReason() async throws {
        let model = try openSettings(
            config: Self.profilesTOML(255), files: ["profiles/extra.toml": "command = \"/bin/cat\"\n"])
        XCTAssertEqual(model.profiles.count, 256)
        XCTAssertEqual(model.profiles.last?.name, "extra")
        XCTAssertFalse(model.canAddProfile, "disabled at the cap")
        let status = try XCTUnwrap(model.profilesStatus, "the status line says why")
        XCTAssertEqual(
            status, "Tessera reads at most 256 profiles, those in profiles/ included; delete one to add another.")
        let before = try file()
        XCTAssertFalse(model.addProfile())
        XCTAssertFalse(model.duplicateProfile(3))
        XCTAssertFalse(model.profilesChanging, "nothing was queued")
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(model.isWriting)
        XCTAssertEqual(try file(), before, "nothing written")

        // The mirror: the core refuses one more, with its own reason.
        let writer = try XCTUnwrap(model.writer)
        let refusal = await Task.detached { () -> String? in
            do throws(TesseraConfigEditError) {
                _ = try writer.addProfile(named: "New Profile", keys: [])
                return nil
            } catch {
                return error.message
            }
        }.value
        XCTAssertTrue(refusal?.contains("256") ?? false, "the core's cap is the app's: \(refusal ?? "added")")
        XCTAssertEqual(try file(), before)

        XCTAssertTrue(model.deleteProfile(5), "a delete still runs")
        XCTAssertNotNil(model.profilesStatus, "while it waits the pane says so")
        try await waitUntil("deleted") { !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        try await waitUntil("the delete applied") { !model.profilesChanging }
        XCTAssertEqual(model.profiles.count, 255)
        XCTAssertNil(model.profilesStatus, "under the cap: no reason")
        XCTAssertTrue(model.canAddProfile)
        XCTAssertTrue(model.addProfile())
        try await waitUntil("the 256th added") { !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        XCTAssertEqual(model.takeSelectionRequest(), 254, "after the 254 entries, before extra")
        try await waitUntil("applied") { !model.profilesChanging && model.profiles.count == 256 }
        XCTAssertEqual(model.profiles.suffix(2).map(\.name), ["New Profile", "extra"])
        XCTAssertNotNil(model.profilesStatus, "at the cap again")
    }

    /// #344 m-b: an earlier edit's configuration, applied just after a
    /// delete was written, does not unlock the list: the delete has not
    /// been applied, so the indices are still the old ones. The list
    /// unlocks once it shows the delete.
    func testAnEarlierEditsConfigurationAppliedAfterADeleteKeepsTheListLocked() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles)
        model.set("profiles[1].command", .string("/bin/sh"))
        XCTAssertTrue(model.deleteProfile(2))
        XCTAssertNotNil(model.profilesStatus, "the status line says why the pane is disabled")
        try await waitUntil("both ran") { !model.isWriting && writer.calls.count == 2 }
        // The watch applies the file as the command's edit left it.
        let commandOnly = Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/sh")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(commandOnly))
        model.reload()
        XCTAssertTrue(model.profilesChanging, "the delete is not applied yet")
        XCTAssertFalse(model.deleteProfile(1), "still locked: nothing is written")
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/sh"))
        // Then the delete.
        let deleted = commandOnly.replacingOccurrences(of: #", {"name": "three"}"#, with: "")
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(deleted))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
        XCTAssertNil(model.profilesStatus)
        XCTAssertEqual(writer.calls.count, 2)
    }

    /// #344 m-d: while the list is locked no edit of a profile's key is
    /// written, whichever way it comes (a field, a slider, a removal).
    func testProfileFieldWritesAreRefusedWhileTheListIsLocked() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.threeProfiles, clock: clock)
        XCTAssertTrue(model.deleteProfile(2))
        model.set("profiles[0].command", .string("/bin/sh"))
        model.remove("profiles[1].command")
        model.change("profiles[1].appearance.opacity", .float(0.4))
        clock.advance(by: .seconds(1))
        model.endChange("profiles[1].appearance.opacity")
        try await waitUntil("the delete ran") { !model.isWriting }
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [.init(key: "profiles[2]", value: nil)])
        XCTAssertNil(model.value("profiles[0].command"), "nothing shown that will not be written")
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/cat"))
        XCTAssertNil(model.value("profiles[1].appearance.opacity"))
        // Keys outside the profiles are still written.
        model.set("appearance.opacity", .float(0.5))
        try await waitUntil("written") { !model.isWriting && writer.calls.count == 2 }
    }

    /// Names a profile cannot have are refused before anything is written,
    /// with why by the name; a name another profile has is refused too.
    func testOddAndTakenProfileNamesAreRefusedWithAMessage() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        let cases = [
            ("", "A profile needs a name."),
            (" ", "A profile needs a name."),
            ("\u{FEFF}", "A profile needs a name."),
            ("a\u{0}b", "A profile name cannot contain control characters or line breaks."),
            ("a\nb", "A profile name cannot contain control characters or line breaks."),
            ("a\u{2028}b", "A profile name cannot contain control characters or line breaks."),
            ("three", "A profile named “three” exists."),
        ]
        for (name, message) in cases {
            model.renameProfile(1, to: name)
            XCTAssertEqual(model.message("profiles[1].name"), message, name.debugDescription)
            XCTAssertEqual(model.value("profiles[1].name"), .string("two"), name.debugDescription)
        }
        // Its own name is no change: the message goes.
        model.renameProfile(1, to: "two")
        XCTAssertNil(model.message("profiles[1].name"))
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [])
        // A name taken by a rename not yet applied is taken too.
        model.renameProfile(2, to: "drei")
        model.renameProfile(1, to: "drei")
        XCTAssertEqual(model.message("profiles[1].name"), "A profile named “drei” exists.")
        model.renameProfile(1, to: "zwei")
        XCTAssertNil(model.message("profiles[1].name"))
        try await waitUntil("both renames ran") { !model.isWriting && writer.calls.count == 2 }
        XCTAssertEqual(writer.calls.map(\.value), [.string("drei"), .string("zwei")])
    }

    /// Add and Duplicate commit the text typed into a field first: the
    /// typed command is written before the new profile, and the copy has
    /// it.
    func testAddAndDuplicateCommitTheTypedDraftFirst() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles)
        let field = NSObject()
        let typed = FakeWriter.Call(key: "profiles[1].command", value: .string("echo typed"))
        model.holdDraft(field) { model.set("profiles[1].command", .string("echo typed")) }
        XCTAssertTrue(model.duplicateProfile(1))
        try await waitUntil("written") { !model.isWriting && writer.calls.count == 2 }
        XCTAssertEqual(writer.calls, [typed, FakeWriter.added("two Copy", ["command": .string("echo typed")])])

        let added = Self.threeProfiles.replacingOccurrences(
            of: #"{"name": "three"}"#,
            with: #"{"name": "three"}, {"name": "two Copy", "command": "echo typed"}"#
        ).replacingOccurrences(of: "/bin/cat", with: "echo typed")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(added))
        model.reload()
        model.holdDraft(field) { model.set("profiles[0].command", .string("first")) }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting && writer.calls.count == 4 }
        XCTAssertEqual(writer.calls.suffix(2).map(\.key), ["profiles[0].command", FakeWriter.addKey])

        // A draft released (the field committed it or went) is not
        // committed again.
        model.holdDraft(field) { XCTFail("a released draft was committed") }
        model.releaseDraft(field)
        model.commitDraft()
    }

    /// A click on another profile writes the text typed into the selected
    /// one's field first (the field goes before it sees the focus go).
    func testAClickOnAnotherProfileCommitsTheTypedText() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
        window.isKeyForTest = true
        window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
        defer {
            window.contentView = nil
            window.close()
        }
        window.layoutIfNeeded()
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: 1)
        try await waitUntil("two's fields are shown") { Self.commandField(in: window)?.stringValue == "/bin/cat" }
        let field = try XCTUnwrap(Self.commandField(in: window))
        XCTAssertTrue(window.makeFirstResponder(field))
        field.currentEditor()?.selectAll(nil)
        (field.currentEditor() as? NSTextView)?.insertText(
            "echo typed", replacementRange: NSRange(location: NSNotFound, length: 0))
        try await Task.sleep(for: .milliseconds(50))
        Self.click(table, row: 2)
        try await waitUntil("three's fields are shown") { Self.commandField(in: window)?.stringValue == "" }
        let typed = FakeWriter.Call(key: "profiles[1].command", value: .string("echo typed"))
        try await waitUntil("the typed text written to two") { writer.calls == [typed] }
        window.makeFirstResponder(nil)
        try await Task.sleep(for: .milliseconds(150))
        XCTAssertEqual(writer.calls, [typed], "once, and nothing to three")
    }

    /// Add and Duplicate, pressed under the list while text is typed into
    /// the selected profile's field (a click on them takes no focus), write
    /// that text first; the copy has it.
    func testPressingAddOrDuplicateCommitsTheTypedText() async throws {
        for press in ["Add", "Duplicate"] {
            let (model, _, writer) = try fakeModel(Self.threeProfiles)
            let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
            window.isKeyForTest = true
            window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
            defer {
                window.contentView = nil
                window.close()
            }
            window.layoutIfNeeded()
            try await waitUntil("the list shows the profiles") {
                Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
            }
            let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
            Self.click(table, row: 1)
            try await waitUntil("two's fields are shown") { Self.commandField(in: window)?.stringValue == "/bin/cat" }
            let field = try XCTUnwrap(Self.commandField(in: window))
            XCTAssertTrue(window.makeFirstResponder(field))
            field.currentEditor()?.selectAll(nil)
            (field.currentEditor() as? NSTextView)?.insertText(
                "echo typed", replacementRange: NSRange(location: NSNotFound, length: 0))
            try await Task.sleep(for: .milliseconds(50))
            // The list's buttons, left to right: Add, Duplicate, Delete.
            let buttons = Self.views(NSButton.self, in: window.contentView)
                .filter { $0.convert($0.bounds, to: nil).minX < 200 }
                .sorted { $0.convert($0.bounds, to: nil).minX < $1.convert($1.bounds, to: nil).minX }
            XCTAssertEqual(buttons.count, 3, "\(buttons)")
            let button = try XCTUnwrap(press == "Add" ? buttons.first : buttons.dropFirst().first)
            // As a click does without Full Keyboard Access, the press
            // leaves the field focused.
            button.performClick(nil)
            XCTAssertTrue(window.firstResponder === field.currentEditor(), "\(press): the field keeps the focus")
            let typed = FakeWriter.Call(key: "profiles[1].command", value: .string("echo typed"))
            let expected =
                press == "Add"
                ? [typed, FakeWriter.added("New Profile")]
                : [typed, FakeWriter.added("two Copy", ["command": .string("echo typed")])]
            try await waitUntil("\(press): the typed text written first") { writer.calls == expected }
            try await waitUntil("\(press): the new profile selected once written") {
                table.numberOfRows == 4 && table.selectedRow == 3
            }
            window.makeFirstResponder(nil)
            try await Task.sleep(for: .milliseconds(150))
            XCTAssertEqual(writer.calls, expected, "\(press): nothing more")
        }
    }

    /// #547: a save followed by a `config.toml` that does not load (a
    /// broken hand edit before the watch read the save) applies nothing,
    /// so the generation stays; the save settles all the same: the control
    /// shows the configuration in effect, and the window the load error,
    /// sanitised (S3). A save made while the file already did not load
    /// (one that repairs it) waits for its configuration as before, and
    /// another failure after it settles it too.
    func testASaveFollowedByAFileThatDoesNotLoadSettlesAndShowsTheError() async throws {
        let (model, source, writer) = try fakeModel(Self.fakeConfig)
        model.set("appearance.opacity", .float(0.5))
        try await waitUntil("the edit ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.count, 1)
        model.reload()
        XCTAssertTrue(model.isPending("appearance.opacity"), "waits for the configuration")
        let error = #"TOML parse error at line 3, column 1\n  |\n3 | not toml\u{202E}\n  | ^\nkey with no value"#
        source.snapshot = TesseraConfigSnapshot(
            generation: 0, config: try Self.json(Self.fakeConfig), loadError: error.replacingOccurrences(
                of: #"\u{202E}"#, with: "\u{202E}\u{1B}]0;x\u{7}"))
        model.reload()
        XCTAssertFalse(model.isPending("appearance.opacity"), "settled by the failure")
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.85), "the configuration in effect")
        let shown = try XCTUnwrap(model.loadError)
        XCTAssertTrue(shown.hasPrefix("config.toml could not be loaded"), shown)
        XCTAssertTrue(shown.contains("\n3 | not toml]0;x\n"), shown)
        for line in shown.split(separator: "\n") {
            XCTAssertEqual(String(line), SettingsModel.sanitisedMessage(String(line)), shown)
        }

        // A save while the file does not load: the same error does not
        // settle it, a configuration applied does (and clears the error).
        model.set("appearance.opacity", .float(0.4))
        try await waitUntil("the second edit ran") { !model.isWriting }
        model.reload()
        XCTAssertTrue(model.isPending("appearance.opacity"), "the failure it was saved under")
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.4))
        source.snapshot = TesseraConfigSnapshot(
            generation: 1, config: try Self.json(Self.fakeConfig.replacingOccurrences(of: "0.85", with: "0.4")))
        model.reload()
        XCTAssertFalse(model.isPending("appearance.opacity"))
        XCTAssertEqual(model.value("appearance.opacity"), .float(0.4))
        XCTAssertNil(model.loadError, "the file loads again")

        // Saved under one failure, settled by another.
        source.snapshot.loadError = "first"
        model.reload()
        model.set("appearance.opacity", .float(0.3))
        try await waitUntil("the third edit ran") { !model.isWriting }
        model.reload()
        XCTAssertTrue(model.isPending("appearance.opacity"))
        source.snapshot.loadError = "second"
        model.reload()
        XCTAssertFalse(model.isPending("appearance.opacity"), "another failure settles it")
        XCTAssertEqual(model.loadError, SettingsModel.loadErrorMessage("second"))
    }

    /// #547 on the core: the file breaks right after the save is written
    /// (on the write queue, before the watch's debounce ends), so the watch
    /// reads only the broken file. The core announces it, the app delegate
    /// has the window read it (nothing posted by the test), and the save
    /// settles with the error shown; fixing the file clears it.
    func testOnTheCoreASaveFollowedByABrokenFileSettlesAndShowsTheError() async throws {
        let model = try openSettings(config: "[appearance]\nopacity = 0.85\n")
        let generation = model.snapshot.generation
        let url = try configFile
        model.set("appearance.opacity", .float(0.5))
        SettingsModel.writeQueue.async {
            try? "[appearance]\nopacity = 0.5\nnot toml\n".write(to: url, atomically: true, encoding: .utf8)
        }
        try await waitUntil("the load error shows") { model.loadError != nil }
        XCTAssertEqual(model.snapshot.generation, generation, "the watch applied nothing")
        XCTAssertFalse(model.isPending("appearance.opacity"), "the save settled")
        XCTAssertTrue(Self.same(model.value("appearance.opacity"), .float(0.85)), "the configuration in effect")
        let shown = try XCTUnwrap(model.loadError)
        XCTAssertTrue(shown.contains("not toml"), shown)
        XCTAssertFalse(shown.contains(#"\n"#), shown)
        try "[appearance]\nopacity = 0.5\n".write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("the fixed file applies") { model.loadError == nil }
        XCTAssertTrue(Self.same(model.value("appearance.opacity"), .float(0.5)))
    }

    /// #547: a closed window's model waiting for its delete goes once the
    /// file stops loading (the delete cannot apply), not at the wait limit.
    func testAClosedWindowsWaitingModelGoesWhenTheFileStopsLoading() async throws {
        appDelegate.settingsWaitLimit = .seconds(60)
        try autoreleasepool { _ = try openSettings(config: Self.threeProfilesTOML) }
        let url = try configFile
        let broken = Self.threeProfilesTOML + "\nnot toml\n"
        let closed = try await opened { controller in
            // No add or delete notice on the settle timer meanwhile.
            controller.model.profileSettleTimeout = .seconds(60)
            XCTAssertTrue(controller.model.deleteProfile(1))
            SettingsModel.writeQueue.async { try? broken.write(to: url, atomically: true, encoding: .utf8) }
            controller.close()
        }
        try await waitUntil("the controller is freed") { closed.controller == nil }
        try await waitUntil("the model is freed once the file stops loading") { closed.model == nil }
        XCTAssertTrue(try file().contains("not toml"), contents)
    }

    /// #547: an Add Profile followed by a file that does not load ends
    /// with the failure: the list unlocks at once and is the applied one
    /// again (the rows after the add with their messages), with the load
    /// error and no "another app" notice, then or at the settle time.
    func testAnAddFollowedByAFileThatDoesNotLoadSettlesWithNoNotice() async throws {
        let clock = ManualClock()
        let writer = FakeWriter()
        let refusal = "`profiles[2].command`: this profile is defined in a file in profiles/, not in config.toml"
        writer.respond = { call, _ in
            call.key == "profiles[2].command" ? TesseraConfigEditError(.invalidArgument, message: refusal) : nil
        }
        let (model, source, _) = try fakeModel(Self.entriesAndFiles, writer: writer, clock: clock)
        let names = ["one", "two", "extra", "zeta"]
        model.set("profiles[2].command", .string("/bin/sh"))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "New Profile", "extra", "zeta"])
        XCTAssertEqual(model.message("profiles[3].command"), refusal)
        XCTAssertTrue(model.profilesChanging)
        let generation = model.profileListGeneration

        source.snapshot.loadError = "broken"
        model.reload()
        XCTAssertEqual(model.loadError, SettingsModel.loadErrorMessage("broken"))
        XCTAssertFalse(model.profilesChanging, "settled by the failure")
        XCTAssertEqual(model.profiles.map(\.name), names, "the list applied")
        XCTAssertEqual(model.message("profiles[2].command"), refusal, "back with extra")
        XCTAssertNil(model.message("profiles[3].command"))
        XCTAssertEqual(model.profileListGeneration, generation + 1, "the controls start again")
        XCTAssertTrue(model.canAddProfile)
        clock.advance(by: .seconds(3))
        XCTAssertNil(model.message(SettingsModel.profilesKey), "no notice")

        // An add queued while the file already does not load (the fake
        // writes it) ends at the settle time, with no notice either.
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written again") { !model.isWriting }
        model.reload()
        XCTAssertTrue(model.profilesChanging, "the failure it was queued under")
        clock.advance(by: .seconds(3))
        XCTAssertFalse(model.profilesChanging, "unlocked at the settle time")
        XCTAssertNil(model.message(SettingsModel.profilesKey), "the load error says why")
        XCTAssertEqual(model.profiles.map(\.name), names)
    }

    /// #547: a closed window's model waiting for its add goes once the
    /// file stops loading, not at the wait limit.
    func testAClosedWindowsWaitingAddGoesWhenTheFileStopsLoading() async throws {
        appDelegate.settingsWaitLimit = .seconds(60)
        try autoreleasepool { _ = try openSettings(config: Self.threeProfilesTOML) }
        let url = try configFile
        let broken = Self.threeProfilesTOML + "\nnot toml\n"
        let closed = try await opened { controller in
            controller.model.profileSettleTimeout = .seconds(60)
            XCTAssertTrue(controller.model.addProfile())
            SettingsModel.writeQueue.async { try? broken.write(to: url, atomically: true, encoding: .utf8) }
            controller.close()
        }
        try await waitUntil("the controller is freed") { closed.controller == nil }
        try await waitUntil("the model is freed once the file stops loading") { closed.model == nil }
        XCTAssertTrue(try file().contains("not toml"), contents)
    }

    /// #570, on the core, in the window the app opens (nothing posted):
    /// another app saves its stale copy of config.toml (without the add,
    /// and not loading) right behind an Add Profile, before the watch
    /// read the add. The add ends with the failure and no notice by the
    /// list, so the Profiles pane shows the load error, as the other panes
    /// do: the add does not go silently. The repair (still without the
    /// add) takes the error away.
    func testTheProfilesPaneShowsTheLoadErrorAnAddEndedWith() async throws {
        let model = try openSettings(config: Self.threeProfilesTOML)
        let window = try XCTUnwrap(appDelegate.settingsWindowController?.window)
        try Self.pressToolbarItem("Profiles", in: window)
        window.layoutIfNeeded()
        let url = try configFile
        let stale = Self.threeProfilesTOML + "\nnot toml\n"
        XCTAssertTrue(model.addProfile())
        SettingsModel.writeQueue.async { try? stale.write(to: url, atomically: true, encoding: .utf8) }
        try await waitUntil("the load error") { model.loadError != nil }
        let error = try XCTUnwrap(model.loadError)
        XCTAssertTrue(error.contains("not toml"), error)
        XCTAssertFalse(model.profilesChanging, "the add ended with the failure")
        XCTAssertNil(model.message(SettingsModel.profilesKey), "no notice by the list")
        try await waitUntil("the Profiles pane shows the load error") {
            Self.shownTexts(in: window.contentView).contains(error)
        }
        XCTAssertFalse(try file().contains("New Profile"), contents)

        try Self.threeProfilesTOML.write(to: url, atomically: true, encoding: .utf8)
        try await waitUntil("the repair applies") { model.loadError == nil }
        try await waitUntil("the pane no longer shows the error") {
            !Self.shownTexts(in: window.contentView).contains { $0.hasPrefix("config.toml could not be loaded") }
        }
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three"])
    }

    /// #570 on stand-ins: the Profiles pane shows the model's load error,
    /// sanitised (S3: the model's text, not the core's), above the list,
    /// and not once the file loads again.
    func testTheProfilesPaneShowsTheSanitisedLoadError() async throws {
        let (model, source, _) = try fakeModel(Self.threeProfiles)
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
        }
        XCTAssertFalse(Self.shownTexts(in: window.contentView).contains { $0.contains("could not be loaded") })
        source.snapshot.loadError = "line 3\u{202E}\u{1B}]0;x\u{7}: not toml"
        model.reload()
        let error = try XCTUnwrap(model.loadError)
        XCTAssertEqual(error, SettingsModel.loadErrorMessage("line 3\u{202E}\u{1B}]0;x\u{7}: not toml"))
        try await waitUntil("the pane shows the load error") { Self.shownTexts(in: window.contentView).contains(error) }
        let shown = Self.shownTexts(in: window.contentView)
        XCTAssertFalse(
            shown.contains { $0.unicodeScalars.contains { $0 == "\u{202E}" || $0 == "\u{1B}" || $0 == "\u{7}" } },
            "\(shown)")
        source.snapshot.loadError = nil
        model.reload()
        try await waitUntil("the error goes") {
            !Self.shownTexts(in: window.contentView).contains { $0.contains("could not be loaded") }
        }
    }

    /// A syntax error's excerpt of the file shows as lines, each sanitised
    /// (S3), not as one line with `\n` in it.
    func testASyntaxErrorShowsAsLines() async throws {
        let escaped = TesseraConfigEditError(
            .refused, message: #"TOML parse error at line 2, column 9\n  |\n2 | [general\n  |         ^\nunclosed table, expected `]`\n"#)
        XCTAssertEqual(
            SettingsModel.displayMessage(escaped),
            "TOML parse error at line 2, column 9\n|\n2 | [general\n| ^\nunclosed table, expected `]`")
        let many = TesseraConfigEditError(.refused, message: (1...40).map { "line \($0)" }.joined(separator: #"\n"#))
        let lines = SettingsModel.displayMessage(many).split(separator: "\n")
        XCTAssertEqual(lines.count, SettingsModel.maxMessageLines)
        XCTAssertEqual(lines.last, "…")
        let hostile = TesseraConfigEditError(.refused, message: "a\u{202E}b\\n\u{1B}]0;x\u{7}c\\n\\n\\n")
        XCTAssertEqual(SettingsModel.displayMessage(hostile), "ab\n]0;xc")

        // On the core, a file that is not TOML.
        let model = try openSettings(config: "[general]\nconfirm_close = true\n")
        try "[general\nconfirm_close = true\n".write(to: configFile, atomically: true, encoding: .utf8)
        model.set("general.confirm_close", .bool(false))
        try await waitUntil("refused") { !model.isWriting }
        let message = try XCTUnwrap(model.message("general.confirm_close"))
        XCTAssertFalse(message.contains(#"\n"#), message)
        XCTAssertGreaterThan(message.split(separator: "\n").count, 1, message)
        for line in message.split(separator: "\n") {
            XCTAssertEqual(String(line), SettingsModel.sanitisedMessage(String(line)), message)
        }
    }

    /// The open panels are sheets on the Settings window, not app-modal:
    /// the window the app opens is the model's, which the panes' Choose…
    /// and Import Theme… present their panels on. (Those two SwiftUI
    /// buttons take no synthetic click in a hosted window, so the test
    /// presents a panel as they do: a recording one, since a real panel's
    /// bridge can block the main thread for a minute in a full run, #492.)
    func testTheOpenPanelsAreSheetsOnTheSettingsWindow() throws {
        let model = try openSettings(config: "")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        let window = try XCTUnwrap(controller.window)
        XCTAssertTrue(model.window === window, "the panels' window is the Settings window")
        var chosen: [URL] = []

        let cancelled = RecordingOpenPanel(url: URL(filePath: "/tmp/not-chosen"))
        SettingsPanels.present(cancelled, on: model.window) { chosen.append($0) }
        XCTAssertEqual(cancelled.runs, [.sheet(on: ObjectIdentifier(window))], "a sheet on the Settings window")
        XCTAssertEqual(chosen, [], "nothing chosen while the sheet is open")
        cancelled.end(.cancel)
        XCTAssertEqual(chosen, [], "cancelled: nothing chosen")

        let file = URL(filePath: "/tmp/scheme.itermcolors")
        let picked = RecordingOpenPanel(url: file)
        SettingsPanels.present(picked, on: model.window) { chosen.append($0) }
        XCTAssertEqual(picked.runs, [.sheet(on: ObjectIdentifier(window))])
        picked.end(.OK)
        XCTAssertEqual(chosen, [file], "chosen: the panel's URL")

        // Without a window: on its own, still not app-modal.
        let alone = RecordingOpenPanel(url: file)
        SettingsPanels.present(alone, on: nil) { chosen.append($0) }
        XCTAssertEqual(alone.runs, [.alone])
    }

    /// A real open panel (the directory one) becomes a sheet on the
    /// Settings window, not app-modal. Skipped when the panel's
    /// out-of-process bridge did not start, which in a full run can take a
    /// minute to time out (`Assertion failure in -[NSOpenPanel
    /// _initBridgeAndStuff]`, #492) and says nothing of `present`.
    func testARealOpenPanelIsASheetOnTheSettingsWindow() async throws {
        let model = try openSettings(config: "")
        let window = try XCTUnwrap(model.window)
        let log = try OSLogStore(scope: .currentProcessIdentifier)
        let since = log.position(date: Date())
        let start = ContinuousClock.now
        let panel = SettingsPanels.directoryPanel()
        // The bridge starts in the panel's init and gives up after a
        // minute; reading the log takes seconds, so only a slow init does.
        if ContinuousClock.now - start > .seconds(10) {
            let bridgeFailed = try log.getEntries(
                at: since, matching: NSPredicate(format: "composedMessage CONTAINS %@", "_initBridgeAndStuff")
            ).contains { _ in true }
            try XCTSkipIf(bridgeFailed, "the open panel's bridge did not start (#492)")
        }
        defer { if window.attachedSheet === panel { window.endSheet(panel, returnCode: .cancel) } }
        var chosen: [URL] = []
        SettingsPanels.present(panel, on: window) { chosen.append($0) }
        try await waitUntil("a sheet") { window.attachedSheet != nil }
        XCTAssertTrue(window.attachedSheet === panel)
        XCTAssertNil(NSApp.modalWindow, "not app-modal")
        window.endSheet(panel, returnCode: .cancel)
        try await waitUntil("the sheet ends") { window.attachedSheet == nil }
        XCTAssertEqual(chosen, [], "cancelled: nothing chosen")
    }

    /// Text typed into a field and not committed, which the model holds
    /// (with the field's binding, which holds the model), is written when
    /// the window closes, and the model is freed all the same.
    func testClosingTheWindowWithTypedTextWritesItAndFreesTheModel() async throws {
        appDelegate.settingsWaitLimit = .seconds(60)
        try autoreleasepool { _ = try openSettings(config: "[[profiles]]\nname = \"one\"\n") }
        let field = NSObject()
        let closed = try await opened { controller in
            let model = controller.model
            model.holdDraft(field) { model.set("profiles[0].command", .string("typed")) }
            controller.close()
        }
        try await waitUntil("the controller and model are freed") { closed.controller == nil && closed.model == nil }
        try await waitUntil("the typed text written") { (try? self.lines())?.contains(#"command = "typed""#) ?? false }
    }

    // MARK: #455

    private static let threeProfilesTOML = """
        [general]
        default_profile = "one"

        [[profiles]]
        name = "one"

        [[profiles]]
        name = "two"
        command = "/bin/cat"

        [[profiles]]
        name = "three"

        """

    /// A copy is one add of the core's, its name and keys together: a
    /// refused one writes nothing (its command alone would be a profile
    /// without a name, which the core loads as "Default"). The list shows
    /// the core's message and is not left locked.
    func testADuplicateWhoseNameIsRefusedWritesNoneOfItsKeys() async throws {
        let writer = FakeWriter()
        writer.respond = { call, _ in
            call.key == FakeWriter.addKey
                ? TesseraConfigEditError(.invalidArgument, message: "profiles[3].name: too long") : nil
        }
        let (model, _, _) = try fakeModel(Self.threeProfiles, writer: writer)
        XCTAssertTrue(model.duplicateProfile(1))
        XCTAssertTrue(model.profilesChanging)
        try await waitUntil("refused") { !model.isWriting }
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [FakeWriter.added("two Copy", ["command": .string("/bin/cat")])])
        XCTAssertNil(model.selectionRequest, "nothing to select")
        XCTAssertFalse(model.profilesChanging, "a refused copy unlocks the list")
        XCTAssertEqual(model.message(SettingsModel.profilesKey), "profiles[3].name: too long")
        XCTAssertNil(model.value("profiles[3].command"))
    }

    /// On the core: a profile whose name is 124 or 125 bytes has no copy,
    /// since " Copy" makes the name longer than the core's 128 bytes.
    /// Nothing of the copy is written, so no nameless profile appears, and
    /// the list shows the core's reason.
    func testDuplicatingAProfileWithALongNameWritesNoProfileWithoutAName() async throws {
        let names = [String(repeating: "p", count: 124), String(repeating: "q", count: 125)]
        let model = try openSettings(
            config: names.map {
                "[[profiles]]\nname = \"\($0)\"\ncommand = \"/bin/cat\"\nworking_directory = \"/tmp\"\n"
            }.joined(separator: "\n"))
        XCTAssertEqual(model.profiles.map(\.name), names)
        let before = try file()
        for index in names.indices {
            XCTAssertTrue(model.duplicateProfile(index))
            try await waitUntil("refused") { !model.isWriting }
            XCTAssertFalse(model.profilesChanging)
            let message = try XCTUnwrap(model.message(SettingsModel.profilesKey), "the list says why")
            XCTAssertTrue(message.contains("128 bytes"), "the core's reason: \(message)")
            XCTAssertEqual(try file(), before, "nothing of the copy written")
        }
        // Nothing for the watch to apply either.
        try await Task.sleep(for: .milliseconds(300))
        model.reload()
        XCTAssertEqual(model.profiles.map(\.name), names, contents)
    }

    /// Settings closed and opened again before a delete is applied (the
    /// watch waits 100 ms to 1 s) edits on with the model that made it,
    /// still locked: an edit of the stale list's last profile would land
    /// past the end of the file's and make a nameless profile. Opened the
    /// way the app opens it. The watch can apply the delete before the
    /// reopen on a loaded machine: that attempt is made again, a few times
    /// at most (#455 m1).
    func testSettingsOpenedAgainBeforeADeleteAppliesStaysLocked() async throws {
        var attempts = 0
        var model: SettingsModel
        repeat {
            attempts += 1
            model = try openSettings(config: Self.threeProfilesTOML)
            XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three"])
            XCTAssertTrue(model.deleteProfile(1))
            try XCTUnwrap(appDelegate.settingsWindowController).close()
            XCTAssertNil(appDelegate.settingsWindowController)
            try await waitUntil("the delete written") { !model.isWriting }
            // Nothing awaited from here to the reopen: the watch's apply (on
            // the main actor) cannot come in between.
            if appDelegate.waitingSettingsModel === model { break }
            guard attempts < 5 else { throw XCTSkip("the watch applied each delete before the reopen") }
        } while true
        MainMenu.run(command: "settings")
        let reopened = try XCTUnwrap(appDelegate.settingsWindowController?.model)
        XCTAssertTrue(reopened === model, "the model that made the delete")
        XCTAssertTrue(reopened.profilesChanging, "still locked")
        XCTAssertNotNil(reopened.profilesStatus)
        // What the stale list's "three" fields would write.
        let command = try XCTUnwrap(ProfilesPane.fields.first { $0.key == "command" })
        command.stringBinding(.init(model: reopened, path: "profiles[2].command", profile: 2)).wrappedValue = "stray"
        reopened.renameProfile(2, to: "renamed")
        try await waitUntil("the delete applied") { !reopened.profilesChanging && reopened.profiles.count == 2 }
        // A stray write would have landed and applied by now.
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(try lines().filter { $0 == "[[profiles]]" }.count, 2, contents)
        XCTAssertFalse(try file().contains("stray"), contents)
        XCTAssertFalse(try file().contains("renamed"), contents)
        XCTAssertEqual(reopened.profiles.map(\.name), ["one", "three"])
    }

    /// A closed window's model that waits for its delete is freed once the
    /// delete applies, or with the core; its controller at once.
    func testAClosedWindowsWaitingModelIsFreedOnceItsDeleteAppliesOrWithTheCore() async throws {
        // Not by the wait limit.
        appDelegate.settingsWaitLimit = .seconds(60)
        try autoreleasepool { _ = try openSettings(config: Self.threeProfilesTOML) }
        let closed = try await opened { controller in
            XCTAssertTrue(controller.model.deleteProfile(1))
            controller.close()
        }
        XCTAssertNil(appDelegate.settingsWindowController)
        try await waitUntil("the controller is freed") { closed.controller == nil }
        try await waitUntil("the model is freed once the delete applies") { closed.model == nil }
        XCTAssertFalse(try file().contains("\"two\""), contents)

        let replaced = try await opened { controller in
            XCTAssertTrue(controller.model.deleteProfile(1))
            controller.close()
            // Before the watch applies the delete.
            try appDelegate.replaceCore(start: true)
        }
        try await waitUntil("freed with the core") { replaced.controller == nil && replaced.model == nil }
        // Settings on the new core reads it afresh, unlocked.
        MainMenu.run(command: "settings")
        XCTAssertEqual(appDelegate.settingsWindowController?.model.profilesChanging, false)
    }

    /// One wait limit at a time, the last close's: Settings closed, opened
    /// again and closed again just before the first close's limit keeps
    /// the model, its delete still waiting, until the second close's limit
    /// has passed; then it goes, still locked. On a manual clock; nothing
    /// awaited between the delete and the last check, so the watch's apply
    /// (on the main actor) cannot come in between.
    func testSettingsClosedAgainKeepsTheModelForTheLastClosesWaitLimit() async throws {
        let clock = ManualClock()
        appDelegate.settingsClock = clock
        let model = try openSettings(config: Self.threeProfilesTOML)
        XCTAssertTrue(model.deleteProfile(2))
        try XCTUnwrap(appDelegate.settingsWindowController).close()
        XCTAssertTrue(appDelegate.waitingSettingsModel === model)
        clock.advance(by: .milliseconds(1900))
        MainMenu.run(command: "settings")
        XCTAssertTrue(appDelegate.settingsWindowController?.model === model)
        try XCTUnwrap(appDelegate.settingsWindowController).close()
        XCTAssertTrue(appDelegate.waitingSettingsModel === model)
        clock.advance(by: .milliseconds(200))
        XCTAssertTrue(appDelegate.waitingSettingsModel === model, "kept past the first close's limit")
        clock.advance(by: .milliseconds(1750))
        XCTAssertTrue(appDelegate.waitingSettingsModel === model, "kept 1.95 s after the second close")
        clock.advance(by: .milliseconds(100))
        XCTAssertTrue(model.profilesChanging, "the delete still waits")
        XCTAssertNil(appDelegate.waitingSettingsModel, "a model still locked goes after the limit")
        MainMenu.run(command: "settings")
        XCTAssertFalse(appDelegate.settingsWindowController?.model === model, "Settings starts afresh")
        // Leaves no write running into the next test.
        try await waitUntil("the delete written") { !model.isWriting }
    }

    /// A closed window's model goes ``AppDelegate/settingsWaitLimit`` after
    /// the close, applied or not (a watch that never applies must not keep
    /// Settings locked): here the core refuses the delete (a profile of
    /// `profiles/*.toml`, removed by its key path: the pane's Delete is
    /// disabled for it), so no configuration is applied after it. Settings
    /// opened then starts afresh.
    func testAClosedWindowsWaitingModelGoesAfterTheWaitLimit() async throws {
        appDelegate.settingsWaitLimit = .milliseconds(300)
        try autoreleasepool {
            _ = try openSettings(
                config: "[general]\ndefault_profile = \"one\"\n\n[[profiles]]\nname = \"one\"\n",
                files: ["profiles/extra.toml": "command = \"/bin/cat\"\n"])
        }
        let closed = try await opened { controller in
            XCTAssertEqual(controller.model.profiles.map(\.name), ["one", "extra"])
            XCTAssertFalse(controller.model.deleteProfile(1), "a file's profile: refused in the model")
            controller.model.remove("profiles[1]")
            XCTAssertTrue(controller.model.profilesChanging)
            controller.close()
        }
        try await waitUntil("the controller is freed") { closed.controller == nil }
        try await waitUntil("the model goes after the wait limit") { closed.model == nil }
        MainMenu.run(command: "settings")
        let model = try XCTUnwrap(appDelegate.settingsWindowController?.model)
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(model.profiles.map(\.name), ["one", "extra"])
    }

    /// The toolbar item labelled `label`, pressed as a click on it does.
    private static func pressToolbarItem(_ label: String, in window: NSWindow) throws {
        let item = try XCTUnwrap(window.toolbar?.items.first { $0.label == label }, "\(label) in the toolbar")
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(item.action), to: item.target, from: item))
    }

    /// Selects `row` of the Profiles pane's list in `window` and types
    /// `text` into its Command field.
    private func typeCommand(_ text: String, row: Int, in window: NSWindow, showing shown: String) async throws {
        window.layoutIfNeeded()
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: row)
        try await waitUntil("the profile's fields are shown") { Self.commandField(in: window)?.stringValue == shown }
        let field = try XCTUnwrap(Self.commandField(in: window))
        XCTAssertTrue(window.makeFirstResponder(field))
        field.currentEditor()?.selectAll(nil)
        (field.currentEditor() as? NSTextView)?.insertText(text, replacementRange: NSRange(location: NSNotFound, length: 0))
        try await Task.sleep(for: .milliseconds(50))
    }

    /// The panes of the Settings window the app opened, in a key window:
    /// a list row is selected only by a click, which a window that is not
    /// key (a locked console) does not take. The toolbar comes along.
    private func appSettingsInAKeyWindow() throws -> (NSTabViewController, KeyControlledWindow) {
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        let tabs = try XCTUnwrap(controller.tabs)
        XCTAssertTrue(tabs is SettingsTabViewController)
        controller.window?.contentViewController = nil
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
        window.isKeyForTest = true
        window.contentViewController = tabs
        return (tabs, window)
    }

    /// Text typed into a profile's Command field is written when another
    /// pane is shown with the toolbar (the field goes with its pane before
    /// it sees the focus go), on the core, in the window the app opens.
    func testTypedCommandTextIsWrittenWhenAnotherPaneShows() async throws {
        _ = try openSettings(config: Self.threeProfilesTOML)
        let (tabs, window) = try appSettingsInAKeyWindow()
        defer {
            window.contentViewController = nil
            window.close()
        }
        try Self.pressToolbarItem("Profiles", in: window)
        XCTAssertEqual(tabs.selectedTabViewItemIndex, 2)
        try await typeCommand("echo typed", row: 1, in: window, showing: "/bin/cat")
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(try file().contains("echo typed"), "not written while typed")
        try Self.pressToolbarItem("General", in: window)
        XCTAssertEqual(tabs.selectedTabViewItemIndex, 0)
        try await waitUntil("the typed text written") { (try? self.lines())?.contains(#"command = "echo typed""#) ?? false }
        // Back on Profiles, two shows it.
        try Self.pressToolbarItem("Profiles", in: window)
        try await waitUntil("shown") {
            Self.views(NSTextField.self, in: window.contentView).contains { $0.stringValue == "echo typed" }
        }
    }

    /// A pane switch made without the toolbar (the tab view controller's
    /// selection, as ``SettingsWindowController/select(pane:)`` sets it)
    /// writes the typed text too, once, and the field's later focus loss
    /// writes nothing more.
    func testTypedCommandTextIsWrittenOnceWhenAnotherPaneIsSelected() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        let controller = SettingsWindowController(
            model: model, actions: SettingsActions(openConfigFile: {}), autosaveName: nil)
        let tabs = try XCTUnwrap(controller.tabs)
        controller.window?.contentViewController = nil
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
        window.isKeyForTest = true
        window.contentViewController = tabs
        defer {
            window.contentViewController = nil
            window.close()
            controller.close()
        }
        tabs.selectedTabViewItemIndex = 2
        try await typeCommand("echo typed", row: 1, in: window, showing: "/bin/cat")
        XCTAssertEqual(writer.calls, [], "not written while typed")
        tabs.selectedTabViewItemIndex = 3
        let typed = FakeWriter.Call(key: "profiles[1].command", value: .string("echo typed"))
        try await waitUntil("the typed text written") { writer.calls == [typed] }
        window.makeFirstResponder(nil)
        try await Task.sleep(for: .milliseconds(150))
        XCTAssertEqual(writer.calls, [typed], "once")
    }

    // MARK: #312

    /// Two `[[profiles]]` entries and two profiles of `profiles/`, as the
    /// core's JSON lists them.
    private static let entriesAndFiles = """
        {"general": {"default_profile": "one"}, "appearance": {"theme": "Tessera Dark", "opacity": 0.85},
         "profiles": [{"name": "one", "source": "config"},
                      {"name": "two", "command": "/bin/sh", "appearance": {"opacity": 0.5}, "source": "config"},
                      {"name": "extra", "command": "/bin/cat", "source": "profiles/extra.toml"},
                      {"name": "zeta", "appearance": {"theme": "Nord"}, "source": "profiles/zeta.toml"}]}
        """

    /// A copy is one add of the core's with the name and every key; once
    /// written it shows where the core put it (after the entries), the
    /// profiles of `profiles/` one further on, until the watch applies the
    /// file. A configuration that does not list it yet keeps it so.
    func testAProfileAddedShowsWhereTheCorePutItUntilApplied() async throws {
        let (model, source, writer) = try fakeModel(Self.entriesAndFiles)
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "extra", "zeta"])
        XCTAssertTrue(model.duplicateProfile(1))
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(
            writer.calls, [FakeWriter.added("two Copy", ["command": .string("/bin/sh"), "appearance.opacity": .float(0.5)])])
        XCTAssertEqual(model.takeSelectionRequest(), 2, "selected where the core put it")
        let shown = { model.profiles.map(\.name) }
        XCTAssertEqual(shown(), ["one", "two", "two Copy", "extra", "zeta"])
        XCTAssertEqual(model.profiles.map(\.index), [0, 1, 2, 3, 4])
        XCTAssertEqual(model.value("profiles[2].name"), .string("two Copy"))
        XCTAssertEqual(model.value("profiles[2].command"), .string("/bin/sh"))
        XCTAssertEqual(model.value("profiles[2].appearance.opacity"), .float(0.5))
        XCTAssertTrue(model.inherits(2, "appearance.theme"))
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/sh"), "the source stays")
        XCTAssertEqual(model.value("profiles[3].command"), .string("/bin/cat"), "extra, one further on")
        XCTAssertEqual(model.value("profiles[4].appearance.theme"), .string("Nord"), "zeta, one further on")
        XCTAssertNil(model.value("profiles[5].name"))
        XCTAssertTrue(model.profilesChanging)

        // An earlier edit's configuration, the profiles as they were.
        let before = try Self.json(Self.entriesAndFiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: before)
        model.reload()
        XCTAssertTrue(model.profilesChanging, "not applied yet")
        XCTAssertEqual(shown(), ["one", "two", "two Copy", "extra", "zeta"])

        // The watch applies it.
        let applied = Self.entriesAndFiles.replacingOccurrences(
            of: #"{"name": "extra""#,
            with: #"{"name": "two Copy", "command": "/bin/sh", "appearance": {"opacity": 0.5}, "source": "config"}, {"name": "extra""#)
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(applied))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(shown(), ["one", "two", "two Copy", "extra", "zeta"])
        XCTAssertEqual(model.value("profiles[3].command"), .string("/bin/cat"))
        XCTAssertNil(model.takeSelectionRequest(), "selected once")
    }

    /// A configuration that lists other profiles than the add was queued
    /// against (the file was edited by hand too, the new one renamed) ends
    /// the add: the list shows the file as applied, unlocked.
    func testAnAddAppliedWithAHandEditEndsIt() async throws {
        let (model, source, _) = try fakeModel(Self.entriesAndFiles)
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertTrue(model.profilesChanging)
        let edited = Self.entriesAndFiles.replacingOccurrences(
            of: #"{"name": "extra""#, with: #"{"name": "renamed", "source": "config"}, {"name": "extra""#)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(edited))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "renamed", "extra", "zeta"])
    }

    /// An add that meets an external edit of the file (`Conflict`) is
    /// written again once, and the list waits for a configuration after
    /// the external edit's.
    func testAnAddThatMeetsAConflictIsRetriedOnce() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        let external = try Self.json(Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/zsh"))
        let applied = source.applied
        writer.respond = { call, attempt in
            guard call.key == FakeWriter.addKey, attempt == 1 else { return nil }
            applied.value = TesseraConfigSnapshot(generation: 1, config: external)
            return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
        }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(writer.calls, [FakeWriter.added("New Profile"), FakeWriter.added("New Profile")])
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        XCTAssertEqual(model.takeSelectionRequest(), 3)
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/zsh"), "reloaded: the external edit stays")
        XCTAssertTrue(model.profilesChanging, "the external edit's configuration does not show the add")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three", "New Profile"])
    }

    /// A retried add the watch applied before its result came back (with
    /// the external edit) unlocks the list at once: no later configuration
    /// is waited for.
    func testARetriedAddAlreadyAppliedUnlocksAtOnce() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        let external = try Self.json(Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/zsh"))
        let both = try Self.json(
            Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/zsh")
                .replacingOccurrences(of: #"{"name": "three"}"#, with: #"{"name": "three"}, {"name": "New Profile"}"#))
        XCTAssertEqual(both["profiles"]?.array?.count, 4, "the fixture adds the profile")
        let applied = source.applied
        writer.respond = { call, attempt in
            guard call.key == FakeWriter.addKey else { return nil }
            if attempt == 1 {
                applied.value = TesseraConfigSnapshot(generation: 1, config: external)
                return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
            }
            applied.value = TesseraConfigSnapshot(generation: 2, config: both)
            return nil
        }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.takeSelectionRequest(), 3)
        XCTAssertFalse(model.profilesChanging, "the reload shows the add")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three", "New Profile"])
    }

    /// A key of a profile past the listed ones is not written: it would
    /// add a profile without a name (a new one goes through the core's add).
    func testAKeyOfAProfilePastTheListedOnesIsNotWritten() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        model.set("profiles[3].command", .string("/bin/sh"))
        model.change("profiles[3].appearance.opacity", .float(0.4))
        model.endChange("profiles[3].appearance.opacity")
        model.set("profiles[2].command", .string("/bin/sh"))
        try await waitUntil("written") { !model.isWriting && !writer.calls.isEmpty }
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [.init(key: "profiles[2].command", value: .string("/bin/sh"))])
        XCTAssertNil(model.value("profiles[3].command"))
        XCTAssertFalse(model.profilesChanging)
    }

    /// On the core, with profiles in `profiles/`: Add and Duplicate (of a
    /// file's profile too) land after the `[[profiles]]` entries with the
    /// name the core gave them (`New Profile` is a file's, so `New Profile
    /// 2`), the files' profiles move up one, and `profiles/` is not
    /// written.
    func testAddAndDuplicateWithProfileFilesOnTheCore() async throws {
        let files = [
            "profiles/extra.toml": "command = \"/bin/cat\"\n\n[appearance]\nopacity = 0.5\n",
            "profiles/taken.toml": "name = \"New Profile\"\n",
        ]
        let model = try openSettings(
            config: """
                [general]
                default_profile = "one"

                [[profiles]]
                name = "one"

                [[profiles]]
                name = "two"
                command = "/bin/sh"

                """, files: files)
        let shown = { model.profiles.map(\.name) }
        XCTAssertEqual(shown(), ["one", "two", "extra", "New Profile"])

        XCTAssertTrue(model.addProfile())
        try await waitUntil("added") { !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        XCTAssertEqual(model.takeSelectionRequest(), 2)
        XCTAssertTrue(try lines().contains(#"name = "New Profile 2""#), contents)
        try await waitUntil("applied") {
            !model.profilesChanging && shown() == ["one", "two", "New Profile 2", "extra", "New Profile"]
        }

        XCTAssertTrue(model.duplicateProfile(3))
        try await waitUntil("duplicated") { !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        XCTAssertEqual(model.takeSelectionRequest(), 3)
        try await waitUntil("applied") {
            !model.profilesChanging && shown() == ["one", "two", "New Profile 2", "extra Copy", "extra", "New Profile"]
        }
        XCTAssertEqual(model.snapshot["profiles[3].command"], .string("/bin/cat"))
        XCTAssertTrue(Self.same(model.snapshot["profiles[3].appearance.opacity"], .float(0.5)))
        XCTAssertEqual(model.snapshot["profiles[3].source"], .string("config"))
        XCTAssertEqual(model.snapshot["profiles[4].source"], .string("profiles/extra.toml"))
        XCTAssertEqual(try lines().filter { $0 == "[[profiles]]" }.count, 4, contents)
        let dir = try XCTUnwrap(dir)
        for (path, text) in files {
            XCTAssertEqual(try String(contentsOf: dir.appendingPathComponent(path), encoding: .utf8), text, path)
        }
    }

    /// On the core, with a profile file present: a copy of a profile with
    /// a value the loader warns about is refused as a whole, with the
    /// core's message naming the key; nothing is written and the list is
    /// not left locked.
    func testADuplicateOfAProfileWithARefusedValueWritesNothing() async throws {
        let model = try openSettings(
            config: """
                [[profiles]]
                name = "odd"
                command = "/bin/cat"

                [profiles.appearance]
                theme = "No Such Theme"

                """, files: ["profiles/extra.toml": "command = \"/bin/sh\"\n"])
        XCTAssertEqual(model.profiles.map(\.name), ["odd", "extra"])
        XCTAssertEqual(model.value("profiles[0].appearance.theme"), .string("No Such Theme"), "the snapshot has it")
        let before = try file()
        XCTAssertTrue(model.duplicateProfile(0))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertFalse(model.profilesChanging)
        let message = try XCTUnwrap(model.message(SettingsModel.profilesKey))
        XCTAssertTrue(message.contains("profiles[1].appearance.theme"), "the core names the key: \(message)")
        XCTAssertEqual(message, SettingsModel.displayMessage(.init(.refused, message: message)), "sanitised")
        XCTAssertEqual(try file(), before, "nothing written")
        XCTAssertEqual(model.profiles.map(\.name), ["odd", "extra"])
    }

    /// The profile appearance fields leave out exactly the keys the window
    /// draws or the app shares.
    func testTheProfileAppearanceFieldsLeaveOutTheWindowAndAppKeys() {
        let left = Set(AppearancePane.fields.map(\.key)).subtracting(ProfilesPane.appearanceFields.map(\.key))
        XCTAssertEqual(left, ["appearance.dim_inactive_panes", "appearance.app_appearance"])
    }
}

// MARK: Stand-ins

// MARK: The Profiles lock's release, file profiles, the cap (#344, #489)

extension SettingsTests {
    /// The text the SwiftUI views in `view` show, as their accessibility
    /// elements give it (labels and values), depth first.
    private static func shownTexts(in view: NSView?) -> [String] {
        var texts: [String] = []
        func walk(_ element: Any, depth: Int) {
            guard depth < 80, let element = element as? NSAccessibilityProtocol else { return }
            if let label = element.accessibilityLabel(), !label.isEmpty { texts.append(label) }
            if let value = element.accessibilityValue() as? String, !value.isEmpty { texts.append(value) }
            for child in element.accessibilityChildren() ?? [] { walk(child, depth: depth + 1) }
        }
        func walkViews(_ view: NSView) {
            walk(view, depth: 0)
            for subview in view.subviews { walkViews(subview) }
        }
        if let view { walkViews(view) }
        return texts
    }

    /// The list's buttons in `window`, left to right: Add, Duplicate,
    /// Delete.
    private static func listButtons(in window: NSWindow) -> [NSButton] {
        views(NSButton.self, in: window.contentView)
            .filter { $0.convert($0.bounds, to: nil).minX < 200 }
            .sorted { $0.convert($0.bounds, to: nil).minX < $1.convert($1.bounds, to: nil).minX }
    }

    /// A Profiles pane on `model` in a key window of the test's own.
    private func profilesPane(_ model: SettingsModel) -> KeyControlledWindow {
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 680, height: 600))
        window.isKeyForTest = true
        window.contentView = NSHostingView(rootView: ProfilesPane(model: model))
        window.layoutIfNeeded()
        return window
    }

    /// #312 adversarial M4, on the core, in the window the app opens:
    /// another app that read config.toml before an Add saves it again just
    /// after, without the profile. No configuration that shows the add
    /// applies, so the Profiles lock goes the settle time after the add
    /// was written (2 s, plus the apply's read), and the list says the add
    /// was overwritten.
    func testAnAddAStaleWriterDropsUnlocksTheProfilesPaneWithANotice() async throws {
        let model = try openSettings(config: Self.threeProfilesTOML)
        let (_, window) = try appSettingsInAKeyWindow()
        defer {
            window.contentViewController = nil
            window.close()
        }
        try Self.pressToolbarItem("Profiles", in: window)
        window.layoutIfNeeded()
        let original = try file()
        let attempts = 3
        for _ in 1...attempts {
            if try await staleWriterDropsTheAdd(model, window, original) { return }
            // The watch applied the add before the rewrite, then the
            // rewrite as a hand edit: back to the three profiles.
            try await waitUntil("the rewrite applied") {
                model.profiles.map(\.name) == ["one", "two", "three"] && !model.profilesChanging
            }
        }
        throw XCTSkip(
            "in each of \(attempts) tries the other app's rewrite came after the watch's 100 ms debounce, "
                + "so a configuration showing the add applied first (a loaded machine)")
    }

    /// One try of the stale writer: `false` if the rewrite came too late
    /// (the watch applied the add first, the lock went with no notice),
    /// so that what this test is about did not happen.
    private func staleWriterDropsTheAdd(
        _ model: SettingsModel, _ window: NSWindow, _ original: String
    ) async throws -> Bool {
        let url = try configFile
        // The other app: as soon as the add is in the file, its stale copy
        // goes over it (inside the watch's 100 ms debounce, unless the
        // machine is loaded).
        let stop = Locked(false)
        let rewrote = Locked(false)
        let staleWriter = Thread {
            let deadline = Date(timeIntervalSinceNow: 10)
            while !stop.value, Date() < deadline {
                if let text = try? String(contentsOf: url, encoding: .utf8), text.contains("New Profile") {
                    try? original.write(to: url, atomically: true, encoding: .utf8)
                    rewrote.value = true
                    return
                }
                usleep(500)
            }
        }
        staleWriter.start()
        defer { stop.value = true }
        let raced = { !model.profilesChanging && model.message(SettingsModel.profilesKey) == nil }

        let start = ContinuousClock.now
        XCTAssertTrue(model.addProfile())
        try await waitUntil("the add written and overwritten") { rewrote.value && !model.isWriting }
        if raced() { return false }
        let waiting = "Waiting for config.toml to be applied; the profiles can be edited once it is."
        try await waitUntil("the pane says it waits, Add disabled") {
            !model.profilesChanging
                || (Self.shownTexts(in: window.contentView).contains(waiting)
                    && Self.listButtons(in: window).first?.isEnabled == false)
        }
        if raced() { return false }
        XCTAssertTrue(model.profilesChanging, "locked: no configuration applied shows the add")
        XCTAssertEqual(model.profilesStatus, waiting)
        try await waitUntil("the lock goes") { !model.profilesChanging }
        if raced() { return false }
        let took = ContinuousClock.now - start
        XCTAssertGreaterThanOrEqual(took, .milliseconds(1900), "by the settle time, not by an apply")
        XCTAssertLessThan(took, .seconds(4), "the settle time and one apply")
        let notice = try XCTUnwrap(model.message(SettingsModel.profilesKey))
        XCTAssertEqual(
            notice,
            "“New Profile” was written but is not in the configuration applied: another app may have saved "
                + "config.toml without it.")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three"])
        XCTAssertNil(model.profilesStatus)
        XCTAssertTrue(model.canAddProfile)
        // The pane: unlocked, and the notice by the list (its message,
        // which SwiftUI draws without a view to read: E6 sees it).
        try await waitUntil("the pane is unlocked") {
            Self.listButtons(in: window).first?.isEnabled == true
                && !Self.shownTexts(in: window.contentView).contains(waiting)
        }
        XCTAssertEqual(try file(), original)
        return true
    }

    /// The same on stand-ins, to the millisecond: written, the add locks
    /// the list until a configuration shows it, at most
    /// ``SettingsModel/profileSettleTimeout``; one that never does (here
    /// another app's rewrite applied with the profiles as they were)
    /// unlocks it then, with the notice by it, the rows after it and their
    /// messages back where they were (#312 adversarial M3).
    func testAnAddAnotherAppDroppedUnlocksAfterTheSettleTimeWithANotice() async throws {
        let clock = ManualClock()
        let writer = FakeWriter()
        let refusal = "`profiles[2].command`: this profile is defined in a file in profiles/, not in config.toml"
        writer.respond = { call, _ in
            call.key == "profiles[2].command" ? TesseraConfigEditError(.invalidArgument, message: refusal) : nil
        }
        let (model, source, _) = try fakeModel(Self.entriesAndFiles, writer: writer, clock: clock)
        let names = ["one", "two", "extra", "zeta"]
        XCTAssertEqual(model.profiles.map(\.name), names)
        model.set("profiles[2].command", .string("/bin/sh"))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertEqual(model.message("profiles[2].command"), refusal)

        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "New Profile", "extra", "zeta"])
        XCTAssertEqual(model.takeSelectionRequest(), 2)
        XCTAssertNil(model.message("profiles[2].command"), "adv M3: not on the new profile")
        XCTAssertEqual(model.message("profiles[3].command"), refusal, "adv M3: moved with extra")
        let generation = model.profileListGeneration

        // Another app's save from before the add is applied.
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(Self.entriesAndFiles))
        model.reload()
        XCTAssertTrue(model.profilesChanging, "a configuration without the add")
        clock.advance(by: .milliseconds(1990))
        XCTAssertTrue(model.profilesChanging, "locked until the settle time")
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        clock.advance(by: .milliseconds(20))
        XCTAssertFalse(model.profilesChanging, "unlocked at the settle time")
        XCTAssertEqual(
            model.message(SettingsModel.profilesKey),
            "“New Profile” was written but is not in the configuration applied: another app may have saved "
                + "config.toml without it.")
        XCTAssertEqual(model.profiles.map(\.name), names)
        XCTAssertEqual(model.message("profiles[2].command"), refusal, "back with extra")
        XCTAssertNil(model.message("profiles[3].command"))
        XCTAssertEqual(model.profileListGeneration, generation + 1, "the controls start again")
        XCTAssertTrue(model.canAddProfile)
    }

    /// A delete another app's save undid unlocks the list the settle time
    /// after it was written, with a notice; one the watch applies in time
    /// leaves no timer behind.
    func testADeleteAnotherAppUndidUnlocksAfterTheSettleTimeWithANotice() async throws {
        let clock = ManualClock()
        let (model, source, _) = try fakeModel(Self.threeProfiles, clock: clock)
        let deleted = Self.threeProfiles.replacingOccurrences(of: #", {"name": "three"}"#, with: "")
        XCTAssertNotEqual(deleted, Self.threeProfiles)

        XCTAssertTrue(model.deleteProfile(2))
        try await waitUntil("written") { !model.isWriting }
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(deleted))
        model.reload()
        XCTAssertFalse(model.profilesChanging, "applied")
        clock.advance(by: .seconds(3))
        XCTAssertNil(model.message(SettingsModel.profilesKey), "applied in time: no notice")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two"])

        XCTAssertTrue(model.deleteProfile(1))
        try await waitUntil("written") { !model.isWriting }
        XCTAssertTrue(model.profilesChanging)
        // Another app's save from before the delete is applied.
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(deleted))
        model.reload()
        XCTAssertTrue(model.profilesChanging, "a configuration with the profile deleted")
        clock.advance(by: .milliseconds(1990))
        XCTAssertTrue(model.profilesChanging)
        clock.advance(by: .milliseconds(20))
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(
            model.message(SettingsModel.profilesKey),
            "The profile deleted is still in the configuration applied: another app may have saved config.toml "
                + "with it.")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two"])
        XCTAssertEqual(model.value("profiles[1].name"), .string("two"))
    }

    /// #312 m4, on the core, in the window the app opens: a profile of
    /// `profiles/` shows its file (the name sanitised, S3), its controls
    /// are read-only and Delete is disabled;
    /// the model refuses to delete it, and an edit by key path shows the
    /// core's refusal verbatim (21.4 M6).
    func testAProfileFileShowsItsFileAndIsReadOnly() async throws {
        let model = try openSettings(
            config: Self.threeProfilesTOML, files: ["profiles/evil\u{202E}txt.toml": "command = \"/bin/cat\"\n"])
        XCTAssertEqual(model.profiles.count, 4)
        XCTAssertEqual(model.snapshot["profiles[3].source"], .string("profiles/evil\u{202E}txt.toml"), "raw")
        let profile = try XCTUnwrap(model.profiles.last)
        XCTAssertEqual(profile.file, "profiles/eviltxt.toml", "sanitised (S3)")
        XCTAssertNil(model.profiles[1].file, "an entry of config.toml")
        let note = "Defined in profiles/eviltxt.toml. Edit that file to change this profile."
        XCTAssertEqual(ProfilesPane.fileNote(try XCTUnwrap(profile.file)), note)

        let (_, window) = try appSettingsInAKeyWindow()
        defer {
            window.contentViewController = nil
            window.close()
        }
        try Self.pressToolbarItem("Profiles", in: window)
        window.layoutIfNeeded()
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 4
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        let editable = { Self.views(NSTextField.self, in: window.contentView).filter { $0.isEditable && $0.isEnabled } }
        // The profile's controls: fields, switches (Inherit too), steppers
        // and pickers, right of the list.
        let controls = {
            Self.views(NSControl.self, in: window.contentView).filter { $0.convert($0.bounds, to: nil).minX > 200 }
        }
        // An entry of config.toml, for comparison.
        Self.click(table, row: 1)
        try await waitUntil("two's fields are shown") { Self.commandField(in: window)?.stringValue == "/bin/cat" }
        XCTAssertEqual(editable().count, 3, "name, command and working directory: \(editable().map(\.stringValue))")
        XCTAssertTrue(controls().contains(where: \.isEnabled))
        XCTAssertTrue(Self.listButtons(in: window).last?.isEnabled ?? false, "Delete")
        XCTAssertFalse(Self.shownTexts(in: window.contentView).contains(note))

        Self.click(table, row: 3)
        try await waitUntil("the file's note is shown") { Self.shownTexts(in: window.contentView).contains(note) }
        XCTAssertEqual(editable().map(\.stringValue), [], "no field is editable")
        XCTAssertGreaterThan(controls().count, 10)
        // The note is selectable text: on macOS 15 SwiftUI backs it with
        // an enabled, non-editable NSTextField (`SelectionTextField`), on
        // later systems with a view that is no NSControl. A label is no
        // input, so it is set apart, and it may only be the note.
        let isLabel = { (control: NSControl) in (control as? NSTextField).map { !$0.isEditable } ?? false }
        XCTAssertEqual(
            controls().filter { $0.isEnabled && !isLabel($0) }.map { "\(type(of: $0))" }, [], "no control is enabled")
        XCTAssertEqual(
            Set(controls().filter { $0.isEnabled && isLabel($0) }.map(\.stringValue)).subtracting([note]), [],
            "the only enabled label is the note")
        let buttons = Self.listButtons(in: window)
        XCTAssertEqual(buttons.count, 3)
        XCTAssertTrue(buttons[0].isEnabled, "Add")
        XCTAssertTrue(buttons[1].isEnabled, "a file's profile can be copied into config.toml")
        XCTAssertFalse(buttons[2].isEnabled, "Delete")
        XCTAssertFalse(
            Self.shownTexts(in: window.contentView).contains { $0.contains("profiles/evil\u{202E}") },
            "the file name is never shown raw")

        XCTAssertFalse(model.deleteProfile(3))
        XCTAssertEqual(
            model.message(SettingsModel.profilesKey),
            "“eviltxt” is defined in profiles/eviltxt.toml; delete that file to remove it.")
        let before = try file()
        model.set("profiles[3].command", .string("/bin/sh"))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertEqual(
            model.message("profiles[3].command"),
            "`profiles[3].command`: this profile is defined in a file in profiles/, not in config.toml; "
                + "edit that file instead",
            "the core's message, verbatim")
        XCTAssertEqual(model.value("profiles[3].command"), .string("/bin/cat"))
        XCTAssertEqual(try file(), before)
    }

    /// 21.4 adversarial M1: the core's `source` carries control, bidi and
    /// line separator characters as the file name has them; the window
    /// shows it sanitised (S3).
    func testAProfileSourceIsShownSanitised() throws {
        let (model, _, _) = try fakeModel(
            #"{"profiles": [{"name": "x", "source": "profiles/a\u001b]0;t\u0007‮b c\u0085.toml"}]}"#)
        let raw = try XCTUnwrap(model.snapshot["profiles[0].source"]?.string)
        XCTAssertTrue(raw.unicodeScalars.contains("\u{202E}"))
        let file = try XCTUnwrap(model.profiles.first?.file)
        let unsafe: Set<Unicode.GeneralCategory> = [.control, .format, .lineSeparator, .paragraphSeparator]
        for text in [file, ProfilesPane.fileNote(file)] {
            XCTAssertFalse(text.unicodeScalars.contains { unsafe.contains($0.properties.generalCategory) }, text)
        }
        XCTAssertEqual(file, SettingsModel.sanitisedMessage(raw))
        XCTAssertTrue(file.hasPrefix("profiles/a"), file)
        XCTAssertTrue(file.hasSuffix(".toml"), file)
    }

    /// 21.6a m4, #344 m-f, in the pane: at the cap Add and Duplicate are
    /// disabled with the reason under the list (their help says it too, an
    /// eye check: SwiftUI's help is not a view's tool tip); while a delete
    /// waits the line says so; under the cap neither shows.
    ///
    /// The waiting line is read in a pane opened while the delete waits,
    /// as Settings opened again shows it. On CI (PR #584: macOS 27.0.1,
    /// Xcode 27.1, app active) the pane open before it kept the cap line
    /// through the whole 2 s lock; the cause (OS, activation) is not
    /// known. #585 tracks it and restores the in-place check.
    func testThePaneSaysWhyAddIsDisabledAtTheCapAndWhileItWaits() async throws {
        let profiles = (0..<256).map { #"{"name": "p\#($0)"}"# }.joined(separator: ", ")
        let (model, source, _) = try fakeModel(#"{"general": {"default_profile": "p0"}, "profiles": [\#(profiles)]}"#)
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        let cap = "Tessera reads at most 256 profiles, those in profiles/ included; delete one to add another."
        let waiting = "Waiting for config.toml to be applied; the profiles can be edited once it is."
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 256
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: 0)
        try await waitUntil("p0's fields are shown") { Self.commandField(in: window) != nil }
        XCTAssertEqual(model.profilesStatus, cap)
        XCTAssertTrue(Self.shownTexts(in: window.contentView).contains(cap), "the status line")
        let buttons = Self.listButtons(in: window)
        XCTAssertEqual(buttons.count, 3)
        XCTAssertFalse(buttons[0].isEnabled, "Add")
        XCTAssertFalse(buttons[1].isEnabled, "Duplicate")
        XCTAssertTrue(buttons[2].isEnabled, "Delete")

        // The lock holds until the delete applies, however long the panes
        // take to read.
        model.profileSettleTimeout = .seconds(60)
        XCTAssertTrue(model.deleteProfile(5))
        XCTAssertEqual(model.profilesStatus, waiting)
        let reopened = profilesPane(model)
        defer {
            reopened.contentView = nil
            reopened.close()
        }
        try await waitUntil("the pane says it waits") { Self.shownTexts(in: reopened.contentView).contains(waiting) }
        XCTAssertFalse(Self.shownTexts(in: reopened.contentView).contains(cap))
        XCTAssertFalse(Self.listButtons(in: reopened).first?.isEnabled ?? true, "Add")
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.profilesStatus, waiting, "written, not yet applied")
        let fewer = (0..<256).filter { $0 != 5 }.map { #"{"name": "p\#($0)"}"# }.joined(separator: ", ")
        source.snapshot = TesseraConfigSnapshot(
            generation: 1, config: try Self.json(#"{"general": {"default_profile": "p0"}, "profiles": [\#(fewer)]}"#))
        model.reload()
        // The buttons and the status line's view update apart: wait for both.
        try await waitUntil("Add is enabled and no status line is shown") {
            [window, reopened].allSatisfy { pane in
                let shown = Self.shownTexts(in: pane.contentView)
                return Self.listButtons(in: pane).first?.isEnabled == true
                    && !shown.contains(cap) && !shown.contains(waiting)
            }
        }
        XCTAssertNil(model.profilesStatus)
    }

    /// #585: a pane open at the cap shows the waiting line in place of the
    /// cap line as soon as a delete waits, within the 2 s settle. On CI's
    /// macOS 27.0.1 (PR #584) the selectable line kept the cap line for the
    /// whole lock; locally (macOS 27.2) it changed in place either way.
    func testAnOpenPaneShowsTheWaitingLineInPlaceOfTheCapLine() async throws {
        let profiles = (0..<256).map { #"{"name": "p\#($0)"}"# }.joined(separator: ", ")
        let (model, source, _) = try fakeModel(#"{"general": {"default_profile": "p0"}, "profiles": [\#(profiles)]}"#)
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        let cap = "Tessera reads at most 256 profiles, those in profiles/ included; delete one to add another."
        let waiting = "Waiting for config.toml to be applied; the profiles can be edited once it is."
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 256
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: 0)
        try await waitUntil("p0's fields are shown") { Self.commandField(in: window) != nil }
        XCTAssertTrue(Self.shownTexts(in: window.contentView).contains(cap), "the status line")

        // The lock holds until the test applies the delete, however slow
        // the pane is to read: a stale line cannot pass by the lock ending.
        model.profileSettleTimeout = .seconds(60)
        XCTAssertTrue(model.deleteProfile(5))
        XCTAssertEqual(model.profilesStatus, waiting)
        try await waitUntil("the open pane says it waits, not the cap") {
            let shown = Self.shownTexts(in: window.contentView)
            return shown.contains(waiting) && !shown.contains(cap)
        }
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.profilesStatus, waiting, "written, not yet applied")
        let fewer = (0..<256).filter { $0 != 5 }.map { #"{"name": "p\#($0)"}"# }.joined(separator: ", ")
        source.snapshot = TesseraConfigSnapshot(
            generation: 1, config: try Self.json(#"{"general": {"default_profile": "p0"}, "profiles": [\#(fewer)]}"#))
        model.reload()
        XCTAssertNil(model.profilesStatus)
    }

    /// #585: an open pane shows a new load error in place of the one it
    /// showed, in the text and to VoiceOver.
    func testAnOpenPaneShowsANewLoadErrorInPlaceOfTheOldOne() async throws {
        let (model, source, _) = try fakeModel(Self.threeProfiles)
        source.snapshot.loadError = "line 1: first"
        model.reload()
        let first = SettingsModel.loadErrorMessage("line 1: first")
        let second = SettingsModel.loadErrorMessage("line 2: second")
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        try await waitUntil("the first error is shown") { Self.shownTexts(in: window.contentView).contains(first) }

        source.snapshot.loadError = "line 2: second"
        model.reload()
        XCTAssertEqual(model.loadError, second)
        try await waitUntil("the second error is shown, not the first") {
            let shown = Self.shownTexts(in: window.contentView)
            return shown.contains(second) && !shown.contains(first)
        }
    }

    /// #585: switching between two profiles of `profiles/` shows the
    /// selected one's file, never the other's.
    func testAnOpenPaneShowsTheFileOfEachSelectedProfile() async throws {
        let (model, _, _) = try fakeModel(Self.entriesAndFiles)
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        let extra = ProfilesPane.fileNote("profiles/extra.toml")
        let zeta = ProfilesPane.fileNote("profiles/zeta.toml")
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 4
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        for (row, note, other) in [(2, extra, zeta), (3, zeta, extra), (2, extra, zeta)] {
            Self.click(table, row: row)
            try await waitUntil("row \(row) shows its own file") {
                let shown = Self.shownTexts(in: window.contentView)
                return shown.contains(note) && !shown.contains(other)
            }
        }
    }

    /// #312 m2, mutant M7: a second add clears the message the first one's
    /// failure left by the list as it starts.
    func testASecondAddClearsTheFirstAddsMessage() async throws {
        let writer = FakeWriter()
        writer.respond = { call, attempt in
            call.key == FakeWriter.addKey && attempt == 1 ? TesseraConfigEditError(.io, message: "disk full") : nil
        }
        let (model, _, _) = try fakeModel(Self.threeProfiles, writer: writer)
        XCTAssertTrue(model.addProfile())
        try await waitUntil("failed") { !model.isWriting }
        XCTAssertEqual(model.message(SettingsModel.profilesKey), "disk full")
        XCTAssertTrue(model.addProfile())
        XCTAssertNil(model.message(SettingsModel.profilesKey), "M7: the next add clears it")
        try await waitUntil("written") { !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
    }

    /// #312 m2, mutant M15: while an add runs, a profile's key is not
    /// written by its key path (the index may name another profile once
    /// it applies).
    func testAProfileKeyIsNotWrittenWhileAnAddRuns() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        XCTAssertTrue(model.addProfile())
        model.set("profiles[1].command", .string("/bin/sh"))
        model.remove("profiles[1].command")
        XCTAssertFalse(model.isPending("profiles[1].command"), "M15: refused while the add runs")
        try await waitUntil("written") { !model.isWriting }
        model.set("profiles[1].command", .string("/bin/sh"))
        XCTAssertFalse(model.isPending("profiles[1].command"), "M15: refused until the add applies")
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [FakeWriter.added("New Profile")])
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/cat"))
    }

    /// #312 m2, mutant M6: an add that failed after its retry reads the
    /// configuration again, so the external edit the conflict was over
    /// shows.
    func testARetriedAddThatFailsShowsTheExternalEdit() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        let external = try Self.json(Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/zsh"))
        let applied = source.applied
        writer.respond = { call, attempt in
            guard call.key == FakeWriter.addKey else { return nil }
            if attempt == 1 {
                applied.value = TesseraConfigSnapshot(generation: 1, config: external)
                return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
            }
            return TesseraConfigEditError(.io, message: "disk full")
        }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("failed") { !model.isWriting }
        XCTAssertEqual(model.message(SettingsModel.profilesKey), "disk full")
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/zsh"), "M6: read again")
        XCTAssertEqual(model.snapshot.generation, 1)
    }

    /// 21.6a m6, #549: a delete that meets a conflict whose external edit
    /// (of another key) the watch applied before the conflict came back:
    /// no later configuration comes, so it is written again at the same
    /// index once the settle time has passed, and unlocks the list when
    /// it applies.
    func testARetriedDeleteWhoseExternalEditAppliedAlreadyIsWrittenAfterTheSettleTime() async throws {
        let clock = ManualClock()
        let writer = FakeWriter()
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer, clock: clock)
        let nord = Self.threeProfiles.replacingOccurrences(of: "Tessera Dark", with: "Nord")
        let external = try Self.json(nord)
        let both = try Self.json(nord.replacingOccurrences(of: #"{"name": "two", "command": "/bin/cat"}, "#, with: ""))
        XCTAssertEqual(both["profiles"]?.array?.count, 2, "the fixture deletes two")
        let applied = source.applied
        writer.respond = { call, attempt in
            guard call.key == "profiles[1]" else { return nil }
            if attempt == 1 {
                applied.value = TesseraConfigSnapshot(generation: 1, config: external)
                return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
            }
            applied.value = TesseraConfigSnapshot(generation: 2, config: both)
            return nil
        }
        let reads = source.pluginReads
        XCTAssertTrue(model.deleteProfile(1))
        try await waitUntil("the conflict came back") { source.pluginReads > reads }
        XCTAssertEqual(model.value("appearance.theme"), .string("Nord"), "the external edit shows")
        XCTAssertEqual(writer.calls.count, 1)
        XCTAssertTrue(model.profilesChanging)
        clock.advance(by: model.profileSettleTimeout)
        try await waitUntil("written again") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "profiles[1]", value: nil), .init(key: "profiles[1]", value: nil)])
        model.reload()
        XCTAssertFalse(model.profilesChanging, "the delete applied")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "three"])
        clock.advance(by: .seconds(3))
        XCTAssertNil(model.message(SettingsModel.profilesKey), "no settle notice either")
    }

    /// The configuration of ``threeProfiles`` with "one" removed by hand
    /// (and the default profile moved to "two").
    private static let firstProfileRemoved = """
        {"general": {"default_profile": "two"}, "appearance": {"theme": "Tessera Dark"},
         "profiles": [{"name": "two", "command": "/bin/cat"}, {"name": "three"}]}
        """

    /// Sets the writer to refuse its next call as a conflict
    /// (an editor saved the file in between; the watch has not applied it
    /// yet), runs `edit` and waits until the conflict came back (the model
    /// read the configuration again).
    private func conflictOnFirstAttempt(
        _ model: SettingsModel, _ source: FakeSource, _ writer: FakeWriter, _ edit: () -> Void
    ) async throws {
        let first = Locked(true)
        writer.respond = { _, _ in
            guard first.value else { return nil }
            first.value = false
            return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
        }
        let reads = source.pluginReads
        edit()
        try await waitUntil("the conflict came back") { source.pluginReads > reads }
        // A reload before the hand edit applies (another write finishing)
        // must not retry by the old list.
        model.reload()
        XCTAssertEqual(writer.calls.count, 1, "not written again before the hand edit applies")
        XCTAssertTrue(model.isWriting)
    }

    /// #549: a profile's edit that meets a hand edit (`Conflict`) which
    /// removed an earlier profile is written again, once the hand edit
    /// applies, at the index its profile has now, not at the one it had.
    func testAProfileEditRetriedAfterAHandEditRemovedAnEarlierProfileGoesToItsProfile() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
        try await conflictOnFirstAttempt(model, source, writer) {
            model.set("profiles[1].command", .string("/bin/sh"))
        }
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/sh"), "shown while it waits")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(Self.firstProfileRemoved))
        model.reload()
        try await waitUntil("written again") { !model.isWriting }
        XCTAssertEqual(
            writer.calls,
            [.init(key: "profiles[1].command", value: .string("/bin/sh")),
             .init(key: "profiles[0].command", value: .string("/bin/sh"))],
            "written to “two”, now the first, not to “three”")
        XCTAssertEqual(model.profiles.map(\.name), ["two", "three"])
        XCTAssertEqual(model.value("profiles[0].command"), .string("/bin/sh"), "shown on “two”")
        XCTAssertNil(model.value("profiles[1].command"), "“three” keeps its own (none)")
        XCTAssertNil(model.message(SettingsModel.profilesKey))
    }

    /// #549: a delete that meets a hand edit which removed an earlier
    /// profile deletes its own profile, now one earlier.
    func testADeleteRetriedAfterAHandEditRemovedAnEarlierProfileDeletesItsProfile() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
        try await conflictOnFirstAttempt(model, source, writer) {
            XCTAssertTrue(model.deleteProfile(2))
        }
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(Self.firstProfileRemoved))
        model.reload()
        try await waitUntil("written again") { !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: "profiles[2]", value: nil), .init(key: "profiles[1]", value: nil)])
        XCTAssertTrue(model.profilesChanging, "the delete of “three” waits to apply")
    }

    /// #549: a profile's edit (and a rename, with the default profile it
    /// moves) that meets a hand edit which removed that profile is
    /// dropped, and the list says the value was not saved; a delete of it
    /// has nothing left to do.
    func testAProfileEditRetriedAfterAHandEditRemovedItsProfileIsDropped() async throws {
        let twoRemoved = """
            {"general": {"default_profile": "one"}, "appearance": {"theme": "Tessera Dark"},
             "profiles": [{"name": "one"}, {"name": "three"}]}
            """
        for edit in ["command", "rename", "delete"] {
            let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
            try await conflictOnFirstAttempt(model, source, writer) {
                switch edit {
                case "command": model.set("profiles[1].command", .string("/bin/sh"))
                case "rename": model.renameProfile(1, to: "Deux")
                default: XCTAssertTrue(model.deleteProfile(1))
                }
            }
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(twoRemoved))
            model.reload()
            try await waitUntil("dropped") { !model.isWriting }
            XCTAssertEqual(writer.calls.count, 1, "\(edit): not written again: \(writer.calls)")
            XCTAssertFalse(model.profilesChanging, edit)
            XCTAssertEqual(model.profiles.map(\.name), ["one", "three"], edit)
            XCTAssertNil(model.value("profiles[1].command"), "\(edit): “three” keeps its own")
            XCTAssertFalse(model.isPending("profiles[1].command"), edit)
            XCTAssertEqual(
                model.message(SettingsModel.profilesKey), edit == "delete" ? nil : SettingsModel.profileGoneNotice,
                edit)
        }
    }

    /// #549: a rename that meets a hand edit which removed an earlier
    /// profile renames its profile at its new index, and moves the default
    /// profile with it, as the run it was queued in.
    func testARenameRetriedAfterAHandEditRemovedAnEarlierProfileKeepsItsRun() async throws {
        let config = Self.threeProfiles.replacingOccurrences(
            of: #""default_profile": "one""#, with: #""default_profile": "three""#)
        let removed = Self.firstProfileRemoved.replacingOccurrences(
            of: #""default_profile": "two""#, with: #""default_profile": "three""#)
        let (model, source, writer) = try fakeModel(config, clock: ManualClock())
        try await conflictOnFirstAttempt(model, source, writer) {
            model.renameProfile(2, to: "Trois")
        }
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(removed))
        model.reload()
        try await waitUntil("written again") { !model.isWriting }
        XCTAssertEqual(
            writer.calls,
            [.init(key: "profiles[2].name", value: .string("Trois")),
             .init(key: "profiles[1].name", value: .string("Trois")),
             .init(key: "general.default_profile", value: .string("Trois"))])
    }

    /// #549: an edit of the key while one waits to be written again
    /// replaces it, and waits with it: only the later value is written, to
    /// the profile it was made for once the hand edit applies.
    func testALaterEditReplacesAProfileEditWaitingToBeRetried() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
        try await conflictOnFirstAttempt(model, source, writer) {
            model.set("profiles[1].command", .string("/bin/sh"))
        }
        model.set("profiles[1].command", .string("/bin/zsh"))
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(writer.calls.count, 1, "not written by the old list while the hand edit waits")
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/zsh"), "shown while it waits")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(Self.firstProfileRemoved))
        model.reload()
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(
            writer.calls,
            [.init(key: "profiles[1].command", value: .string("/bin/sh")),
             .init(key: "profiles[0].command", value: .string("/bin/zsh"))],
            "written to “two”, now the first, not to “three”")
        XCTAssertEqual(model.value("profiles[0].command"), .string("/bin/zsh"))
    }

    /// #549 round 1 (P1): an edit of a profile's key already queued behind
    /// one that meets a conflict is the latest word: the earlier value is
    /// not written again over it once the hand edit applies.
    func testALaterQueuedEditIsNotOverwrittenByARetriedEarlierOne() async throws {
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
        let first = Locked(true)
        writer.respond = { _, _ in
            guard first.value else { return nil }
            first.value = false
            return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
        }
        model.set("profiles[1].command", .string("/bin/sh"))
        model.set("profiles[1].command", .string("/bin/zsh"))
        try await waitUntil("both ran") { writer.calls.count == 2 && !model.isWriting }
        // The file as the watch applies it: the hand edit and the later value.
        let handEdited = Self.threeProfiles.replacingOccurrences(of: "Tessera Dark", with: "Nord")
            .replacingOccurrences(of: "/bin/cat", with: "/bin/zsh")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(handEdited))
        model.reload()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(
            writer.calls,
            [.init(key: "profiles[1].command", value: .string("/bin/sh")),
             .init(key: "profiles[1].command", value: .string("/bin/zsh"))],
            "the earlier value is not written again over the later one")
        XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/zsh"))
        XCTAssertFalse(model.isWriting)
    }

    /// #549 round 1 (P6): a profile's edit that meets a conflict as its
    /// window closes (a draft committed on close) is still written again,
    /// as an edit outside the profiles is, and the model goes after it.
    func testAProfileEditThatMeetsAConflictAsTheWindowClosesIsStillWritten() async throws {
        let writer = FakeWriter()
        let clock = ManualClock()
        var model: SettingsModel? = try fakeModel(Self.threeProfiles, writer: writer, clock: clock).0
        let first = Locked(true)
        writer.respond = { _, _ in
            guard first.value else { return nil }
            first.value = false
            return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
        }
        model?.set("profiles[1].command", .string("/bin/sh"))
        weak var gone = model
        model = nil
        try await waitUntil("first attempt") { writer.calls.count >= 1 }
        try await Task.sleep(for: .milliseconds(300))
        clock.advance(by: .seconds(3))
        try await waitUntil("written again") { writer.calls.count == 2 }
        XCTAssertEqual(writer.calls.last, .init(key: "profiles[1].command", value: .string("/bin/sh")))
        try await waitUntil("the model goes once written") { gone == nil }
    }

    /// #549 adversarial B-A: Settings closed while a profile's edit meets a
    /// hand edit (`Conflict`), before the conflict comes back or once its
    /// retry waits, keeps its model, so Settings opened again edits on with
    /// it: a later edit of the field replaces the waiting one, and the
    /// closed window's older value is never written over it. Opened and
    /// closed as the app does, nothing posted, on a stand-in model the app
    /// takes as its waiting one, on a manual clock.
    func testSettingsOpenedAgainWhileAProfileEditWaitsEditsOnWithItsModel() async throws {
        try openSettings(config: Self.threeProfilesTOML)
        try XCTUnwrap(appDelegate.settingsWindowController).close()
        XCTAssertNil(appDelegate.waitingSettingsModel)
        let handEdited = Self.threeProfiles.replacingOccurrences(of: "Tessera Dark", with: "Nord")
        for closedFirst in [true, false] {
            let what = closedFirst ? "closed before the conflict came back" : "closed while the retry waits"
            let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: ManualClock())
            let clock = try XCTUnwrap(model.clock as? ManualClock)
            appDelegate.waitingSettingsModel = model
            MainMenu.run(command: "settings")
            XCTAssertTrue(appDelegate.settingsWindowController?.model === model, what)
            let first = Locked(true)
            writer.respond = { _, _ in
                guard first.value else { return nil }
                first.value = false
                return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
            }
            let reads = source.pluginReads
            model.set("profiles[1].command", .string("/bin/sh"))
            // The write runs off the main actor: its conflict comes back
            // after this close.
            if closedFirst { try XCTUnwrap(appDelegate.settingsWindowController).close() }
            try await waitUntil("the conflict came back") { source.pluginReads > reads && writer.calls.count == 1 }
            try await Task.sleep(for: .milliseconds(100))
            if !closedFirst { try XCTUnwrap(appDelegate.settingsWindowController).close() }
            XCTAssertNil(appDelegate.settingsWindowController, what)
            XCTAssertTrue(appDelegate.waitingSettingsModel === model, "\(what): kept while its edit waits")
            MainMenu.run(command: "settings")
            XCTAssertTrue(appDelegate.settingsWindowController?.model === model, "\(what): edited on with it")
            model.set("profiles[1].command", .string("/bin/zsh"))
            // The watch applies the hand edit (of the theme).
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(handEdited))
            model.reload()
            try await waitUntil("written") { !model.isWriting }
            // Past every settle time and wait limit.
            clock.advance(by: .seconds(5))
            try await Task.sleep(for: .milliseconds(200))
            XCTAssertEqual(
                writer.calls,
                [.init(key: "profiles[1].command", value: .string("/bin/sh")),
                 .init(key: "profiles[1].command", value: .string("/bin/zsh"))],
                "\(what): the older value is not written over the later one")
            XCTAssertEqual(model.value("profiles[1].command"), .string("/bin/zsh"), what)
            try XCTUnwrap(appDelegate.settingsWindowController).close()
            XCTAssertNil(appDelegate.waitingSettingsModel, "\(what): nothing left to wait for")
        }
    }

    /// The configuration of profiles named `names`, with no other keys.
    private static func namedProfiles(_ names: [String]) -> String {
        let list = names.map { #"{"name": "\#($0)"}"# }.joined(separator: ", ")
        return #"{"general": {"default_profile": "q0"}, "appearance": {"theme": "Tessera Dark"}, "profiles": [\#(list)]}"#
    }

    /// #549 adversarial B-D: the watch applies a hand edit that removed the
    /// first profile before the conflict comes back, so a profile's edit
    /// waits (the settle time) while the window shows the moved list. A
    /// later edit of that profile, at its new index, replaces it, whatever
    /// configuration applies while both wait (facet 1); an edit of the
    /// profile now at its old index is another profile's and is written
    /// to it, with the waiting edit to its own (facet 2). Every list
    /// length and index up to 7 profiles (each run once: a waiting retry
    /// once followed the order of a `Dictionary`).
    func testAWaitingRetryAfterAnAppliedHandEditKeepsALaterAndANeighboursEdit() async throws {
        var report: [String] = []
        var runs = 0
        for count in 3...7 {
            for index in 1..<count {
                for facet in [1, 2] where facet == 1 || index + 1 < count {
                    runs += 1
                    let names = (0..<count).map { "q\($0)" }
                    let (model, source, writer) = try fakeModel(Self.namedProfiles(names), clock: ManualClock())
                    let clock = try XCTUnwrap(model.clock as? ManualClock)
                    let applied = source.applied
                    let removed = try Self.json(Self.namedProfiles(Array(names.dropFirst())))
                    let first = Locked(true)
                    writer.respond = { _, _ in
                        guard first.value else { return nil }
                        first.value = false
                        // The watch applied the hand edit before the conflict came back.
                        applied.value = TesseraConfigSnapshot(generation: 1, config: removed)
                        return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
                    }
                    let reads = source.pluginReads
                    model.set("profiles[\(index)].command", .string("/bin/old"))
                    try await waitUntil("the conflict came back") { source.pluginReads > reads }
                    XCTAssertEqual(model.profiles.map(\.name), Array(names.dropFirst()), "the hand edit shows")
                    if facet == 1 {
                        model.set("profiles[\(index - 1)].command", .string("/bin/new"))
                        try await Task.sleep(for: .milliseconds(30))
                        // Any configuration applying meanwhile (another key's write).
                        source.snapshot = TesseraConfigSnapshot(generation: 2, config: removed)
                        model.reload()
                    } else {
                        model.set("profiles[\(index)].command", .string("/bin/other"))
                    }
                    try await Task.sleep(for: .milliseconds(50))
                    clock.advance(by: .seconds(3))
                    try await waitUntil("written") { !model.isWriting }
                    try await Task.sleep(for: .milliseconds(30))
                    clock.advance(by: .seconds(3))
                    let calls = writer.calls.dropFirst()
                    let listed = calls.map { "\($0.key)=\($0.value?.string ?? "nil")" }
                    let name = "\(count) profiles, index \(index), facet \(facet)"
                    if facet == 1 {
                        let last = calls.last { $0.key == "profiles[\(index - 1)].command" }
                        if last?.value != .string("/bin/new")
                            || calls.contains(where: { $0.key != "profiles[\(index - 1)].command" })
                        {
                            report.append("\(name): \(listed)")
                        }
                    } else {
                        let own = calls.contains { $0 == .init(key: "profiles[\(index - 1)].command", value: .string("/bin/old")) }
                        let other = calls.contains { $0 == .init(key: "profiles[\(index)].command", value: .string("/bin/other")) }
                        if !own || !other {
                            report.append(
                                "\(name): \(listed) notice: \(model.message(SettingsModel.profilesKey) ?? "none")")
                        }
                    }
                }
            }
        }
        XCTAssertEqual(report.count, 0, "\(report.count) of \(runs):\n" + report.joined(separator: "\n"))
    }

    /// #549 adversarial B-D, the two failing seeds of its fuzz: one hand
    /// edit per round lands during a profile's edit (a `Conflict`: an
    /// earlier profile removed, two swapped, one inserted, or another key),
    /// more edits follow while the retry waits, and the hand edit applies,
    /// sometimes before the conflict comes back; then every profile still
    /// listed holds the last value set for it, by its name, and no other
    /// was written. Six rounds a seed, each bounded by the settle times on
    /// a manual clock.
    func testProfileEditsAroundHandEditsLeaveEachProfilesLastValue() async throws {
        for seed: UInt64 in [13_064_056_694_810_536_062, 10_372_713_005_361_028_285] {
            var random = SplitMix(seed)
            let file = HandEditedProfiles((0..<4).map { .init(name: "p\($0)", command: "/bin/orig\($0)") })
            let writer = FakeWriter()
            let clock = ManualClock()
            let source = FakeSource(writer: writer)
            source.snapshot = TesseraConfigSnapshot(generation: 0, config: try Self.json(file.json()))
            sources.append(source)
            let model = SettingsModel(source: source, clock: clock)
            writer.respond = { call, _ in file.respond(call) }
            func apply() throws {
                source.snapshot = TesseraConfigSnapshot(
                    generation: source.snapshot.generation + 1, config: try Self.json(file.json()))
                model.reload()
            }
            var intents: [String: String] = [:]
            var counter = 0
            var notes: [String] = []
            func userEdit(twice: Bool) async throws {
                let listed = model.snapshot["profiles"]?.array ?? []
                guard !listed.isEmpty else { return }
                let index = Int(random.next() % UInt64(listed.count))
                let name = listed[index]["name"]?.string ?? "?"
                if random.next() % 4 == 0 {
                    counter += 1
                    model.set("appearance.theme", .string("T\(counter)"))
                }
                for _ in 0..<(twice ? 2 : 1) {
                    counter += 1
                    let value = "/bin/v\(counter)"
                    let calls = writer.calls.count
                    model.set("profiles[\(index)].command", .string(value))
                    intents[name] = value
                    notes.append("set \(name) (index \(index)) = \(value)")
                    if !twice {
                        let deadline = ContinuousClock.now + .milliseconds(150)
                        while writer.calls.count == calls, ContinuousClock.now < deadline {
                            try await Task.sleep(for: .milliseconds(5))
                        }
                        try await Task.sleep(for: .milliseconds(25))
                    }
                }
                if twice { try await Task.sleep(for: .milliseconds(80)) }
            }
            for round in 0..<6 {
                let kind = Int(random.next() % 4)
                let quick = random.next() % 3 == 0
                file.quickApply = quick ? source.applied : nil
                let before = Int(random.next() % 3)
                for edit in 0..<(before + 1) {
                    if edit == before { file.arm(kind) }
                    try await userEdit(twice: edit == before && kind == 3 && random.next() % 2 == 0)
                }
                for _ in 0..<Int(random.next() % 4) { try await userEdit(twice: false) }
                file.disarm()
                file.quickApply = nil
                if !quick || random.next() % 2 == 0 { try apply() }
                try await Task.sleep(for: .milliseconds(60))
                try apply()
                clock.advance(by: .seconds(3))
                let deadline = ContinuousClock.now + .seconds(3)
                while model.isWriting, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
                try apply()
                clock.advance(by: .seconds(3))
                try await Task.sleep(for: .milliseconds(30))
                if model.isWriting { notes.append("round \(round): still writing") }
                if let message = model.message(SettingsModel.profilesKey) { notes.append("notice: \(message)") }
            }
            for profile in file.profiles {
                let original = profile.name.hasPrefix("p") ? "/bin/orig\(profile.name.dropFirst())" : nil
                XCTAssertEqual(
                    profile.command, intents[profile.name] ?? original,
                    "seed \(seed), \(profile.name):\n  " + (notes + file.log).joined(separator: "\n  "))
            }
        }
    }

    /// #344 round 1: a configuration applied while a slider is dragged (an
    /// earlier value of the drag) does not move the slider back: the value
    /// it shows stays until the drag ends and its own value applies.
    func testAConfigurationAppliedMidDragKeepsTheSlidersValue() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        // The slider is pressed: its onEditingChanged.
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == key })
        let editingChanged = field.editingChanged(.init(model: model, path: key))
        editingChanged(true)
        model.change(key, .float(0.5))
        try await waitUntil("the first value written") { writer.calls.count == 1 && !model.isWriting }
        clock.advance(by: SettingsModel.coalescingInterval)
        model.change(key, .float(0.6))
        try await waitUntil("the second written") { writer.calls.count == 2 && !model.isWriting }
        // The watch applies the first value's file.
        let first = Self.fakeConfig.replacingOccurrences(of: "0.85", with: "0.5")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(first))
        model.reload()
        XCTAssertEqual(model.value(key), .float(0.6), "the drag's value, not an earlier one of it")
        editingChanged(false)
        let second = Self.fakeConfig.replacingOccurrences(of: "0.85", with: "0.6")
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(second))
        model.reload()
        XCTAssertFalse(model.isPending(key), "its own value applied")
        XCTAssertEqual(model.value(key), .float(0.6))
    }

    /// 24.12 adversarial B1: a step of the real Stepper by VoiceOver
    /// (AXIncrement, as the keyboard) has no press and no release, so no
    /// ``SettingsModel/endChange(_:)``. Its value stops waiting once a
    /// configuration after it applies: a hand edit after it shows, and the
    /// next step goes from the hand edit's value, not over it.
    func testAStepperStepWithoutAPressFollowsAHandEditAfterIt() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == key })
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 500, height: 200))
        window.isKeyForTest = true
        window.contentView = NSHostingView(
            rootView: SettingsFieldRow(field: field, context: .init(model: model, path: key)).padding())
        defer {
            window.contentView = nil
            window.close()
        }
        window.layoutIfNeeded()
        func incrementor() throws -> NSAccessibilityProtocol {
            var found: NSAccessibilityProtocol?
            func walk(_ element: Any, depth: Int) {
                guard found == nil, depth < 60, let element = element as? NSAccessibilityProtocol else { return }
                if element.accessibilityRole() == .incrementor { found = element }
                for child in element.accessibilityChildren() ?? [] { walk(child, depth: depth + 1) }
            }
            func walkViews(_ view: NSView) {
                walk(view, depth: 0)
                view.subviews.forEach(walkViews)
            }
            walkViews(try XCTUnwrap(window.contentView))
            return try XCTUnwrap(found, "the stepper's incrementor")
        }
        func apply(_ size: String, generation: UInt64) throws {
            let text = Self.fakeConfig.replacingOccurrences(of: #""font_size": 13.0"#, with: #""font_size": \#(size)"#)
            source.snapshot = TesseraConfigSnapshot(generation: generation, config: try Self.json(text))
            model.reload()
        }
        try await waitUntil("the stepper is shown") { (try? incrementor()) != nil }

        // Its result is false though the step is taken.
        _ = try incrementor().accessibilityPerformIncrement()
        try await waitUntil("the step written") { writer.calls.count == 1 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(14)))
        try apply("14.0", generation: 1)
        XCTAssertFalse(model.isPending(key), "its own value applied")
        clock.advance(by: .seconds(3))
        try apply("20.0", generation: 2)
        XCTAssertEqual(model.value(key), .float(20), "the hand edit shows")

        window.layoutIfNeeded()
        _ = try incrementor().accessibilityPerformIncrement()
        try await waitUntil("the next step written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(21)), "from the hand edit's 20")
    }

    // MARK: A configuration applied during a press (#517)

    /// The real stepper of the font size, in a window of its own, on `model`.
    private func hostedFontSizeStepper(
        _ context: SettingsFieldContext
    ) async throws -> (KeyControlledWindow, NSStepper) {
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.font_size" })
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 500, height: 200))
        window.isKeyForTest = true
        window.contentView = NSHostingView(
            rootView: SettingsFieldRow(field: field, context: context).padding())
        window.layoutIfNeeded()
        func find(_ view: NSView) -> NSStepper? {
            (view as? NSStepper) ?? view.subviews.lazy.compactMap(find).first
        }
        var stepper: NSStepper?
        try await waitUntil("the stepper is shown") {
            window.layoutIfNeeded()
            stepper = window.contentView.flatMap(find)
            return stepper != nil
        }
        return (window, try XCTUnwrap(stepper))
    }

    /// A press of `stepper`'s up arrow, as AppKit's mouse tracking makes it
    /// on the control: the cell is told the press started and ended
    /// (SwiftUI's cell reports both to `onEditingChanged`), and each step
    /// is the new value with the control's action. The tracking loop itself
    /// ends at once in a test: no mouse button is down.
    private struct StepperPress {
        let stepper: NSStepper
        let cell: NSCell
        let point: NSPoint

        @MainActor
        func step() {
            stepper.doubleValue += stepper.increment
            stepper.sendAction(stepper.action, to: stepper.target)
        }

        @MainActor
        func release() {
            cell.stopTracking(last: point, current: point, in: stepper, mouseIsUp: true)
        }
    }

    /// Presses the up arrow, and takes the step a press starts with.
    private func pressUpArrow(of stepper: NSStepper) throws -> StepperPress {
        let cell = try XCTUnwrap(stepper.cell)
        let point = NSPoint(x: stepper.bounds.midX, y: stepper.bounds.midY - stepper.bounds.height / 4)
        XCTAssertTrue(cell.startTracking(at: point, in: stepper), "the cell tracks the press")
        let press = StepperPress(stepper: stepper, cell: cell, point: point)
        press.step()
        return press
    }

    private func applyFontSize(_ size: String, generation: UInt64, _ model: SettingsModel, _ source: FakeSource) throws {
        let text = Self.fakeConfig.replacingOccurrences(of: #""font_size": 13.0"#, with: #""font_size": \#(size)"#)
        source.snapshot = TesseraConfigSnapshot(generation: generation, config: try Self.json(text))
        model.reload()
    }

    /// #517: the real stepper, held with the mouse. A hand edit of the
    /// key applied during the press shows once it is let go, and the next
    /// step goes from it instead of writing over it.
    func testAHandEditDuringAHeldStepperShowsAfterTheReleaseAndTheNextStepKeepsIt() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        let (window, stepper) = try await hostedFontSizeStepper(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let press = try pressUpArrow(of: stepper)
        try await waitUntil("the step written") { writer.calls.count == 1 && !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: key, value: .float(14))])
        try applyFontSize("14.0", generation: 1, model, source)
        try applyFontSize("20.0", generation: 2, model, source)
        XCTAssertEqual(model.value(key), .float(14), "held: the control keeps the value it shows")

        press.release()
        clock.advance(by: .zero)
        XCTAssertEqual(model.value(key), .float(20), "the hand edit shows after the release")
        XCTAssertFalse(model.isPending(key))
        XCTAssertEqual(writer.calls.count, 1, "the release writes nothing")

        window.layoutIfNeeded()
        try pressUpArrow(of: stepper).release()
        try await waitUntil("the next step written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(21)), "from the hand edit's 20")
    }

    /// #517: what the press itself wrote is not a hand edit. An earlier
    /// value of the press applied, read back as the core's `f32`, leaves
    /// the last value showing after the release, until it applies too.
    func testAnEarlierValueOfThePressAppliedDoesNotReplaceTheLastAfterTheRelease() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == key })
        let editingChanged = field.editingChanged(.init(model: model, path: key))
        editingChanged(true)
        model.change(key, .float(0.3))
        try await waitUntil("the first value written") { writer.calls.count == 1 && !model.isWriting }
        clock.advance(by: SettingsModel.coalescingInterval)
        model.change(key, .float(0.6))
        try await waitUntil("the second written") { writer.calls.count == 2 && !model.isWriting }
        let widened = Self.fakeConfig.replacingOccurrences(of: "0.85", with: "\(Double(Float(0.3)))")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(widened))
        model.reload()
        editingChanged(false)
        XCTAssertEqual(model.value(key), .float(0.6), "not the earlier value the press wrote")
        XCTAssertTrue(model.isPending(key))
        let last = Self.fakeConfig.replacingOccurrences(of: "0.85", with: "\(Double(Float(0.6)))")
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(last))
        model.reload()
        XCTAssertFalse(model.isPending(key), "its own value applied")
        XCTAssertEqual(writer.calls.count, 2)
    }

    /// #517: a configuration applied that changes another key says nothing
    /// about this one: the value written and not yet applied stays.
    func testAnotherKeyAppliedDuringAPressLeavesTheValueShown() async throws {
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: ManualClock())
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        let other = Self.fakeConfig.replacingOccurrences(of: "Tessera Dark", with: "Nord")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(other))
        model.reload()
        model.endChange(key)
        XCTAssertEqual(model.value(key), .float(14))
        XCTAssertTrue(model.isPending(key))
    }

    /// #517: a step held back by the coalescing when the hand edit applies
    /// was taken before it: it is not written over the edit, by its timer
    /// or at the release.
    func testAHeldBackStepIsNotWrittenOverAHandEditAppliedAfterIt() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        model.change(key, .float(15))
        try applyFontSize("20.0", generation: 1, model, source)
        clock.advance(by: SettingsModel.coalescingInterval * 2)
        model.endChange(key)
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [.init(key: key, value: .float(14))], "15 is not written")
        XCTAssertEqual(model.value(key), .float(20))
        XCTAssertFalse(model.isPending(key))
    }

    /// #517: a step taken after the hand edit applied (the press goes on)
    /// is the person's latest word: written, and shown after the release.
    func testAStepAfterTheHandEditInTheSamePressIsWritten() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("20.0", generation: 1, model, source)
        // Within the coalescing interval: the release writes it.
        model.change(key, .float(15))
        model.endChange(key)
        try await waitUntil("the step written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(15)))
        XCTAssertEqual(model.value(key), .float(15))
        XCTAssertTrue(model.isPending(key), "until it applies")
    }

    /// #517: the values a press remembers go once they are settled, so a
    /// later hand edit back to one of them is still a hand edit.
    func testAHandEditBackToAnEarlierPressesValueShowsAfterTheRelease() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        model.endChange(key)
        try applyFontSize("14.0", generation: 1, model, source)
        XCTAssertFalse(model.isPending(key))
        try applyFontSize("18.0", generation: 2, model, source)

        clock.advance(by: .seconds(1))
        model.beginChange(key)
        model.change(key, .float(19))
        try await waitUntil("written") { writer.calls.count == 2 && !model.isWriting }
        // The step reached the file and the edit went over it before the
        // watch looked: one configuration, the edit's.
        try applyFontSize("14.0", generation: 3, model, source)
        model.endChange(key)
        clock.advance(by: .zero)
        XCTAssertEqual(model.value(key), .float(14), "the hand edit, though the first press wrote 14 too")
        XCTAssertFalse(model.isPending(key))
    }

    /// #517: a value of this press that has applied is settled too: a hand
    /// edit back to it, in the same press, is still a hand edit.
    func testAHandEditBackToAValueThePressAlreadySawAppliedShows() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        clock.advance(by: SettingsModel.coalescingInterval)
        model.change(key, .float(15))
        try await waitUntil("written") { writer.calls.count == 2 && !model.isWriting }
        // The watch saw both writes at once: 15 settles 14 as well.
        try applyFontSize("15.0", generation: 1, model, source)
        try applyFontSize("14.0", generation: 2, model, source)
        model.endChange(key)
        clock.advance(by: .zero)
        XCTAssertEqual(model.value(key), .float(14))
        XCTAssertFalse(model.isPending(key))
    }

    /// #517, S5: the values remembered per key are capped however long a
    /// press lasts with no configuration applied.
    func testTheValuesAPressRemembersAreBounded() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        let steps = SettingsModel.maxUnappliedWrites * 3
        for step in 0..<steps {
            model.change(key, .float(Double(14 + step)))
            clock.advance(by: SettingsModel.coalescingInterval)
        }
        try await waitUntil("written") { writer.calls.count == steps && !model.isWriting }
        XCTAssertEqual(model.unappliedWriteCount(key), SettingsModel.maxUnappliedWrites)
        model.endChange(key)
    }

    /// #517 on the core: `config.toml` edited by hand during a press, seen
    /// through the watch. After the release the window shows the file's
    /// value, and the next step keeps the rest of what the edit changed.
    func testAHandEditOfTheFileDuringAPressShowsAfterTheReleaseOnTheCore() async throws {
        let model = try openSettings(config: "[appearance]\nfont_size = 13.0\nopacity = 0.5\n")
        let key = "appearance.font_size"
        try await waitUntil("the file's value") { model.value(key) == .float(13) }
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("the step applied") { model.snapshot[key] == .float(14) && !model.isWriting }
        let generation = model.snapshot.generation
        try "[appearance]\nfont_size = 20.0\nopacity = 0.25\n".write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("the hand edit applied") { model.snapshot.generation > generation }
        XCTAssertEqual(model.value(key), .float(14), "held: the control keeps the value it shows")
        model.endChange(key)
        try await waitUntil("shown after the release") { model.value(key) == .float(20) }
        model.change(key, .float(21))
        try await waitUntil("the next step applied") { model.snapshot[key] == .float(21) }
        XCTAssertTrue(try lines().contains { $0.hasPrefix("font_size") && $0.contains("21") }, contents)
        XCTAssertTrue(try lines().contains { $0.hasPrefix("opacity") && $0.contains("0.25") }, contents)
    }

    /// #517 B1: a configuration applied during a press with the key where
    /// it was (another app saved its copy over the step). Nothing about the
    /// key looks new, so the press sees no hand edit: once the watch has had
    /// its time after the release, the window shows what is applied.
    func testAnApplyLeavingThePressedKeyAsItWasShowsAfterTheSettleTime() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("13.0", generation: 1, model, source)
        model.endChange(key)
        XCTAssertEqual(model.value(key), .float(14), "the step may still be on its way")
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(model.value(key), .float(13))
        XCTAssertFalse(model.isPending(key))
        XCTAssertEqual(writer.calls.count, 1, "nothing written over it")
    }

    /// #517 B1: the file ends on an earlier value of the press.
    func testAFileEndingOnAnEarlierValueOfThePressShowsAfterTheSettleTime() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        clock.advance(by: SettingsModel.coalescingInterval)
        model.change(key, .float(15))
        try await waitUntil("written") { writer.calls.count == 2 && !model.isWriting }
        try applyFontSize("14.0", generation: 1, model, source)
        model.endChange(key)
        XCTAssertEqual(model.value(key), .float(15), "14 may be the press's own")
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(model.value(key), .float(14))
        XCTAssertFalse(model.isPending(key))
    }

    /// #517 B1: a hand edit removes a profile, and the row under the press
    /// is another one with no value of its own, before and after.
    func testARowThatMovedUnderAPressShowsItsOwnValueAfterTheSettleTime() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: clock)
        let key = "profiles[1].appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        let removed = Self.threeProfiles.replacingOccurrences(of: #"{"name": "one"}, "#, with: "")
        XCTAssertNotEqual(removed, Self.threeProfiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(removed))
        model.reload()
        model.endChange(key)
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(model.profiles.map(\.name), ["two", "three"])
        XCTAssertNil(model.value(key), "“three” has no size of its own")
        XCTAssertFalse(model.isPending(key))
    }

    /// #517 B1: a new press cancels the reload the last release set up.
    func testANewPressCancelsTheSettleOfTheLastRelease() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("13.0", generation: 1, model, source)
        model.endChange(key)
        model.beginChange(key)
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(Self.fakeConfig))
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(model.snapshot.generation, 1, "not read again")
    }

    /// #517 B1: with nothing applied since the last write the value goes on
    /// waiting for the watch, however long it takes.
    func testAReleaseWaitsForTheWatchWhenNothingAppliedSinceTheWrite() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        model.endChange(key)
        let applied = Self.fakeConfig.replacingOccurrences(of: #""font_size": 13.0"#, with: #""font_size": 14.0"#)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(applied))
        clock.advance(by: .seconds(30))
        XCTAssertEqual(model.snapshot.generation, 0, "no reload was set up")
        XCTAssertEqual(model.value(key), .float(14))
        XCTAssertTrue(model.isPending(key))
        model.reload()
        XCTAssertFalse(model.isPending(key))
    }

    /// #517 B1 on the core: another app saves its copy, with the value from
    /// before the step, before the watch has read the step.
    func testAStaleSaveDuringAPressShowsAfterTheReleaseOnTheCore() async throws {
        let model = try openSettings(config: "[appearance]\nfont_size = 13.0\nopacity = 0.5\n")
        let key = "appearance.font_size"
        try await waitUntil("the file's value") { model.value(key) == .float(13) }
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { !model.isWriting && self.contents.contains("14") }
        try "[appearance]\nfont_size = 13.0\nopacity = 0.25\n".write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("applied") { Self.same(model.snapshot["appearance.opacity"], .float(0.25)) }
        model.endChange(key)
        try await waitUntil("the file's value after the release") {
            model.value(key) == .float(13) && !model.isPending(key)
        }
        XCTAssertTrue(try lines().contains { $0.hasPrefix("font_size") && $0.contains("13") }, contents)
    }

    /// #517 B1 on the core: the same with a profile removed under the press.
    func testARemovedProfileUnderAPressShowsAfterTheReleaseOnTheCore() async throws {
        let three = "[[profiles]]\nname = \"one\"\n\n[[profiles]]\nname = \"two\"\n\n[[profiles]]\nname = \"three\"\n"
        let model = try openSettings(config: three)
        let key = "profiles[1].appearance.font_size"
        try await waitUntil("the profiles") { model.profiles.count == 3 }
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { !model.isWriting && self.contents.contains("14") }
        let removed = "[[profiles]]\nname = \"two\"\n\n[[profiles]]\nname = \"three\"\n"
        try removed.write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("applied") { model.profiles.count == 2 }
        model.endChange(key)
        try await waitUntil("the row's own value after the release") {
            model.value(key) == nil && !model.isPending(key)
        }
        XCTAssertFalse(contents.contains("font_size"), contents)
    }

    /// #517: an own value applied, then a hand edit back to the value at the
    /// press: new against what the press saw last, so it shows.
    func testAHandEditBackToThePressStartAfterAnOwnValueShows() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("14.0", generation: 1, model, source)
        try applyFontSize("13.0", generation: 2, model, source)
        model.endChange(key)
        clock.advance(by: .zero)
        XCTAssertEqual(model.value(key), .float(13))
        XCTAssertFalse(model.isPending(key))
    }

    /// The reload a hand edit held back waits for the end of the release
    /// event: until then the binding still equals the knob, and the final
    /// action of an `NSSlider` is no edit.
    func testTheHeldBackReloadRunsAfterTheReleaseEvent() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("20.0", generation: 1, model, source)
        model.endChange(key)
        XCTAssertEqual(model.value(key), .float(14), "still the knob's value during the release")
        clock.advance(by: .zero)
        XCTAssertEqual(model.value(key), .float(20))
        XCTAssertFalse(model.isPending(key))
        XCTAssertEqual(writer.calls.count, 1)
    }

    /// #534: stepped up and back inside the watch's debounce, 13 is pending
    /// with 14 and 13 unapplied. A press without a step during which a hand
    /// edit to exactly 14 applies has nothing to write: the edit looks like
    /// the press's own value, and it stays.
    func testAPressWithoutAStepWritesNothingOverAHandEditEqualToAnUnappliedWrite() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("14 written") { writer.calls.count == 1 && !model.isWriting }
        clock.advance(by: SettingsModel.coalescingInterval)
        model.change(key, .float(13))
        try await waitUntil("13 written") { writer.calls.count == 2 && !model.isWriting }
        model.endChange(key)
        XCTAssertTrue(model.isPending(key))

        model.beginChange(key)
        try applyFontSize("14.0", generation: 1, model, source)
        model.endChange(key)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 2, "the release of a press without a step writes nothing")
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(model.value(key), .float(14))
        XCTAssertFalse(model.isPending(key))
    }

    /// #534: a press without a step on a row whose profile moved away,
    /// inside the settle time of the last release, writes nothing to the
    /// profile that now has that index.
    func testAPressWithoutAStepOnARowThatMovedWritesNothing() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.threeProfiles, clock: clock)
        let key = "profiles[1].appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        let removed = Self.threeProfiles.replacingOccurrences(of: #"{"name": "one"}, "#, with: "")
        XCTAssertNotEqual(removed, Self.threeProfiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(removed))
        model.reload()
        model.endChange(key)

        model.beginChange(key)
        model.endChange(key)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 1, "nothing goes to the profile now at that index")
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertNil(model.value(key), "“three” has no size of its own")
        XCTAssertFalse(model.isPending(key))
    }

    /// A step still held back when a press begins is the press's to write:
    /// its release cancels the timer that would have.
    func testAStepHeldBackWhenAPressBeginsIsWrittenByItsRelease() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.font_size"
        model.change(key, .float(14))
        try await waitUntil("14 written") { writer.calls.count == 1 && !model.isWriting }
        model.change(key, .float(15))
        model.beginChange(key)
        model.endChange(key)
        try await waitUntil("15 written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(15)))
    }

    private var opacityField: SettingsField {
        get throws { try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.opacity" }) }
    }

    /// #534: a slider pressed and let go without moving sends the knob's
    /// value, the step nearest the value shown: for one off the step (0.123
    /// by hand, or 0.85 as the core's `f32` reads it back) that is no step,
    /// and nothing is written.
    func testANoMovePressOfAValueOffTheStepWritesNothing() async throws {
        for (file, knob) in [("0.12300000339746475", 0.12), ("0.8500000238418579", 0.85)] {
            let clock = ManualClock()
            let config = Self.fakeConfig.replacingOccurrences(of: #""opacity": 0.85"#, with: #""opacity": \#(file)"#)
            let (model, _, writer) = try fakeModel(config, clock: clock)
            let field = try opacityField
            let context = SettingsFieldContext(model: model, path: field.key)
            let shown = try XCTUnwrap(model.value(field.key))
            field.editingChanged(context)(true)
            field.doubleBinding(context).wrappedValue = knob
            field.editingChanged(context)(false)
            clock.advance(by: .zero)
            try await Task.sleep(for: .milliseconds(50))
            clock.advance(by: model.profileSettleTimeout)
            XCTAssertEqual(writer.calls, [], "\(file): the press writes nothing")
            XCTAssertEqual(model.value(field.key), shown, file)
            XCTAssertFalse(model.isPending(field.key), file)
        }
    }

    /// #534 on the macOS 15 SDK's `NSSlider`: a hand edit off the step
    /// applies while the knob is held without moving; the mouse-up action
    /// after the release sends the step nearest it, which writes nothing.
    func testAHandEditOffTheStepDuringANoMoveHoldIsNotRounded() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let field = try opacityField
        let context = SettingsFieldContext(model: model, path: field.key)
        field.editingChanged(context)(true)
        try applyOpacity("0.12300000339746475", generation: 1, model, source)
        field.editingChanged(context)(false)
        field.doubleBinding(context).wrappedValue = 0.12
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(writer.calls, [], "the release writes nothing")
        XCTAssertEqual(model.value(field.key), .float(0.12300000339746475))
        XCTAssertFalse(model.isPending(field.key))
    }

    /// The configuration of ``fakeConfig`` with `dim_inactive_panes` at `dim`.
    private func dimConfig(_ dim: String) -> String {
        Self.fakeConfig.replacingOccurrences(
            of: #""opacity": 0.85"#, with: #""opacity": 0.85, "dim_inactive_panes": \#(dim)"#)
    }

    /// #550 (the #534 adversarial X1): on the dim slider (1 % label, 5 %
    /// step) a hand edit off the step (0.33) applies while the knob is held
    /// without moving; the `NSSlider` mouse-up action after the release
    /// sends the knob's step (0.35). It writes nothing: the hand edit stays.
    func testADimHandEditDuringANoMoveHoldIsNotOverwrittenOnRelease() async throws {
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(dimConfig("0.6000000238418579"), clock: clock)
        let context = SettingsFieldContext(model: model, path: field.key)
        field.editingChanged(context)(true)
        field.doubleBinding(context).wrappedValue = 0.6  // the press's own knob action
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(dimConfig("0.33000001311302185")))
        model.reload()
        XCTAssertEqual(model.value(field.key), .float(0.33000001311302185), "the hand edit shows")
        field.editingChanged(context)(false)
        field.doubleBinding(context).wrappedValue = 0.35  // NSSlider mouse-up, knob re-synced to the step
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(80))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(80))
        XCTAssertEqual(writer.calls, [], "the hand edit 0.33 is overwritten: \(writer.calls)")
        XCTAssertEqual(model.value(field.key), .float(0.33000001311302185))
        XCTAssertFalse(model.isPending(field.key))
    }

    /// #550 on every slider: after a no-move hold with a hand edit applied
    /// during it, the mouse-up action writes nothing, whether it comes on
    /// its own or in a press of its own (as the macOS 15 SDK's `NSSlider`
    /// wraps it), and whether it carries the step nearest the hand edit or
    /// the knob's value from before it (a knob not moved to the edit). Once
    /// the release's events are handled, a step writes again.
    func testAMouseUpAfterAHandEditDuringANoMoveHoldWritesNothingOnEverySlider() async throws {
        let dim = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        let sliders = AppearancePane.fields.filter { if case .slider = $0.control { true } else { false } }
        XCTAssertEqual(sliders.map(\.key), ["appearance.opacity", dim.key], "every slider is covered")
        let cases: [(SettingsField, start: String, held: Double, edit: String, shown: Double, knobs: [Double])] = [
            (try opacityField, "0.8500000238418579", 0.85, "0.12300000339746475", 0.123, [0.12, 0.85]),
            (dim, "0.6000000238418579", 0.6, "0.33000001311302185", 0.33, [0.35, 0.6]),
        ]
        for (field, start, held, edit, shown, knobs) in cases {
            for knob in knobs {
                for ownPress in [false, true] {
                    let name = "\(field.key) knob \(knob)\(ownPress ? " in its own press" : "")"
                    let clock = ManualClock()
                    let config = { (value: String) in
                        Self.fakeConfig.replacingOccurrences(
                            of: #""opacity": 0.85"#,
                            with: field.key == dim.key
                                ? #""opacity": 0.85, "dim_inactive_panes": \#(value)"# : #""opacity": \#(value)"#)
                    }
                    let (model, source, writer) = try fakeModel(config(start), clock: clock)
                    let context = SettingsFieldContext(model: model, path: field.key)
                    field.editingChanged(context)(true)
                    field.doubleBinding(context).wrappedValue = held
                    source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(config(edit)))
                    model.reload()
                    XCTAssertEqual(model.value(field.key)?.double ?? 0, shown, accuracy: 0.0001, name)
                    field.editingChanged(context)(false)
                    if ownPress { field.editingChanged(context)(true) }
                    field.doubleBinding(context).wrappedValue = knob
                    if ownPress { field.editingChanged(context)(false) }
                    clock.advance(by: .zero)
                    try await Task.sleep(for: .milliseconds(50))
                    clock.advance(by: model.profileSettleTimeout)
                    try await Task.sleep(for: .milliseconds(50))
                    XCTAssertEqual(writer.calls, [], name)
                    XCTAssertEqual(model.value(field.key)?.double ?? 0, shown, accuracy: 0.0001, name)
                    XCTAssertFalse(model.isPending(field.key), name)

                    field.editingChanged(context)(true)
                    field.doubleBinding(context).wrappedValue = 0.5
                    field.editingChanged(context)(false)
                    try await waitUntil("\(name): a later step written") { writer.calls.count == 1 && !model.isWriting }
                    XCTAssertEqual(writer.calls, [.init(key: field.key, value: .float(0.5))], name)
                }
            }
        }
    }

    /// A slider moved off a value off the step and back writes the step
    /// it ends on: only the value shown is compared, not where the press
    /// began.
    func testASliderMovedFromAValueOffTheStepAndBackWritesTheStep() async throws {
        let clock = ManualClock()
        let config = Self.fakeConfig.replacingOccurrences(of: #""opacity": 0.85"#, with: #""opacity": 0.123"#)
        let (model, _, writer) = try fakeModel(config, clock: clock)
        let field = try opacityField
        let context = SettingsFieldContext(model: model, path: field.key)
        field.editingChanged(context)(true)
        field.doubleBinding(context).wrappedValue = 0.2
        try await waitUntil("0.2 written") { writer.calls.count == 1 && !model.isWriting }
        field.doubleBinding(context).wrappedValue = 0.12
        field.editingChanged(context)(false)
        try await waitUntil("0.12 written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last, .init(key: field.key, value: .float(0.12)))
    }

    /// #534 review M1: the dim slider's label counts in 1 % but its step
    /// is 5 %, so a value off the step (0.33 by hand, `33 %`) and its
    /// nearest step (`35 %`) look different: a press that ends on that
    /// step writes it, as does one that ends on the range's end (0.79 by
    /// hand, `80 %`).
    func testASliderEndedOnAStepShownDifferentlyFromAValueOffItWritesTheStep() async throws {
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        for (file, set, knob) in [("0.33000001311302185", 0.33, 0.35), ("0.7900000214576721", 0.79, 0.8)] {
            let config = Self.fakeConfig.replacingOccurrences(
                of: #""opacity": 0.85"#, with: #""opacity": 0.85, "dim_inactive_panes": \#(file)"#)
            XCTAssertNotEqual(config, Self.fakeConfig)
            let (model, _, writer) = try fakeModel(config, clock: ManualClock())
            let context = SettingsFieldContext(model: model, path: field.key)
            XCTAssertEqual(model.value(field.key)?.double ?? 0, set, accuracy: 0.0001, file)
            field.editingChanged(context)(true)
            field.doubleBinding(context).wrappedValue = knob
            field.editingChanged(context)(false)
            try await waitUntil("\(knob) written") { writer.calls.count == 1 && !model.isWriting }
            XCTAssertEqual(writer.calls, [.init(key: field.key, value: .float(knob))], file)
        }
    }

    /// #534 review M3: an add released by the settle time (no
    /// configuration showed it) moves the list generation as a hand edit
    /// does, and ends the press of a profile's held slider the same way:
    /// its writes are forgotten, and neither its late action nor its
    /// release writes anything.
    func testAnAddReleasedDuringAHeldProfileSliderEndsThePress() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.profileOpacities, clock: clock)
        let field = try opacityField
        let key = "profiles[1].appearance.opacity"
        let held = SettingsFieldContext(
            model: model, path: key, fallback: field.key, profile: 1, listGeneration: model.profileListGeneration)
        field.editingChanged(held)(true)
        field.doubleBinding(held).wrappedValue = 0.6
        try await waitUntil("0.6 written") { writer.calls.count == 1 && !model.isWriting }
        XCTAssertEqual(model.unappliedWriteCount(key), 1)

        XCTAssertTrue(model.addProfile())
        try await waitUntil("added") { writer.calls.count == 2 && !model.isWriting }
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertNotNil(model.message(SettingsModel.profilesKey), "the add was released")
        XCTAssertNotEqual(held.listGeneration, model.profileListGeneration, "the row is made again")
        XCTAssertEqual(model.unappliedWriteCount(key), 0, "the press ended, its writes forgotten")
        field.doubleBinding(held).wrappedValue = 0.4
        field.editingChanged(held)(false)
        clock.advance(by: .seconds(3))
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 2, "nothing more is written: \(writer.calls)")
    }

    /// The configuration of #536: the second profile has an opacity, and
    /// so does the third, which a hand edit can move to its index.
    private static let profileOpacities = """
        {"general": {"default_profile": "one"}, "appearance": {"theme": "Tessera Dark", "opacity": 0.85},
         "profiles": [{"name": "one"}, {"name": "two", "appearance": {"opacity": 0.5}},
                      {"name": "three", "appearance": {"opacity": 0.7}}]}
        """

    /// #536: the profile list changes while the second profile's opacity
    /// is held (the first profile removed, or the held profile's key
    /// removed). The pane makes the row again, so the slider goes with no
    /// release: the press ends there, nothing more is written, the row
    /// shows the applied value at once, and the old slider changes
    /// nothing. A press of the new row writes as any.
    func testAListChangeDuringAHeldSliderEndsThePress() async throws {
        let removedFirst = Self.profileOpacities.replacingOccurrences(of: #"{"name": "one"}, "#, with: "")
        let removedKey = Self.profileOpacities.replacingOccurrences(
            of: #"{"name": "two", "appearance": {"opacity": 0.5}}"#, with: #"{"name": "two", "appearance": null}"#)
        XCTAssertNotEqual(removedFirst, Self.profileOpacities)
        XCTAssertNotEqual(removedKey, Self.profileOpacities)
        for (edit, shown) in [(removedFirst, TesseraConfigValue.float(0.7)), (removedKey, .float(0.85))] {
            let clock = ManualClock()
            let (model, source, writer) = try fakeModel(Self.profileOpacities, clock: clock)
            let field = try opacityField
            let key = "profiles[1].appearance.opacity"
            let held = SettingsFieldContext(
                model: model, path: key, fallback: field.key, profile: 1, listGeneration: model.profileListGeneration)
            field.editingChanged(held)(true)
            field.doubleBinding(held).wrappedValue = 0.6
            try await waitUntil("0.6 written") { writer.calls.count == 1 && !model.isWriting }
            clock.advance(by: SettingsModel.coalescingInterval / 2)
            field.doubleBinding(held).wrappedValue = 0.59
            XCTAssertEqual(writer.calls.count, 1, "0.59 held back")

            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(edit))
            model.reload()
            XCTAssertNotEqual(held.listGeneration, model.profileListGeneration, "the row is made again")
            XCTAssertFalse(model.isPending(key), "the press ended")
            XCTAssertEqual(model.unappliedWriteCount(key), 0, "its writes forgotten")
            XCTAssertEqual(held.current, shown, "the applied value, at once")
            // The old slider, still pressed, and its release.
            field.doubleBinding(held).wrappedValue = 0.4
            field.editingChanged(held)(false)
            clock.advance(by: .seconds(3))
            try await Task.sleep(for: .milliseconds(50))
            XCTAssertEqual(writer.calls.count, 1, "nothing more is written: \(writer.calls)")
            XCTAssertEqual(held.current, shown)
            XCTAssertFalse(model.isPending(key))

            let row = SettingsFieldContext(
                model: model, path: key, fallback: field.key, profile: 1, listGeneration: model.profileListGeneration)
            field.editingChanged(row)(true)
            field.doubleBinding(row).wrappedValue = 0.3
            field.editingChanged(row)(false)
            try await waitUntil("the new row's press written") { writer.calls.count == 2 && !model.isWriting }
            XCTAssertEqual(writer.calls.last, .init(key: key, value: .float(0.3)))
        }
    }

    /// #536 with the press's write still running when the list changes:
    /// the step held back behind it is dropped with the press, not written
    /// to the profile now at that index once the write returns.
    func testAListChangeDuringAHeldSliderDropsTheStepHeldBehindAWrite() async throws {
        let clock = ManualClock()
        let gate = DispatchSemaphore(value: 0)
        defer { gate.signal() }
        let writer = FakeWriter()
        writer.respond = { _, attempt in
            if attempt == 1 { gate.wait() }
            return nil
        }
        let (model, source, _) = try fakeModel(Self.profileOpacities, writer: writer, clock: clock)
        let key = "profiles[1].appearance.opacity"
        model.beginChange(key)
        model.change(key, .float(0.6))
        try await waitUntil("0.6 being written") { writer.calls.count == 1 }
        model.change(key, .float(0.59))
        let removed = Self.profileOpacities.replacingOccurrences(of: #"{"name": "one"}, "#, with: "")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(removed))
        model.reload()
        gate.signal()
        try await waitUntil("the write returned") { !model.isWriting }
        clock.advance(by: .seconds(3))
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 1, "0.59 is not written: \(writer.calls)")
    }

    /// #536 on the core, in the window the app opens: the first profile is
    /// removed by another app while the second's opacity is held. With no
    /// release the row shows the applied value (the third profile's, now
    /// at that index), and nothing is written to it.
    func testARemovedProfileDuringAHeldSliderShowsTheAppliedValueOnTheCore() async throws {
        let profiles = """
            [[profiles]]
            name = "one"

            [[profiles]]
            name = "two"
            [profiles.appearance]
            opacity = 0.5

            [[profiles]]
            name = "three"
            [profiles.appearance]
            opacity = 0.7

            """
        let model = try openSettings(config: profiles)
        let key = "profiles[1].appearance.opacity"
        try await waitUntil("the profiles") { model.profiles.count == 3 }
        model.beginChange(key)
        model.change(key, .float(0.6))
        try await waitUntil("written") { !model.isWriting && self.contents.contains("0.6") }
        let removed = profiles.replacingOccurrences(of: "[[profiles]]\nname = \"one\"\n\n", with: "")
        XCTAssertNotEqual(removed, profiles)
        try removed.write(to: configFile, atomically: true, encoding: .utf8)
        try await waitUntil("applied, and the row shows it") {
            model.profiles.count == 2 && !model.isPending(key) && Self.same(model.value(key), .float(0.7))
        }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertFalse(model.isWriting)
        XCTAssertEqual(try file(), removed, "nothing written")
        XCTAssertTrue(Self.same(model.value(key), .float(0.7)))
    }

    /// The real opacity slider where it is an `NSSlider`: in an app linked
    /// against the macOS 15 SDK, which is what CI and releases build. An
    /// app linked against a later SDK gets SwiftUI's own slider, and the
    /// test is skipped.
    private func hostedOpacitySlider(
        _ context: SettingsFieldContext
    ) async throws -> (KeyControlledWindow, NSSlider) {
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.opacity" })
        let window = KeyControlledWindow(contentRect: NSRect(x: 0, y: 0, width: 500, height: 200))
        window.isKeyForTest = true
        window.contentView = NSHostingView(
            rootView: SettingsFieldRow(field: field, context: context).padding())
        window.orderFrontRegardless()
        func find(_ view: NSView) -> NSSlider? {
            (view as? NSSlider) ?? view.subviews.lazy.compactMap(find).first
        }
        for _ in 0..<50 {
            window.layoutIfNeeded()
            if let slider = window.contentView.flatMap(find) { return (window, slider) }
            try await Task.sleep(for: .milliseconds(10))
        }
        window.contentView = nil
        window.close()
        throw XCTSkip("Slider is not an NSSlider when linked against this SDK")
    }

    /// Forwards a control's action to the target SwiftUI gave it, and
    /// keeps the control's value at each one. An action alone proves
    /// nothing: the press itself sends one with the knob where it was (the
    /// CI runner, macOS 15, sent that and no more). An action with a value
    /// well away from the one at the press means AppKit moved the knob.
    @MainActor
    private final class ActionCounter: NSObject {
        let target: AnyObject?
        let action: Selector?
        let pressedAt: Double
        var values: [Double] = []

        init(_ control: NSControl) {
            target = control.target
            action = control.action
            pressedAt = control.doubleValue
            super.init()
            control.target = self
            control.action = #selector(forward(_:))
        }

        /// Whether a value is at least `distance` from the one at the press.
        func moved(_ distance: Double) -> Bool { values.contains { abs($0 - pressedAt) >= distance } }

        @objc func forward(_ sender: Any?) {
            values.append((sender as? NSControl)?.doubleValue ?? .nan)
            NSApp.sendAction(action ?? #selector(forward(_:)), to: target, from: sender)
        }
    }

    /// Presses `slider`'s knob with real events, drags it 40 points left,
    /// runs `whileHeld`, and lets go. With no mouse button really down the
    /// press returns at once and the control takes the drags and the
    /// release as they are dispatched, which is done here so that the
    /// release has happened on return. If AppKit's tracking loop runs
    /// instead, it takes them from the queue while the script runs on the
    /// main queue it keeps serving.
    ///
    /// Where the first drag does not move the knob (no action from the
    /// slider with a value 0.05 or more from the start, a seventh of the
    /// drag), the test is skipped with what was seen: AppKit did not track
    /// the synthetic drag, so the product was not exercised. Once the knob
    /// has moved, every check of the test applies, and a model that did
    /// not follow it fails.
    private func pressKnob(
        of slider: NSSlider, in window: NSWindow,
        whileHeld: @escaping @MainActor (_ drag: (CGFloat) -> Void) async throws -> Void
    ) async throws {
        let cell = try XCTUnwrap(slider.cell as? NSSliderCell)
        let knob = cell.knobRect(flipped: slider.isFlipped)
        let start = slider.convert(NSPoint(x: knob.midX, y: knob.midY), to: nil)
        let hit = window.contentView?.hitTest(window.contentView?.convert(start, from: nil) ?? start)
        let actions = ActionCounter(slider)
        defer {
            if slider.target === actions {
                slider.target = actions.target
                slider.action = actions.action
            }
        }
        var location = start
        var pressReturned = false
        var pressReturnedBeforeTheDrag = false
        func event(_ type: NSEvent.EventType) -> NSEvent? {
            NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: [],
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber,
                context: nil, eventNumber: 0, clickCount: 1, pressure: 1)
        }
        func deliver(_ type: NSEvent.EventType) {
            guard let posted = event(type) else { return XCTFail("no \(type) event") }
            NSApp.postEvent(posted, atStart: false)
            guard pressReturned else { return }
            while let next = NSApp.nextEvent(
                matching: [.leftMouseDragged, .leftMouseUp], until: .distantPast, inMode: .default, dequeue: true)
            {
                NSApp.sendEvent(next)
            }
        }
        func drag(_ offset: CGFloat) {
            location = NSPoint(x: start.x + offset, y: start.y)
            deliver(.leftMouseDragged)
        }
        let down = try XCTUnwrap(event(.leftMouseDown))
        let script = Task { @MainActor () throws -> Bool in
            defer { deliver(.leftMouseUp) }
            pressReturnedBeforeTheDrag = pressReturned
            drag(-40)
            let deadline = ContinuousClock.now + .seconds(2)
            while !actions.moved(0.05) && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            guard actions.moved(0.05) else { return false }
            try await whileHeld(drag)
            return true
        }
        window.sendEvent(down)
        pressReturned = true
        guard try await script.value else {
            let system = ProcessInfo.processInfo.operatingSystemVersionString
            throw XCTSkip(
                """
                the synthetic drag did not move the NSSlider on macOS \(system): \
                actions with the values \(actions.values) from \(actions.pressedAt); \
                press returned before the drag: \(pressReturnedBeforeTheDrag), \
                hit at the knob: \(hit.map { String(describing: type(of: $0)) } ?? "nothing"), \
                slider bounds \(slider.bounds), knob \(knob)
                """)
        }
    }

    private func applyOpacity(
        _ opacity: String, generation: UInt64, _ model: SettingsModel, _ source: FakeSource
    ) throws {
        let text = Self.fakeConfig.replacingOccurrences(of: #""opacity": 0.85"#, with: #""opacity": \#(opacity)"#)
        source.snapshot = TesseraConfigSnapshot(generation: generation, config: try Self.json(text))
        model.reload()
    }

    /// The adversarial case of #517, on the `NSSlider` CI and releases get:
    /// the knob is dragged and held, a hand edit applies, and the slider is
    /// let go without moving. Its final mouse-up action must not start a
    /// write of the stale knob value over the edit.
    func testAHandEditDuringAHeldNSSliderSurvivesTheRelease() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        var written = 0
        try await pressKnob(of: slider, in: window) { _ in
            try await self.waitUntil("the drag written") { !writer.calls.isEmpty && !model.isWriting }
            written = writer.calls.count
            try self.applyOpacity("0.3", generation: 1, model, source)
        }
        XCTAssertGreaterThan(written, 0)
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, written, "the release writes nothing: \(writer.calls)")
        XCTAssertEqual(model.value(key), .float(0.3), "the hand edit shows")
        XCTAssertFalse(model.isPending(key))
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(writer.calls.count, written)
        XCTAssertEqual(model.value(key), .float(0.3))
    }

    /// #534 on the `NSSlider` CI and releases get: a value off the step,
    /// clicked on the knob with no drag. Where AppKit sends no action for
    /// the click the press did not reach the slider, and the test is
    /// skipped; otherwise nothing is written and the file's value stays.
    func testANoMoveClickOnAnNSSliderKeepsAValueOffTheStep() async throws {
        let clock = ManualClock()
        let config = Self.fakeConfig.replacingOccurrences(
            of: #""opacity": 0.85"#, with: #""opacity": 0.12300000339746475"#)
        let (model, _, writer) = try fakeModel(config, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let cell = try XCTUnwrap(slider.cell as? NSSliderCell)
        let knob = cell.knobRect(flipped: slider.isFlipped)
        let point = slider.convert(NSPoint(x: knob.midX, y: knob.midY), to: nil)
        let actions = ActionCounter(slider)
        defer {
            slider.target = actions.target
            slider.action = actions.action
        }
        func event(_ type: NSEvent.EventType) throws -> NSEvent {
            try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: point, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        }
        NSApp.postEvent(try event(.leftMouseUp), atStart: false)
        window.sendEvent(try event(.leftMouseDown))
        while let next = NSApp.nextEvent(matching: .leftMouseUp, until: .distantPast, inMode: .default, dequeue: true) {
            NSApp.sendEvent(next)
        }
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        guard !actions.values.isEmpty || !writer.calls.isEmpty || model.isPending(key) else {
            throw XCTSkip("the synthetic click did not reach the NSSlider: no action, no change")
        }
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertEqual(writer.calls, [], "a click that does not move writes nothing")
        XCTAssertEqual(model.value(key), .float(0.12300000339746475))
        XCTAssertFalse(model.isPending(key))
    }

    /// The same slider without a hand edit: the last value of the drag,
    /// held back by the coalescing, is written by the release.
    func testADraggedNSSliderWritesItsLastValueOnRelease() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        var held: TesseraConfigValue?
        try await pressKnob(of: slider, in: window) { drag in
            try await self.waitUntil("the drag written") { !writer.calls.isEmpty && !model.isWriting }
            let first = model.value(key)
            drag(-80)
            try await self.waitUntil("the knob moved on") { model.value(key) != first }
            held = model.value(key)
            XCTAssertEqual(writer.calls.count, 1, "the second value waits for the coalescing")
        }
        try await waitUntil("the last value written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(writer.calls.last?.value, held)
        XCTAssertEqual(model.value(key), held)
    }

    /// Presses `slider`'s knob with real events and holds it without
    /// moving while `whileHeld` runs, then lets go. As in ``pressKnob``,
    /// the release has happened on return. Where the press does not reach
    /// the slider (no action from it), the test is skipped with what was
    /// seen.
    private func holdKnob(
        of slider: NSSlider, in window: NSWindow, _ actions: ActionCounter,
        whileHeld: @escaping @MainActor (_ drag: (CGFloat) -> Void) async throws -> Void
    ) async throws {
        let cell = try XCTUnwrap(slider.cell as? NSSliderCell)
        let knob = cell.knobRect(flipped: slider.isFlipped)
        let start = slider.convert(NSPoint(x: knob.midX, y: knob.midY), to: nil)
        var location = start
        var pressReturned = false
        func event(_ type: NSEvent.EventType) -> NSEvent? {
            NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: [],
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber,
                context: nil, eventNumber: 0, clickCount: 1, pressure: 1)
        }
        func deliver(_ type: NSEvent.EventType) {
            guard let posted = event(type) else { return XCTFail("no \(type) event") }
            NSApp.postEvent(posted, atStart: false)
            guard pressReturned else { return }
            while let next = NSApp.nextEvent(
                matching: [.leftMouseDragged, .leftMouseUp], until: .distantPast, inMode: .default, dequeue: true)
            {
                NSApp.sendEvent(next)
            }
        }
        let down = try XCTUnwrap(event(.leftMouseDown))
        let script = Task { @MainActor () throws -> Bool in
            defer { deliver(.leftMouseUp) }
            let deadline = ContinuousClock.now + .seconds(2)
            while actions.values.isEmpty && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            guard !actions.values.isEmpty else { return false }
            try await whileHeld { offset in
                location = NSPoint(x: start.x + offset, y: start.y)
                deliver(.leftMouseDragged)
            }
            return true
        }
        window.sendEvent(down)
        pressReturned = true
        guard try await script.value else {
            let system = ProcessInfo.processInfo.operatingSystemVersionString
            throw XCTSkip("the synthetic press did not reach the NSSlider on macOS \(system): no action")
        }
    }

    /// Skips a test whose press dragged `actions`' slider without moving it
    /// (no action 0.05 or more from the value at the press): the press then
    /// writes nothing, and the product was not exercised. The CI runner,
    /// macOS 15, does this, as ``pressKnob`` records; a drag that moved the
    /// knob runs every check of the test.
    private func skipUnlessTheDragMoved(_ actions: ActionCounter) throws {
        guard !actions.moved(0.05) else { return }
        let system = ProcessInfo.processInfo.operatingSystemVersionString
        throw XCTSkip(
            """
            the synthetic drag did not move the NSSlider on macOS \(system): \
            actions with the values \(actions.values) from \(actions.pressedAt)
            """)
    }

    /// Cancels the press `slider` is tracking as AppKit does when it cancels
    /// the active mouse gesture recognizers (a window moved by the window
    /// server, #564): the slider's `NSControlGestureRecognizer`, begun, is
    /// disabled and enabled again. Nil where the press is tracked without
    /// one (the classic tracking loop), with the recognizers seen.
    private func cancelTracking(of slider: NSSlider) -> String? {
        let recognizers = slider.gestureRecognizers
        guard
            let tracking = recognizers.first(where: {
                String(describing: type(of: $0)) == "NSControlGestureRecognizer"
                    && [.began, .changed].contains($0.state)
            })
        else {
            return recognizers.map { "\(type(of: $0)) state \($0.state.rawValue)" }.joined(separator: ", ")
        }
        tracking.isEnabled = false
        tracking.isEnabled = true
        return nil
    }

    /// #564: a hand edit applies while the `NSSlider` CI and releases get is
    /// held without moving, and the hold is cancelled (as AppKit does when
    /// the window server moves the window). The slider then sends the value
    /// under the pointer, the one from before the edit, inside the press;
    /// that is no step and is not written over the hand edit.
    func testACancelledNSSliderHoldAfterAHandEditKeepsTheEdit() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let actions = ActionCounter(slider)
        defer {
            slider.target = actions.target
            slider.action = actions.action
        }
        var resent: [Double] = []
        var diagnosis = ""
        try await holdKnob(of: slider, in: window, actions) { _ in
            // The window server counts no synthetic press: it is held.
            model.leftMousePress = { 1 }
            try self.applyOpacity("0.3", generation: 1, model, source)
            XCTAssertEqual(model.value(key), .float(0.3), "the hand edit shows")
            try await self.waitUntil("the hand edit on the knob") { abs(slider.doubleValue - 0.3) < 0.005 }
            let before = actions.values.count
            diagnosis = "before the cancel: actions \(actions.values), writes \(writer.calls), "
                + "value before the edit kept \(String(describing: model.valueBeforeHandEdit(key)))"
            if let seen = self.cancelTracking(of: slider) {
                throw XCTSkip("the press is tracked without a gesture recognizer: \(seen)")
            }
            resent = Array(actions.values[before...])
        }
        guard resent.contains(where: { abs($0 - 0.85) < 0.005 }) else {
            throw XCTSkip("the cancelled press re-sent no value from before the edit: \(resent)")
        }
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls, [], "the re-sent 0.85 is written over the hand edit; \(diagnosis)")
        XCTAssertEqual(model.value(key), .float(0.3))
        XCTAssertFalse(model.isPending(key))
    }

    /// #564 (critic B1): the hold made a step before the hand edit applied,
    /// then is cancelled. The slider sends the value under the press point,
    /// which is no step after the edit and is not written over it.
    func testACancelledNSSliderHoldAfterAStepAndAHandEditKeepsTheEdit() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let actions = ActionCounter(slider)
        defer {
            slider.target = actions.target
            slider.action = actions.action
        }
        var resent: [Double] = []
        try await holdKnob(of: slider, in: window, actions) { drag in
            model.leftMousePress = { 1 }
            drag(-40)
            let deadline = ContinuousClock.now + .seconds(2)
            while !actions.moved(0.05) && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            guard actions.moved(0.05) else {
                throw XCTSkip("the synthetic drag did not move the NSSlider: \(actions.values)")
            }
            try await self.waitUntil("the step written") { writer.calls.count == 1 && !model.isWriting }
            try self.applyOpacity("0.3", generation: 1, model, source)
            // The step stays pending through the hand edit: the knob stays on it.
            try await Task.sleep(for: .milliseconds(50))
            let before = actions.values.count
            if let seen = self.cancelTracking(of: slider) {
                throw XCTSkip("the press is tracked without a gesture recognizer: \(seen)")
            }
            resent = Array(actions.values[before...])
        }
        guard resent.contains(where: { abs($0 - 0.85) < 0.005 }) else {
            throw XCTSkip("the cancelled press re-sent no value from before the edit: \(resent)")
        }
        clock.advance(by: .zero)
        try await Task.sleep(for: .milliseconds(50))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(writer.calls.count, 1, "the re-sent 0.85 is written over the hand edit: \(writer.calls)")
        XCTAssertFalse(writer.calls.contains { abs(($0.value?.double ?? 0) - 0.85) < 0.005 }, "\(writer.calls)")
        XCTAssertEqual(model.value(key), .float(0.3))
    }

    /// #564: a real move of the knob after a hand edit applied during the
    /// hold is a step, and is written.
    func testAMoveAfterAHandEditDuringAnNSSliderHoldIsWritten() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let actions = ActionCounter(slider)
        defer {
            slider.target = actions.target
            slider.action = actions.action
        }
        try await holdKnob(of: slider, in: window, actions) { drag in
            model.leftMousePress = { 1 }
            try self.applyOpacity("0.3", generation: 1, model, source)
            try await self.waitUntil("the hand edit on the knob") { abs(slider.doubleValue - 0.3) < 0.005 }
            drag(-40)
            let deadline = ContinuousClock.now + .seconds(2)
            while writer.calls.isEmpty && !actions.moved(0.05) && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            guard actions.moved(0.05) else {
                throw XCTSkip("the synthetic drag did not move the NSSlider: \(actions.values)")
            }
        }
        try await waitUntil("the move written") { !writer.calls.isEmpty && !model.isWriting }
        let moved = try XCTUnwrap(writer.calls.last?.value?.double)
        XCTAssertGreaterThan(abs(moved - 0.85), 0.005, "the value from before the edit: \(writer.calls)")
        XCTAssertGreaterThan(abs(moved - 0.3), 0.005, "the hand edit: \(writer.calls)")
        XCTAssertEqual(model.value(key)?.double ?? 0, moved, accuracy: 0.0001)
    }

    /// #564 on every slider and SDK: after a hand edit applied during a
    /// hold with no step, the slider sending the value from before the
    /// edit again (a cancelled `NSSlider` press sends the value under the
    /// pointer) writes nothing, also where that is the step nearest a value
    /// off the step (0.123 by hand); a move to another value is written,
    /// and so is a move back to the value from before the edit after it.
    func testAValueFromBeforeAHandEditResentDuringTheHoldWritesNothingOnEverySlider() async throws {
        let dim = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        let cases: [(SettingsField, start: String, knob: Double, edit: String, shown: Double, moved: Double)] = [
            (try opacityField, "0.8500000238418579", 0.85, "0.30000001192092896", 0.3, 0.6),
            (try opacityField, "0.12300000339746475", 0.12, "0.5", 0.5, 0.7),
            (dim, "0.6000000238418579", 0.6, "0.33000001311302185", 0.33, 0.2),
        ]
        for (field, start, knob, edit, shown, moved) in cases {
            let name = "\(field.key) from \(start)"
            let config = { (value: String) in
                Self.fakeConfig.replacingOccurrences(
                    of: #""opacity": 0.85"#,
                    with: field.key == dim.key
                        ? #""opacity": 0.85, "dim_inactive_panes": \#(value)"# : #""opacity": \#(value)"#)
            }
            let (model, source, writer) = try fakeModel(config(start), clock: ManualClock())
            let context = SettingsFieldContext(model: model, path: field.key)
            let binding = field.doubleBinding(context)
            field.editingChanged(context)(true)
            binding.wrappedValue = knob  // the press's own action
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(config(edit)))
            model.reload()
            XCTAssertEqual(model.value(field.key)?.double ?? 0, shown, accuracy: 0.0001, name)
            binding.wrappedValue = knob  // re-sent: the press cancelled
            try await Task.sleep(for: .milliseconds(50))
            XCTAssertEqual(writer.calls, [], "\(name): the value from before the edit is written over it")
            XCTAssertEqual(model.value(field.key)?.double ?? 0, shown, accuracy: 0.0001, name)
            binding.wrappedValue = moved
            try await waitUntil("\(name): the move written") { writer.calls.count == 1 && !model.isWriting }
            binding.wrappedValue = knob
            field.editingChanged(context)(false)
            try await waitUntil("\(name): the way back written") { writer.calls.count == 2 && !model.isWriting }
            XCTAssertEqual(
                writer.calls, [.init(key: field.key, value: .float(moved)), .init(key: field.key, value: .float(knob))],
                name)
        }
    }

    /// #564 where the press has not begun for the model (SwiftUI begins a
    /// no-move `NSSlider` press only with its cancel): a hand edit that
    /// applies while the mouse button is held keeps the value from before
    /// it for that press, so the slider sending it again writes nothing.
    /// In a later press, or with the button up, the same value is a click
    /// on the slider and is written; a stepper's step there always is.
    func testAValueFromBeforeAHandEditInTheMousePressAloneWritesNothing() async throws {
        let stepper = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.font_size" })
        for (field, start, edit, before) in [
            (try opacityField, "0.85", "0.30000001192092896", 0.85),
            (stepper, "13.0", "15.0", 13.0),
        ] {
            let name = field.key
            let config = { (value: String) in
                Self.fakeConfig
                    .replacingOccurrences(of: #""opacity": 0.85"#, with: #""opacity": \#(name == stepper.key ? "0.85" : value)"#)
                    .replacingOccurrences(of: #""font_size": 13.0"#, with: #""font_size": \#(name == stepper.key ? value : "13.0")"#)
            }
            let (model, source, writer) = try fakeModel(config(start), clock: ManualClock())
            let context = SettingsFieldContext(model: model, path: field.key)
            let binding = field.doubleBinding(context)
            model.leftMousePress = { 7 }
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(config(edit)))
            model.reload()
            XCTAssertNotEqual(model.value(field.key)?.double ?? 0, before, accuracy: 0.0001, "\(name): the hand edit shows")
            field.editingChanged(context)(true)
            binding.wrappedValue = before  // the press point, sent with the cancel
            try await Task.sleep(for: .milliseconds(50))
            if name == stepper.key {
                try await waitUntil("\(name): the step written") { writer.calls.count == 1 && !model.isWriting }
                continue
            }
            XCTAssertEqual(writer.calls, [], "\(name): the value from before the edit is written over it")
            model.leftMousePress = { 8 }  // let go and pressed again, no release seen
            binding.wrappedValue = before  // a click there in the next press
            try await waitUntil("\(name): the next press written") { writer.calls.count == 1 && !model.isWriting }
            XCTAssertEqual(writer.calls.last?.value?.double ?? 0, before, accuracy: 0.0001, name)
            field.editingChanged(context)(false)
        }
    }

    /// #564 (critic B1) on every slider: a step, then a hand edit, during
    /// one press; the slider sending the press point again writes nothing,
    /// while a step after the hand edit is written.
    func testAPressPointResentAfterAStepAndAHandEditWritesNothingOnEverySlider() async throws {
        let dim = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        let cases: [(SettingsField, start: String, knob: Double, step: Double, edit: String, moved: Double)] = [
            (try opacityField, "0.8500000238418579", 0.85, 0.75, "0.30000001192092896", 0.6),
            (dim, "0.6000000238418579", 0.6, 0.5, "0.33000001311302185", 0.2),
        ]
        for (field, start, knob, step, edit, moved) in cases {
            let name = field.key
            let config = { (value: String) in
                Self.fakeConfig.replacingOccurrences(
                    of: #""opacity": 0.85"#,
                    with: field.key == dim.key
                        ? #""opacity": 0.85, "dim_inactive_panes": \#(value)"# : #""opacity": \#(value)"#)
            }
            let clock = ManualClock()
            let (model, source, writer) = try fakeModel(config(start), clock: clock)
            model.leftMousePress = { 3 }
            let context = SettingsFieldContext(model: model, path: field.key)
            let binding = field.doubleBinding(context)
            field.editingChanged(context)(true)
            binding.wrappedValue = step
            try await waitUntil("\(name): the step written") { writer.calls.count == 1 && !model.isWriting }
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(config(edit)))
            model.reload()
            binding.wrappedValue = knob  // re-sent: the press cancelled
            try await Task.sleep(for: .milliseconds(50))
            XCTAssertEqual(writer.calls.count, 1, "\(name): the press point is written over the hand edit: \(writer.calls)")
            binding.wrappedValue = moved
            clock.advance(by: .seconds(1))
            try await waitUntil("\(name): the move written") { writer.calls.count == 2 && !model.isWriting }
            binding.wrappedValue = knob
            field.editingChanged(context)(false)
            clock.advance(by: .seconds(1))
            try await waitUntil("\(name): the way back written") { writer.calls.count == 3 && !model.isWriting }
            XCTAssertEqual(writer.calls.last?.value?.double ?? 0, knob, accuracy: 0.0001, name)
        }
    }

    /// The left mouse press a test reports to the model
    /// (``SettingsModel/leftMousePress``), changed while a press runs.
    @MainActor private final class MousePress {
        var now: UInt32?
        init(_ now: UInt32? = nil) { self.now = now }
    }

    /// #564: press 1 steps and lets go, press 2 is down when press 1's own
    /// write applies and steps back to where press 1 began. The own write
    /// applying is no hand edit, so the step back is written, on both
    /// sliders and whether press 2 begins before the apply or at its step.
    func testAStepBackInAQuickRepressAfterTheFirstPressAppliesIsWrittenOnEverySlider() async throws {
        let dim = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.dim_inactive_panes" })
        let cases: [(SettingsField, start: String, step: Double, applied: String, back: Double, beginFirst: Bool)] = [
            (dim, "0.6000000238418579", 0.55, "0.550000011920929", 0.6, false),
            (dim, "0.6000000238418579", 0.55, "0.550000011920929", 0.6, true),
            (try opacityField, "0.8500000238418579", 0.84, "0.8399999737739563", 0.85, false),
        ]
        for (field, start, step, applied, back, beginFirst) in cases {
            let name = "\(field.key) begin first \(beginFirst)"
            let config = { (value: String) in
                Self.fakeConfig.replacingOccurrences(
                    of: #""opacity": 0.85"#,
                    with: field.key == dim.key
                        ? #""opacity": 0.85, "dim_inactive_panes": \#(value)"# : #""opacity": \#(value)"#)
            }
            let clock = ManualClock()
            let (model, source, writer) = try fakeModel(config(start), clock: clock)
            let press = MousePress(1)
            model.leftMousePress = { press.now }
            let context = SettingsFieldContext(model: model, path: field.key)
            let binding = field.doubleBinding(context)
            field.editingChanged(context)(true)
            binding.wrappedValue = step
            try await waitUntil("\(name): press 1 written") { writer.calls.count == 1 && !model.isWriting }
            press.now = nil
            field.editingChanged(context)(false)
            clock.advance(by: .zero)
            press.now = 2
            if beginFirst { field.editingChanged(context)(true) }
            source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(config(applied)))
            model.reload()
            if !beginFirst { field.editingChanged(context)(true) }
            binding.wrappedValue = back
            try await Task.sleep(for: .milliseconds(50))
            press.now = nil
            field.editingChanged(context)(false)
            clock.advance(by: .seconds(1))
            try await Task.sleep(for: .milliseconds(100))
            clock.advance(by: model.profileSettleTimeout)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(writer.calls.count, 2, "\(name): the step back is not written: \(writer.calls)")
            XCTAssertEqual(writer.calls.last?.value?.double ?? 0, back, accuracy: 0.0001, name)
        }
    }

    /// #564: the same on the `NSSlider` CI and releases get: a drag, let go,
    /// a second press that is down when the first one's write applies and
    /// drags back. The second press's move is written.
    func testAMoveBackInAQuickNSSliderRepressAfterTheFirstPressAppliesIsWritten() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let press = MousePress()
        model.leftMousePress = { press.now }
        let first = ActionCounter(slider)
        try await holdKnob(of: slider, in: window, first) { drag in
            press.now = 1
            drag(-40)
            try await Task.sleep(for: .milliseconds(50))
            press.now = nil
        }
        slider.target = first.target
        slider.action = first.action
        try skipUnlessTheDragMoved(first)
        try await waitUntil("press 1 written") { !writer.calls.isEmpty && !model.isWriting }
        let written = try XCTUnwrap(writer.calls.last?.value?.double)
        let second = ActionCounter(slider)
        defer {
            slider.target = second.target
            slider.action = second.action
        }
        try await holdKnob(of: slider, in: window, second) { drag in
            press.now = 2
            try self.applyOpacity(String(Double(Float(written))), generation: 1, model, source)
            try await Task.sleep(for: .milliseconds(50))
            drag(40)
            try await Task.sleep(for: .milliseconds(50))
            press.now = nil  // the button is up when the mouse-up is handled
        }
        clock.advance(by: .seconds(1))
        try await Task.sleep(for: .milliseconds(100))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(100))
        let moved = try XCTUnwrap(second.values.last)
        XCTAssertNotEqual(moved, written, accuracy: 0.005, "press 2 did not move: \(second.values)")
        XCTAssertEqual(
            writer.calls.last?.value?.double ?? 0, moved, accuracy: 0.005,
            "press 2's move is not written: \(writer.calls), actions \(second.values)")
    }

    /// #564: press 1 drags and lets go, press 2 is down before press 1's
    /// write applies, a hand edit applies and press 2 is cancelled. The
    /// slider re-sends the value press 2 began on, press 1's step the
    /// window showed but the file never had; that is no step and is not
    /// written over the hand edit.
    func testACancelledQuickNSSliderRepressAfterAHandEditKeepsTheEdit() async throws {
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let key = "appearance.opacity"
        let (window, slider) = try await hostedOpacitySlider(.init(model: model, path: key))
        defer {
            window.contentView = nil
            window.close()
        }
        let press = MousePress()
        model.leftMousePress = { press.now }
        let first = ActionCounter(slider)
        try await holdKnob(of: slider, in: window, first) { drag in
            press.now = 1
            drag(-40)
            try await Task.sleep(for: .milliseconds(50))
            press.now = nil
        }
        slider.target = first.target
        slider.action = first.action
        try skipUnlessTheDragMoved(first)
        try await waitUntil("press 1 written") { !writer.calls.isEmpty && !model.isWriting }
        let second = ActionCounter(slider)
        defer {
            slider.target = second.target
            slider.action = second.action
        }
        var resent: [Double] = []
        try await holdKnob(of: slider, in: window, second) { _ in
            press.now = 2
            try self.applyOpacity("0.30000001192092896", generation: 1, model, source)
            try await Task.sleep(for: .milliseconds(80))
            let before = second.values.count
            if let seen = self.cancelTracking(of: slider) {
                throw XCTSkip("the press is tracked without a gesture recognizer: \(seen)")
            }
            resent = Array(second.values[before...])
            try await Task.sleep(for: .milliseconds(50))
        }
        press.now = nil
        clock.advance(by: .seconds(1))
        try await Task.sleep(for: .milliseconds(100))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(
            model.value(key)?.double ?? 0, 0.3, accuracy: 0.0001,
            "the hand edit is overwritten: re-sent \(resent), writes \(writer.calls)")
        XCTAssertEqual(writer.calls.count, 1, "re-sent \(resent): \(writer.calls)")
    }

    /// #564: the binding twin of the cancelled quick re-press, for any
    /// slider: press 2 begins on press 1's unapplied step, a hand edit
    /// applies, and the step is re-sent inside press 2. Nothing is written.
    func testAValueResentInAQuickRepressAfterAHandEditWritesNothing() async throws {
        let field = try opacityField
        let clock = ManualClock()
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: clock)
        let context = SettingsFieldContext(model: model, path: field.key)
        let binding = field.doubleBinding(context)
        let press = MousePress(1)
        model.leftMousePress = { press.now }
        field.editingChanged(context)(true)
        binding.wrappedValue = 0.74
        try await waitUntil("press 1 written") { writer.calls.count == 1 && !model.isWriting }
        press.now = nil
        field.editingChanged(context)(false)
        press.now = 2
        try applyOpacity("0.30000001192092896", generation: 1, model, source)
        field.editingChanged(context)(true)
        binding.wrappedValue = 0.74  // re-sent with the cancel: the value press 2 began on
        field.editingChanged(context)(false)
        press.now = nil
        clock.advance(by: .seconds(1))
        try await Task.sleep(for: .milliseconds(100))
        clock.advance(by: model.profileSettleTimeout)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(writer.calls.count, 1, "the re-sent step is written over the hand edit: \(writer.calls)")
        XCTAssertEqual(model.value(field.key)?.double ?? 0, 0.3, accuracy: 0.0001)
    }

    /// #517: an own value applied and a step held back: another key applying
    /// is no hand edit of this one, so the release writes the step.
    func testAHeldStepIsWrittenWhenAnotherKeyAppliesAfterAnOwnValue() async throws {
        let (model, source, writer) = try fakeModel(Self.fakeConfig, clock: ManualClock())
        let key = "appearance.font_size"
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
        try applyFontSize("14.0", generation: 1, model, source)
        model.change(key, .float(15))
        XCTAssertEqual(writer.calls.count, 1, "held back")
        let other = Self.fakeConfig
            .replacingOccurrences(of: #""font_size": 13.0"#, with: #""font_size": 14.0"#)
            .replacingOccurrences(of: "Tessera Dark", with: "Nord")
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(other))
        model.reload()
        model.endChange(key)
        try await waitUntil("written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertEqual(model.value(key), .float(15))
    }

    /// 24.12 m2: once the notice is taken back, a further apply moves nothing.
    func testAFurtherApplyAfterTheNoticeWasTakenBackMovesNothing() async throws {
        let (model, source, _, refusal) = try await modelWithAReleasedAdd()
        let applied = Self.withNewProfile(Self.entriesAndFiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(applied))
        model.reload()
        XCTAssertEqual(model.message("profiles[3].command"), refusal)
        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(applied))
        model.reload()
        XCTAssertEqual(model.message("profiles[3].command"), refusal)
        XCTAssertNil(model.message("profiles[4].command"))
    }

    /// #530 on the core: a slider dragged away and back in one press, its
    /// first value written and not applied yet. The file has moved on, so
    /// the way back is written too.
    func testASliderDraggedAwayAndBackEndsOnTheLastValueOnTheCore() async throws {
        let model = try openSettings(config: "[appearance]\nopacity = 0.5\n")
        let key = "appearance.opacity"
        try await waitUntil("the file's value") { Self.same(model.value(key), .float(0.5)) }
        model.beginChange(key)
        model.change(key, .float(0.6))
        try await waitUntil("written") { !model.isWriting && self.contents.contains("0.6") }
        model.change(key, .float(0.5))
        model.endChange(key)
        try await waitUntil("the last value, shown and in the file") {
            Self.same(model.value(key), .float(0.5)) && !self.contents.contains("0.6")
        }
        XCTAssertTrue(try lines().contains { $0.hasPrefix("opacity") && $0.contains("0.5") }, contents)
    }

    /// #530 on the core: the same with a stepper, up and down again.
    func testAStepperSteppedUpAndDownEndsOnTheLastValueOnTheCore() async throws {
        let model = try openSettings(config: "[appearance]\nfont_size = 13.0\n")
        let key = "appearance.font_size"
        try await waitUntil("the file's value") { model.value(key) == .float(13) }
        model.beginChange(key)
        model.change(key, .float(14))
        try await waitUntil("written") { !model.isWriting && self.contents.contains("14") }
        model.change(key, .float(13))
        model.endChange(key)
        try await waitUntil("the last value, shown and in the file") {
            model.value(key) == .float(13) && !self.contents.contains("14")
        }
        XCTAssertTrue(try lines().contains { $0.hasPrefix("font_size") && $0.contains("13") }, contents)
    }

    /// #530 on the core: two clicks of a stepper, each its own press.
    func testTwoStepperClicksUpAndDownEndOnTheLastValueOnTheCore() async throws {
        let model = try openSettings(config: "[appearance]\nfont_size = 13.0\n")
        let key = "appearance.font_size"
        try await waitUntil("the file's value") { model.value(key) == .float(13) }
        model.beginChange(key)
        model.change(key, .float(14))
        model.endChange(key)
        try await waitUntil("written") { !model.isWriting && self.contents.contains("14") }
        model.beginChange(key)
        model.change(key, .float(13))
        model.endChange(key)
        try await waitUntil("the last value, shown and in the file") {
            model.value(key) == .float(13) && !self.contents.contains("14")
        }
        XCTAssertTrue(try lines().contains { $0.hasPrefix("font_size") && $0.contains("13") }, contents)
    }

    /// 24.4: a test that opens Settings again leaves no directory behind.
    func testEveryAttemptsConfigurationDirectoryIsRemoved() throws {
        try openSettings(config: "")
        let first = try XCTUnwrap(dir)
        try XCTUnwrap(appDelegate.settingsWindowController).close()
        try openSettings(config: "")
        let second = try XCTUnwrap(dir)
        XCTAssertNotEqual(first, second)
        try appDelegate.replaceCore(start: false)
        removeTemporaryDirectories()
        XCTAssertFalse(FileManager.default.fileExists(atPath: first.path), "the earlier attempt's")
        XCTAssertFalse(FileManager.default.fileExists(atPath: second.path))
    }

    /// 24.12 m2: a watch slower than the settle time. The add applies after
    /// the list gave up on it: the notice goes, and the rows after it and
    /// their messages move on again. Another change of the list before
    /// that leaves the notice.
    func testAnAddAppliedAfterTheSettleTimeTakesBackTheNotice() async throws {
        let (model, source, notice, refusal) = try await modelWithAReleasedAdd()
        let renamed = Self.entriesAndFiles.replacingOccurrences(of: #""name": "one""#, with: #""name": "uno""#)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(renamed))
        model.reload()
        XCTAssertEqual(model.message(SettingsModel.profilesKey), notice, "the add is still not shown")

        source.snapshot = TesseraConfigSnapshot(generation: 2, config: try Self.json(Self.withNewProfile(renamed)))
        model.reload()
        XCTAssertNil(model.message(SettingsModel.profilesKey), "applied after all")
        XCTAssertEqual(model.profiles.map(\.name), ["uno", "two", "New Profile", "extra", "zeta"])
        XCTAssertNil(model.message("profiles[2].command"))
        XCTAssertEqual(model.message("profiles[3].command"), refusal, "moved on with extra")
    }

    /// 24.12 m2: a new add after the notice is followed on its own: when it
    /// applies, the rows after it move once, not again for the old one.
    func testANewAddAfterAReleasedOneMovesTheMessagesOnce() async throws {
        let (model, source, _, refusal) = try await modelWithAReleasedAdd()
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.message("profiles[3].command"), refusal, "moved by the new add")
        let applied = Self.withNewProfile(Self.entriesAndFiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(applied))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(model.message("profiles[3].command"), refusal)
        XCTAssertNil(model.message("profiles[4].command"), "not moved twice")
    }

    /// 24.12 m2: so does a delete. A list that later has a profile of the
    /// old add's name at its place moves no message.
    func testADeleteAfterAReleasedAddMovesNoMessageForIt() async throws {
        let (model, source, _, refusal) = try await modelWithAReleasedAdd(refusedRow: 3)
        XCTAssertTrue(model.deleteProfile(1))
        try await waitUntil("written") { !model.isWriting }
        let applied = Self.entriesAndFiles
            .replacingOccurrences(of: #""name": "two""#, with: #""name": "other""#)
            .replacingOccurrences(of: #""name": "extra""#, with: #""name": "New Profile""#)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(applied))
        model.reload()
        XCTAssertFalse(model.profilesChanging)
        XCTAssertEqual(model.profiles.map(\.name), ["one", "other", "New Profile", "zeta"])
        XCTAssertEqual(model.message("profiles[3].command"), refusal, "zeta is still at 3")
        XCTAssertNil(model.message("profiles[4].command"))
    }

    /// `entriesAndFiles`-shaped `config` with the added profile in place.
    private static func withNewProfile(_ config: String) -> String {
        config.replacingOccurrences(
            of: #"{"name": "extra""#, with: #"{"name": "New Profile", "source": "config"}, {"name": "extra""#)
    }

    /// A refused edit of a row after the add, then an add no configuration
    /// showed within the settle time: returns its notice and the refusal.
    private func modelWithAReleasedAdd(
        refusedRow row: Int = 2
    ) async throws -> (SettingsModel, FakeSource, String, String) {
        let clock = ManualClock()
        let writer = FakeWriter()
        let key = "profiles[\(row)].command"
        let refusal = "`\(key)`: this profile is defined in a file in profiles/, not in config.toml"
        writer.respond = { call, _ in
            call.key == key ? TesseraConfigEditError(.invalidArgument, message: refusal) : nil
        }
        let (model, source, _) = try fakeModel(Self.entriesAndFiles, writer: writer, clock: clock)
        model.set(key, .string("/bin/sh"))
        try await waitUntil("refused") { !model.isWriting }
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        clock.advance(by: model.profileSettleTimeout)
        let notice = try XCTUnwrap(model.message(SettingsModel.profilesKey))
        XCTAssertTrue(notice.contains("was written"), notice)
        XCTAssertEqual(model.message(key), refusal, "back with its row")
        return (model, source, notice, refusal)
    }

    /// 24.12 m2: the same for a delete the watch applies late.
    func testADeleteAppliedAfterTheSettleTimeTakesBackTheNotice() async throws {
        let clock = ManualClock()
        let (model, source, _) = try fakeModel(Self.threeProfiles, clock: clock)
        XCTAssertTrue(model.deleteProfile(2))
        try await waitUntil("written") { !model.isWriting }
        clock.advance(by: model.profileSettleTimeout)
        XCTAssertNotNil(model.message(SettingsModel.profilesKey))
        let deleted = Self.threeProfiles.replacingOccurrences(of: #", {"name": "three"}"#, with: "")
        XCTAssertNotEqual(deleted, Self.threeProfiles)
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(deleted))
        model.reload()
        XCTAssertNil(model.message(SettingsModel.profilesKey), "applied after all")
    }

    /// 24.12 m6: only a failed edit is read as "Not saved"; a notice says
    /// what happened itself.
    func testOnlyAFailedEditIsSpokenAsNotSaved() {
        XCTAssertEqual(SettingsMessage.spoken("too big", notSaved: true), "Not saved: too big")
        XCTAssertEqual(SettingsMessage.spoken(SettingsModel.heldValueNotice, notSaved: false), SettingsModel.heldValueNotice)
        XCTAssertTrue(SettingsMessage(text: "x").notSaved, "an edit's message by default")
    }

    /// 24.12 c2 minor, on the real control: a profile inheriting its font
    /// size steps from the inherited value.
    func testTheRealStepperOfAnInheritingProfileStepsFromTheInheritedSize() async throws {
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: ManualClock())
        let key = "appearance.font_size"
        let path = "profiles[0].\(key)"
        let context = SettingsFieldContext(model: model, path: path, fallback: key, profile: 0)
        let (window, stepper) = try await hostedFontSizeStepper(context)
        defer {
            window.contentView = nil
            window.close()
        }
        try pressUpArrow(of: stepper).release()
        try await waitUntil("the step written") { writer.calls.count == 1 && !model.isWriting }
        XCTAssertEqual(writer.calls, [.init(key: path, value: .float(14))], "13 + 1, not the bottom of the range")
    }

    /// 24.12 c2 minor, on the core: a profile with no font size of its own
    /// shows the one it inherits, whether `[appearance]` sets it or the
    /// default applies, and its first step goes on from that value, not
    /// from the bottom of the range.
    func testAProfilesFirstStepGoesOnFromTheInheritedFontSize() async throws {
        let profile = "[[profiles]]\nname = \"one\"\ncommand = \"/bin/cat\"\n"
        for config in ["[appearance]\nfont_size = 17.0\n\n" + profile, profile] {
            let model = try openSettings(config: config)
            let key = "appearance.font_size"
            let path = "profiles[0].\(key)"
            let field = try XCTUnwrap(ProfilesPane.appearanceFields.first { $0.key == key })
            let context = SettingsFieldContext(model: model, path: path, fallback: key, profile: 0)
            let inherited = try XCTUnwrap(model.value(key)?.double, "the size in effect")
            XCTAssertGreaterThan(inherited, 5, config)
            XCTAssertTrue(model.inherits(0, key), config)
            XCTAssertEqual(field.doubleBinding(context).wrappedValue, inherited, config)
            model.setInherits(0, key, false)
            let binding = field.doubleBinding(context)
            XCTAssertEqual(binding.wrappedValue, inherited, config)
            binding.wrappedValue += 1
            try await waitUntil("the step applied") { Self.same(model.snapshot[path], .float(inherited + 1)) }
            try XCTUnwrap(appDelegate.settingsWindowController).close()
        }
    }

    /// 21.6a adversarial M6: a profile's slider value held back by the
    /// coalescing when a delete locks the profiles is not written (its
    /// index may name another profile once the delete applies), and the
    /// list says so; the slider shows what was written of the drag.
    func testASlidersProfileValueHeldBackByADeleteIsNotSavedWithANotice() async throws {
        let clock = ManualClock()
        let (model, _, writer) = try fakeModel(Self.threeProfiles, clock: clock)
        let key = "profiles[1].appearance.opacity"
        model.change(key, .float(0.4))
        try await waitUntil("the first value written") { writer.calls.count == 1 && !model.isWriting }
        model.change(key, .float(0.5))
        XCTAssertTrue(model.deleteProfile(2))
        try await waitUntil("the delete written") { writer.calls.count == 2 && !model.isWriting }
        XCTAssertNil(model.message(SettingsModel.profilesKey))
        clock.advance(by: SettingsModel.coalescingInterval)
        XCTAssertEqual(model.message(SettingsModel.profilesKey), SettingsModel.heldValueNotice)
        XCTAssertEqual(model.value(key), .float(0.4), "what was written of the drag")
        model.endChange(key)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(
            writer.calls, [.init(key: key, value: .float(0.4)), .init(key: "profiles[2]", value: nil)],
            "0.5 is not written")
    }

    /// 21.6a adversarial M4: the core compares profile names byte for
    /// byte, so a decomposed "café" is another name than the composed one,
    /// and the window takes it as the core does.
    func testAProfileNameIsComparedByteForByte() throws {
        let (model, _, _) = try fakeModel(#"{"profiles": [{"name": "café"}, {"name": "two"}]}"#)
        XCTAssertEqual(model.profileNameRefusal("caf\u{E9}", at: 1), "A profile named “café” exists.")
        XCTAssertNil(model.profileNameRefusal("cafe\u{301}", at: 1), "decomposed: another name to the core")
        XCTAssertNil(model.profileNameRefusal("caf\u{E9}", at: 0), "its own name")
    }

    /// 21.6a adversarial M5: Add pressed while a profile's Name field holds
    /// a name the window refuses says, by the list, that the name was not
    /// saved (its own message goes from view with the selection).
    func testAddAfterARefusedRenameSaysTheNameWasNotSaved() async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        let field = NSObject()
        model.holdDraft(field) { model.renameProfile(1, to: "one") }
        XCTAssertTrue(model.addProfile())
        let notice = "The name typed was not saved: A profile named “one” exists."
        XCTAssertEqual(model.message(SettingsModel.profilesKey), notice)
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(writer.calls, [FakeWriter.added("New Profile")])
        XCTAssertEqual(model.message(SettingsModel.profilesKey), notice)

        // A name it takes says nothing.
        let (other, _, _) = try fakeModel(Self.threeProfiles)
        other.holdDraft(field) { other.renameProfile(1, to: "zwei") }
        XCTAssertTrue(other.duplicateProfile(1))
        XCTAssertNil(other.message(SettingsModel.profilesKey))
        try await waitUntil("written") { !other.isWriting }
    }

    /// #312 adversarial M1: a configuration an earlier edit applies while
    /// the add runs (the keys this window had pending when it was queued
    /// now applied) does not end the add: its row stays and the list stays
    /// locked until a configuration shows it.
    func testAnEarlierEditAppliedBeforeTheAddsResultKeepsTheAdd() async throws {
        let gate = DispatchSemaphore(value: 0)
        let writer = FakeWriter()
        writer.respond = { call, _ in
            if call.key == FakeWriter.addKey { gate.wait() }
            return nil
        }
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        model.set("profiles[1].command", .string("/bin/sh"))
        XCTAssertTrue(model.addProfile())
        try await waitUntil("the command written, the add running") { writer.calls.count == 2 }
        // The command's result comes back to the main thread.
        try await Task.sleep(for: .milliseconds(100))
        // The watch applies the command's edit before the add's result.
        let commandOnly = Self.threeProfiles.replacingOccurrences(of: "/bin/cat", with: "/bin/sh")
        source.snapshot = TesseraConfigSnapshot(generation: 1, config: try Self.json(commandOnly))
        model.reload()
        XCTAssertFalse(model.isPending("profiles[1].command"), "applied")
        gate.signal()
        try await waitUntil("added") { !model.isWriting }
        XCTAssertTrue(model.profilesChanging, "adv M1: the add waits for a list that shows it")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "two", "three", "New Profile"])
    }

    /// #312 m1 and m5: an add holds no pane, so a model whose add was
    /// written and never applied goes with its last reference (the pane
    /// and window closed; the settle timer holds it weakly). A pane shown
    /// on a model whose add was written meanwhile selects the profile.
    func testAnAddWrittenWithNoPaneHoldsNothingAndThePaneShownThenSelectsIt() async throws {
        weak var gone: SettingsModel?
        do {
            let (model, _, writer) = try fakeModel(Self.threeProfiles)
            gone = model
            XCTAssertTrue(model.addProfile())
            try await waitUntil("written") { writer.calls.count == 1 && !model.isWriting }
            XCTAssertTrue(model.profilesChanging, "written, never applied")
        }
        try await waitUntil("m1: the model goes, its add never applied") { gone == nil }

        let (model, _, _) = try fakeModel(Self.threeProfiles)
        XCTAssertTrue(model.addProfile())
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(model.selectionRequest, 3)
        let window = profilesPane(model)
        defer {
            window.contentView = nil
            window.close()
        }
        try await waitUntil("m5: the pane shown later selects the copy") {
            let table = Self.views(NSTableView.self, in: window.contentView).first
            return table?.numberOfRows == 4 && table?.selectedRow == 3
        }
        XCTAssertNil(model.selectionRequest, "taken")
    }

    /// 21.6a m7: a directory chosen with Choose… while text is typed into
    /// the Working directory field is the last write, and the field shows
    /// it: the typed text is written first, not over it once the field
    /// loses the focus.
    func testChooseAfterTypedTextKeepsTheChosenDirectory() async throws {
        try await chooseAfterTypedText(onWindow: true)
    }

    /// Without a window to take the focus from the field (the panel runs
    /// on its own), the field still writes its text no more once the
    /// model committed it: each guard alone keeps the chosen directory.
    func testChooseAfterTypedTextKeepsTheChosenDirectoryWithTheFieldFocused() async throws {
        try await chooseAfterTypedText(onWindow: false)
    }

    /// Types into two's working directory, then Choose… picks another one
    /// (21.6a m7): the typed text is written first, the chosen directory
    /// last, and the field shows it.
    private func chooseAfterTypedText(onWindow: Bool) async throws {
        let (model, _, writer) = try fakeModel(Self.threeProfiles)
        let window = profilesPane(model)
        if onWindow { model.window = window }
        defer {
            window.contentView = nil
            window.close()
        }
        try await waitUntil("the list shows the profiles") {
            Self.views(NSTableView.self, in: window.contentView).first?.numberOfRows == 3
        }
        let table = try XCTUnwrap(Self.views(NSTableView.self, in: window.contentView).first)
        Self.click(table, row: 1)
        let directory = {
            Self.views(NSTextField.self, in: window.contentView).first {
                $0.placeholderString == "Home folder" && $0.isEditable
            }
        }
        try await waitUntil("two's fields are shown") { directory() != nil }
        let field = try XCTUnwrap(directory())
        XCTAssertTrue(window.makeFirstResponder(field))
        field.currentEditor()?.selectAll(nil)
        (field.currentEditor() as? NSTextView)?.insertText(
            "/typed", replacementRange: NSRange(location: NSNotFound, length: 0))
        try await Task.sleep(for: .milliseconds(50))

        let panel = RecordingOpenPanel(url: URL(filePath: "/tmp/chosen", directoryHint: .notDirectory))
        let path = "profiles[1].working_directory"
        SettingsFieldRow.chooseDirectory(
            panel, for: ProfilesPane.fields[2], .init(model: model, path: path, profile: 1))
        XCTAssertEqual(panel.runs, [onWindow ? .sheet(on: ObjectIdentifier(window)) : .alone])
        try await Task.sleep(for: .milliseconds(100))
        panel.end(.OK)
        if onWindow {
            // Choose… let the field go: it shows the directory without a
            // click elsewhere.
            try await waitUntil("the field shows it") { directory()?.stringValue == "/tmp/chosen" }
        }
        window.makeFirstResponder(nil)
        try await Task.sleep(for: .milliseconds(150))
        try await waitUntil("written") { !model.isWriting }
        XCTAssertEqual(
            writer.calls,
            [.init(key: path, value: .string("/typed")), .init(key: path, value: .string("/tmp/chosen"))],
            "the chosen directory last")
        XCTAssertEqual(model.value(path), .string("/tmp/chosen"))
        try await waitUntil("the field shows it") { directory()?.stringValue == "/tmp/chosen" }
    }
}

/// Records each edit and answers with `respond` (by the call and how many
/// times its key was edited, from 1); asserts no edit runs on the main
/// thread.
final class FakeWriter: SettingsConfigWriting, @unchecked Sendable {
    struct Call: Equatable {
        var key: String
        var value: TesseraConfigValue?
    }

    private let lock = NSLock()
    private var recorded: [Call] = []
    private var onMain = 0
    private var responder: @Sendable (Call, Int) -> TesseraConfigEditError? = { _, _ in nil }
    var importResult: Result<String, TesseraConfigEditError> = .success("imported")

    var calls: [Call] { lock.withLock { recorded } }
    var mainThreadCalls: Int { lock.withLock { onMain } }
    var respond: @Sendable (Call, Int) -> TesseraConfigEditError? {
        get { lock.withLock { responder } }
        set { lock.withLock { responder = newValue } }
    }

    private func record(_ call: Call) throws(TesseraConfigEditError) {
        XCTAssertFalse(Thread.isMainThread, "\(call.key) edited on the main thread")
        let (attempt, respond) = lock.withLock {
            recorded.append(call)
            if Thread.isMainThread { onMain += 1 }
            return (recorded.filter { $0.key == call.key }.count, responder)
        }
        if let error = respond(call, attempt) { throw error }
    }

    func set(_ key: String, to value: TesseraConfigValue) throws(TesseraConfigEditError) {
        try record(Call(key: key, value: value))
    }

    func remove(_ key: String) throws(TesseraConfigEditError) {
        try record(Call(key: key, value: nil))
    }

    func importTheme(at file: URL) throws(TesseraConfigEditError) -> String {
        try record(Call(key: "import:\(file.lastPathComponent)", value: nil))
        return try importResult.get()
    }

    /// The key of the call an add of a profile records.
    static let addKey = "add_profile"

    /// An add of a profile named after `base` with `keys`, as recorded.
    static func added(_ base: String, _ keys: [String: TesseraConfigValue] = [:]) -> Call {
        Call(key: addKey, value: .table(keys.merging(["name": .string(base)]) { $1 }))
    }

    /// The configuration in effect (``FakeSource``'s), which an add reads
    /// as the core does: the profile lands after the `[[profiles]]`
    /// entries (the profiles whose `source` is not a file in `profiles/`).
    var applied: Locked<TesseraConfigSnapshot>?

    func addProfile(
        named base: String, keys: [(key: String, value: TesseraConfigValue)]
    ) throws(TesseraConfigEditError) -> TesseraAddedProfile {
        // After the entries of the file as it is before the edit.
        let profiles = applied?.value["profiles"]?.array ?? []
        let entries = profiles.filter { $0["source"]?.string?.hasPrefix("profiles/") != true }.count
        try record(Self.added(base, Dictionary(keys, uniquingKeysWith: { $1 })))
        return TesseraAddedProfile(index: entries, name: base)
    }
}

/// The configuration and themes the model reads, set by the test (the
/// snapshot from any thread, as the core's watch would apply it).
@MainActor
final class FakeSource: SettingsConfigSource {
    /// The configuration in effect.
    let applied = Locked(TesseraConfigSnapshot(generation: 0, config: .table([:])))
    var themeList: [TesseraTheme] = []
    let writer: FakeWriter

    init(writer: FakeWriter) {
        self.writer = writer
        writer.applied = applied
    }

    var snapshot: TesseraConfigSnapshot {
        get { applied.value }
        set { applied.value = newValue }
    }

    func configSnapshot() throws -> TesseraConfigSnapshot { snapshot }
    func themes() throws -> [TesseraTheme] { themeList }
    /// The key map, if the test gives one; else it cannot be read.
    var keymapResult: TesseraKeymap?
    func keymap() throws -> TesseraKeymap {
        guard let keymapResult else { throw TesseraCoreError.invalidArgument }
        return keymapResult
    }
    /// The plugins, or `nil` when they cannot be read; counts the reads.
    var pluginList: [TesseraPlugin]? = []
    private(set) var pluginReads = 0
    func plugins() throws -> [TesseraPlugin] {
        pluginReads += 1
        guard let pluginList else { throw TesseraCoreError.invalidArgument }
        return pluginList
    }
    var settingsWriter: any SettingsConfigWriting { writer }
}

/// A value behind a lock.
final class Locked<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: Value

    init(_ value: Value) {
        stored = value
    }

    var value: Value {
        get { lock.withLock { stored } }
        set { lock.withLock { stored = newValue } }
    }
}

/// A clock the test moves: timers run as ``advance(by:)`` passes them.
@MainActor
final class ManualClock: SettingsClock {
    private(set) var now = ContinuousClock.now
    private var timers: [(due: ContinuousClock.Instant, timer: SettingsTimer, action: @MainActor () -> Void)] = []

    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) -> SettingsTimer {
        let timer = SettingsTimer()
        timers.append((now + delay, timer, action))
        return timer
    }

    func advance(by duration: Duration) {
        let target = now + duration
        while let next = timers.indices.filter({ timers[$0].due <= target }).min(by: { timers[$0].due < timers[$1].due }) {
            let (due, timer, action) = timers.remove(at: next)
            now = due
            if !timer.isCancelled { action() }
        }
        now = target
    }
}

/// An open panel that records how `SettingsPanels.present` ran it, and
/// ends when the test says (#492).
@MainActor
private final class RecordingOpenPanel: SettingsOpenPanel {
    enum Run: Equatable {
        case sheet(on: ObjectIdentifier)
        case alone
        case modal
    }

    private(set) var runs: [Run] = []
    let url: URL?
    private var handler: ((NSApplication.ModalResponse) -> Void)?

    init(url: URL?) { self.url = url }

    func beginSheetModal(for window: NSWindow, completionHandler handler: @escaping (NSApplication.ModalResponse) -> Void) {
        runs.append(.sheet(on: ObjectIdentifier(window)))
        self.handler = handler
    }

    func begin(completionHandler handler: @escaping (NSApplication.ModalResponse) -> Void) {
        runs.append(.alone)
        self.handler = handler
    }

    func runModal() -> NSApplication.ModalResponse {
        runs.append(.modal)
        return .cancel
    }

    func end(_ response: NSApplication.ModalResponse) {
        let handler = handler
        self.handler = nil
        handler?(response)
    }
}

/// A stand-in `config.toml` of profiles for ``FakeWriter/respond``: edited
/// as the core edits it (a profile by its index, in the file as it is at
/// the call), with a hand edit armed to land during the next call of a
/// profile's key, which then reports `Conflict` (#549).
final class HandEditedProfiles: @unchecked Sendable {
    struct Profile {
        var name: String
        var command: String?
    }

    private let lock = NSLock()
    private var listed: [Profile]
    private var armed: Int?
    private var inserted = 0
    private var entries: [String] = []
    /// The configuration in effect, applied at once with the hand edit
    /// when set (the watch was quicker than the conflict).
    var quickApply: Locked<TesseraConfigSnapshot>?

    init(_ profiles: [Profile]) {
        listed = profiles
    }

    var profiles: [Profile] { lock.withLock { listed } }
    /// What happened to the file, in order.
    var log: [String] { lock.withLock { entries } }

    /// Lands a hand edit during the next call of a profile's key: 0
    /// removes the first profile, 1 swaps the first two, 2 inserts one
    /// first, 3 edits another key.
    func arm(_ kind: Int) { lock.withLock { armed = kind } }
    func disarm() { lock.withLock { armed = nil } }

    /// The file as the core's JSON.
    func json() -> String {
        lock.withLock { Self.json(listed) }
    }

    private static func json(_ profiles: [Profile]) -> String {
        let list = profiles.map { profile -> [String: Any] in
            var entry: [String: Any] = ["name": profile.name]
            if let command = profile.command { entry["command"] = command }
            return entry
        }
        let all: [String: Any] = [
            "general": ["default_profile": "p0"], "appearance": ["theme": "Tessera Dark"], "profiles": list,
        ]
        return String(decoding: try! JSONSerialization.data(withJSONObject: all), as: UTF8.self)
    }

    private func handEdit(_ kind: Int) {
        switch kind {
        case 0 where listed.count > 2:
            entries.append("hand edit: remove \(listed[0].name)")
            listed.removeFirst()
        case 1 where listed.count > 1:
            entries.append("hand edit: swap \(listed[0].name) and \(listed[1].name)")
            listed.swapAt(0, 1)
        case 2:
            inserted += 1
            entries.append("hand edit: insert h\(inserted)")
            listed.insert(Profile(name: "h\(inserted)", command: nil), at: 0)
        default:
            entries.append("hand edit: another key")
        }
    }

    /// The writer's answer to `call`.
    func respond(_ call: FakeWriter.Call) -> TesseraConfigEditError? {
        lock.withLock {
            let profile = call.key.hasPrefix("profiles[")
            if let kind = armed, profile {
                armed = nil
                handEdit(kind)
                entries.append("conflict: \(call.key) = \(call.value?.string ?? "nil")")
                if let quickApply, let config = try? TesseraConfigValue.parse(json: Data(Self.json(listed).utf8)) {
                    quickApply.value = TesseraConfigSnapshot(generation: quickApply.value.generation + 1, config: config)
                }
                return TesseraConfigEditError(.conflict, message: "config.toml changed on disk")
            }
            entries.append("written: \(call.key) = \(call.value?.string ?? "nil")")
            guard profile, let close = call.key.firstIndex(of: "]"),
                let index = Int(call.key[call.key.index(call.key.startIndex, offsetBy: 9)..<close])
            else { return nil }
            guard listed.indices.contains(index) else {
                entries.append("  past the end")
                return nil
            }
            if call.key[close...] == "].command" { listed[index].command = call.value?.string }
            return nil
        }
    }
}

/// SplitMix64: a seeded generator, so a seed replays the same edits.
struct SplitMix {
    private var state: UInt64

    init(_ seed: UInt64) {
        state = seed
    }

    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var mixed = state
        mixed = (mixed ^ (mixed >> 30)) &* 0xBF58_476D_1CE4_E5B9
        mixed = (mixed ^ (mixed >> 27)) &* 0x94D0_49BB_1331_11EB
        return mixed ^ (mixed >> 31)
    }
}
