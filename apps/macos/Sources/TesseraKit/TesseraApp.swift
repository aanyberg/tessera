import Foundation
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

    private let core: CoreHandle
    // `user_data` of the installed callbacks (passed unretained): must stay
    // alive until `tessera_app_free` has returned.
    private let bridge: CallbackBridge

    /// Creates the core application with the default configuration and no
    /// sessions.
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
    ///   an empty program, a string with a NUL byte or a working directory
    ///   that is not a file URL;
    ///   ``TesseraCoreError/spawnFailed`` if the program cannot be started.
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

    /// Closes every session and frees the core. Idempotent; afterwards every
    /// call that needs the core throws ``TesseraCoreError/nullPointer``.
    ///
    /// Blocks until the children have exited. Sessions with a
    /// ``TesseraSession/write(_:)`` in progress on another thread are closed
    /// first, one after another, which wakes the write and makes it fail; each
    /// session with a write in flight can take up to about two seconds (a child
    /// that ignores `SIGHUP`, or a cooked-mode child with a write blocked into
    /// it, waits out the hangup and kill grace). Freeing the core then takes at
    /// most about two seconds more, however many of the remaining children
    /// ignore `SIGHUP`.
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
