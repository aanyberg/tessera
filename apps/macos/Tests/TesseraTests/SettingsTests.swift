import AppKit
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
        if let bar = NSApp.mainMenu { MainMenu.setProfiles([], in: bar) }
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        sources = []
        appDelegate.opener = WorkspaceOpener()
        appDelegate.configFileCreator = nil
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

    /// Gives the app a core of its own on `config` in a fresh temporary
    /// directory and opens Settings on it; returns the window's model.
    @discardableResult
    private func openSettings(config: String) throws -> SettingsModel {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-settings-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
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

    /// The blur radius slider (#390, ADR 0044 mirror of `blur_radius`)
    /// writes whole numbers within 0...64, coalesced like any slider, never
    /// a float (which the core refuses for an integer key).
    func testTheBlurRadiusSliderWritesWholeNumbersInItsRange() async throws {
        let (model, _, writer) = try fakeModel(Self.fakeConfig, clock: ManualClock())
        let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == "appearance.blur_radius" })
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
            let field = try XCTUnwrap(AppearancePane.fields.first { $0.key == key })
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
        XCTAssertNil(model.addProfile())
        XCTAssertNil(model.duplicateProfile(1))
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
        XCTAssertEqual(model.duplicateProfile(1), 2)
        XCTAssertNil(model.addProfile(), "the copy is not applied yet")
        try await waitUntil("the copy ran") { !model.isWriting }
        XCTAssertEqual(writer.calls.dropFirst().map(\.key), ["profiles[2].name"])
        XCTAssertEqual(writer.calls.last?.value, .string("three Copy"))
    }

    /// The list and the profile's controls are disabled while an add or
    /// delete waits for the watch; a failed one enables them again at once
    /// with its message by the list.
    func testTheProfileListIsLockedUntilAnAddOrDeleteIsAppliedOrFails() async throws {
        let writer = FakeWriter()
        writer.respond = { call, _ in
            call.key == "profiles[3].name" ? TesseraConfigEditError(.io, message: "disk\u{202E} full") : nil
        }
        let (model, source, _) = try fakeModel(Self.threeProfiles, writer: writer)
        XCTAssertFalse(model.profilesChanging)
        model.set("profiles[2].command", .string("/bin/sh"))
        XCTAssertFalse(model.profilesChanging, "an edit of one profile moves no index")
        XCTAssertEqual(model.addProfile(), 3)
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
            Edit(key: "appearance.theme", value: .string(theme), line: "theme = \"\(theme)\""),
            Edit(key: "appearance.font_family", value: .string(font), line: "font_family = \"\(font)\""),
            Edit(key: "appearance.font_size", value: .float(15), line: "font_size = 15.0"),
            Edit(key: "appearance.opacity", value: .float(0.5), line: "opacity = 0.5"),
            Edit(key: "appearance.blur", value: .bool(true), line: "blur = true"),
            Edit(key: "appearance.blur_radius", value: .integer(20), line: "blur_radius = 20"),
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

        XCTAssertEqual(model.addProfile(), 2)
        try await waitUntil("added") { !model.isWriting }
        XCTAssertTrue(try file().contains(#"name = "New Profile""#), contents)
        try await waitUntil("the window lists it") { model.profiles.map(\.name) == ["one", "two", "New Profile"] }

        XCTAssertEqual(model.duplicateProfile(1), 3)
        try await waitUntil("duplicated") { !model.isWriting }
        try await waitUntil("the copy is listed with the command") {
            model.profiles.map(\.name) == ["one", "two", "New Profile", "two Copy"]
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
            ["general", "appearance", "profiles", "keys", "advanced"])
        controller.select(pane: "profiles")
        XCTAssertEqual(controller.tabs?.selectedTabViewItemIndex, 2)
    }

    func testClosingTheWindowFreesItsControllerAndModel() async throws {
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

// MARK: Stand-ins

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
