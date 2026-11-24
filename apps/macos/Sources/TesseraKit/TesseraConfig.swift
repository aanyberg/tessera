import Foundation
internal import TesseraCore

/// A value of the configuration, as the core reports it
/// (``TesseraApp/configSnapshot()``) and as an edit passes it
/// (``TesseraConfigWriter/set(_:to:)``): what JSON can hold.
///
/// The core decides what a value means and whether it may be written (ADR
/// 0040); this type only carries it across the C boundary.
public enum TesseraConfigValue: Sendable, Hashable {
    case string(String)
    case integer(Int64)
    case float(Double)
    case bool(Bool)
    case array([TesseraConfigValue])
    /// A table. Read only: an edit never writes a table (ADR 0040).
    case table([String: TesseraConfigValue])
    /// A key the configuration has no value for (an optional key left out).
    case null

    /// The value at `path` under this one: keys joined by `.`, each
    /// optionally followed by `[N]` (`appearance.opacity`,
    /// `profiles[1].appearance.theme`), as the core's key paths, but bare
    /// keys only. `nil` when any step is missing; a JSON `null` is `nil` too.
    public subscript(path: String) -> TesseraConfigValue? {
        var value: TesseraConfigValue? = self
        for step in path.split(separator: ".", omittingEmptySubsequences: false) {
            var key = Substring(step)
            var indices: [Int] = []
            while key.hasSuffix("]"), let open = key.lastIndex(of: "[") {
                guard let index = Int(key[key.index(after: open)..<key.index(before: key.endIndex)]) else {
                    return nil
                }
                indices.insert(index, at: 0)
                key = key[..<open]
            }
            guard case .table(let table)? = value, let next = table[String(key)] else { return nil }
            value = next
            for index in indices {
                guard case .array(let array)? = value, array.indices.contains(index) else { return nil }
                value = array[index]
            }
        }
        if case .null? = value { return nil }
        return value
    }

    /// The string, for a string.
    public var string: String? {
        if case .string(let string) = self { return string }
        return nil
    }

    /// The number, for an integer or a float.
    public var double: Double? {
        switch self {
        case .integer(let integer): Double(integer)
        case .float(let float): float
        default: nil
        }
    }

    /// The integer, for an integer (or a float without a fraction).
    public var integer: Int64? {
        switch self {
        case .integer(let integer): integer
        case .float(let float) where float.rounded() == float && abs(float) < 9.0e18: Int64(float)
        default: nil
        }
    }

    /// The flag, for a boolean.
    public var bool: Bool? {
        if case .bool(let bool) = self { return bool }
        return nil
    }

    /// The elements, for an array.
    public var array: [TesseraConfigValue]? {
        if case .array(let array) = self { return array }
        return nil
    }

    /// The value as the JSON text ``TesseraConfigWriter/set(_:to:)`` passes
    /// to the core, or `nil` for one an edit cannot write: a table, `null`,
    /// a float that is not finite. A float always has a fraction or an
    /// exponent (`1.0`, not `1`), so the core keeps a float key a float.
    var json: String? {
        switch self {
        case .string(let string): Self.quoted(string)
        case .integer(let integer): String(integer)
        case .float(let float):
            // Swift's shortest round-trip form: `1.0`, `0.85`, `1e-05`.
            float.isFinite ? "\(float)" : nil
        case .bool(let bool): bool ? "true" : "false"
        case .array(let elements):
            elements.reduce(into: Optional("")) { text, element in
                guard let previous = text, let json = element.json else { return text = nil }
                text = previous.isEmpty ? json : previous + "," + json
            }.map { "[\($0)]" }
        case .table, .null: nil
        }
    }

    /// `string` as a JSON string: quotes, backslashes and C0 controls
    /// escaped, everything else as is.
    static func quoted(_ string: String) -> String {
        var text = "\""
        for scalar in string.unicodeScalars {
            switch scalar {
            case "\"": text += "\\\""
            case "\\": text += "\\\\"
            case "\n": text += "\\n"
            case "\r": text += "\\r"
            case "\t": text += "\\t"
            case _ where scalar.value < 0x20:
                text += String(format: "\\u%04x", scalar.value)
            default: text.unicodeScalars.append(scalar)
            }
        }
        return text + "\""
    }
}

extension TesseraConfigValue {
    /// The value of the JSON text `json`. A number with a fraction or an
    /// exponent (`13.0`) is a float, one without an integer, as the core
    /// writes them: `JSONDecoder` would read `13.0` as an integer, and an
    /// edit that writes it back would turn a float key into an integer.
    /// `JSONSerialization` drops a U+FEFF that starts a string, so a text
    /// holding one takes its strings from `JSONDecoder` (#489): a string
    /// reaches the app byte for byte, as the core has it.
    ///
    /// - Throws: ``TesseraCoreError/invalidArgument`` if it is not JSON.
    public static func parse(json: Data) throws -> TesseraConfigValue {
        let object: Any
        do {
            object = try JSONSerialization.jsonObject(with: json, options: [.fragmentsAllowed])
        } catch {
            throw TesseraCoreError.invalidArgument
        }
        let escaped = String(decoding: json, as: UTF8.self)
            .range(of: "\\uFEFF", options: [.literal, .caseInsensitive])
        guard json.range(of: Data("\u{FEFF}".utf8)) != nil || escaped != nil else {
            return try TesseraConfigValue(object)
        }
        do {
            return try JSONDecoder().decode(ExactJSON.self, from: json).value(numbers: object)
        } catch {
            throw TesseraCoreError.invalidArgument
        }
    }

    private init(_ object: Any) throws {
        switch object {
        case is NSNull:
            self = .null
        case let number as NSNumber:
            if CFGetTypeID(number) == CFBooleanGetTypeID() {
                self = .bool(number.boolValue)
            } else if CFNumberIsFloatType(number) {
                self = .float(number.doubleValue)
            } else if number.compare(NSNumber(value: number.int64Value)) == .orderedSame {
                self = .integer(number.int64Value)
            } else {
                // Above Int64.max: the core writes no such integer.
                self = .float(number.doubleValue)
            }
        case let string as String:
            self = .string(string)
        case let array as [Any]:
            self = .array(try array.map(TesseraConfigValue.init))
        case let table as [String: Any]:
            self = .table(try table.mapValues(TesseraConfigValue.init))
        default:
            throw TesseraCoreError.invalidArgument
        }
    }
}

/// A JSON text as `JSONDecoder` reads it: every string exact, but a number
/// with a fraction no longer told from an integer (`13.0` decodes as both),
/// which ``value(numbers:)`` takes from `JSONSerialization`'s reading.
private enum ExactJSON: Decodable {
    case null
    case bool(Bool)
    case string(String)
    case number(integer: Int64?, float: Double)
    case array([ExactJSON])
    case table([String: ExactJSON])

    init(from decoder: any Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() {
            self = .null
        } else if let bool = try? value.decode(Bool.self) {
            self = .bool(bool)
        } else if let string = try? value.decode(String.self) {
            self = .string(string)
        } else if let float = try? value.decode(Double.self) {
            self = .number(integer: try? value.decode(Int64.self), float: float)
        } else if let array = try? value.decode([ExactJSON].self) {
            self = .array(array)
        } else {
            self = .table(try value.decode([String: ExactJSON].self))
        }
    }

    /// This value, with `numbers` the same place in `JSONSerialization`'s
    /// reading (whose table keys have lost a leading U+FEFF).
    func value(numbers: Any?) -> TesseraConfigValue {
        switch self {
        case .null:
            return .null
        case .bool(let bool):
            return .bool(bool)
        case .string(let string):
            return .string(string)
        case .number(let integer, let float):
            let number = numbers as? NSNumber
            let isFloat = number.map { CFGetTypeID($0) != CFBooleanGetTypeID() && CFNumberIsFloatType($0) } ?? false
            if !isFloat, let integer { return .integer(integer) }
            return .float(float)
        case .array(let array):
            let others = numbers as? [Any]
            return .array(
                array.enumerated().map { index, item in
                    item.value(numbers: others.flatMap { index < $0.count ? $0[index] : nil })
                })
        case .table(let table):
            let others = numbers as? [String: Any]
            var values: [String: TesseraConfigValue] = [:]
            for (key, item) in table {
                let bare = key.unicodeScalars.first == "\u{FEFF}" ? String(key.unicodeScalars.dropFirst()) : key
                values[key] = item.value(numbers: others?[key] ?? others?[bare])
            }
            return .table(values)
        }
    }
}

/// The configuration in effect (the one the views use: `config.toml` as
/// last applied, defaults filled in), from ``TesseraApp/configSnapshot()``.
public struct TesseraConfigSnapshot: Sendable, Equatable {
    /// How many configurations the core has applied before this one (0
    /// for the one it started with); ``TesseraAppDelegate/configurationDidApply(generation:)``
    /// reports the same count.
    public var generation: UInt64
    /// Every key `config.toml` documents, under the same names
    /// (`general`, `appearance`, `terminal`, `profiles`, `plugins`, `keys`).
    public var config: TesseraConfigValue
    /// Why `config.toml` does not load, while it does not (as the core
    /// words it, quoting the file escaped; sanitise it before showing it,
    /// S3): ``config`` is then the last good configuration, and an edit
    /// saved since is not in it (#547).
    /// ``TesseraAppDelegate/configurationDidNotLoad()`` reports a new one.
    public var loadError: String?

    public init(generation: UInt64, config: TesseraConfigValue, loadError: String? = nil) {
        self.generation = generation
        self.config = config
        self.loadError = loadError
    }

    /// The value at `path` (``TesseraConfigValue/subscript(_:)``).
    public subscript(path: String) -> TesseraConfigValue? {
        config[path]
    }
}

/// A theme `appearance.theme` can name, from ``TesseraApp/themes()``.
public struct TesseraTheme: Sendable, Hashable, Decodable {
    /// For display. The core removed control and bidirectional formatting
    /// characters and cut it to 128 bytes.
    public var name: String
    /// What to write to `appearance.theme`.
    public var id: String
    /// Bundled with the app, or a file in `themes/`.
    public var source: Source

    public enum Source: String, Sendable, Decodable {
        case bundled
        case user
    }

    public init(name: String, id: String, source: Source) {
        self.name = name
        self.id = id
        self.source = source
    }
}

/// The profile ``TesseraConfigWriter/addProfile(named:keys:)`` added: where
/// the configuration lists it and the name the core gave it.
public struct TesseraAddedProfile: Sendable, Equatable {
    /// Its index in `profiles`, also its key path index
    /// (`profiles[N].command`), once the watch has applied the file: after
    /// the `[[profiles]]` entries, before the profiles of `profiles/`, which
    /// move up one.
    public var index: Int
    /// The base name, or the base name numbered (`New Profile 2`) when a
    /// profile has it. Not sanitised (S3): show it as a profile name is.
    public var name: String

    public init(index: Int, name: String) {
        self.index = index
        self.name = name
    }
}

/// An edit of `config.toml` or an import the core did not make.
public struct TesseraConfigEditError: Error, Sendable, Equatable {
    public enum Kind: Sendable, Equatable {
        /// The loader would warn about the result (a value out of range,
        /// an unknown key, a theme that does not exist): nothing was
        /// written. `TESSERA_ERROR_REFUSED`.
        case refused
        /// The file changed on disk while the edit was made (an editor
        /// saved it), or an imported theme's file exists: nothing was
        /// written, the other change stays. `TESSERA_ERROR_CONFLICT`.
        case conflict
        /// An edit addressed by a profile's name
        /// (``TesseraConfigWriter/setProfile(named:ordinal:_:to:)``) found
        /// no `[[profiles]]` entry of that name and ordinal in
        /// `config.toml` as the edit read it (a hand edit renamed or
        /// removed it): nothing was written.
        /// `TESSERA_ERROR_NO_SUCH_PROFILE`.
        case noSuchProfile
        /// A key path or value that does not parse, or an edit the editor
        /// does not make; for an import, a file that is not a colour
        /// scheme. `TESSERA_ERROR_INVALID_ARGUMENT`.
        case invalidArgument
        /// The file cannot be read or written, or is not one the editor
        /// writes (read-only, a second hard link, not TOML).
        /// `TESSERA_ERROR_IO`.
        case io
        /// Any other failure, such as ``TesseraCoreError/nullPointer``
        /// after ``TesseraApp/shutdown()``.
        case core(TesseraCoreError)
    }

    public var kind: Kind
    /// The core's message for the user. It may quote values from the file
    /// (docs/security.md S8: never log it) and is not sanitised: show it
    /// only after removing control and bidirectional formatting characters
    /// (S3).
    public var message: String

    public init(_ kind: Kind, message: String) {
        self.kind = kind
        self.message = message
    }

    init(_ status: TesseraError, message: String) {
        let kind: Kind =
            switch status {
            case TESSERA_ERROR_REFUSED: .refused
            case TESSERA_ERROR_CONFLICT: .conflict
            case TESSERA_ERROR_NO_SUCH_PROFILE: .noSuchProfile
            case TESSERA_ERROR_INVALID_ARGUMENT: .invalidArgument
            case TESSERA_ERROR_IO: .io
            default: .core(TesseraCoreError(status) ?? .unknown(rawValue: status.rawValue))
            }
        self.init(kind, message: message)
    }
}

/// Edits `config.toml` and imports themes, one call at a time, from any
/// thread (ADR 0044): the Settings window runs them on a background queue
/// so a slow disk never stalls the main thread. Each call opens the file,
/// makes one edit, checks it with the loader and saves it atomically (ADR
/// 0040); the core's configuration watch then applies the file and
/// ``TesseraAppDelegate/configurationDidApply(generation:)`` follows. After
/// ``TesseraApp/shutdown()`` every call fails with
/// ``TesseraCoreError/nullPointer``; the shutdown waits for a call in
/// progress.
public final class TesseraConfigWriter: Sendable {
    /// Longest message read back from the core, in bytes.
    static let messageCapacity = 8 * 1024

    private let core: CoreHandle

    init(core: CoreHandle) {
        self.core = core
    }

    /// Sets the setting at the key path `key` (`appearance.opacity`,
    /// `profiles[1].appearance.theme`) to `value` and saves the file; a new
    /// profile goes through ``addProfile(named:keys:)``, since the profiles
    /// of `profiles/` come after the entries and refuse edits by key path.
    /// Creates the configuration
    /// directory and a default `config.toml` first when they are missing.
    ///
    /// - Throws: ``TesseraConfigEditError`` with the core's message; a
    ///   value that has no JSON form (a table, `null`, a float that is not
    ///   finite) as `.invalidArgument` without calling the core.
    public func set(_ key: String, to value: TesseraConfigValue) throws(TesseraConfigEditError) {
        guard let json = value.json else {
            throw TesseraConfigEditError(.invalidArgument, message: "This value cannot be written.")
        }
        try edit(key: key, json: json)
    }

    /// Removes the setting, table or profile at `key` (`profiles[1]`
    /// removes the second profile and moves the later ones down) so its
    /// default applies again, and saves the file; nothing happens when it
    /// is not there.
    ///
    /// - Throws: ``TesseraConfigEditError`` with the core's message.
    public func remove(_ key: String) throws(TesseraConfigEditError) {
        try edit(key: key, json: nil)
    }

    /// Sets `key`, a key path within a profile (`command`,
    /// `appearance.theme`), of the `[[profiles]]` entry named `name`, the
    /// `ordinal`-th (from 0) of the snapshot's profiles of that name, to
    /// `value` and saves the file (`tessera_config_set_profile`). The
    /// profile is looked up in `config.toml` as the core reads it for this
    /// edit, under the lock every edit holds, so an edit made before a hand
    /// edit inserted or removed another profile ahead of it still lands on
    /// its profile (#565), and of two entries with one name the one shown.
    ///
    /// - Throws: ``TesseraConfigEditError``: `.noSuchProfile` when fewer
    ///   than `ordinal + 1` entries have the name, `.invalidArgument` for
    ///   a name past `TESSERA_MAX_PROFILE_NAME_LEN` bytes, a negative
    ///   ordinal or a profile of `profiles/`, otherwise as ``set(_:to:)``.
    public func setProfile(
        named name: String, ordinal: Int, _ key: String, to value: TesseraConfigValue
    ) throws(TesseraConfigEditError) {
        guard let json = value.json else {
            throw TesseraConfigEditError(.invalidArgument, message: "This value cannot be written.")
        }
        try edit(profile: name, ordinal: ordinal, key: key, json: json)
    }

    /// Removes `key`, a key path within the profile named `name`, so its
    /// default applies again; an empty `key` removes the profile's entry
    /// itself and moves the later ones down
    /// (`tessera_config_remove_profile`). The profile, of that `ordinal`,
    /// is looked up as for ``setProfile(named:ordinal:_:to:)``; nothing
    /// happens when the key is not there.
    ///
    /// - Throws: ``TesseraConfigEditError``: `.noSuchProfile` as for
    ///   ``setProfile(named:ordinal:_:to:)``, otherwise as ``remove(_:)``.
    public func removeProfile(
        named name: String, ordinal: Int, _ key: String
    ) throws(TesseraConfigEditError) {
        try edit(profile: name, ordinal: ordinal, key: key, json: nil)
    }

    /// Adds a profile to `config.toml` with `keys` set in it, in one edit
    /// saved at once (`tessera_config_add_profile`), whatever profiles the
    /// files in `profiles/` define: a new `[[profiles]]` entry after the
    /// last one, named the first of `base`, `base 2`, `base 3`… no profile
    /// has. `keys` are key paths within the profile (`command`,
    /// `appearance.theme`) with their values, at most
    /// `TESSERA_MAX_ADDED_PROFILE_KEYS`; one the core refuses refuses the
    /// profile, and nothing is written.
    ///
    /// - Returns: the index and name the core gave the profile.
    /// - Throws: ``TesseraConfigEditError`` with the core's message, which
    ///   names the refused key path (`profiles[3].appearance.theme`); a
    ///   value that has no JSON form as `.invalidArgument` without calling
    ///   the core.
    public func addProfile(
        named base: String, keys: [(key: String, value: TesseraConfigValue)] = []
    ) throws(TesseraConfigEditError) -> TesseraAddedProfile {
        var object = "{\"name\":" + TesseraConfigValue.quoted(base)
        for (key, value) in keys {
            guard let json = value.json else {
                throw TesseraConfigEditError(
                    .invalidArgument, message: "The profile’s \(key) has a value that cannot be written.")
            }
            object += "," + TesseraConfigValue.quoted(key) + ":" + json
        }
        let profile = Array((object + "}").utf8)
        var index = 0
        var name = [UInt8](repeating: 0, count: Int(TESSERA_MAX_PROFILE_NAME_LEN))
        var nameLength = 0
        var message = [UInt8](repeating: 0, count: Self.messageCapacity)
        var messageLength = 0
        let status = core.call { app in
            profile.withUnsafeBufferPointer { profile in
                name.withUnsafeMutableBufferPointer { nameBuffer in
                    message.withUnsafeMutableBufferPointer { messageBuffer in
                        tessera_config_add_profile(
                            app, profile.baseAddress, profile.count, &index, nameBuffer.baseAddress,
                            nameBuffer.count, &nameLength, messageBuffer.baseAddress, messageBuffer.count,
                            &messageLength)
                    }
                }
            }
        }
        guard status == TESSERA_ERROR_OK else {
            throw TesseraConfigEditError(status, message: Self.text(message, messageLength))
        }
        return TesseraAddedProfile(index: index, name: Self.text(name, nameLength))
    }

    /// Imports the colour scheme at `file` (an iTerm2 `.itermcolors` or a
    /// base16 `.yaml`, at most 1 MiB) as a new file in `themes/` and
    /// returns its id, what `appearance.theme` names it by. It is not
    /// selected.
    ///
    /// - Throws: ``TesseraConfigEditError``: `.invalidArgument` for a file
    ///   the importers refuse, `.conflict` when `themes/` has a file of
    ///   that name, `.io` when it cannot be read or the theme written.
    public func importTheme(at file: URL) throws(TesseraConfigEditError) -> String {
        let path = Array(file.path(percentEncoded: false).utf8)
        var id = [UInt8](repeating: 0, count: Int(TESSERA_THEME_ID_LEN))
        var idLength = 0
        var message = [UInt8](repeating: 0, count: Self.messageCapacity)
        var messageLength = 0
        let status = core.call { app in
            path.withUnsafeBufferPointer { path in
                id.withUnsafeMutableBufferPointer { idBuffer in
                    message.withUnsafeMutableBufferPointer { messageBuffer in
                        tessera_theme_import(
                            app, path.baseAddress, path.count, idBuffer.baseAddress, idBuffer.count, &idLength,
                            messageBuffer.baseAddress, messageBuffer.count, &messageLength)
                    }
                }
            }
        }
        guard status == TESSERA_ERROR_OK else {
            throw TesseraConfigEditError(status, message: Self.text(message, messageLength))
        }
        return Self.text(id, idLength)
    }

    private func edit(key: String, json: String?) throws(TesseraConfigEditError) {
        let key = Array(key.utf8)
        let value = json.map { Array($0.utf8) }
        var message = [UInt8](repeating: 0, count: Self.messageCapacity)
        var messageLength = 0
        let status = core.call { app in
            key.withUnsafeBufferPointer { key in
                message.withUnsafeMutableBufferPointer { messageBuffer in
                    if let value {
                        value.withUnsafeBufferPointer { value in
                            tessera_config_set(
                                app, key.baseAddress, key.count, value.baseAddress, value.count,
                                messageBuffer.baseAddress, messageBuffer.count, &messageLength)
                        }
                    } else {
                        tessera_config_remove(
                            app, key.baseAddress, key.count, messageBuffer.baseAddress, messageBuffer.count,
                            &messageLength)
                    }
                }
            }
        }
        guard status == TESSERA_ERROR_OK else {
            throw TesseraConfigEditError(status, message: Self.text(message, messageLength))
        }
    }

    private func edit(
        profile name: String, ordinal: Int, key: String, json: String?
    ) throws(TesseraConfigEditError) {
        guard ordinal >= 0 else {
            throw TesseraConfigEditError(.invalidArgument, message: "No such profile.")
        }
        let name = Array(name.utf8)
        let key = Array(key.utf8)
        let value = json.map { Array($0.utf8) }
        var message = [UInt8](repeating: 0, count: Self.messageCapacity)
        var messageLength = 0
        let status = core.call { app in
            name.withUnsafeBufferPointer { name in
                key.withUnsafeBufferPointer { key in
                    message.withUnsafeMutableBufferPointer { messageBuffer in
                        if let value {
                            value.withUnsafeBufferPointer { value in
                                tessera_config_set_profile(
                                    app, name.baseAddress, name.count, ordinal, key.baseAddress,
                                    key.count, value.baseAddress, value.count, messageBuffer.baseAddress,
                                    messageBuffer.count, &messageLength)
                            }
                        } else {
                            tessera_config_remove_profile(
                                app, name.baseAddress, name.count, ordinal, key.baseAddress,
                                key.count, messageBuffer.baseAddress, messageBuffer.count, &messageLength)
                        }
                    }
                }
            }
        }
        guard status == TESSERA_ERROR_OK else {
            throw TesseraConfigEditError(status, message: Self.text(message, messageLength))
        }
    }

    /// The first `length` bytes of `bytes` as UTF-8 (the core cuts on a
    /// character boundary; a broken sequence would be replaced).
    private static func text(_ bytes: [UInt8], _ length: Int) -> String {
        String(decoding: bytes.prefix(max(0, min(length, bytes.count))), as: UTF8.self)
    }
}

extension TesseraApp {
    /// The configuration in effect, with its generation. Read it when a
    /// settings window opens and again on every
    /// ``TesseraAppDelegate/configurationDidApply(generation:)``.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``;
    ///   ``TesseraCoreError/invalidArgument`` if the core's JSON does not
    ///   decode.
    public func configSnapshot() throws -> TesseraConfigSnapshot {
        let json = try Self.readJSON { buffer, capacity, length in
            tessera_config_json(core.pointer, buffer, capacity, length)
        }
        let document = try TesseraConfigValue.parse(json: Data(json))
        guard let generation = document["generation"]?.integer, generation >= 0, let config = document["config"] else {
            throw TesseraCoreError.invalidArgument
        }
        return TesseraConfigSnapshot(
            generation: UInt64(generation), config: config, loadError: document["load_error"]?.string)
    }

    /// The themes `appearance.theme` can name: the bundled ones, then the
    /// files in `themes/` in name order (at most 256).
    ///
    /// - Throws: as ``configSnapshot()``.
    public func themes() throws -> [TesseraTheme] {
        struct Document: Decodable { var themes: [TesseraTheme] }
        let json = try Self.readJSON { buffer, capacity, length in
            tessera_theme_list(core.pointer, buffer, capacity, length)
        }
        do {
            return try JSONDecoder().decode(Document.self, from: Data(json)).themes
        } catch {
            throw TesseraCoreError.invalidArgument
        }
    }

    /// Edits `config.toml` from any thread (``TesseraConfigWriter``).
    public var configWriter: TesseraConfigWriter {
        TesseraConfigWriter(core: core)
    }
}
