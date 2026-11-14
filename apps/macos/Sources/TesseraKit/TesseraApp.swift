import AppKit
internal import TesseraCore

/// Receives a ``TesseraApp``'s session events, always on the main actor.
///
/// Events are coalesced per session, so a program that floods output, bells
/// or titles cannot flood the main actor: at most one delivery per session is
/// queued at a time, and it reports everything since the previous one, in this
/// order: ``sessionDidOutput(_:)`` once if there was any output (all of it is
/// applied by then), ``session(_:didSetTitle:)`` once with the latest title,
/// ``session(_:didChangeRemote:)`` once with the latest remote state,
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
    /// every view (colours, opacity, blur, font size or the default cursor
    /// style), or new ``TesseraApp/DisplayOptions`` that changed how a view is
    /// drawn. `opaque` is whether every view's background is now fully opaque.
    /// Read each session's ``TesseraSession/appearance()`` again, make each
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
    /// Where the session's shell runs changed (#205): the core decided it from
    /// the shell's working-directory reports (OSC 7) and, with none naming a
    /// host, from `ssh` in the foreground. Reported only on a change; a new
    /// session starts ``TesseraRemote/local``. Several changes between two
    /// deliveries are reported once, with the latest. The default does nothing.
    func session(_ session: TesseraSession, didChangeRemote remote: TesseraRemote)
}

extension TesseraAppDelegate {
    public func configurationDidApply(generation: UInt64) {}
    public func session(_ session: TesseraSession, didChangeRemote remote: TesseraRemote) {}
}

/// Where a session's shell runs, as the core reports it
/// (``TesseraAppDelegate/session(_:didChangeRemote:)``). A display hint: the
/// child can claim any host.
public enum TesseraRemote: Sendable, Hashable {
    /// On this Mac, or nothing says otherwise.
    case local
    /// On the machine its last working-directory report named. The core
    /// removed control, bidirectional-formatting and line-separator
    /// characters; never empty, at most 253 bytes.
    case host(String)
    /// On another machine whose name is not known: `ssh` runs in the
    /// foreground and no report named a host (or the one that did could not
    /// be shown).
    case unnamed
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

    // Internal for the configuration wrapper (TesseraConfig.swift).
    let core: CoreHandle
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
    static func readJSON(
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
    /// clamped by the core, and 0 while
    /// ``DisplayOptions/increaseContrast`` is on (ADR 0041). Read again after
    /// a configuration is applied and after ``setDisplayOptions(_:)``.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``.
    public func dimInactivePanes() throws -> Float {
        var amount: Float = 0
        try TesseraCoreError.check(tessera_app_dim_inactive_panes(core.pointer, &amount))
        return amount
    }

    /// `[appearance] blur_radius` in effect (ADR 0045): the window's blur
    /// radius, 0 to 64, clamped by the core. 0 keeps each pane's own
    /// material blur; above 0 a window whose translucent panes all ask for
    /// blur is blurred whole at that radius. It does not change with
    /// ``DisplayOptions/reduceTransparency``: the caller sets no radius then.
    /// Read again after a configuration is applied.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``.
    public func blurRadius() throws -> UInt32 {
        var radius: UInt32 = 0
        try TesseraCoreError.check(tessera_app_blur_radius(core.pointer, &radius))
        return radius
    }

    /// `[appearance] app_appearance` (#365): how the app draws its own
    /// windows, menus, sheets and Settings. The terminal's colours stay the
    /// theme's.
    public enum AppAppearance: Sendable, Equatable {
        /// Dark whatever the system's setting; the default.
        case dark
        /// Light whatever the system's setting.
        case light
        /// As the system's Light or Dark setting, following it live.
        case system
    }

    /// `[appearance] app_appearance` in effect: ``AppAppearance/dark`` with
    /// no key and for an unknown value (the core warns). App-wide, so no
    /// profile and no display option changes it. No callback reports a
    /// change: read it at start and after every configuration is applied.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``.
    public func appAppearance() throws -> AppAppearance {
        var raw: UInt32 = 0
        try TesseraCoreError.check(tessera_app_app_appearance(core.pointer, &raw))
        switch TesseraAppAppearance(rawValue: raw) {
        case TESSERA_APP_APPEARANCE_LIGHT: return .light
        case TESSERA_APP_APPEARANCE_SYSTEM: return .system
        default: return .dark
        }
    }

    /// The system's display settings that change how every session is drawn
    /// (ADR 0043). A setting added later defaults to off.
    public struct DisplayOptions: Sendable, Equatable {
        /// The system asks for less transparency (Accessibility, Display,
        /// Reduce transparency): every background is drawn opaque and none is
        /// blurred, whatever the configuration asks for.
        public var reduceTransparency: Bool
        /// The system asks for more contrast (Accessibility, Display,
        /// Increase contrast): the cursor and the selection are drawn opaque
        /// and ``TesseraApp/dimInactivePanes()`` reports 0, whatever the theme
        /// and configuration ask for (ADR 0041).
        public var increaseContrast: Bool

        public init(reduceTransparency: Bool = false, increaseContrast: Bool = false) {
            self.reduceTransparency = reduceTransparency
            self.increaseContrast = increaseContrast
        }
    }

    /// Applies `options` to every session, open and future, until the next
    /// call; they survive configuration reloads. Call it at start and when the
    /// system setting changes. A call that changes nothing does nothing;
    /// otherwise ``TesseraAppDelegate/appearanceDidChange(opaque:)`` follows
    /// when a view's opacity or blur changed, and ``dimInactivePanes()`` holds
    /// the new amount.
    ///
    /// - Throws: ``TesseraCoreError/nullPointer`` after ``shutdown()``.
    public func setDisplayOptions(_ options: DisplayOptions) throws {
        var raw = TesseraDisplayOptions()
        raw.reduce_transparency = options.reduceTransparency ? 1 : 0
        raw.increase_contrast = options.increaseContrast ? 1 : 0
        try TesseraCoreError.check(tessera_app_set_display_options(core.pointer, &raw))
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
