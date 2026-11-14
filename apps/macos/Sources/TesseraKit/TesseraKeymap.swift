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

    /// One `[keys]` entry as the core read it (#355): what a settings view
    /// lists. Every text is escaped by the core (S3) and capped, so it is
    /// for display and matching only: ``value`` is never written back.
    public struct Entry: Sendable, Equatable, Decodable {
        /// Whether the entry took effect.
        public enum Effect: String, Sendable, Equatable {
            /// It binds its chord: a command, a text or `none`.
            case bound
            /// `unbound`: it took its chord from every command.
            case unbound
            /// An earlier entry for the same chord took effect.
            case shadowed
            /// Left out for itself (a chord or value that does not parse,
            /// an unknown command, an invisible character).
            case refused
        }

        /// The entry's value as written, escaped and capped.
        public enum Value: Sendable, Equatable {
            /// A command (`select_tab:3`), `unbound`, `none`, or a value
            /// that does not parse.
            case text(String)
            /// The text after `send_text:`, its escapes as written.
            case sendText(String)
            /// The value is not a string.
            case notAString
        }

        /// The entry's place in `[keys]`, from 0.
        public let index: Int
        /// The table key as written (`shift+cmd+u`), escaped and capped at
        /// 256 bytes.
        public let key: String
        /// Its canonical chord (`cmd+shift+u`, as ``Command/chords`` lists
        /// it), or `nil` when the key does not parse.
        public let chord: String?
        public let value: Value
        public let effect: Effect
        /// Why it was left out, or the caveat it is honoured with.
        public let warning: String?

        private enum CodingKeys: String, CodingKey {
            case index, key, chord, value, effect, warning
        }

        private enum ValueKeys: String, CodingKey {
            case sendText = "send_text"
        }

        public init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            index = try container.decode(Int.self, forKey: .index)
            key = try container.decode(String.self, forKey: .key)
            chord = try container.decodeIfPresent(String.self, forKey: .chord)
            if try container.decodeNil(forKey: .value) {
                value = .notAString
            } else if let text = try? container.decode(String.self, forKey: .value) {
                value = .text(text)
            } else {
                let object = try container.nestedContainer(keyedBy: ValueKeys.self, forKey: .value)
                value = .sendText(try object.decode(String.self, forKey: .sendText))
            }
            // An effect a newer core adds reads as refused: it binds nothing
            // the pane knows of.
            effect = Effect(rawValue: try container.decode(String.self, forKey: .effect)) ?? .refused
            warning = try container.decodeIfPresent(String.self, forKey: .warning)
        }
    }

    /// The configurations the core applied before this map (0 for the one
    /// it started with).
    public let generation: UInt64
    /// Every command, in id order.
    public let commands: [Command]
    /// The first 512 `[keys]` entries in file order, refused ones included,
    /// read with the chords (one generation).
    public let entries: [Entry]
    /// Why `[keys]` entries were left out or honoured with a caveat (escaped
    /// and bounded by the core), and last the one for the entries past the
    /// first 512, if any; it has logged them already.
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
