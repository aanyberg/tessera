import AppKit
import Carbon
import TesseraKit
import XCTest

@testable import Tessera

/// The chord the Keys pane records for a pressed key is one the core runs
/// for that key (#40c adversarial B1): every character key of the US,
/// Swedish, French and German layouts, with Command, Control and Shift, is
/// named by ``KeyChord``, the names are bound in `[keys]` of a real core,
/// and the key event the terminal view would send resolves to the command
/// its name was bound to. A layout's chords are bound together, as many at
/// once as there are commands, so a chord recorded for one key that takes
/// another key's lookup shows too.
@MainActor
final class KeyChordResolveTests: XCTestCase {
    @MainActor
    private struct Press {
        var layout: String
        var keyCode: UInt16
        var unshifted: Unicode.Scalar?
        var shifted: Unicode.Scalar?
        var flags: NSEvent.ModifierFlags

        var label: String {
            let own = unshifted.map { String($0) } ?? "nil"
            let typed = shifted.map { String($0) } ?? "nil"
            return "\(layout) kc\(keyCode) \(KeyChord.display(flags: flags)) \(own) (\(typed))"
        }
    }

    private var dir: URL?
    private var previous: String?

    override func setUp() async throws {
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
    }

    func testARecordedChordIsTheOnePressingItsKeyRuns() throws {
        // The adversarial report's cases, whatever layouts the host has.
        var presses = [
            Press(layout: "Swedish", keyCode: 27, unshifted: "+", shifted: "?", flags: [.command, .shift]),
            Press(layout: "French", keyCode: 18, unshifted: "&", shifted: "1", flags: [.command, .shift]),
            Press(layout: "German", keyCode: 30, unshifted: "+", shifted: "*", flags: [.command, .shift]),
        ]
        let layouts = [
            "com.apple.keylayout.US", "com.apple.keylayout.Swedish-Pro", "com.apple.keylayout.French",
            "com.apple.keylayout.German",
        ]
        let modifierSets: [NSEvent.ModifierFlags] = [
            [.command], [.control], [.command, .shift], [.control, .shift], [.command, .option, .shift],
        ]
        let keyCodes = (UInt16(0)...50).filter { TerminalSurfaceView.namedKey($0) == nil && $0 != 49 } + [93, 94]
        for id in layouts {
            let layout = try XCTUnwrap(Self.layout(id), "\(id) is installed")
            for keyCode in keyCodes {
                let unshifted = Self.character(layout, keyCode, shift: false)
                let shifted = Self.character(layout, keyCode, shift: true)
                guard unshifted != nil else { continue }  // no key there
                for flags in modifierSets {
                    presses.append(
                        Press(layout: id, keyCode: keyCode, unshifted: unshifted, shifted: shifted, flags: flags))
                }
            }
        }
        XCTAssertEqual(
            KeyChord.chord(keyCode: 27, flags: [.command, .shift], unshifted: "+", shifted: "?"), "cmd+shift+/",
            "Swedish ⇧⌘+ types ?")
        XCTAssertEqual(
            KeyChord.chord(keyCode: 18, flags: [.command, .shift], unshifted: "&", shifted: "1"), "cmd+1",
            "French ⇧⌘& types 1")

        // Per layout (a user's own), each chord gets a command of its own;
        // as many chords per core as there are commands.
        let named = presses.map {
            ($0, KeyChord.chord(keyCode: $0.keyCode, flags: $0.flags, unshifted: $0.unshifted, shifted: $0.shifted))
        }
        // Refused: a key beyond ASCII with no US key at its place, or a US
        // shifted symbol with Shift, whose Shift character is not ASCII
        // either (the § key, French ⇧⌘) typing °).
        let refused = named.filter { $0.1 == nil }.map(\.0)
        for press in refused {
            let own = try XCTUnwrap(press.unshifted)
            let noUSKey = !own.isASCII && TerminalSurfaceView.usLayoutCharacter(press.keyCode) == nil
            let shiftedSymbol = own.isASCII && press.flags.contains(.shift)
            let shiftedASCII =
                press.flags.contains(.shift)
                && press.shifted.map { (0x21..<0x7F).contains($0.value) && !("A"..."Z").contains($0) } ?? false
            XCTAssertTrue((noUSKey || shiftedSymbol) && !shiftedASCII, "\(press.label) is refused")
        }
        let commands = try withCore(config: nil) { core, _ in try core.keymap().commands.map(\.command) }
        var checked = 0
        for layout in Set(presses.map(\.layout)).sorted() {
            try check(named.filter { $0.0.layout == layout }, commands, &checked)
        }
        XCTAssertEqual(checked, presses.count - refused.count)
    }

    /// Binds every chord of `named` to a command of its own and checks that
    /// each press resolves to its chord's command.
    private func check(_ named: [(Press, String?)], _ commands: [String], _ checked: inout Int) throws {
        var chords: [String] = []
        for case let (_, chord?) in named where !chords.contains(chord) { chords.append(chord) }
        for start in stride(from: 0, to: chords.count, by: commands.count) {
            let batch = Dictionary(
                uniqueKeysWithValues: zip(chords[start..<min(start + commands.count, chords.count)], commands))
            let config = "[keys]\n" + batch.map { "\(Self.quoted($0.key)) = \(Self.quoted($0.value))\n" }.joined()
            try withCore(config: config) { core, session in
                XCTAssertEqual(
                    try core.keymap().warnings.filter { !$0.contains("honoured") }, [], "every chord applies")
                for case let (press, chord?) in named {
                    guard let command = batch[chord] else { continue }
                    checked += 1
                    let outcome = try session.handle(key: Self.event(press), bindingsOnly: true)
                    XCTAssertEqual(outcome, .command(command), "\(press.label) records \(chord)")
                }
            }
        }
    }

    // MARK: Helpers

    /// The event `TerminalSurfaceView` sends the core for `press`
    /// (`KeyInfo`, `modifiers(of:)`).
    private static func event(_ press: Press) -> TesseraSession.KeyEvent {
        var modifiers: TesseraSession.Modifiers = []
        if press.flags.contains(.shift) { modifiers.insert(.shift) }
        if press.flags.contains(.option) { modifiers.insert(.alt) }
        if press.flags.contains(.control) { modifiers.insert(.control) }
        if press.flags.contains(.command) { modifiers.insert(.super) }
        let base = TerminalSurfaceView.usLayoutCharacter(press.keyCode)
        return TesseraSession.KeyEvent(
            key: .char, codepoint: press.unshifted, shifted: press.shifted,
            baseLayout: base == press.unshifted ? nil : base, modifiers: modifiers)
    }

    /// Runs `body` with a core on `config` (none: no `config.toml`) and a
    /// session of it.
    private func withCore<T>(config: String?, _ body: (TesseraApp, TesseraSession) throws -> T) throws -> T {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-chord-\(UUID().uuidString)")
        self.dir = dir
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        if let config {
            try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        }
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        defer { core.shutdown() }
        let session = try core.spawn(.init(program: "/bin/cat", arguments: []))
        return try body(core, session)
    }

    /// `text` as a TOML basic string.
    private static func quoted(_ text: String) -> String {
        "\"" + text.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }

    /// The key layout data of the installed input source `id`.
    private static func layout(_ id: String) -> Data? {
        let filter = [kTISPropertyInputSourceID as String: id] as CFDictionary
        guard let sources = TISCreateInputSourceList(filter, true)?.takeRetainedValue() as? [TISInputSource],
            let source = sources.first,
            let pointer = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData)
        else { return nil }
        return Unmanaged<CFData>.fromOpaque(pointer).takeUnretainedValue() as Data
    }

    /// The one character `layout` types at `keyCode` (a dead key's own),
    /// as `KeyInfo` reads it, or `nil`.
    private static func character(_ layout: Data, _ keyCode: UInt16, shift: Bool) -> Unicode.Scalar? {
        var deadKeys: UInt32 = 0
        var characters = [UniChar](repeating: 0, count: 8)
        var length = 0
        layout.withUnsafeBytes { raw in
            guard let layout = raw.bindMemory(to: UCKeyboardLayout.self).baseAddress else { return }
            _ = UCKeyTranslate(
                layout, keyCode, UInt16(kUCKeyActionDown), shift ? UInt32(shiftKey >> 8) : 0,
                UInt32(LMGetKbdType()), OptionBits(kUCKeyTranslateNoDeadKeysMask), &deadKeys, characters.count,
                &length, &characters)
        }
        let scalars = String(utf16CodeUnits: characters, count: length).unicodeScalars
        guard scalars.count == 1, let scalar = scalars.first, !TerminalSurfaceView.isKeyCharacter(scalar) else {
            return nil
        }
        return scalar
    }
}
