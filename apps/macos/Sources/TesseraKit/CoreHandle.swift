import Foundation
internal import TesseraCore

/// Owns the C `TesseraApp *` and makes `tessera_app_free` safe against
/// `tessera_session_write` calls from other threads.
///
/// The header allows `tessera_session_write` from any thread concurrently with
/// every call except `tessera_app_free`, and a write may block (with no time
/// limit) while its session's input queue is full. So the handle counts the
/// writes in flight per session, and ``free()`` first withdraws the pointer
/// (new writes then see `NULL` and fail with `.nullPointer`), closes the
/// sessions that have a write in flight (which wakes a blocked write), waits
/// for those writes to return, and only then frees the application.
///
/// The Settings window's edits of `config.toml` (`tessera_config_set`,
/// `_remove`, `tessera_theme_import`; ADR 0044) run on a background queue
/// through ``call(_:)``, which ``free()`` waits for in the same way. They
/// never block for long: each reads and writes one small file.
///
/// Every other `tessera_*` call is made on the main thread, which is also
/// where ``free()`` runs, so those calls never race with it; they read
/// ``pointer`` and pass `NULL` after the free, which the core rejects.
final class CoreHandle: @unchecked Sendable {
    // Guards `raw`, `writers` and `calls`; `free()` waits on it for both to
    // drain.
    private let condition = NSCondition()
    private var raw: OpaquePointer?
    // Session id -> number of `tessera_session_write` calls in progress.
    private var writers: [UInt64: Int] = [:]
    // Number of ``call(_:)``s in progress.
    private var calls = 0

    init(_ raw: OpaquePointer) {
        self.raw = raw
    }

    /// The live handle, or `nil` once ``free()`` has started. Main thread only.
    var pointer: OpaquePointer? {
        condition.lock()
        defer { condition.unlock() }
        return raw
    }

    /// Whether ``free()`` has run.
    var isFreed: Bool { pointer == nil }

    /// `tessera_session_write` for any thread.
    func write(session id: UInt64, bytes: UnsafeRawBufferPointer) -> TesseraError {
        condition.lock()
        let app = raw
        if app != nil {
            writers[id, default: 0] += 1
        }
        condition.unlock()
        // A withdrawn handle is passed as NULL so the core reports it.
        guard let app else {
            return tessera_session_write(nil, id, nil, 0)
        }
        let status = tessera_session_write(
            app, id, bytes.baseAddress?.assumingMemoryBound(to: UInt8.self), bytes.count)
        condition.lock()
        if let count = writers[id], count > 1 {
            writers[id] = count - 1
        } else {
            writers[id] = nil
        }
        if writers.isEmpty {
            condition.broadcast()
        }
        condition.unlock()
        return status
    }

    /// Runs `body` with the live handle, or `NULL` once ``free()`` has
    /// started, from any thread; ``free()`` waits until it has returned.
    /// For calls the core allows off the main thread that return on their
    /// own (no blocking on a session), so ``free()`` need not wake them.
    func call<Result>(_ body: (OpaquePointer?) -> Result) -> Result {
        condition.lock()
        let app = raw
        if app != nil {
            calls += 1
        }
        condition.unlock()
        guard app != nil else { return body(nil) }
        defer {
            condition.lock()
            calls -= 1
            if calls == 0 {
                condition.broadcast()
            }
            condition.unlock()
        }
        return body(app)
    }

    /// Frees the application. Main thread only; idempotent. Returns once no
    /// callback runs or can run again (see `tessera_app_free`).
    func free() {
        condition.lock()
        guard let app = raw else {
            condition.unlock()
            return
        }
        raw = nil
        let busy = Array(writers.keys)
        condition.unlock()

        // Closing a session wakes a write blocked on its full input queue; a
        // write that has not reached the core yet then finds no session.
        for id in busy {
            _ = tessera_session_close(app, id)
        }
        condition.lock()
        while !writers.isEmpty || calls > 0 {
            condition.wait()
        }
        condition.unlock()
        tessera_app_free(app)
    }
}
