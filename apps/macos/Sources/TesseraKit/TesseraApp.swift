import AppKit
internal import TesseraCore

/// Receives a ``TesseraApp``'s session events, always on the main actor.
///
/// Events are coalesced per session, so a program that floods output, bells
/// or titles cannot flood the main actor: at most one delivery per session is
/// queued at a time, and it reports everything since the previous one, in this
/// order: ``sessionDidOutput(_:)`` once if there was any output (all of it is
/// applied by then), ``session(_:didSetTitle:)`` once with the latest title,
/// ``sessionDidRingBell(_:)`` once however many bells rang, and finally
/// ``session(_:didExitWithCode:signal:)``, which always comes last. Deliveries
/// keep their order; the interleaving of events within one delivery is not
/// kept. An event is dropped, not queued, if its session has been closed, if
/// ``TesseraApp/shutdown()`` has run, or if no delegate is set when it is
/// delivered (the exit included).
@MainActor
public protocol TesseraAppDelegate: AnyObject {
    /// The session wrote output and the terminal has applied it; schedule a
    /// redraw. Bursts of output are coalesced into one call.
    func sessionDidOutput(_ session: TesseraSession)
    /// The session's program set its window or tab title. Several titles set
    /// between two deliveries are reported once, with the latest.
    func session(_ session: TesseraSession, didSetTitle title: String)
    /// The session's program rang the bell, once or more since the previous
    /// delivery.
    func sessionDidRingBell(_ session: TesseraSession)
    /// The session's child exited: `code` is its exit code, or -1 when it was
    /// killed by a signal; `signal` is that signal, or 0. The last event for the
    /// session; writes now fail, and the host should ``TesseraSession/close()``
    /// it. Not sent for a session the host closed.
    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32)
    /// The core applied an edit of the configuration or of the theme in use to
    /// every view (colours, opacity, font size or the default cursor style).
    /// `opaque` is whether the background is now fully opaque. Make every
    /// terminal window opaque or translucent to match and redraw every view:
    /// each surface switches its compositing mode at its next frame. Several
    /// changes between two deliveries are reported once, with the latest
    /// `opaque`. Not sent after ``TesseraApp/shutdown()``.
    func appearanceDidChange(opaque: Bool)
    /// The core applied a configuration (the user saved `config.toml`, or a
    /// theme it names): read ``TesseraApp/keymap()`` again and give the
    /// menus its key equivalents. `generation` counts the configurations
    /// applied so far. Several between two deliveries are reported once,
    /// with the latest. Not sent after ``TesseraApp/shutdown()``. The
    /// default does nothing.
    func configurationDidApply(generation: UInt64)
}

extension TesseraAppDelegate {
    public func configurationDidApply(generation: UInt64) {}
}

/// The Rust core's application: its configuration and every session it runs.
///
/// Main actor only, like the C handle it wraps. Call ``shutdown()`` before
/// dropping the last reference; it closes every session and frees the core.
@MainActor
public final class TesseraApp {
    /// The version of the linked Rust core. Callable from any thread.
    public nonisolated static var coreVersion: String {
        String(cString: tessera_version())
    }

    /// Path of the main configuration file (`config.toml`).
    public let configPath: URL

    /// Receives session events. Held weakly. Events delivered while it is
    /// `nil`, the exit included, are dropped, not queued.
    public var delegate: (any TesseraAppDelegate)? {
        get { bridge.delegate }
        set { bridge.delegate = newValue }
    }

    /// Where programs' clipboard writes (OSC 52) go: the general pasteboard.
    /// Written for an open session whether or not a ``delegate`` is set.
    var pasteboard: NSPasteboard {
        get { bridge.pasteboard }
        set { bridge.pasteboard = newValue }
    }

    private let core: CoreHandle
    // `user_data` of the installed callbacks (passed unretained): must stay
    // alive until `tessera_app_free` has returned.
    private let bridge: CallbackBridge

    /// Creates the core application and no sessions.
    ///
    /// The core reads `config.toml` (and the theme it names) from
    /// `~/Library/Application Support/Tessera`, or from the directory in the
    /// `TESSERA_CONFIG_DIR` environment variable, and applies every later
    /// edit of it live (see ``TesseraAppDelegate/appearanceDidChange(opaque:)``).
    /// A missing or broken file gives the defaults rather than an error.
    /// Creating the app writes nothing; ``createConfig()`` creates the file.
    ///
    /// - Throws: ``TesseraCoreError/io`` if the home directory cannot be
    ///   determined.
    public init() throws {
        var raw: OpaquePointer?
        try TesseraCoreError.check(tessera_app_new(&raw))
        guard let raw else { throw TesseraCoreError.nullPointer }
        let core = CoreHandle(raw)
        let bridge = CallbackBridge()
        do {
            var callbacks = bridge.callbacks()
            try TesseraCoreError.check(tessera_app_set_callbacks(raw, &callbacks))
            configPath = try Self.readConfigPath(raw)
            // The registry is fixed for the core's lifetime: only the chords
            // change with the configuration.
            bridge.commandNames = try TesseraKeymap.read(raw).commands.map(\.command)
        } catch {
            core.free()
            throw error
        }
        self.core = core
        self.bridge = bridge
    }

    deinit {
        // Last resort only: `shutdown()` is the contract. This deinit is
        // nonisolated (Swift 6.2's opt-in `isolated deinit` needs a newer
        // toolchain than CI's 6.1) and may run off the main thread, so free on
        // the main thread, keeping the bridge alive until then.
        guard !core.isFreed else { return }
        assertionFailure("TesseraApp released without shutdown()")
        if Thread.isMainThread {
            core.free()
            MainActor.assumeIsolated { bridge.shutDown() }
        } else {
            Task { @MainActor [core, bridge] in
                core.free()
                bridge.shutDown()
            }
        }
    }

    /// Starts a session.
    ///
    /// Its events may be emitted before this returns; they are delivered to the
    /// delegate afterwards, once the returned session is known.
    ///
    /// - Throws: ``TesseraCoreError/invalidArgument`` for zero rows or columns,
    ///   more than `TESSERA_MAX_ROWS` (1024) rows or `TESSERA_MAX_COLS` (2048)
    ///   columns, an empty program, a string with a NUL byte or a working
    ///   directory that is not a file URL;
    ///   ``TesseraCoreError/spawnFailed`` if the program cannot be started,
    ///   including when its arguments and the environment together exceed
    ///   the system's `ARG_MAX` (1 MiB on macOS, less a tenth).
    public func spawn(_ options: TesseraSession.SpawnOptions = .init()) throws -> TesseraSession {
        let app = core.pointer
        var id: UInt64 = 0
        let status = try options.withCOptions { cOptions in
            tessera_session_spawn(app, &cOptions, &id)
        }
        try TesseraCoreError.check(status)
        let session = TesseraSession(id: id, core: core, bridge: bridge)
        bridge.register(session)
        return session
    }

    /// Creates the configuration directory and a commented `config.toml` with
    /// the defaults where they are missing, and starts applying edits of them
    /// live. Call it before opening ``configPath`` for editing (⌘,,
    /// Preferences). Idempotent: an existing file is never changed.
    ///
    /// - Throws: ``TesseraCoreError/io`` if the directory or the file cannot
    ///   be created.
    public func createConfig() throws {
        try TesseraCoreError.check(tessera_app_create_config(core.pointer))
    }

    /// The key map in effect: the default chords with `config.toml`'s
    /// `[keys]` laid over them, for the menus' key equivalents.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``; a
    ///   decoding error if the core's JSON does not match this wrapper.
    public func keymap() throws -> TesseraKeymap {
        try TesseraKeymap.read(core.pointer)
    }

    /// The names of the configured profiles (`[[profiles]]` in
    /// `config.toml`), in file order, as written: what ``TesseraSession/SpawnOptions/profile``
    /// takes. Not sanitised; a menu shows them only after removing control
    /// and bidirectional formatting characters. Read again after a
    /// configuration is applied.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``, or
    ///   ``TesseraCoreError/invalidArgument`` if the configuration could not
    ///   be read or decoded.
    public func profileNames() throws -> [String] {
        struct Document: Decodable {
            struct Config: Decodable {
                struct Profile: Decodable { var name: String }
                var profiles: [Profile]
            }
            var config: Config
        }
        let json = try Self.readJSON { buffer, capacity, length in
            tessera_config_json(core.pointer, buffer, capacity, length)
        }
        do {
            return try JSONDecoder().decode(Document.self, from: Data(json)).config.profiles.map(\.name)
        } catch {
            throw TesseraCoreError.invalidArgument
        }
    }

    /// Reads a JSON buffer from a core call that reports its length and
    /// refuses a short buffer with `TESSERA_ERROR_INVALID_ARGUMENT`: sized
    /// by a probe, retried if it grew in between.
    private static func readJSON(
        _ call: (UnsafeMutablePointer<UInt8>?, Int, UnsafeMutablePointer<Int>) -> TesseraError
    ) throws -> [UInt8] {
        var length = 0
        let probe = call(nil, 0, &length)
        if probe != TESSERA_ERROR_INVALID_ARGUMENT {
            try TesseraCoreError.check(probe)
        }
        for _ in 0..<3 {
            var buffer = [UInt8](repeating: 0, count: length + 4096)
            let capacity = buffer.count
            let status = buffer.withUnsafeMutableBufferPointer { call($0.baseAddress, capacity, &length) }
            if status == TESSERA_ERROR_INVALID_ARGUMENT { continue }
            try TesseraCoreError.check(status)
            return Array(buffer.prefix(length))
        }
        throw TesseraCoreError.invalidArgument
    }

    /// `[appearance] dim_inactive_panes` in effect: how much to darken the
    /// panes of a split tab that do not have the focus, 0 (off) to 0.8,
    /// clamped by the core. Read again after a configuration is applied.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``.
    public func dimInactivePanes() throws -> Float {
        var amount: Float = 0
        try TesseraCoreError.check(tessera_app_dim_inactive_panes(core.pointer, &amount))
        return amount
    }

    /// Closes every session and frees the core. Idempotent; afterwards every
    /// call that needs the core throws ``TesseraCoreError/nullPointer``.
    ///
    /// Blocks until the children have exited. Sessions with a
    /// ``TesseraSession/write(_:)`` in progress on another thread are closed
    /// first, which wakes the write and makes it fail; each of those closes
    /// returns at once. Freeing the core then waits for every close, these and
    /// earlier ``TesseraSession/close()`` calls still reaping in the
    /// background: at most about two seconds, however many children ignore
    /// `SIGHUP`, because their grace periods run in parallel.
    public func shutdown() {
        // Both steps are idempotent. Waits for in-flight callbacks; `user_data` is unused once it returns.
        core.free()
        bridge.shutDown()
    }

    private static func readConfigPath(_ app: OpaquePointer) throws -> URL {
        var length = 0
        try TesseraCoreError.check(tessera_app_config_path(app, nil, 0, &length))
        var buffer = [CChar](repeating: 0, count: length + 1)
        try TesseraCoreError.check(tessera_app_config_path(app, &buffer, buffer.count, nil))
        return buffer.withUnsafeBufferPointer {
            URL(fileURLWithFileSystemRepresentation: $0.baseAddress!, isDirectory: false, relativeTo: nil)
        }
    }
}
