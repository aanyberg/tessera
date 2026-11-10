import AppKit
import Observation
import SwiftUI
import TesseraKit
import os

// MARK: - Chords

/// Chords in the notation of `[keys]` (ADR 0036), as the Keys pane records
/// and shows them. The core parses and checks every chord the pane writes;
/// this only names a pressed key by a chord the core's key map resolves for
/// it, the way the core's canonical form writes it (`cmd+ctrl+alt+shift`,
/// then the key), so a recorded chord is the one the key map lists, and
/// turns a canonical chord into the glyphs macOS
/// shows (`⇧⌘E`).
@MainActor
enum KeyChord {
    /// The modifiers a chord holds, in canonical order.
    private static let modifiers: [(flag: NSEvent.ModifierFlags, name: String)] = [
        (.command, "cmd"), (.control, "ctrl"), (.option, "alt"), (.shift, "shift"),
    ]

    /// The US-layout symbols typed with Shift, by the key under them
    /// (`tessera_config::keys`'s `US_SHIFTED`): the core writes `cmd+?` as
    /// `cmd+shift+/`, and refuses `cmd++`.
    private static let usShifted: [Unicode.Scalar: Unicode.Scalar] = [
        "?": "/", "{": "[", "}": "]", "+": "=", "~": "`", "!": "1", "@": "2", "#": "3", "$": "4", "%": "5",
        "^": "6", "&": "7", "*": "8", "(": "9", ")": "0", "_": "-", ":": ";", "\"": "'", "<": ",", ">": ".",
        "|": "\\",
    ]

    /// The flags of `flags` a chord can hold: Caps Lock, Fn and the keypad
    /// flag are not chord modifiers.
    static func chordFlags(_ flags: NSEvent.ModifierFlags) -> NSEvent.ModifierFlags {
        flags.intersection([.command, .control, .option, .shift])
    }

    /// The chord of `event`, a key-down; `nil` for a key no chord the
    /// core would match to it can name.
    static func chord(of event: NSEvent) -> String? {
        // What the terminal view hands the core for this key.
        let keys = KeyInfo(event)
        return chord(
            keyCode: event.keyCode, flags: event.modifierFlags, unshifted: keys.unshifted, shifted: keys.shifted)
    }

    /// The chord of the key at `keyCode` pressed with `flags`, as the
    /// terminal view sends it to the core: `unshifted` and `shifted` are the
    /// key's characters without modifiers and with Shift in the active
    /// layout (`KeyInfo`).
    ///
    /// A key the terminal view names (Escape, F5, keypad 1) is that name. A
    /// character key is named by the first of the core's lookups
    /// (`Keymap::resolve`, in its order) a chord on an ASCII key can fill,
    /// so the chord recorded is the one pressing the key runs:
    ///
    /// 1. its own character with the modifiers held (`cmd+shift+e`); a
    ///    symbol the US layout types with Shift is its key plus Shift
    ///    (Swedish ⌘+ is `cmd+shift+=`), so with Shift held no chord fills
    ///    this lookup for such a symbol;
    /// 2. with Shift held, the character typed with Shift, without Shift
    ///    (Swedish ⇧⌘+ types `?`: `cmd+shift+/`; French ⇧⌘& types `1`:
    ///    `cmd+1`);
    /// 3. for a character beyond ASCII, the US-layout key at its place
    ///    (Russian ⌘Т is `cmd+t`, Swedish ⇧⌘Å `cmd+shift+[`).
    ///
    /// The pane writes no chord on a character beyond ASCII, so a lookup of
    /// one finds none of the pane's chords, and the lookups after it are
    /// the ones a recorded chord must fill.
    ///
    /// `nil` when no lookup can be filled: a key with no name and no
    /// character (a dead key types nothing yet, and the core gets no key),
    /// a key beyond ASCII where the US layout has none (`§`) whose shifted
    /// character is not ASCII either, or a US shifted symbol pressed with
    /// Shift whose shifted character no chord holds.
    static func chord(
        keyCode: UInt16, flags: NSEvent.ModifierFlags, unshifted: Unicode.Scalar?, shifted: Unicode.Scalar?
    ) -> String? {
        let flags = chordFlags(flags)
        if keyCode == 49 {  // kVK_Space
            return name(flags, "space")
        }
        if let named = TerminalSurfaceView.namedKey(keyCode) {
            return self.name(of: named).flatMap { name(flags, $0) }
        }
        guard let unshifted else { return nil }
        // The core lowercases the key itself, not the shifted character.
        let own = lowercased(unshifted)
        var lookups: [(NSEvent.ModifierFlags, Unicode.Scalar?)] = [(flags, own)]
        if flags.contains(.shift) {
            lookups.append((flags.subtracting(.shift), shifted))
        }
        if !unshifted.isASCII, let us = TerminalSurfaceView.usLayoutCharacter(keyCode), us != own {
            lookups.append((flags, us))
        }
        return lookups.lazy.compactMap { flags, key in key.flatMap { chord(flags, filling: $0) } }.first
    }

    /// The chord the core looks `key` up under with `flags` as: `key`
    /// itself, or for a US shifted symbol without Shift its key plus Shift
    /// (the core enters `cmd+shift+/` under `cmd+?` too). `nil` when the
    /// pane writes no chord there: a US shifted symbol with Shift, or a key
    /// that is not a lowercase visible ASCII character or the space bar.
    private static func chord(_ flags: NSEvent.ModifierFlags, filling key: Unicode.Scalar) -> String? {
        if let base = usShifted[key] {
            return flags.contains(.shift) ? nil : name(flags.union(.shift), String(base))
        }
        guard (0x20..<0x7F).contains(key.value), !("A"..."Z").contains(key) else { return nil }
        return name(flags, key == " " ? "space" : String(key))
    }

    /// `scalar` lowercased when its lowercase is one scalar, as the core's
    /// `char_key`.
    private static func lowercased(_ scalar: Unicode.Scalar) -> Unicode.Scalar {
        let lower = String(scalar).lowercased().unicodeScalars
        return lower.count == 1 ? lower.first! : scalar
    }

    /// `key` with the modifiers of `flags`, in canonical order.
    private static func name(_ flags: NSEvent.ModifierFlags, _ key: String) -> String {
        (modifiers.filter { flags.contains($0.flag) }.map(\.name) + [key]).joined(separator: "+")
    }

    /// The chord name of a key the terminal view names; `nil` for the keys
    /// `[keys]` has no name for (Caps Lock, Print Screen, keypad Begin).
    private static func name(of key: TesseraSession.Key) -> String? {
        switch key {
        case .escape: return "escape"
        case .enter: return "enter"
        case .tab: return "tab"
        case .backspace: return "backspace"
        case .delete: return "delete"
        case .insert: return "insert"
        case .left: return "left"
        case .right: return "right"
        case .up: return "up"
        case .down: return "down"
        case .home: return "home"
        case .end: return "end"
        case .pageUp: return "page_up"
        case .pageDown: return "page_down"
        case .kpDecimal: return "kp_decimal"
        case .kpDivide: return "kp_divide"
        case .kpMultiply: return "kp_multiply"
        case .kpSubtract: return "kp_subtract"
        case .kpAdd: return "kp_add"
        case .kpEnter: return "kp_enter"
        case .kpEqual: return "kp_equal"
        default:
            let raw = key.rawValue
            if (TesseraSession.Key.f1.rawValue...TesseraSession.Key.f35.rawValue).contains(raw) {
                return "f\(raw - TesseraSession.Key.f1.rawValue + 1)"
            }
            if (TesseraSession.Key.kp0.rawValue...TesseraSession.Key.kp9.rawValue).contains(raw) {
                return "kp_\(raw - TesseraSession.Key.kp0.rawValue)"
            }
            return nil
        }
    }

    /// The glyphs of a chord's modifiers, in the order macOS shows them.
    private static let glyphs: [(name: String, glyph: String)] = [
        ("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘"),
    ]

    /// The modifiers of `flags` as macOS shows them (`⌃⌥⇧⌘`).
    static func display(flags: NSEvent.ModifierFlags) -> String {
        let names = Set(modifiers.filter { flags.contains($0.flag) }.map(\.name))
        return glyphs.filter { names.contains($0.name) }.map(\.glyph).joined()
    }

    /// `chord` (canonical, as the key map lists it) as macOS writes a
    /// shortcut: modifier glyphs, then the key (`⇧⌘E`, `⌥F5`, `⌘Space`,
    /// `⇧⌘[`), sanitised (S3) since it comes from `config.toml`.
    static func display(_ chord: String) -> String {
        var parts = chord.split(separator: "+", omittingEmptySubsequences: false).map(String.init)
        var key = parts.popLast() ?? ""
        if key.isEmpty, chord.hasSuffix("+") {
            // `cmd++` is not canonical, but show it as written.
            key = "+"
            if parts.last == "" { parts.removeLast() }
        }
        let names = Set(parts)
        var text = glyphs.filter { names.contains($0.name) }.map(\.glyph).joined()
        // A modifier no glyph stands for is shown by name.
        for part in parts where !glyphs.contains(where: { $0.name == part }) {
            text += part + "+"
        }
        text += keyDisplay(key)
        return SettingsModel.sanitisedMessage(text)
    }

    private static func keyDisplay(_ key: String) -> String {
        switch key {
        case "escape": return "⎋"
        case "enter": return "↩"
        case "tab": return "⇥"
        case "backspace": return "⌫"
        case "delete": return "⌦"
        case "insert": return "Insert"
        case "left": return "←"
        case "right": return "→"
        case "up": return "↑"
        case "down": return "↓"
        case "home": return "↖"
        case "end": return "↘"
        case "page_up": return "⇞"
        case "page_down": return "⇟"
        case "space": return "Space"
        case "kp_decimal": return "Keypad ."
        case "kp_divide": return "Keypad /"
        case "kp_multiply": return "Keypad *"
        case "kp_subtract": return "Keypad −"
        case "kp_add": return "Keypad +"
        case "kp_enter": return "Keypad ⌤"
        case "kp_equal": return "Keypad ="
        default:
            if key.hasPrefix("kp_") { return "Keypad " + key.dropFirst(3) }
            // A letter, a symbol, a function key (`F5`).
            return key.uppercased()
        }
    }
}

// MARK: - Event monitor

/// Something installed (an event monitor, a notification observer) that is
/// taken out again by ``remove()`` or, at the latest, when this is freed.
final class KeysRegistration {
    private var removal: (() -> Void)?

    init(_ removal: @escaping () -> Void) {
        self.removal = removal
    }

    func remove() {
        let removal = removal
        self.removal = nil
        removal?()
    }

    deinit {
        removal?()
    }
}

/// Watches the app's key events while the Keys pane records a shortcut:
/// `NSEvent`'s local monitor, or a stand-in in tests.
@MainActor
protocol KeyEventMonitoring {
    /// Installs `handler` for key-down and modifier events; it returns
    /// whether it took the event (no one else then sees it). The monitor
    /// goes with the registration.
    func monitorKeys(_ handler: @escaping @MainActor (NSEvent) -> Bool) -> KeysRegistration
}

/// ``KeyEventMonitoring`` on `NSEvent.addLocalMonitorForEvents`.
@MainActor
struct SystemKeyEventMonitoring: KeyEventMonitoring {
    func monitorKeys(_ handler: @escaping @MainActor (NSEvent) -> Bool) -> KeysRegistration {
        let monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .flagsChanged]) { event in
            // Local monitors run on the main thread.
            let taken = MainActor.assumeIsolated { handler(event) }
            return taken ? nil : event
        }
        return KeysRegistration {
            if let monitor { NSEvent.removeMonitor(monitor) }
        }
    }
}

// MARK: - Model

/// The Keys pane's state (ADR 0044, Keys): every command of the core's key
/// map with its chords, the shortcut being recorded, a conflict waiting for
/// the user, and the pane's edits of `[keys]`.
///
/// The key map and `[keys]` as written come from the ``SettingsModel``
/// (both of one generation); an edit is a run of single-key sets and
/// removes of `keys."<chord>"` on ``SettingsModel/writeQueue``, in order,
/// stopping at the first that fails. From the moment one is queued until a
/// later key map applies (or it fails, with its message by the row) the
/// pane shows the chord it set and edits nothing else, as the Profiles
/// pane does while its indices move.
@MainActor
@Observable
final class KeysModel {
    /// One command as a row of the pane.
    struct Row: Identifiable, Equatable {
        /// The command as `[keys]` names it (`select_tab:3`).
        var command: String
        /// Its menu item's title, or its name, sanitised (S3).
        var title: String
        /// The menu its item is in, or "Other".
        var group: String
        /// Its chords, canonical, the one its menu item shows first.
        var chords: [String]
        /// A command a later version runs: bindable, does nothing yet.
        var planned: Bool

        var id: String { command }
        /// Its chords as macOS writes shortcuts, joined.
        @MainActor var display: String { chords.map { KeyChord.display($0) }.joined(separator: ", ") }
    }

    /// Rows under the title of their menu.
    struct Group: Identifiable, Equatable {
        var title: String
        var rows: [Row]
        var id: String { title }
    }

    /// A recorded chord something else has: nothing is written until the
    /// user replaces it.
    struct Conflict: Equatable {
        /// The command the chord was recorded for.
        var command: String
        var chord: String
        /// What the chord does now, as the pane says it (sanitised).
        var text: String
    }

    /// One write of `[keys]`: a chord (the table key as written) set to a
    /// value, or removed.
    enum Edit: Equatable, Sendable {
        case set(String, String)
        case remove(String)

        /// The key path of `chord`: `keys."<chord>"`, quotes and backslashes
        /// escaped.
        static func path(_ chord: String) -> String {
            let escaped = chord.replacingOccurrences(of: "\\", with: "\\\\")
                .replacingOccurrences(of: "\"", with: "\\\"")
            return "keys.\"\(escaped)\""
        }
    }

    /// The key of the message Restore Defaults shows.
    static let restoreKey = "action:restore_keys"
    /// At most this many of the key map's warnings are listed.
    static let maxWarnings = 20

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "settings")

    /// The configuration and key map this pane edits; the settings model
    /// owns this one.
    @ObservationIgnored weak var settings: SettingsModel?
    /// The Settings window: only its key events are recorded.
    @ObservationIgnored weak var window: NSWindow?
    /// Where key events come from while recording.
    @ObservationIgnored var monitoring: any KeyEventMonitoring = SystemKeyEventMonitoring()

    /// The command whose shortcut is being recorded.
    private(set) var recording: String?
    /// The modifiers held while recording, as macOS shows them.
    private(set) var heldModifiers = ""
    private(set) var conflict: Conflict?
    /// Messages by command (or ``restoreKey``), sanitised.
    private(set) var messages: [String: String] = [:]
    /// The command and chords the edit being made sets, shown until it
    /// applies.
    private var editing: (command: String, chords: [String])?
    private var inFlight = false
    /// The configuration's generation when the latest edit was queued.
    private var queuedAt: UInt64?

    /// The monitor and window observers of a recording.
    @ObservationIgnored private var registrations: [KeysRegistration] = []

    init() {}

    // MARK: Reading

    /// The key map and `[keys]` as written (chord → value), when both are
    /// of one generation: a key map torn from its configuration edits
    /// nothing.
    private var state: (map: TesseraKeymap, entries: [String: String])? {
        guard let settings, let map = settings.keymap, map.generation == settings.snapshot.generation else {
            return nil
        }
        var entries: [String: String] = [:]
        if case .table(let table)? = settings.snapshot["keys"] {
            for (key, value) in table {
                if let value = value.string { entries[key] = value }
            }
        }
        return (map, entries)
    }

    /// Whether an edit is queued, running, or written and not yet applied:
    /// the pane edits nothing until it is.
    var isChanging: Bool {
        if inFlight { return true }
        guard let queuedAt, let generation = settings?.keymap?.generation else { return false }
        return generation <= queuedAt
    }

    /// Whether the key map could be read.
    var isAvailable: Bool { settings?.keymap != nil }

    /// The key map's warnings about `[keys]` (escaped and bounded by the
    /// core), sanitised again for the window, at most ``maxWarnings``.
    var warnings: [String] {
        let all = settings?.keymap?.warnings ?? []
        var shown = all.prefix(Self.maxWarnings).map(SettingsModel.sanitisedMessage)
        if all.count > Self.maxWarnings {
            shown.append("And \(all.count - Self.maxWarnings) more; see config.toml.")
        }
        return shown
    }

    func message(_ key: String) -> String? {
        messages[key]
    }

    /// Whether `[keys]` binds a command or unbinds a default: what Restore
    /// Defaults removes.
    var hasCustomShortcuts: Bool {
        guard case let (map, entries)? = state else { return false }
        return entries.values.contains { Self.isRestorable($0, map) }
    }

    /// The commands, grouped by the menu their item is in, matching `query`
    /// (in a title, a command name or a chord), in the key map's order.
    func groups(matching query: String) -> [Group] {
        guard let map = settings?.keymap else { return [] }
        let changing = isChanging
        let query = query.trimmingCharacters(in: .whitespaces).lowercased()
        var groups: [Group] = []
        for command in map.commands {
            let item = MainMenu.item(forCommand: command.command, in: NSApp.mainMenu)
            var chords = command.chords
            if changing, let editing, editing.command == command.command {
                chords = editing.chords
            }
            let row = Row(
                command: command.command, title: Self.title(of: command, item: item),
                group: Self.group(of: item), chords: chords, planned: command.owner == .planned)
            if !query.isEmpty {
                let haystack = [row.title, row.command, row.display] + row.chords
                guard haystack.contains(where: { $0.lowercased().contains(query) }) else { continue }
            }
            if let index = groups.firstIndex(where: { $0.title == row.group }) {
                groups[index].rows.append(row)
            } else {
                groups.append(Group(title: row.group, rows: [row]))
            }
        }
        return groups
    }

    /// The menu item's title, or the command's name in words
    /// (`equalize_panes` → "Equalize panes"), sanitised.
    private static func title(of command: TesseraKeymap.Command, item: NSMenuItem?) -> String {
        if let title = item?.title, !title.isEmpty {
            return SettingsModel.sanitisedMessage(title)
        }
        var words = command.name.replacingOccurrences(of: "_", with: " ")
        words = words.prefix(1).uppercased() + words.dropFirst()
        if let parameter = command.parameter { words += " " + parameter }
        return SettingsModel.sanitisedMessage(words)
    }

    private static func group(of item: NSMenuItem?) -> String {
        guard let menu = item?.menu else { return "Other" }
        let title = SettingsModel.sanitisedMessage(menu.title)
        return title.isEmpty ? "Tessera" : title
    }

    /// The title the pane gives `command`.
    private func title(of command: String) -> String {
        guard let spec = settings?.keymap?.command(named: command) else {
            return SettingsModel.sanitisedMessage(command)
        }
        return Self.title(of: spec, item: MainMenu.item(forCommand: command, in: NSApp.mainMenu))
    }

    // MARK: Recording

    /// Records a shortcut for `command`: the next key pressed in the
    /// Settings window with or without modifiers (modifiers alone wait for
    /// a key). Escape cancels, Delete clears the command's shortcut. Only
    /// the Settings window's keys are taken, and only until one is
    /// recorded, the window resigns key or closes, or the pane goes.
    func startRecording(_ command: String) {
        guard !isChanging, let window else { return }
        stopRecording()
        conflict = nil
        messages[command] = nil
        recording = command
        let monitor = monitoring.monitorKeys { [weak self] event in
            self?.handle(event) ?? false
        }
        let center = NotificationCenter.default
        let observers = [NSWindow.didResignKeyNotification, NSWindow.willCloseNotification].map { name in
            center.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.stopRecording() }
            }
        }
        registrations = [
            monitor,
            KeysRegistration { observers.forEach(center.removeObserver) },
        ]
    }

    /// Stops recording, if it was: the monitor and observers go.
    func stopRecording() {
        let registrations = registrations
        self.registrations = []
        registrations.forEach { $0.remove() }
        recording = nil
        heldModifiers = ""
    }

    /// How many monitors and observers a recording has installed (for
    /// tests).
    var registrationCount: Int { registrations.count }

    /// A key event while recording: whether the pane took it. Only events
    /// of the Settings window are looked at; every other passes.
    func handle(_ event: NSEvent) -> Bool {
        guard let command = recording, let window, event.window === window else { return false }
        switch event.type {
        case .flagsChanged:
            heldModifiers = KeyChord.display(flags: KeyChord.chordFlags(event.modifierFlags))
            return false
        case .keyDown:
            let bare = KeyChord.chordFlags(event.modifierFlags).isEmpty
            if bare, event.keyCode == 53 {  // Escape
                stopRecording()
            } else if bare, event.keyCode == 51 || event.keyCode == 117 {  // Delete, forward delete
                stopRecording()
                clear(command)
            } else if let chord = KeyChord.chord(of: event) {
                stopRecording()
                record(chord, for: command)
            } else {
                messages[command] = "This key cannot be part of a shortcut."
            }
            return true
        default:
            return false
        }
    }

    // MARK: Editing

    /// Gives `command` the shortcut `chord`, its only one: unless something
    /// else has the chord, then the conflict waits for ``replace()``.
    func record(_ chord: String, for command: String) {
        guard case let (map, entries)? = state else {
            messages[command] = "config.toml is being applied; try again."
            return
        }
        conflict = nil
        messages[command] = nil
        if let owner = map.commands.first(where: { $0.command != command && $0.chords.contains(chord) }) {
            conflict = Conflict(
                command: command, chord: chord,
                text: "\(KeyChord.display(chord)) is the shortcut of “\(title(of: owner.command))”.")
            return
        }
        // An entry that sends text or is `none`; any other value the core
        // does not run (an unknown command) binds nothing.
        if let value = entries[chord], value == "none" || (value.hasPrefix("send_text:") && value != "send_text:") {
            let does = value == "none" ? "turned off in config.toml" : "set to send text in config.toml"
            conflict = Conflict(command: command, chord: chord, text: "\(KeyChord.display(chord)) is \(does).")
            return
        }
        assign(chord, to: command)
    }

    /// Gives the conflicting chord to the command it was recorded for; what
    /// had it loses it (an entry of `[keys]` is replaced, a default goes to
    /// the entry).
    func replace() {
        guard let conflict else { return }
        self.conflict = nil
        assign(conflict.chord, to: conflict.command)
    }

    /// Keeps what the conflicting chord does.
    func cancelConflict() {
        conflict = nil
    }

    /// Makes `chord` the one shortcut of `command`: `keys."<chord>"` set to
    /// it, the command's other entries removed and the defaults no entry
    /// took set to `unbound`. Writes nothing it need not.
    private func assign(_ chord: String, to command: String) {
        guard case let (map, entries)? = state, let spec = map.command(named: command) else {
            messages[command] = "config.toml is being applied; try again."
            return
        }
        let own = entries.filter { $0.value == command }.map(\.key).sorted()
        let defaults = Self.defaults(of: spec, own: own)
        var edits: [Edit] = []
        if entries[chord] != command, !defaults.contains(chord) {
            edits.append(.set(chord, command))
        }
        edits += own.filter { $0 != chord }.map { .remove($0) }
        edits += defaults.filter { $0 != chord }.map { .set($0, "unbound") }
        run(edits, key: command, showing: (command, [chord]))
    }

    /// Takes every shortcut from `command`: its entries removed, its
    /// defaults set to `unbound`.
    func clear(_ command: String) {
        guard !isChanging else { return }
        guard case let (map, entries)? = state, let spec = map.command(named: command) else {
            messages[command] = "config.toml is being applied; try again."
            return
        }
        conflict = nil
        let own = entries.filter { $0.value == command }.map(\.key).sorted()
        let edits = own.map { Edit.remove($0) } + Self.defaults(of: spec, own: own).map { .set($0, "unbound") }
        run(edits, key: command, showing: (command, []))
    }

    /// The default chords `command` still has: the key map lists the
    /// entries' chords first (one per entry, in document order), then the
    /// defaults no entry took.
    private static func defaults(of command: TesseraKeymap.Command, own: [String]) -> [String] {
        command.chords.dropFirst(own.count).filter { !own.contains($0) }
    }

    /// Whether Restore Defaults removes an entry with `value`: one binding
    /// a command or unbinding a default, what the pane writes. Entries that
    /// send text or turn a key off (`none`) stay.
    private static func isRestorable(_ value: String, _ map: TesseraKeymap) -> Bool {
        value == "unbound" || map.command(named: value) != nil
    }

    /// Restore Defaults: removes each `[keys]` entry that binds a command
    /// or unbinds a default, one key at a time; comments, other tables and
    /// the entries that send text or turn a key off stay.
    func restoreDefaults() {
        guard !isChanging, case let (map, entries)? = state else { return }
        stopRecording()
        conflict = nil
        let edits = entries.filter { Self.isRestorable($0.value, map) }.map(\.key).sorted().map { Edit.remove($0) }
        run(edits, key: Self.restoreKey, showing: nil)
    }

    /// Runs `edits` in order on the write queue, stopping at the first that
    /// fails; a conflict (the file changed since the core read it) is run
    /// once more. Nothing is queued for no edits.
    private func run(_ edits: [Edit], key: String, showing: (command: String, chords: [String])?) {
        guard !edits.isEmpty else { return }
        guard let settings, let writer = settings.writer else {
            messages[key] = "Tessera’s core is not running."
            return
        }
        messages[key] = nil
        inFlight = true
        queuedAt = settings.snapshot.generation
        editing = showing
        SettingsModel.writeQueue.async { [weak self] in
            var retried = false
            var failure: TesseraConfigEditError?
            for edit in edits {
                var result = Self.perform(writer, edit)
                if case .failure(let error) = result, error.kind == .conflict {
                    retried = true
                    result = Self.perform(writer, edit)
                }
                if case .failure(let error) = result {
                    failure = error
                    break
                }
            }
            let outcome = failure
            Task { @MainActor [weak self] in
                self?.finish(key, failure: outcome, retried: retried)
            }
        }
    }

    private nonisolated static func perform(
        _ writer: any SettingsConfigWriting, _ edit: Edit
    ) -> Result<Void, TesseraConfigEditError> {
        do throws(TesseraConfigEditError) {
            switch edit {
            case .set(let chord, let value): try writer.set(Edit.path(chord), to: .string(value))
            case .remove(let chord): try writer.remove(Edit.path(chord))
            }
            return .success(())
        } catch {
            return .failure(error)
        }
    }

    private func finish(_ key: String, failure: TesseraConfigEditError?, retried: Bool) {
        inFlight = false
        if let failure {
            // S8: the message may quote the file; only its kind is logged.
            Self.log.info("shortcut not saved: \(String(describing: failure.kind), privacy: .public)")
            messages[key] = SettingsModel.displayMessage(failure)
            queuedAt = nil
            editing = nil
        }
        if retried || failure != nil {
            settings?.reload()
            if failure == nil, let settings {
                // The other edit was applied first: wait for one after
                // this.
                queuedAt = settings.snapshot.generation
            }
        }
    }
}

// MARK: - View

/// Keys: every command with its shortcuts, a search field, recording a
/// shortcut by pressing it, conflicts shown before anything is replaced,
/// the key map's warnings, and Restore Defaults.
struct KeysPane: View {
    let model: SettingsModel
    @State private var query = ""
    @State private var confirmingRestore = false

    var body: some View {
        let keys = model.keys
        let changing = keys.isChanging
        VStack(alignment: .leading, spacing: 12) {
            if let error = model.loadError {
                SettingsMessage(text: error)
            }
            TextField("Search", text: $query, prompt: Text("Search commands and shortcuts"))
                .textFieldStyle(.roundedBorder)
            if keys.isAvailable {
                List {
                    ForEach(keys.groups(matching: query)) { group in
                        Section(group.title) {
                            ForEach(group.rows) { row in
                                KeysRow(keys: keys, row: row)
                            }
                        }
                    }
                }
                .listStyle(.bordered(alternatesRowBackgrounds: true))
                .frame(height: 380)
                .disabled(changing)
            } else {
                Text("The shortcuts could not be read.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 380)
            }
            let warnings = keys.warnings
            if !warnings.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Problems in config.toml’s [keys]").font(.headline)
                    ForEach(Array(warnings.enumerated()), id: \.offset) { _, warning in
                        Text(warning)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            HStack(alignment: .firstTextBaseline) {
                SettingsMessage(text: keys.message(KeysModel.restoreKey))
                Spacer()
                Button("Restore Defaults…") { confirmingRestore = true }
                    .disabled(changing || !keys.hasCustomShortcuts)
            }
        }
        .padding(20)
        .frame(width: 620)
        .fixedSize(horizontal: false, vertical: true)
        // Another pane, or the window closing, ends a recording.
        .onDisappear { keys.stopRecording() }
        .confirmationDialog("Restore the default shortcuts?", isPresented: $confirmingRestore) {
            Button("Restore Defaults", role: .destructive) { keys.restoreDefaults() }
        } message: {
            Text(
                "The shortcuts set in config.toml are removed, including ones written there by hand. Entries that send text or turn a key off stay, and so do comments."
            )
        }
    }
}

/// One command: its title, its shortcuts (a button that records a new
/// one), a conflict waiting for the user, and a refusal's message.
private struct KeysRow: View {
    let keys: KeysModel
    let row: KeysModel.Row

    var body: some View {
        let recording = keys.recording == row.command
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(row.title)
                    if row.planned {
                        Text("Not available yet")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Button {
                    if recording { keys.stopRecording() } else { keys.startRecording(row.command) }
                } label: {
                    Text(recording ? keys.heldModifiers + "…" : (row.chords.isEmpty ? "None" : row.display))
                        .lineLimit(1)
                        .frame(minWidth: 110)
                        .foregroundStyle(row.chords.isEmpty && !recording ? .secondary : .primary)
                }
                .accessibilityLabel("Shortcut for \(row.title)")
                .accessibilityValue(recording ? "Recording" : (row.chords.isEmpty ? "None" : row.display))
                .accessibilityHint(
                    "Press the new shortcut. It replaces the command’s other shortcuts. Escape cancels, Delete removes the shortcut."
                )
            }
            if let conflict = keys.conflict, conflict.command == row.command {
                HStack {
                    Label(conflict.text, systemImage: "exclamationmark.triangle")
                        .font(.callout)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    Button("Cancel") { keys.cancelConflict() }
                        .keyboardShortcut(.cancelAction)
                    Button("Replace") { keys.replace() }
                        .accessibilityLabel("Replace, giving \(KeyChord.display(conflict.chord)) to \(row.title)")
                }
            }
            SettingsMessage(text: keys.message(row.command))
        }
        .padding(.vertical, 2)
    }
}
