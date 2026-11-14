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
        // It is shown as its menu item shows it (#356 M3).
        XCTAssertEqual(keys.conflict?.chord, "cmd+shift+[")
        XCTAssertEqual(keys.conflict?.text, "⌘{ is the shortcut of “Show Previous Tab”.")
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

    /// Restore Defaults over 200 entries is 200 writes, which the watch
    /// applies as they land: the pane stays locked until the key map shows
    /// the last (#356 m3), read on every apply and polled in between.
    func testRestoreDefaultsOverManyEntriesStaysLockedUntilTheLastIsApplied() async throws {
        var lines = ["[keys]"]
        let modifiers = ["cmd+ctrl", "cmd+ctrl+alt", "cmd+ctrl+shift", "cmd+ctrl+alt+shift"]
        let letters = "abcdefghijklmnopqrstuvwxyz".map(String.init)
        let keys = modifiers.flatMap { mods in letters.map { "\(mods)+\($0)" } }
            + modifiers.flatMap { mods in (1...24).map { "\(mods)+f\($0)" } }
        XCTAssertEqual(keys.count, 200)
        for (index, key) in keys.enumerated() {
            lines.append("\"\(key)\" = \"\(index.isMultiple(of: 2) ? "find" : "unbound")\"")
        }
        let controller = try openKeys(config: lines.joined(separator: "\n") + "\n", monitor: FakeKeyMonitor())
        let model = controller.model
        let pane = model.keys
        // Only the key map showing the run may unlock it.
        pane.settleTimeout = .seconds(300)
        XCTAssertEqual(model.keymap?.entries.count, 200)
        XCTAssertTrue(pane.hasCustomShortcuts)

        @MainActor final class Counts {
            var applies = 0
            var midRun = 0
            var unlockedEarly = 0
        }
        let counts = Counts()
        let router = appDelegate.router
        let reload = router.onConfigurationApplied
        defer { router.onConfigurationApplied = reload }
        router.onConfigurationApplied = { [weak model, weak pane] in
            reload?()
            MainActor.assumeIsolated {
                counts.applies += 1
                guard let model, let pane, model.keymap?.entries.isEmpty == false else { return }
                counts.midRun += 1
                if !pane.isChanging { counts.unlockedEarly += 1 }
            }
        }
        pane.restoreDefaults()
        XCTAssertTrue(pane.isChanging)
        var polledEarly = 0
        let deadline = ContinuousClock.now + .seconds(120)
        while model.keymap?.entries.isEmpty != true || pane.isChanging {
            if model.keymap?.entries.isEmpty == false, !pane.isChanging { polledEarly += 1 }
            guard ContinuousClock.now < deadline else { return XCTFail("not restored: \(counts.applies) applies") }
            try await Task.sleep(for: .milliseconds(1))
        }
        XCTAssertEqual(counts.unlockedEarly, 0, "unlocked on an apply before the last")
        XCTAssertEqual(polledEarly, 0, "unlocked between applies")
        XCTAssertFalse(pane.hasCustomShortcuts)
        XCTAssertEqual(Self.chords("find", controller), ["cmd+f"])
        // An apply mid-run needs the writes to outlast the watch's 1 s
        // max_wait, which a fast host may not: then this run proves nothing
        // about one, and the stand-in test of a key map showing part of a
        // run (`testAnEditWaitsForAKeyMapShowingIt…`) covers it.
        if counts.midRun == 0 {
            throw XCTSkip("the 200 writes were applied at once; no apply came before the last")
        }
    }

    // MARK: Entries as written (core, #355)

    /// Entries written in another spelling of their chord are found by the
    /// chord the core reads them as: each shows on its command's row, a
    /// conflict with one says what it does, and Replace or a new shortcut
    /// changes the entry under its own key, never adding a second one.
    func testHandWrittenEntriesAreFoundByTheirCanonicalChord() async throws {
        let config = """
            [keys]
            "shift+cmd+u" = "copy"
            "opt+cmd+j" = "none"
            "d+cmd" = "find"
            "cmd+d" = "no_such_command"
            "super+d" = "select_all"
            "command+d" = "paste"
            "shift+cmd+k" = "send_text:hi"

            """
        let controller = try openKeys(config: config, monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        func row(_ command: String) -> KeysModel.Row? {
            keys.groups(matching: "").flatMap(\.rows).first { $0.command == command }
        }
        XCTAssertEqual(row("copy")?.chords, ["cmd+shift+u", "cmd+c"])
        XCTAssertEqual(row("copy")?.display, "⇧⌘U, ⌘C")
        XCTAssertEqual(row("select_all")?.chords, ["cmd+d", "cmd+a"], "the spelling that took effect")
        XCTAssertEqual(controller.model.keymap?.entries[3].effect, .refused, "an unknown command claims no chord")
        XCTAssertEqual(row("split_right")?.chords, [])
        XCTAssertEqual(row("paste")?.chords, ["cmd+v"], "a shadowed entry binds nothing")
        XCTAssertEqual(row("find")?.chords, ["cmd+f"], "nor does a refused one")

        // `none` and a text, each in another spelling, are conflicts.
        keys.record("cmd+alt+j", for: "find")
        XCTAssertEqual(keys.conflict?.text, "⌥⌘J is turned off in config.toml.")
        keys.cancelConflict()
        keys.record("cmd+shift+k", for: "find")
        XCTAssertEqual(keys.conflict?.text, "⇧⌘K is set to send text in config.toml.")
        keys.cancelConflict()
        keys.record("cmd+d", for: "find")
        XCTAssertEqual(keys.conflict?.text, "⌘D is the shortcut of “Select All”.")
        keys.cancelConflict()
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(try file(), config, "a cancelled conflict writes nothing")

        // Replace on ⌘D changes the spelling that took it, not the first.
        keys.record("cmd+d", for: "find")
        keys.replace()
        try await waitUntil("replaced") { Self.chords("find", controller) == ["cmd+d"] && !keys.isChanging }
        var text = try file()
        XCTAssertTrue(text.contains("\"super+d\" = \"find\""), text)
        XCTAssertTrue(text.contains("\"cmd+d\" = \"no_such_command\""), text)
        XCTAssertTrue(text.contains("\"cmd+f\" = \"unbound\""), text)
        XCTAssertEqual(Self.chords("select_all", controller), ["cmd+a"])

        // Replace on `shift+cmd+u` changes that entry: one for the chord.
        keys.record("cmd+shift+u", for: "paste")
        XCTAssertEqual(keys.conflict?.text, "⇧⌘U is the shortcut of “Copy”.")
        keys.replace()
        try await waitUntil("replaced") {
            Self.chords("paste", controller) == ["cmd+shift+u"] && !keys.isChanging
        }
        text = try file()
        XCTAssertTrue(text.contains("\"shift+cmd+u\" = \"paste\""), text)
        XCTAssertFalse(text.contains("\"cmd+shift+u\""), "a second entry for the chord: \(text)")
        XCTAssertFalse(text.contains("\"command+d\""), "Paste's shadowed entry goes: \(text)")
        XCTAssertTrue(text.contains("\"cmd+v\" = \"unbound\""), text)
        XCTAssertEqual(Self.chords("copy", controller), ["cmd+c"])

        // Recording again takes Paste off `shift+cmd+u`, and clearing Find
        // takes it off `super+d`, which Split Right gets back.
        keys.record("cmd+shift+o", for: "paste")
        try await waitUntil("recorded") { Self.chords("paste", controller) == ["cmd+shift+o"] && !keys.isChanging }
        keys.clear("find")
        try await waitUntil("cleared") { Self.chords("find", controller) == [] && !keys.isChanging }
        XCTAssertEqual(Self.chords("split_right", controller), ["cmd+d"])
        text = try file()
        XCTAssertFalse(text.contains("shift+cmd+u"), text)
        XCTAssertFalse(text.contains("super+d"), text)
        for kept in [
            "\"opt+cmd+j\" = \"none\"", "\"shift+cmd+k\" = \"send_text:hi\"", "\"d+cmd\" = \"find\"",
            "\"cmd+d\" = \"no_such_command\"",
        ] {
            XCTAssertTrue(text.contains(kept), "\(kept) kept: \(text)")
        }
    }

    /// A hand-written `unbound` entry in another spelling holds its chord:
    /// recording the chord changes that entry, never adds one the core
    /// would refuse as a second for the chord.
    func testAnUnboundEntryInAnotherSpellingIsTheOneRecordingChanges() async throws {
        let config = "[keys]\n\"command+t\" = \"unbound\"\n"
        let controller = try openKeys(config: config, monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        XCTAssertEqual(Self.chords("new_tab", controller), [])
        keys.record("cmd+t", for: "find")
        XCTAssertNil(keys.conflict, "an unbound chord is free")
        try await waitUntil("recorded") { Self.chords("find", controller) == ["cmd+t"] && !keys.isChanging }
        let text = try file()
        XCTAssertTrue(text.contains("\"command+t\" = \"find\""), text)
        XCTAssertFalse(text.contains("\"cmd+t\""), "a second entry for the chord: \(text)")
        XCTAssertNil(keys.message("find"))
    }

    /// Clearing a command removes every entry of its own, the shadowed
    /// spelling of a chord too, which would bind it again once the first
    /// goes.
    func testClearRemovesTheShadowedSpellingToo() async throws {
        let config = "[keys]\n\"super+u\" = \"new_tab\"\n\"cmd+u\" = \"new_tab\"\n"
        let controller = try openKeys(config: config, monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        XCTAssertEqual(controller.model.keymap?.entries.map(\.effect), [.bound, .shadowed])
        XCTAssertEqual(Self.chords("new_tab", controller), ["cmd+u", "cmd+t"])
        keys.clear("new_tab")
        try await waitUntil("cleared") { Self.chords("new_tab", controller) == [] && !keys.isChanging }
        // Past one more apply, should one come.
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertEqual(Self.chords("new_tab", controller), [])
        let text = try file()
        XCTAssertFalse(text.contains("+u\""), text)
        XCTAssertTrue(text.contains("\"cmd+t\" = \"unbound\""), text)
    }

    /// A conflict shown before a hand edit says what the chord does now
    /// when Replace is pressed, and writes nothing until it is pressed
    /// again (#356 M2).
    func testReplaceAfterAHandEditSaysWhatTheChordDoesNowAndWritesOnTheSecond() async throws {
        let controller = try openKeys(config: "", monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        keys.record("cmd+e", for: "find")
        let first = try XCTUnwrap(keys.conflict?.text)
        XCTAssertTrue(first.hasPrefix("⌘E is the shortcut of “"), first)

        let edited = "[keys]\n\"super+e\" = \"send_text:hi\"\n"
        try edited.write(to: try XCTUnwrap(dir).appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the hand edit applied") {
            controller.model.keymap?.entries.first?.value == .sendText("hi")
                && controller.model.keymap?.generation == controller.model.snapshot.generation
        }
        keys.replace()
        XCTAssertEqual(keys.conflict?.text, "⌘E is set to send text in config.toml.")
        XCTAssertFalse(keys.isChanging, "nothing queued")
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(try file(), edited)

        keys.replace()
        XCTAssertNil(keys.conflict)
        try await waitUntil("replaced") { Self.chords("find", controller) == ["cmd+e"] && !keys.isChanging }
        let text = try file()
        XCTAssertTrue(text.contains("\"super+e\" = \"find\""), text)
    }

    /// The key map's problems (entries left out, and the overflow) and the
    /// caveats bound entries are honoured with, as the core reports them.
    func testProblemsAndCaveatsAreListedApart() async throws {
        var lines = ["[keys]", "\"cmd+h\" = \"find\"", "\"d+cmd\" = \"copy\""]
        let modifiers = ["", "cmd+", "ctrl+", "alt+", "shift+", "cmd+ctrl+", "cmd+alt+", "cmd+shift+", "ctrl+alt+",
            "ctrl+shift+", "alt+shift+", "cmd+ctrl+alt+", "cmd+ctrl+shift+", "cmd+alt+shift+", "ctrl+alt+shift+"]
        let fkeys = modifiers.flatMap { mods in (1...35).map { "\(mods)f\($0)" } }.prefix(515)
        XCTAssertEqual(fkeys.count, 515)
        lines += fkeys.map { "\"\($0)\" = \"unbound\"" }
        let controller = try openKeys(config: lines.joined(separator: "\n") + "\n", monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        XCTAssertEqual(controller.model.keymap?.entries.count, 512)
        XCTAssertEqual(keys.caveats.count, 1, "\(keys.caveats)")
        XCTAssertTrue(keys.caveats.first?.contains("hide") ?? false, "\(keys.caveats)")
        XCTAssertEqual(keys.problems.count, 2, "\(keys.problems)")
        XCTAssertTrue(keys.problems.first?.contains("d+cmd") ?? false, "\(keys.problems)")
        XCTAssertTrue(keys.problems.last?.contains("512") ?? false, "the overflow last: \(keys.problems)")
    }

    // MARK: Menus (core, #356 M3)

    /// Every shortcut shows as its menu item shows it: a US shifted symbol
    /// as the symbol without Shift (⌘? for `cmd+shift+/`), from the core's
    /// key equivalents.
    func testShortcutsShowAsTheirMenuItemsShowThem() throws {
        let controller = try openKeys(config: "", monitor: FakeKeyMonitor())
        let keys = controller.model.keys
        let rows = keys.groups(matching: "").flatMap(\.rows)
        var compared = 0
        for row in rows {
            guard let item = MainMenu.item(forCommand: row.command, in: NSApp.mainMenu) else { continue }
            let (key, modifiers) = authoredKeyEquivalent(of: item)
            guard key.unicodeScalars.count == 1, let scalar = key.unicodeScalars.first,
                (0x21..<0x7F).contains(scalar.value)
            else { continue }
            let label = zip(row.chords, row.labels).first { KeyChord.goesOnMenu($0.0) }?.1
            XCTAssertEqual(label, KeyChord.display(flags: modifiers) + key.uppercased(), row.command)
            compared += 1
        }
        XCTAssertGreaterThan(compared, 30)
        for (command, label) in [("help", "⌘?"), ("font_bigger", "⌘+"), ("tab_previous", "⌘{"), ("tab_next", "⌘}")] {
            XCTAssertEqual(rows.first { $0.command == command }?.labels.first, label, command)
        }
        XCTAssertEqual(rows.first { $0.command == "font_bigger" }?.display, "⌘+, ⌘=")
        XCTAssertEqual(keys.groups(matching: "⌘?").flatMap(\.rows).map(\.command), ["help"])
        keys.record("cmd+shift+/", for: "find")
        XCTAssertTrue(keys.conflict?.text.hasPrefix("⌘? is the shortcut of “") ?? false, "\(String(describing: keys.conflict))")
        XCTAssertEqual(keys.label(of: "cmd+shift+["), "⌘{")
    }

    // MARK: Focus (core, #356 m5)

    /// The first editable text field in `view`'s tree.
    private static func textField(in view: NSView) -> NSTextField? {
        if let field = view as? NSTextField, field.isEditable { return field }
        return view.subviews.lazy.compactMap { textField(in: $0) }.first
    }

    func testTheSearchFieldTakingTheFocusEndsARecording() async throws {
        let monitor = FakeKeyMonitor()
        let controller = try openKeys(config: "", monitor: monitor)
        let keys = controller.model.keys
        let window = try XCTUnwrap(controller.window)
        var field: NSTextField?
        try await waitUntil("the search field") {
            window.contentView?.layoutSubtreeIfNeeded()
            field = window.contentView.flatMap(Self.textField)
            return field != nil
        }
        let search = try XCTUnwrap(field)

        keys.startRecording("find")
        XCTAssertEqual(keys.recording, "find")
        XCTAssertTrue(window.makeFirstResponder(search))
        XCTAssertTrue(window.firstResponder is NSText, "the field editor has the focus")
        XCTAssertNil(keys.recording, "the field took the focus")
        XCTAssertEqual(monitor.live, 0)
        XCTAssertEqual(keys.registrationCount, 0)
        XCTAssertFalse(monitor.send(try Self.key(14, "e", [.command, .shift], in: window)), "keys go to the field")

        // Started while the field has the focus, a recording takes it, so
        // clicking into the field again ends it.
        keys.startRecording("find")
        XCTAssertEqual(keys.recording, "find")
        XCTAssertFalse(window.firstResponder is NSText)
        XCTAssertTrue(window.makeFirstResponder(search))
        XCTAssertNil(keys.recording)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(try file(), "")
    }

    // MARK: VoiceOver (#356 m6)

    func testConflictsAndRefusalsAreAnnouncedOnceEach() async throws {
        let monitor = FakeKeyMonitor()
        let controller = try openKeys(config: "", monitor: monitor)
        let keys = controller.model.keys
        let poster = PostRecorder(clock: { 0 })
        keys.poster = poster
        let window = try XCTUnwrap(controller.window)
        func announcements() -> [String] {
            poster.posts.filter { $0.notification == .announcementRequested }
                .compactMap { $0.userInfo?[.announcement] as? String }
        }

        // ⌘Q is Quit's: the conflict is said.
        keys.startRecording("find")
        XCTAssertTrue(monitor.send(try Self.key(12, "q", [.command], in: window)))
        let conflict = try XCTUnwrap(keys.conflict?.text)
        XCTAssertEqual(announcements(), [conflict])
        let post = try XCTUnwrap(poster.posts.first)
        XCTAssertTrue(post.element === NSApp)
        XCTAssertEqual(post.userInfo?[.priority] as? Int, NSAccessibilityPriorityLevel.high.rawValue)

        // Replace: the core refuses to shadow Quit, and the refusal is said.
        keys.replace()
        try await waitUntil("refused") { keys.message("find") != nil && !keys.isChanging }
        XCTAssertEqual(announcements(), [conflict, try XCTUnwrap(keys.message("find"))])

        // A flood of keys no shortcut can hold: said once.
        keys.startRecording("find")
        for _ in 0..<1_000 {
            XCTAssertTrue(monitor.send(try Self.key(200, "", [.command], in: window)))
        }
        XCTAssertEqual(keys.message("find"), "This key cannot be part of a shortcut.")
        XCTAssertEqual(announcements().count, 3, "\(announcements())")
        XCTAssertEqual(announcements().last, "This key cannot be part of a shortcut.")
        keys.stopRecording()
    }

    // MARK: Edits (stand-ins)

    /// A key map of four commands, and `[keys]` as `entries` (in order,
    /// each key its own chord, bound or `unbound`).
    private func fakeKeys(
        entries: KeyValuePairs<String, String> = [:], warnings: [String] = [], newTabChords: [String] = ["cmd+t"],
        generation: UInt64 = 0, writer: FakeWriter = FakeWriter()
    ) throws -> (SettingsModel, FakeSource, FakeWriter) {
        let source = FakeSource(writer: writer)
        source.snapshot = Self.snapshot(generation: generation, entries)
        source.keymapResult = try Self.keymap(
            generation: generation, newTabChords: newTabChords, warnings: warnings, entries: Self.entries(entries))
        sources.append(source)
        return (SettingsModel(source: source), source, writer)
    }

    /// The configuration with `[keys]` as `entries`.
    private static func snapshot(generation: UInt64, _ entries: KeyValuePairs<String, String>) -> TesseraConfigSnapshot {
        var table: [String: TesseraConfigValue] = [:]
        for (key, value) in entries { table[key] = .string(value) }
        return TesseraConfigSnapshot(generation: generation, config: .table(["keys": .table(table)]))
    }

    /// The key map's entries for `entries`: each key its own chord, bound
    /// or `unbound`.
    private static func entries(_ entries: KeyValuePairs<String, String>) -> [[String: Any]] {
        entries.enumerated().map { index, entry in
            Self.entry(index, entry.key, chord: entry.key, value: entry.value, effect: entry.value == "unbound" ? "unbound" : "bound")
        }
    }

    /// One `[keys]` entry as the core's key map reports it.
    private static func entry(
        _ index: Int, _ key: String, chord: String?, value: Any, effect: String, warning: String? = nil
    ) -> [String: Any] {
        [
            "index": index, "key": key, "chord": chord ?? NSNull(), "value": value, "effect": effect,
            "warning": warning ?? NSNull(),
        ]
    }

    private static func keymap(
        generation: UInt64, newTabChords: [String] = ["cmd+t"], fontChords: [String] = ["cmd+shift+=", "cmd+="],
        warnings: [String] = [], entries: [[String: Any]] = []
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
            "entries": entries,
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
        // Two entries, then the defaults no entry took; the second written
        // as no canonical chord is.
        try sources[0].keymapResult = Self.keymap(
            generation: 0, fontChords: ["cmd+u", "cmd+shift+o", "cmd+shift+=", "cmd+="],
            entries: [
                Self.entry(0, "cmd+u", chord: "cmd+u", value: "font_bigger", effect: "bound"),
                Self.entry(1, "shift+cmd+o", chord: "cmd+shift+o", value: "font_bigger", effect: "bound"),
            ])
        model.reload()
        let keys = model.keys
        keys.record("cmd+shift+e", for: "font_bigger")
        try await waitUntil("written") { writer.calls.count == 5 }
        // The entries go under their own keys, the last first.
        XCTAssertEqual(
            writer.calls,
            [
                .init(key: "keys.\"cmd+shift+e\"", value: .string("font_bigger")),
                .init(key: "keys.\"shift+cmd+o\"", value: nil),
                .init(key: "keys.\"cmd+u\"", value: nil),
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

    func testAnEditWaitsForAKeyMapShowingItAndNothingIsEditedFromATornRead() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        keys.settleTimeout = .seconds(60)
        let monitor = FakeKeyMonitor()
        keys.monitoring = monitor
        keys.window = testWindow()
        keys.record("cmd+shift+e", for: "find")
        try await waitUntil("written") { writer.calls.count == 2 }
        XCTAssertTrue(keys.isChanging, "until a key map shows it")
        XCTAssertEqual(keys.groups(matching: "").flatMap(\.rows).first { $0.command == "find" }?.chords, ["cmd+shift+e"])
        keys.startRecording("new_tab")
        XCTAssertNil(keys.recording, "nothing is recorded meanwhile")
        XCTAssertEqual(monitor.live, 0)
        keys.restoreDefaults()
        keys.clear("new_tab")
        model.reload()
        XCTAssertTrue(keys.isChanging, "the same generation again")
        // A later key map that shows only part of the run (the set, not the
        // unbinding) is an apply in between (#356 m3).
        let set = Self.entry(0, "cmd+shift+e", chord: "cmd+shift+e", value: "find", effect: "bound")
        source.snapshot = Self.snapshot(generation: 1, ["cmd+shift+e": "find"])
        source.keymapResult = try Self.keymap(generation: 1, entries: [set])
        model.reload()
        XCTAssertTrue(keys.isChanging, "an apply in between")
        XCTAssertEqual(writer.calls.count, 2)

        // The configuration was read at a generation the key map is not
        // (#348), every time: nothing is edited.
        source.snapshot = Self.snapshot(generation: 3, ["cmd+shift+e": "find", "cmd+f": "unbound"])
        source.keymapResult = try Self.keymap(
            generation: 2,
            entries: [set, Self.entry(1, "cmd+f", chord: "cmd+f", value: "unbound", effect: "unbound")])
        model.reload()
        XCTAssertFalse(keys.isChanging, "the key map shows every key written")
        keys.record("cmd+shift+y", for: "new_tab")
        XCTAssertEqual(keys.message("new_tab"), "config.toml is being applied; try again.")
        XCTAssertEqual(writer.calls.count, 2)
    }

    /// A write that met another edit of the file and was run again: that
    /// edit's apply may have come first, and the pane reads the file again
    /// instead of waiting for an apply that is over (#356 m4).
    func testARetriedEditWhoseApplyCameFirstEndsUnlocked() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        keys.settleTimeout = .seconds(60)
        writer.respond = { _, attempt in
            attempt == 1 ? TesseraConfigEditError(.conflict, message: "config.toml changed") : nil
        }
        // What the run leaves, already applied when the retry lands.
        let shown: KeyValuePairs<String, String> = ["cmd+shift+e": "find", "cmd+f": "unbound"]
        source.snapshot = Self.snapshot(generation: 1, shown)
        source.keymapResult = try Self.keymap(generation: 1, entries: Self.entries(shown))
        keys.record("cmd+shift+e", for: "find")
        XCTAssertTrue(keys.isChanging)
        try await waitUntil("unlocked") { !keys.isChanging }
        XCTAssertEqual(writer.calls.count, 4, "each edit run twice")
        XCTAssertEqual(model.snapshot.generation, 1)
        XCTAssertNil(keys.message("find"))
    }

    /// A write the key map never shows (the watch saw no change, or
    /// another edit undid it) ends after ``KeysModel/settleTimeout`` with
    /// the file read again (#356 m4).
    func testAnEditTheKeyMapNeverShowsEndsAfterTheTimeoutWithTheFileReadAgain() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        keys.settleTimeout = .seconds(1)
        let start = ContinuousClock.now
        keys.record("cmd+shift+e", for: "find")
        try await waitUntil("written") { writer.calls.count == 2 }
        XCTAssertTrue(keys.isChanging)
        // A hand edit, applied, that is not the run.
        source.snapshot = Self.snapshot(generation: 1, ["cmd+shift+y": "find"])
        source.keymapResult = try Self.keymap(generation: 1, entries: Self.entries(["cmd+shift+y": "find"]))
        try await waitUntil("released") { !keys.isChanging }
        let elapsed = ContinuousClock.now - start
        XCTAssertGreaterThanOrEqual(elapsed, .seconds(1), "released before the timeout")
        XCTAssertLessThan(elapsed, .seconds(6))
        XCTAssertEqual(model.snapshot.generation, 1, "the file read again")
        // And the pane edits on.
        keys.record("cmd+shift+u", for: "new_tab")
        try await waitUntil("written again") { writer.calls.count == 4 }
    }

    /// The timeout of a run the key map showed does not end a later run.
    func testAnEarlierRunsTimeoutLeavesTheNextLocked() async throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(writer: writer)
        let keys = model.keys
        keys.settleTimeout = .seconds(1)
        keys.record("cmd+shift+e", for: "find")
        try await waitUntil("written") { writer.calls.count == 2 }
        let first = ContinuousClock.now
        let shown: KeyValuePairs<String, String> = ["cmd+shift+e": "find", "cmd+f": "unbound"]
        source.snapshot = Self.snapshot(generation: 1, shown)
        source.keymapResult = try Self.keymap(generation: 1, entries: Self.entries(shown))
        model.reload()
        try await waitUntil("shown") { !keys.isChanging }
        // The next run waits for its key map alone.
        keys.settleTimeout = .seconds(60)
        keys.record("cmd+shift+u", for: "new_tab")
        try await waitUntil("written again") { writer.calls.count == 4 }
        // Past the first run's timeout, by a margin.
        try await Task.sleep(until: first + .seconds(2.5))
        XCTAssertTrue(keys.isChanging, "the first run's timeout ended the second")
    }

    /// An entry whose key, as the key map reports it (escaped, capped), is
    /// not one of `[keys]` as written is never edited: nothing is written
    /// under a key the file does not have.
    func testAnEntryNotNamedAsWrittenIsNotEdited() throws {
        let writer = FakeWriter()
        let (model, source, _) = try fakeKeys(
            entries: ["cmd+u": "new_tab"], newTabChords: ["cmd+u", "cmd+t"], writer: writer)
        source.snapshot = Self.snapshot(generation: 0, ["cmd+u\u{202E}": "new_tab"])
        model.reload()
        let keys = model.keys
        keys.record("cmd+shift+e", for: "new_tab")
        XCTAssertEqual(
            keys.message("new_tab"), "An entry this changes is written in config.toml in a way Settings cannot edit.")
        XCTAssertFalse(keys.isChanging)
        XCTAssertFalse(keys.hasCustomShortcuts, "nor does Restore Defaults remove it")
        XCTAssertEqual(writer.calls, [])
    }

    /// Past ``KeysModel/maxWarnings`` a list says how many more; the
    /// problems keep their last (the overflow line), and a notation the
    /// loader refused, which only its entry carries, is a problem.
    func testTheWarningListsAreBounded() throws {
        let letters = "abcdefghijklmnopqrstuvwxy".map(String.init)
        XCTAssertEqual(letters.count, 25)
        var entries = letters.enumerated().map { index, letter in
            Self.entry(
                index, "cmd+ctrl+\(letter)", chord: "cmd+ctrl+\(letter)", value: "find", effect: "bound",
                warning: "caveat \(index); honoured")
        }
        entries.append(Self.entry(25, "d+cmd", chord: nil, value: "find", effect: "refused", warning: "refused d+cmd"))
        let problems = (0..<40).map { "problem \($0)" }
        let (model, _, _) = try fakeKeys()
        try sources[0].keymapResult = Self.keymap(
            generation: 0, warnings: zip(letters.indices, problems).flatMap { ["caveat \($0); honoured", $1] }
                + problems.dropFirst(25),
            entries: entries)
        model.reload()
        let keys = model.keys
        XCTAssertEqual(keys.problems.count, KeysModel.maxWarnings + 1)
        XCTAssertEqual(keys.problems.first, "refused d+cmd")
        XCTAssertEqual(keys.problems[1], "problem 0")
        XCTAssertEqual(keys.problems[KeysModel.maxWarnings - 1], "And 21 more; see config.toml.")
        XCTAssertEqual(keys.problems.last, "problem 39")
        XCTAssertEqual(keys.caveats.count, KeysModel.maxWarnings + 1)
        XCTAssertEqual(keys.caveats.first, "caveat 0; honoured")
        XCTAssertEqual(keys.caveats.last, "And 5 more; see config.toml.")
        XCTAssertFalse(keys.problems.contains { $0.contains("caveat") }, "\(keys.problems)")
    }

    func testEscapeCancelsDeleteClearsAndOtherWindowsKeysPass() async throws {
        let writer = FakeWriter()
        let (model, _, _) = try fakeKeys(entries: ["cmd+u": "font_bigger"], writer: writer)
        try sources[0].keymapResult = Self.keymap(
            generation: 0, fontChords: ["cmd+u", "cmd+shift+=", "cmd+="],
            entries: Self.entries(["cmd+u": "font_bigger"]))
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
        XCTAssertEqual(keys.registrationCount, 3, "one monitor, one observer set and the focus observation")
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
        XCTAssertEqual(keys.problems.count, KeysModel.maxWarnings + 1)
        XCTAssertEqual(keys.problems.first, "eviltxt.exe line")
        keys.problems.forEach { clean($0, "a warning") }

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
