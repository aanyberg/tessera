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
    private var dir: URL?
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
        if let dir { try? FileManager.default.removeItem(at: dir) }
        sources = []
        appDelegate.opener = WorkspaceOpener()
        appDelegate.configFileCreator = nil
        appDelegate.settingsWaitLimit = .seconds(2)
        appDelegate.settingsClock = SystemSettingsClock()
        appDelegate = nil
    }

    // MARK: Helpers

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

        XCTAssertTrue(model.deleteProfile(5))
        try await waitUntil("the pane says it waits") { Self.shownTexts(in: window.contentView).contains(waiting) }
        XCTAssertFalse(Self.shownTexts(in: window.contentView).contains(cap))
        try await waitUntil("written") { !model.isWriting }
        let fewer = (0..<256).filter { $0 != 5 }.map { #"{"name": "p\#($0)"}"# }.joined(separator: ", ")
        source.snapshot = TesseraConfigSnapshot(
            generation: 1, config: try Self.json(#"{"general": {"default_profile": "p0"}, "profiles": [\#(fewer)]}"#))
        model.reload()
        // The buttons and the status line's view update apart: wait for both.
        try await waitUntil("Add is enabled and no status line is shown") {
            let shown = Self.shownTexts(in: window.contentView)
            return Self.listButtons(in: window).first?.isEnabled == true
                && !shown.contains(cap) && !shown.contains(waiting)
        }
        XCTAssertNil(model.profilesStatus)
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

    /// 21.6a m6: a delete retried after a conflict, which the reload shows
    /// applied already (the external edit was of another key), unlocks the
    /// list at once: no later configuration would change the list again.
    func testARetriedDeleteAlreadyAppliedUnlocksAtOnce() async throws {
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
        XCTAssertTrue(model.deleteProfile(1))
        try await waitUntil("written") { !model.isWriting }
        XCTAssertFalse(model.profilesChanging, "m6: the reload shows the delete")
        XCTAssertEqual(model.profiles.map(\.name), ["one", "three"])
        XCTAssertEqual(model.value("appearance.theme"), .string("Nord"))
        clock.advance(by: .seconds(3))
        XCTAssertNil(model.message(SettingsModel.profilesKey), "no settle notice either")
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
