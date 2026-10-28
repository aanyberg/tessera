import Foundation
internal import TesseraCore

/// One terminal session: a child process in its own PTY, run by a
/// ``TesseraApp``.
///
/// Created by ``TesseraApp/spawn(_:)``. The application keeps the session
/// until ``close()`` or ``TesseraApp/shutdown()``; when the child exits the
/// delegate hears ``TesseraAppDelegate/session(_:didExitWithCode:signal:)``
/// and the session stays known (writes fail) until it is closed.
///
/// ``write(_:)`` may be called from any thread. Every other method runs on the
/// main actor, mirroring the C header's thread contract.
public final class TesseraSession: Sendable, Identifiable {
    /// The core's id for this session: unique within its application, never 0.
    public let id: UInt64

    let core: CoreHandle
    // Strong while the session is registered; the bridge drops its sessions
    // on close and shutdown, which breaks the cycle.
    private let bridge: CallbackBridge

    init(id: UInt64, core: CoreHandle, bridge: CallbackBridge) {
        self.id = id
        self.core = core
        self.bridge = bridge
    }

    /// Queues `bytes` as input for the session's child. Callable from any
    /// thread, concurrently with the other calls.
    ///
    /// Blocks while the session's 1 MiB input queue is full because the child
    /// is not reading, until it reads or the session is closed, so write large
    /// pastes from a background thread.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once the session is
    ///   closed, ``TesseraCoreError/io`` after the child exited or when the
    ///   write was blocked as ``close()`` or ``TesseraApp/shutdown()`` ran,
    ///   ``TesseraCoreError/nullPointer`` after the application shut down.
    public func write(_ bytes: some DataProtocol) throws {
        let status: TesseraError
        if let contiguous = bytes.withContiguousStorageIfAvailable({
            core.write(session: id, bytes: UnsafeRawBufferPointer($0))
        }) {
            status = contiguous
        } else {
            status = Array(bytes).withUnsafeBytes { core.write(session: id, bytes: $0) }
        }
        try TesseraCoreError.check(status)
    }

    /// Tells the child its window is now `rows` x `cols` cells and
    /// `widthPx` x `heightPx` pixels (0 when unknown); it receives `SIGWINCH`.
    ///
    /// - Throws: ``TesseraCoreError/invalidArgument`` for zero rows or columns.
    @MainActor
    public func resize(rows: UInt16, cols: UInt16, widthPx: UInt16 = 0, heightPx: UInt16 = 0) throws {
        try TesseraCoreError.check(
            tessera_session_resize(core.pointer, id, rows, cols, widthPx, heightPx))
    }

    /// Discards the session's scrollback; the visible screen is kept.
    @MainActor
    public func clearScrollback() throws {
        try TesseraCoreError.check(tessera_session_clear_scrollback(core.pointer, id))
    }

    /// Hangs up the child (killing it if it does not exit within a second) and
    /// forgets the session. No delegate call for it follows, not even the exit.
    /// Takes at most about two seconds.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` if it was already closed.
    @MainActor
    public func close() throws {
        // Only success means the core forgot the id. On any other error (a
        // caught panic, say) the session may still be live and emitting, so it
        // stays registered and its events keep reaching the delegate; after
        // shutdown the bridge has already dropped every session.
        try TesseraCoreError.check(tessera_session_close(core.pointer, id))
        bridge.unregister(id)
    }
}
