import AppKit
import SwiftUI
import XCTest

@testable import Tessera
import TesseraKit

/// The Keys pane of the Settings window (ADR 0044, Keys). Recording,
/// conflicts, Restore Defaults and refusals run against a core of the
/// test's own on a `config.toml` in a temporary directory (never the
/// user's), with key events from a stand-in monitor; the monitor's
/// lifetime, which events it takes, chord naming and sanitising run on
/// stand-ins.
@MainActor
final class KeysPaneTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL?
    private var previous: String?
    private var replacedCore = false
    /// The stand-in sources of the test's models, which hold them weakly.
    private var sources: [FakeSource] = []
    private var windows: [NSWindow] = []

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertNil(appDelegate.settingsWindowController, "a previous test left Settings open")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        appDelegate.settingsWindowController?.close()
        windows.forEach { $0.close() }
        windows = []
        if replacedCore {
            // A core on an empty configuration puts the default shortcuts
            // back on the menus.
            let empty = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-keys-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
            setenv("TESSERA_CONFIG_DIR", empty.path, 1)
            try appDelegate.replaceCore(start: true)
            try appDelegate.replaceCore(start: false)
            try? FileManager.default.removeItem(at: empty)
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
    /// directory and opens Settings on its Keys pane, with key events from
    /// `monitor`.
    private func openKeys(config: String, monitor: FakeKeyMonitor) throws -> SettingsWindowController {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-keys-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true
        try appDelegate.replaceCore(start: true)
        MainMenu.run(command: "settings")
        let controller = try XCTUnwrap(appDelegate.settingsWindowController)
        controller.model.keys.monitoring = monitor
        controller.select(pane: "keys")
        return controller
    }

    /// A key-down of `keyCode` in `window`, typing `characters`.
    private static func key(
        _ keyCode: UInt16, _ characters: String, _ flags: NSEvent.ModifierFlags, in window: NSWindow?
    ) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                windowNumber: window?.windowNumber ?? 0, context: nil, characters: characters,
                charactersIgnoringModifiers: characters, isARepeat: false, keyCode: keyCode))
    }

    private static func flags(_ flags: NSEvent.ModifierFlags, in window: NSWindow?) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .flagsChanged, location: .zero, modifierFlags: flags, timestamp: 0,
                windowNumber: window?.windowNumber ?? 0, context: nil, characters: "",
                charactersIgnoringModifiers: "", isARepeat: false, keyCode: 56))
    }

    /// The command's chords in the key map the window last read.
    private static func chords(_ command: String, _ controller: SettingsWindowController) -> [String]? {
        controller.model.keymap?.command(named: command)?.chords
    }

    // MARK: Recording (core)

    func testRecordingCmdShiftEWritesTheEntryAndTheMenuShowsItOnceApplied() async throws {
        let monitor = FakeKeyMonitor()
        let controller = try openKeys(config: "# my settings\n[appearance]\nfont_size = 14.0 # big\n", monitor: monitor)
        let keys = controller.model.keys
        let window = try XCTUnwrap(controller.window)
        keys.startRecording("clear_scrollback")
        XCTAssertEqual(keys.recording, "clear_scrollback")
        XCTAssertEqual(monitor.live, 1)

        // Modifiers alone are shown and wait for a key.
        XCTAssertFalse(monitor.send(try Self.flags([.command, .shift], in: window)))
        XCTAssertEqual(keys.heldModifiers, "⇧⌘")
        XCTAssertEqual(keys.recording, "clear_scrollback")
        XCTAssertTrue(monitor.send(try Self.key(14, "E", [.command, .shift], in: window)), "the pane takes the key")
        XCTAssertNil(keys.recording)
        XCTAssertEqual(monitor.live, 0, "the monitor goes once a key is recorded")
        XCTAssertTrue(keys.isChanging)
        XCTAssertEqual(
            keys.groups(matching: "Clear to Start").flatMap(\.rows).first?.chords, ["cmd+shift+e"],
            "the recorded chord shows at once")

        // The set, then the old default's unbinding, one key at a time.
        try await waitUntil("written") {
            let text = (try? self.file()) ?? ""
            return text.contains("\"cmd+shift+e\" = \"clear_scrollback\"") && text.contains("\"cmd+k\" = \"unbound\"")
        }
        let text = try file()
        XCTAssertTrue(text.contains("# my settings") && text.contains("font_size = 14.0 # big"), text)

        // The watch applies the file: on_config_applied gives the menus and
        // the pane the new map.
        let item = try XCTUnwrap(MainMenu.item(forCommand: "clear_scrollback", in: NSApp.mainMenu))
        try await waitUntil("the menu shows ⇧⌘E") {
            item.keyEquivalent == "e" && item.keyEquivalentModifierMask == [.command, .shift]
        }
        try await waitUntil("the pane shows the applied map") {
            Self.chords("clear_scrollback", controller) == ["cmd+shift+e"] && !keys.isChanging
        }
    }

    func testTheSystemMonitorTakesOnlyTheSettingsWindowsKeyWhileRecording() async throws {
        let controller = try openKeys(config: "", monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        keys.monitoring = SystemKeyEventMonitoring()
        let window = try XCTUnwrap(controller.window)
        keys.startRecording("copy_find_matches")
        NSApp.sendEvent(try Self.key(14, "E", [.command, .shift], in: window))
        XCTAssertNil(keys.recording, "the local monitor saw the key")
        try await waitUntil("written") { (try? self.file())?.contains("\"cmd+shift+e\" = \"copy_find_matches\"") ?? false }
    }

    /// Counts the key-downs a local monitor of the test's sees, and the
    /// ones it sees after the pane's monitor has taken the key (`leaks`).
    /// `NSEvent` runs local monitors in no fixed order (a probe saw both),
    /// so a key seen before the pane's monitor ran is no leak.
    @MainActor
    private final class OtherMonitor {
        var keyDowns = 0
        var leaks = 0
        var taken: () -> Bool = { false }
        private var monitor: Any?

        init() {
            monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.keyDowns += 1
                    if self.taken() { self.leaks += 1 }
                }
                return event
            }
        }

        func remove() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }
    }

    func testTheSystemMonitorSwallowsTheRecordedKeyAndNoOther() async throws {
        let window = testWindow()
        let key = try Self.key(14, "e", [.control, .shift], in: window)
        let other = OtherMonitor()
        defer { other.remove() }
        // Twenty rounds, so both monitor orders come up.
        for round in 0..<20 {
            let (model, _, writer) = try fakeKeys()
            let keys = model.keys
            keys.monitoring = SystemKeyEventMonitoring()
            keys.window = window
            keys.startRecording("find")
            other.taken = { keys.recording == nil }
            other.keyDowns = 0
            NSApp.sendEvent(key)
            XCTAssertNil(keys.recording, "round \(round): recorded")
            XCTAssertLessThanOrEqual(other.keyDowns, 1)
            let seen = other.keyDowns
            other.taken = { false }
            NSApp.sendEvent(key)
            NSApp.sendEvent(key)
            XCTAssertEqual(other.keyDowns, seen + 2, "round \(round): the keys after it go on")
            XCTAssertEqual(keys.registrationCount, 0)
            // The chord is written on SettingsModel.writeQueue (#373).
            try await waitUntil("round \(round): written") { !writer.calls.isEmpty }
            XCTAssertEqual(writer.calls.first?.value, .string("find"))
        }
        XCTAssertEqual(other.leaks, 0, "the recorded key reaches no monitor after the pane's")
    }

    func testTheSystemMonitorGoesWithItsRegistration() throws {
        @MainActor final class Count { var value = 0 }
        let window = testWindow()
        let key = try Self.key(14, "e", [.control], in: window)
        let seen = Count()
        let registration = SystemKeyEventMonitoring().monitorKeys { _ in
            seen.value += 1
            return false
        }
        let later = OtherMonitor()
        defer { later.remove() }
        NSApp.sendEvent(key)
        XCTAssertEqual(seen.value, 1)
        XCTAssertEqual(later.keyDowns, 1, "a key the handler does not take goes on")
        registration.remove()
        NSApp.sendEvent(key)
        XCTAssertEqual(seen.value, 1, "removed")

        // Freed without remove().
        let freed = Count()
        autoreleasepool {
            _ = SystemKeyEventMonitoring().monitorKeys { _ in
                freed.value += 1
                return false
            }
        }
        NSApp.sendEvent(key)
        XCTAssertEqual(freed.value, 0, "the registration's end removes it")
        XCTAssertEqual(later.keyDowns, 3)
    }

    // MARK: Conflicts (core)

    func testAConflictingChordShowsTheConflictAndWritesNothingUntilReplaced() async throws {
        let monitor = FakeKeyMonitor()
        let config = "[keys]\n# mine\n\"cmd+u\" = \"new_tab\"\n"
        let controller = try openKeys(config: config, monitor: monitor)
        let keys = controller.model.keys
        let window = try XCTUnwrap(controller.window)

        // ⌘W is Close Tab's default: shown, nothing written, Cancel keeps it.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(13, "w", [.command], in: window)))
        XCTAssertEqual(keys.conflict?.chord, "cmd+w")
        XCTAssertEqual(keys.conflict?.text, "⌘W is the shortcut of “Close Tab”.")
        XCTAssertFalse(keys.isChanging, "nothing is queued")
        keys.cancelConflict()
        XCTAssertNil(keys.conflict)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(try file(), config, "a cancelled conflict writes nothing")

        // ⇧⌘[ as the US layout's key at its place is Previous Tab's chord,
        // as the key map writes it.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(33, "[", [.command, .shift], in: window)))
        XCTAssertEqual(keys.conflict?.chord, "cmd+shift+[")
        XCTAssertEqual(keys.conflict?.text, "⇧⌘[ is the shortcut of “Show Previous Tab”.")
        keys.cancelConflict()

        // ⌘U is New Tab's by an entry: Replace writes over that entry.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(32, "u", [.command], in: window)))
        XCTAssertEqual(keys.conflict?.text, "⌘U is the shortcut of “New Tab”.")
        XCTAssertEqual(try file(), config)
        keys.replace()
        try await waitUntil("replaced") { (try? self.file())?.contains("\"cmd+u\" = \"find\"") ?? false }
        try await waitUntil("applied") { Self.chords("find", controller) == ["cmd+u"] && !keys.isChanging }
        XCTAssertEqual(Self.chords("new_tab", controller), ["cmd+t"], "New Tab keeps its default")
        let text = try file()
        XCTAssertFalse(text.contains("\"new_tab\""), text)
        XCTAssertTrue(text.contains("\"cmd+f\" = \"unbound\""), text)
        XCTAssertTrue(text.contains("# mine"), text)

        // A default chord replaced: the other command no longer has it.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(13, "w", [.command], in: window)))
        keys.replace()
        try await waitUntil("applied") { Self.chords("find", controller) == ["cmd+w"] && !keys.isChanging }
        XCTAssertEqual(Self.chords("close_tab", controller), [])
        XCTAssertFalse(try file().contains("\"cmd+u\""), "the previous entry for Find goes")
    }

    /// An entry naming no command binds nothing (the core warns and leaves
    /// it out): no conflict, unlike one that sends text (#40c adversarial
    /// M1).
    func testAnEntryNamingNoCommandIsNoConflict() async throws {
        let config = "[keys]\n\"cmd+shift+u\" = \"no_such_command\"\n\"cmd+shift+y\" = \"send_text:hi\"\n"
        let controller = try openKeys(config: config, monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        keys.record("cmd+shift+y", for: "find")
        XCTAssertEqual(keys.conflict?.text, "⇧⌘Y is set to send text in config.toml.")
        keys.cancelConflict()
        keys.record("cmd+shift+u", for: "find")
        XCTAssertNil(keys.conflict)
        try await waitUntil("written") { (try? self.file())?.contains("\"cmd+shift+u\" = \"find\"") ?? false }
        try await waitUntil("applied") { Self.chords("find", controller) == ["cmd+shift+u"] && !keys.isChanging }
    }

    func testAChordTheCoreRefusesShowsItsSanitisedReasonAndWritesNothing() async throws {
        let monitor = FakeKeyMonitor()
        let controller = try openKeys(config: "", monitor: monitor)
        let keys = controller.model.keys
        let window = try XCTUnwrap(controller.window)

        // ⌘Q is Quit's (an AppKit item): the core refuses to shadow it.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(12, "q", [.command], in: window)))
        XCTAssertNotNil(keys.conflict)
        keys.replace()
        try await waitUntil("refused") { keys.message("find") != nil && !keys.isChanging }
        let message = try XCTUnwrap(keys.message("find"))
        XCTAssertTrue(message.contains("quit"), message)
        XCTAssertEqual(message, SettingsModel.sanitisedMessage(message))

        // A plain letter would take typing.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(0, "a", [], in: window)))
        XCTAssertNil(keys.conflict, "no command has A")
        try await waitUntil("refused") { keys.message("find") != nil && !keys.isChanging }
        XCTAssertNotEqual(keys.message("find"), message)
        XCTAssertEqual(try file(), "", "nothing written")
        XCTAssertEqual(Self.chords("find", controller), ["cmd+f"])
    }

    // MARK: Restore Defaults (core)

    func testRestoreDefaultsRemovesOnlyCommandAndUnboundEntriesAndKeepsComments() async throws {
        let config = """
            # My configuration.
            [keys]
            # Keys I set by hand.
            "cmd+shift+e" = "clear_scrollback"
            "cmd+k" = "unbound"
            "ctrl+shift+t" = "select_tab:3"
            # Text for the shell.
            "cmd+shift+y" = "send_text:clear\\n"
            "cmd+shift+x" = "none"

            [appearance]
            # Bigger.
            font_size = 14.0

            """
        let controller = try openKeys(config: config, monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        XCTAssertEqual(Self.chords("clear_scrollback", controller), ["cmd+shift+e"])
        XCTAssertTrue(keys.hasCustomShortcuts)
        keys.restoreDefaults()
        try await waitUntil("restored") { Self.chords("clear_scrollback", controller) == ["cmd+k"] && !keys.isChanging }
        let text = try file()
        for gone in ["\"cmd+shift+e\"", "\"cmd+k\"", "\"ctrl+shift+t\""] {
            XCTAssertFalse(text.contains(gone), "\(gone) removed: \(text)")
        }
        for kept in [
            "# My configuration.", "[keys]", "# Text for the shell.", "\"cmd+shift+y\" = \"send_text:clear\\n\"",
            "\"cmd+shift+x\" = \"none\"", "[appearance]", "# Bigger.", "font_size = 14.0",
        ] {
            XCTAssertTrue(text.contains(kept), "\(kept) kept: \(text)")
        }
        XCTAssertEqual(Self.chords("select_tab:3", controller), ["cmd+3"])
        XCTAssertFalse(keys.hasCustomShortcuts)
    }

    // MARK: Edits (stand-ins)

    /// A key map of four commands, and `[keys]` as `entries`.
    private func fakeKeys(
        entries: [String: String] = [:], warnings: [String] = [], newTabChords: [String] = ["cmd+t"],
        generation: UInt64 = 0, writer: FakeWriter = FakeWriter()
    ) throws -> (SettingsModel, FakeSource, FakeWriter) {
        let source = FakeSource(writer: writer)
        source.snapshot = TesseraConfigSnapshot(
            generation: generation, config: .table(["keys": .table(entries.mapValues { .string($0) })]))
        source.keymapResult = try Self.keymap(generation: generation, newTabChords: newTabChords, warnings: warnings)
        sources.append(source)
        return (SettingsModel(source: source), source, writer)
    }

    private static func keymap(
        generation: UInt64, newTabChords: [String] = ["cmd+t"], fontChords: [String] = ["cmd+shift+=", "cmd+="],
        warnings: [String] = []
    ) throws -> TesseraKeymap {
        func command(_ id: Int, _ name: String, _ owner: String, _ chords: [String]) -> [String: Any] {
            ["id": id, "command": name, "name": name, "owner": owner, "chords": chords, "menu": [[String: Any]]()]
        }
        let object: [String: Any] = [
            "generation": generation,
            "commands": [
                command(0, "new_tab", "app", newTabChords), command(1, "find", "app", ["cmd+f"]),
                command(2, "font_bigger", "app", fontChords), command(3, "copy_mode", "planned", ["cmd+shift+c"]),
            ],
            "warnings": warnings,
        ]
        return try JSONDecoder().decode(TesseraKeymap.self, from: JSONSerialization.data(withJSONObject: object))
    }

    /// A window events can name, never shown key.
    private func testWindow() -> NSWindow {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.orderBack(nil)
        windows.append(window)
        return window
    }

    func testRecordingReplacesEveryOtherChordInOrderAndStopsAtTheFirstFailure() async throws {
        let writer = FakeWriter()
        let (model, _, _) = try fakeKeys(
            entries: ["cmd+u": "font_bigger", "shift+cmd+o": "font_bigger"], writer: writer)
        // Two entries, then the defaults no entry took.
        try sources[0].keymapResult = Self.keymap(
            generation: 0, fontChords: ["cmd+u", "cmd+shift+o", "cmd+shift+=", "cmd+="])
        model.reload()
        let keys = model.keys
        keys.record("cmd+shift+e", for: "font_bigger")
        try await waitUntil("written") { writer.calls.count == 5 }
        XCTAssertEqual(
            writer.calls,
            [
                .init(key: "keys.\"cmd+shift+e\"", value: .string("font_bigger")),
                .init(key: "keys.\"cmd+u\"", value: nil),
                .init(key: "keys.\"shift+cmd+o\"", value: nil),
                .init(key: "keys.\"cmd+shift+=\"", value: .string("unbound")),
                .init(key: "keys.\"cmd+=\"", value: .string("unbound")),
            ])

        // A refused set writes nothing after it.
        let refusing = FakeWriter()
        refusing.respond = { call, _ in
            call.value == .string("find") ? TesseraConfigEditError(.refused, message: "nope \u{202E}x") : nil
        }
        let (other, _, _) = try fakeKeys(entries: ["cmd+u": "find"], writer: refusing)
        other.keys.record("cmd+shift+e", for: "find")
        try await waitUntil("refused") { other.keys.message("find") != nil }
        XCTAssertEqual(refusing.calls, [.init(key: "keys.\"cmd+shift+e\"", value: .string("find"))])
        XCTAssertEqual(other.keys.message("find"), "nope x")
        XCTAssertFalse(other.keys.isChanging, "a failure ends the edit at once")
    }

    func testAnEditWaitsForAKeyMapAppliedAfterItAndNothingIsEditedFromATornRead() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        let monitor = FakeKeyMonitor()
        keys.monitoring = monitor
        keys.window = testWindow()
        keys.record("cmd+shift+e", for: "find")
        try await waitUntil("written") { writer.calls.count == 2 }
        XCTAssertTrue(keys.isChanging, "until a later key map applies")
        XCTAssertEqual(keys.groups(matching: "").flatMap(\.rows).first { $0.command == "find" }?.chords, ["cmd+shift+e"])
        keys.startRecording("new_tab")
        XCTAssertNil(keys.recording, "nothing is recorded meanwhile")
        XCTAssertEqual(monitor.live, 0)
        keys.restoreDefaults()
        keys.clear("new_tab")
        model.reload()
        XCTAssertTrue(keys.isChanging, "the same generation again")
        XCTAssertEqual(writer.calls.count, 2)

        // The configuration was read at a generation the key map is not
        // (#348), every time: nothing is edited.
        source.snapshot = TesseraConfigSnapshot(
            generation: 2, config: .table(["keys": .table(["cmd+shift+e": .string("find")])]))
        source.keymapResult = try Self.keymap(generation: 1)
        model.reload()
        XCTAssertFalse(keys.isChanging)
        keys.record("cmd+shift+y", for: "new_tab")
        XCTAssertEqual(keys.message("new_tab"), "config.toml is being applied; try again.")
        XCTAssertEqual(writer.calls.count, 2)
    }

    func testEscapeCancelsDeleteClearsAndOtherWindowsKeysPass() async throws {
        let writer = FakeWriter()
        let (model, _, _) = try fakeKeys(entries: ["cmd+u": "font_bigger"], writer: writer)
        try sources[0].keymapResult = Self.keymap(generation: 0, fontChords: ["cmd+u", "cmd+shift+=", "cmd+="])
        model.reload()
        let keys = model.keys
        let monitor = FakeKeyMonitor()
        keys.monitoring = monitor
        let window = testWindow()
        let other = testWindow()
        XCTAssertNotEqual(window.windowNumber, other.windowNumber)
        keys.window = window

        keys.startRecording("font_bigger")
        // Another window's key passes, and recording goes on.
        XCTAssertFalse(monitor.send(try Self.key(14, "e", [.command], in: other)))
        XCTAssertFalse(monitor.send(try Self.key(14, "e", [.command], in: nil)))
        XCTAssertEqual(keys.recording, "font_bigger")
        // Escape cancels.
        XCTAssertTrue(monitor.send(try Self.key(53, "\u{1B}", [], in: window)))
        XCTAssertNil(keys.recording)
        XCTAssertEqual(monitor.live, 0)
        XCTAssertFalse(monitor.send(try Self.key(14, "e", [.command], in: window)), "not recording: the key passes")
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(writer.calls, [])

        // Delete clears: the entry goes and the defaults are unbound.
        keys.startRecording("font_bigger")
        XCTAssertTrue(monitor.send(try Self.key(51, "\u{7F}", [], in: window)))
        XCTAssertNil(keys.recording)
        try await waitUntil("cleared") { writer.calls.count == 3 }
        XCTAssertEqual(
            writer.calls,
            [
                .init(key: "keys.\"cmd+u\"", value: nil),
                .init(key: "keys.\"cmd+shift+=\"", value: .string("unbound")),
                .init(key: "keys.\"cmd+=\"", value: .string("unbound")),
            ])
        XCTAssertEqual(keys.groups(matching: "font").flatMap(\.rows).first?.chords, [])
    }

    func testAKeyFloodWhileRecordingRecordsOneChordAndHoldsNothing() async throws {
        let writer = FakeWriter()
        let (model, _, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        let monitor = FakeKeyMonitor()
        keys.monitoring = monitor
        let window = testWindow()
        keys.window = window
        keys.startRecording("find")
        let start = ContinuousClock.now
        for index in 0..<10_000 {
            _ = monitor.send(try Self.flags(index.isMultiple(of: 2) ? [.command] : [.command, .shift], in: window))
        }
        XCTAssertEqual(keys.registrationCount, 2, "one monitor and one observer set")
        XCTAssertEqual(keys.recording, "find")
        for _ in 0..<10_000 {
            _ = monitor.send(try Self.key(14, "e", [.command, .shift], in: window))
        }
        let elapsed = ContinuousClock.now - start
        XCTAssertLessThan(elapsed, .seconds(5), "20 000 events in \(elapsed)")
        try await waitUntil("written") { writer.calls.count == 2 }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(writer.calls.count, 2, "one chord recorded: its set and the old default's unbinding")
        XCTAssertEqual(monitor.live, 0)
    }

    // MARK: Lifetime

    func testTheMonitorGoesWhenThePaneTheWindowsKeyOrTheWindowGoesAndTheWindowFreesItsModel() async throws {
        let monitor = FakeKeyMonitor()
        weak var weakController: SettingsWindowController?
        weak var weakModel: SettingsModel?
        weak var weakKeys: KeysModel?
        var controller: SettingsWindowController?
        try autoreleasepool {
            let model = try fakeKeys().0
            model.keys.monitoring = monitor
            let opened = SettingsWindowController(model: model, actions: SettingsActions {}, autosaveName: nil)
            opened.showWindow(nil)
            opened.select(pane: "keys")
            opened.window?.contentView?.layoutSubtreeIfNeeded()
            controller = opened
            weakController = opened
            weakModel = model
            weakKeys = model.keys
        }
        let window = try XCTUnwrap(controller?.window)
        try await Task.sleep(for: .milliseconds(100))
        func keys() throws -> KeysModel { try XCTUnwrap(weakKeys) }

        // Another pane.
        try keys().startRecording("find")
        XCTAssertEqual(monitor.live, 1)
        autoreleasepool {
            controller?.select(pane: "general")
            window.contentView?.layoutSubtreeIfNeeded()
        }
        try await waitUntil("the monitor goes with the pane") { monitor.live == 0 }
        XCTAssertNil(try keys().recording)

        // The window resigns key.
        autoreleasepool {
            controller?.select(pane: "keys")
            window.contentView?.layoutSubtreeIfNeeded()
        }
        try await Task.sleep(for: .milliseconds(100))
        try keys().startRecording("find")
        XCTAssertEqual(monitor.live, 1)
        NotificationCenter.default.post(name: NSWindow.didResignKeyNotification, object: window)
        XCTAssertEqual(monitor.live, 0)
        XCTAssertNil(try keys().recording)

        // The window closes.
        try keys().startRecording("find")
        XCTAssertEqual(monitor.live, 1)
        autoreleasepool { controller?.close() }
        XCTAssertEqual(monitor.live, 0)
        controller = nil
        sources = []
        try await waitUntil("freed") { weakController == nil && weakModel == nil && weakKeys == nil }
    }

    func testTheWindowClosingOrResigningKeyEndsTheRecordingWithoutThePane() throws {
        let (model, _, _) = try fakeKeys()
        let monitor = FakeKeyMonitor()
        model.keys.monitoring = monitor
        let window = testWindow()
        model.keys.window = window
        model.keys.startRecording("find")
        XCTAssertEqual(monitor.live, 1)
        window.close()
        XCTAssertEqual(monitor.live, 0, "the window's close ends it")
        XCTAssertNil(model.keys.recording)
        XCTAssertEqual(model.keys.registrationCount, 0)

        // Another window closing or resigning key does not.
        let other = testWindow()
        model.keys.window = other
        model.keys.startRecording("find")
        testWindow().close()
        NotificationCenter.default.post(name: NSWindow.didResignKeyNotification, object: window)
        XCTAssertEqual(model.keys.recording, "find")
        NotificationCenter.default.post(name: NSWindow.didResignKeyNotification, object: other)
        XCTAssertNil(model.keys.recording)
        XCTAssertEqual(monitor.live, 0)
    }

    func testTheModelFreedWhileRecordingTakesItsMonitorWithIt() throws {
        let monitor = FakeKeyMonitor()
        weak var weakKeys: KeysModel?
        try autoreleasepool {
            let (model, _, _) = try fakeKeys()
            model.keys.monitoring = monitor
            model.keys.window = testWindow()
            model.keys.startRecording("find")
            weakKeys = model.keys
            XCTAssertEqual(monitor.live, 1)
        }
        XCTAssertNil(weakKeys)
        XCTAssertEqual(monitor.live, 0)
    }

    // MARK: Chords

    func testAPressedKeyIsNamedAsTheCoreWritesItsChord() {
        struct Case {
            var keyCode: UInt16
            var flags: NSEvent.ModifierFlags
            var character: String?
            var shifted: String? = nil
            var chord: String?
            var line: UInt = #line
        }
        let cases: [Case] = [
            Case(keyCode: 14, flags: [.command, .shift], character: "e", chord: "cmd+shift+e"),
            Case(keyCode: 14, flags: [.command, .capsLock], character: "E", chord: "cmd+e"),
            Case(keyCode: 14, flags: [.shift, .option, .control, .command], character: "e", chord: "cmd+ctrl+alt+shift+e"),
            Case(keyCode: 14, flags: [.command, .function, .numericPad], character: "e", chord: "cmd+e"),
            // ⌥ and function keys.
            Case(keyCode: 96, flags: [.option, .function], character: "\u{F708}", chord: "alt+f5"),
            Case(keyCode: 111, flags: [.command], character: "\u{F70F}", chord: "cmd+f12"),
            Case(keyCode: 90, flags: [.control], character: "\u{F717}", chord: "ctrl+f20"),
            Case(keyCode: 0, flags: [.option], character: "a", chord: "alt+a"),
            // Named keys.
            Case(keyCode: 49, flags: [.control], character: " ", chord: "ctrl+space"),
            Case(keyCode: 36, flags: [.command, .shift], character: "\r", chord: "cmd+shift+enter"),
            Case(keyCode: 53, flags: [.command], character: "\u{1B}", chord: "cmd+escape"),
            Case(keyCode: 117, flags: [.command], character: "\u{F728}", chord: "cmd+delete"),
            Case(keyCode: 123, flags: [.command, .option, .function, .numericPad], character: "\u{F702}", chord: "cmd+alt+left"),
            Case(keyCode: 116, flags: [.command], character: "\u{F72C}", chord: "cmd+page_up"),
            Case(keyCode: 82, flags: [.command, .numericPad], character: "0", chord: "cmd+kp_0"),
            Case(keyCode: 69, flags: [.command, .numericPad], character: "+", chord: "cmd+kp_add"),
            // Symbols the US layout types with Shift are their key plus Shift.
            Case(keyCode: 44, flags: [.command, .shift], character: "/", shifted: "?", chord: "cmd+shift+/"),
            Case(keyCode: 33, flags: [.command, .shift], character: "[", shifted: "{", chord: "cmd+shift+["),
            Case(keyCode: 27, flags: [.command], character: "+", chord: "cmd+shift+="),  // Swedish +
            Case(keyCode: 18, flags: [.command], character: "&", chord: "cmd+shift+7"),  // French &
            // With Shift, such a symbol is what the key types with Shift (#40c
            // adversarial B1): Swedish, French, German twice.
            Case(keyCode: 27, flags: [.command, .shift], character: "+", shifted: "?", chord: "cmd+shift+/"),
            Case(keyCode: 18, flags: [.command, .shift], character: "&", shifted: "1", chord: "cmd+1"),
            Case(keyCode: 30, flags: [.command, .shift], character: "+", shifted: "*", chord: "cmd+shift+8"),
            Case(keyCode: 42, flags: [.command, .shift], character: "#", shifted: "'", chord: "cmd+'"),
            Case(keyCode: 27, flags: [.control, .shift], character: "+", shifted: "?", chord: "ctrl+shift+/"),
            Case(keyCode: 27, flags: [.command, .shift], character: "+", shifted: nil, chord: nil),
            Case(keyCode: 27, flags: [.command, .shift], character: "+", shifted: "Q", chord: nil),
            // A key beyond ASCII is the US key at its place.
            Case(keyCode: 17, flags: [.command], character: "е", chord: "cmd+t"),  // Russian
            Case(keyCode: 33, flags: [.command], character: "å", chord: "cmd+["),  // Swedish
            Case(keyCode: 19, flags: [.command], character: "ě", chord: "cmd+2"),  // Czech
            Case(keyCode: 33, flags: [.command, .shift], character: "å", shifted: "Å", chord: "cmd+shift+["),
            // With Shift, after what it types with Shift (French ⇧⌘é types 2).
            Case(keyCode: 19, flags: [.command, .shift], character: "é", shifted: "2", chord: "cmd+2"),
            // A dead key types nothing yet: the core gets no key to match.
            Case(keyCode: 30, flags: [.command], character: "", chord: nil),
            Case(keyCode: 30, flags: [.command], character: nil, chord: nil),
            // The pane writes no chord on a character beyond ASCII: a key the
            // US layout does not have is refused unless Shift types ASCII.
            Case(keyCode: 10, flags: [.command], character: "§", chord: nil),
            Case(keyCode: 10, flags: [.command, .shift], character: "§", shifted: "°", chord: nil),
            Case(keyCode: 10, flags: [.command, .shift], character: "§", shifted: "1", chord: "cmd+1"),
            // ⇧⌘ on a Hangul key types the same jamo: its US key.
            Case(keyCode: 0, flags: [.command, .shift], character: "ㅁ", shifted: "ㅁ", chord: "cmd+shift+a"),
            // No name, no character.
            Case(keyCode: 71, flags: [.command], character: "\u{F739}", chord: nil),
            Case(keyCode: 200, flags: [.command], character: "\u{200B}", chord: nil),
            Case(keyCode: 200, flags: [.command], character: nil, chord: nil),
        ]
        // As `KeyInfo` reads a key's characters.
        func scalar(_ text: String?) -> Unicode.Scalar? {
            guard let scalars = text?.unicodeScalars, scalars.count == 1, let scalar = scalars.first,
                !TerminalSurfaceView.isKeyCharacter(scalar)
            else { return nil }
            return scalar
        }
        for test in cases {
            XCTAssertEqual(
                KeyChord.chord(
                    keyCode: test.keyCode, flags: test.flags, unshifted: scalar(test.character),
                    shifted: scalar(test.shifted)),
                test.chord, line: test.line)
        }
    }

    func testChordsAndWarningsAreShownSanitised() throws {
        XCTAssertEqual(KeyChord.display("cmd+shift+e"), "⇧⌘E")
        XCTAssertEqual(KeyChord.display("cmd+ctrl+alt+shift+space"), "⌃⌥⇧⌘Space")
        XCTAssertEqual(KeyChord.display("alt+f5"), "⌥F5")
        XCTAssertEqual(KeyChord.display("cmd+kp_1"), "⌘Keypad 1")
        XCTAssertEqual(KeyChord.display("cmd+shift+["), "⇧⌘[")
        XCTAssertEqual(KeyChord.display("ctrl+page_down"), "⌃⇟")

        let hostile = "evil\u{202E}txt.exe\u{0007}\u{200B}\u{2028}line"
        let (model, _, _) = try fakeKeys(
            newTabChords: ["cmd+\u{202E}a\u{200F}"], generation: 0)
        try sources[0].keymapResult = Self.keymap(
            generation: 0, newTabChords: ["cmd+\u{202E}a\u{200F}"],
            warnings: [hostile] + (0..<30).map { "warning \($0)" })
        model.reload()
        let keys = model.keys
        func clean(_ text: String, _ what: String) {
            for scalar in text.unicodeScalars {
                let category = scalar.properties.generalCategory
                XCTAssertFalse(
                    [.control, .format, .lineSeparator, .paragraphSeparator].contains(category),
                    "\(what) shows U+\(String(scalar.value, radix: 16))")
            }
        }
        let row = try XCTUnwrap(keys.groups(matching: "").flatMap(\.rows).first { $0.command == "new_tab" })
        clean(row.display, "a chord")
        clean(row.title, "a title")
        XCTAssertEqual(row.display, "⌘A")
        XCTAssertEqual(keys.warnings.count, KeysModel.maxWarnings + 1)
        XCTAssertEqual(keys.warnings.first, "eviltxt.exe line")
        keys.warnings.forEach { clean($0, "a warning") }

        // A conflict names the chord and the command sanitised.
        keys.record("cmd+\u{202E}a\u{200F}", for: "find")
        clean(try XCTUnwrap(keys.conflict?.text), "a conflict")
    }

    func testPlannedCommandsAreMarkedAndSearchMatchesTitlesCommandsAndChords() throws {
        let (model, _, _) = try fakeKeys()
        let keys = model.keys
        let rows = keys.groups(matching: "").flatMap(\.rows)
        XCTAssertEqual(rows.map(\.command), ["new_tab", "find", "font_bigger", "copy_mode"])
        XCTAssertEqual(rows.map(\.planned), [false, false, false, true])
        XCTAssertEqual(keys.groups(matching: "copy_mode").flatMap(\.rows).map(\.command), ["copy_mode"])
        XCTAssertEqual(keys.groups(matching: "⇧⌘=").flatMap(\.rows).map(\.command), ["font_bigger"])
        XCTAssertEqual(keys.groups(matching: "cmd+f").flatMap(\.rows).map(\.command), ["find"])
        XCTAssertEqual(keys.groups(matching: "new tab").flatMap(\.rows).map(\.command), ["new_tab"])
        XCTAssertEqual(keys.groups(matching: "zzz").count, 0)
    }
}

/// Key events delivered by the test: every installed handler, in order of
/// installation, until one takes the event.
@MainActor
final class FakeKeyMonitor: KeyEventMonitoring {
    private var handlers: [(id: Int, handler: @MainActor (NSEvent) -> Bool)] = []
    private var next = 0

    /// The monitors installed and not removed.
    var live: Int { handlers.count }

    func monitorKeys(_ handler: @escaping @MainActor (NSEvent) -> Bool) -> KeysRegistration {
        let id = next
        next += 1
        handlers.append((id, handler))
        return KeysRegistration { [weak self] in
            MainActor.assumeIsolated { self?.handlers.removeAll { $0.id == id } }
        }
    }

    /// Whether a monitor took `event`.
    func send(_ event: NSEvent) -> Bool {
        for (_, handler) in handlers where handler(event) {
            return true
        }
        return false
    }
}
