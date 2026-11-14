import Foundation
internal import TesseraCore

/// One plugin built into this version of Tessera, as the core's plugin
/// host reports it (ADR 0047): what the Settings window's Plugins pane
/// lists.
///
/// Read with ``TesseraApp/plugins()`` when the pane is shown and again on
/// every ``TesseraAppDelegate/configurationDidApply(generation:)``. The
/// host activates and deactivates plugins on its own thread, so right
/// after an apply a plugin can still read as it was (``isSettling``).
/// Every text is the core's: the manifest's id, name and version are
/// sanitised and capped by the core, and ``message`` is written by the
/// host, never by the plugin; a view still sanitises them (S3).
public struct TesseraPlugin: Sendable, Equatable, Decodable, Identifiable {
    /// Where the plugin stands.
    public enum State: String, Sendable, Equatable {
        /// Running: it gets its events and its status items show.
        case active
        /// Listed in `[plugins] enabled`, but it declares a capability this
        /// version does not grant: never activated.
        case refused
        /// It panicked or its activation failed: no further calls until it
        /// is removed from `[plugins] enabled` and listed again.
        case failed
        /// Not running.
        case disabled
    }

    /// One status bar item the plugin shows (the status bar is #49).
    public struct StatusItem: Sendable, Equatable, Decodable {
        /// The plugin's id for it (at most 64 bytes).
        public let id: String
        /// Its text (at most 256 bytes).
        public let text: String
        /// Its tooltip (at most 1024 bytes).
        public let tooltip: String?
    }

    /// The manifest's id, what `[plugins] enabled` lists
    /// (`dev.tessera.hello`).
    public let id: String
    /// The manifest's name.
    public let name: String
    /// The manifest's version.
    public let version: String
    /// The capabilities the manifest declares, as the core names them
    /// (`sessions`, `status_bar`, `storage`, `output`, `panels`,
    /// `commands`, `fs`, `http`).
    public let capabilities: [String]
    /// Whether `[plugins] enabled` lists it, in the configuration the host
    /// last applied.
    public let enabled: Bool
    public let state: State
    /// Why it is refused or failed (`refused: needs the output capability,
    /// which this version does not grant`, `failed: panicked`); `nil`
    /// otherwise.
    public let message: String?
    /// Its status items; empty unless active.
    public let statusItems: [StatusItem]

    private enum CodingKeys: String, CodingKey {
        case id, name, version, capabilities, enabled, state, message
        case statusItems = "status_items"
    }

    public init(
        id: String, name: String, version: String, capabilities: [String], enabled: Bool, state: State,
        message: String? = nil, statusItems: [StatusItem] = []
    ) {
        self.id = id
        self.name = name
        self.version = version
        self.capabilities = capabilities
        self.enabled = enabled
        self.state = state
        self.message = message
        self.statusItems = statusItems
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decode(String.self, forKey: .name)
        version = try container.decode(String.self, forKey: .version)
        capabilities = try container.decode([String].self, forKey: .capabilities)
        enabled = try container.decode(Bool.self, forKey: .enabled)
        // A state a newer core adds reads as failed: not running, with its
        // message, if any.
        state = State(rawValue: try container.decode(String.self, forKey: .state)) ?? .failed
        message = try container.decodeIfPresent(String.self, forKey: .message)
        statusItems = try container.decode([StatusItem].self, forKey: .statusItems)
    }

    /// Whether the host has yet to start or stop it for the configuration
    /// it last applied: listed and not yet active (or failed), or no longer
    /// listed and still active (or failed). Read the plugins again shortly.
    public var isSettling: Bool {
        enabled ? state == .disabled : state == .active || state == .failed
    }

    /// Decodes the core's JSON (`tessera_app_plugins_json`).
    static func decode(_ json: [UInt8]) throws -> [TesseraPlugin] {
        try JSONDecoder().decode([TesseraPlugin].self, from: Data(json))
    }
}

extension TesseraApp {
    /// Every plugin built into this version, in a fixed order, with its
    /// state (ADR 0047). Never waits for a plugin.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``;
    ///   ``TesseraCoreError/invalidArgument`` if the core's JSON does not
    ///   decode.
    public func plugins() throws -> [TesseraPlugin] {
        let json = try Self.readJSON { buffer, capacity, length in
            tessera_app_plugins_json(core.pointer, buffer, capacity, length)
        }
        do {
            return try TesseraPlugin.decode(json)
        } catch {
            throw TesseraCoreError.invalidArgument
        }
    }
}
