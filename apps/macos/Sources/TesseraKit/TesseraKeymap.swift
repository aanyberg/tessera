import AppKit
internal import TesseraCore

/// The key map in effect (ADR 0036): every command the core knows, with the
/// chords `config.toml`'s `[keys]` and the defaults bind to it, and the
/// chords a menu item can show as its key equivalent.
///
/// Read with ``TesseraApp/keymap()`` at start and again on every
/// ``TesseraAppDelegate/configurationDidApply(generation:)``. The menus show
/// it; keys reach the core first (``TesseraSession/handle(key:bindingsOnly:)``),
/// which resolves every chord, menu or not.
public struct TesseraKeymap: Sendable, Equatable, Decodable {
    /// Who runs a command.
    public enum Owner: String, Sendable, Equatable {
        /// An `AppKit` menu item (Quit, Hide, Minimize): bindable, warned about.
        case system
        /// A Tessera menu item or core action.
        case app
        /// A command a later version runs; bindable, does nothing yet.
        case planned
    }

    /// A key equivalent a menu item can carry.
    public struct MenuShortcut: Sendable, Equatable, Decodable {
        /// The item's `keyEquivalent`: one character, the shifted symbol for
        /// a chord with Shift on a symbol key (`{` for ⇧⌘[), a function key's
        /// `NS…FunctionKey` character.
        public let keyEquivalent: String
        /// The item's `keyEquivalentModifierMask`; always holds Command.
        public let modifiers: NSEvent.ModifierFlags

        /// Bits as the core sends them: Shift 1, Option 2, Control 4,
        /// Command 8.
        private static let bits: [(UInt8, NSEvent.ModifierFlags)] = [
            (1, .shift), (2, .option), (4, .control), (8, .command),
        ]

        private enum CodingKeys: String, CodingKey {
            case keyEquivalent = "key_equivalent"
            case modifiers
        }

        public init(keyEquivalent: String, modifiers: NSEvent.ModifierFlags) {
            self.keyEquivalent = keyEquivalent
            self.modifiers = modifiers
        }

        public init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            keyEquivalent = try container.decode(String.self, forKey: .keyEquivalent)
            let raw = try container.decode(UInt8.self, forKey: .modifiers)
            modifiers = Self.bits.reduce(into: []) { flags, bit in
                if raw & bit.0 != 0 { flags.insert(bit.1) }
            }
        }
    }

    /// One command of the core's registry.
    public struct Command: Sendable, Equatable, Decodable {
        /// The id ``TesseraSession/handle(key:bindingsOnly:)`` resolves.
        public let id: UInt32
        /// The command as `[keys]` names it (`new_tab`, `select_tab:3`); the
        /// `identifier` of its menu item.
        public let command: String
        /// The command's name without its parameter (`select_tab`).
        public let name: String
        /// Its parameter (`3`), if it takes one.
        public let parameter: String?
        public let owner: Owner
        /// The chords bound to it, canonical (`cmd+shift+[`), the user's
        /// first.
        public let chords: [String]
        /// The chords a menu item can show: the first is the item's own, the
        /// rest are hidden alternates. Chords without Command never go on a
        /// menu.
        public let menu: [MenuShortcut]

        private enum CodingKeys: String, CodingKey {
            case id, command, name, parameter, owner, chords, menu
        }

        public init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            id = try container.decode(UInt32.self, forKey: .id)
            command = try container.decode(String.self, forKey: .command)
            name = try container.decode(String.self, forKey: .name)
            parameter = try container.decodeIfPresent(String.self, forKey: .parameter)
            // An owner a newer core adds reads as planned: nothing to run.
            owner = Owner(rawValue: try container.decode(String.self, forKey: .owner)) ?? .planned
            chords = try container.decode([String].self, forKey: .chords)
            menu = try container.decode([MenuShortcut].self, forKey: .menu)
        }
    }

    /// The configurations the core applied before this map (0 for the one
    /// it started with).
    public let generation: UInt64
    /// Every command, in id order.
    public let commands: [Command]
    /// Why `[keys]` entries were left out or honoured with a caveat (escaped
    /// and bounded by the core); it has logged them already.
    public let warnings: [String]

    /// The command `[keys]` calls `command` (`new_tab`, `select_tab:3`).
    public func command(named command: String) -> Command? {
        commands.first { $0.command == command }
    }

    /// Decodes the core's JSON (`tessera_app_keymap_json`).
    static func decode(_ json: [UInt8]) throws -> TesseraKeymap {
        try JSONDecoder().decode(TesseraKeymap.self, from: Data(json))
    }

    /// Reads the key map of `app`: the size, then the JSON, again with room
    /// to spare if a configuration applied in between made it longer.
    static func read(_ app: OpaquePointer?) throws -> TesseraKeymap {
        var length = 0
        let probe = tessera_app_keymap_json(app, nil, 0, &length)
        if probe != TESSERA_ERROR_INVALID_ARGUMENT {
            try TesseraCoreError.check(probe)
        }
        for _ in 0..<3 {
            var buffer = [UInt8](repeating: 0, count: length + 4096)
            let capacity = buffer.count
            let status = buffer.withUnsafeMutableBufferPointer {
                tessera_app_keymap_json(app, $0.baseAddress, capacity, &length)
            }
            if status == TESSERA_ERROR_INVALID_ARGUMENT { continue }
            try TesseraCoreError.check(status)
            return try decode(Array(buffer.prefix(length)))
        }
        throw TesseraCoreError.invalidArgument
    }
}
