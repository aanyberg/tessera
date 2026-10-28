internal import TesseraCore

/// A failed call into the Rust core: one case per non-OK `TesseraError` the
/// C header defines.
///
/// The core logs the details of a failure; the code is all the host receives.
/// A code this wrapper does not know (a newer header) is kept as
/// ``unknown(rawValue:)`` instead of trapping.
public enum TesseraCoreError: Error, Hashable, Sendable {
    /// A pointer the core needed was `NULL`. After ``TesseraApp/shutdown()``
    /// every call that needs the application handle fails with this, because
    /// the handle is gone.
    case nullPointer
    /// An argument was out of range or malformed, for example zero rows or
    /// columns, an empty program name, or a string containing a NUL byte.
    case invalidArgument
    /// The session id does not name a live session of this application
    /// (it was closed, or the application was shut down).
    case sessionNotFound
    /// The session could not be started: no PTY, the program does not exist
    /// or is not executable, or a thread could not be created.
    case spawnFailed
    /// An operating-system I/O call failed, for example writing to a session
    /// whose child has exited.
    case io
    /// The core panicked. The panic was caught and logged; the application
    /// stays usable.
    case panic
    /// The renderer failed: it could not be created (no GPU can present to
    /// the layer, or no usable font), the layer's swapchain was lost (the
    /// surface is then detached), or drawing a frame failed.
    case gpu
    /// The session has no surface attached.
    case noSurface
    /// A code this version of the wrapper does not know.
    case unknown(rawValue: UInt32)

    /// Maps a C status to an error, or `nil` for `TESSERA_ERROR_OK`.
    init?(_ status: TesseraError) {
        switch status {
        case TESSERA_ERROR_OK: return nil
        case TESSERA_ERROR_NULL_POINTER: self = .nullPointer
        case TESSERA_ERROR_INVALID_ARGUMENT: self = .invalidArgument
        case TESSERA_ERROR_SESSION_NOT_FOUND: self = .sessionNotFound
        case TESSERA_ERROR_SPAWN_FAILED: self = .spawnFailed
        case TESSERA_ERROR_IO: self = .io
        case TESSERA_ERROR_PANIC: self = .panic
        case TESSERA_ERROR_GPU: self = .gpu
        case TESSERA_ERROR_NO_SURFACE: self = .noSurface
        default: self = .unknown(rawValue: status.rawValue)
        }
    }

    /// Throws the error for a non-OK status.
    static func check(_ status: TesseraError) throws(TesseraCoreError) {
        if let error = TesseraCoreError(status) {
            throw error
        }
    }
}

extension TesseraCoreError: CustomStringConvertible {
    public var description: String {
        switch self {
        case .nullPointer: "the core received a null pointer (is the application shut down?)"
        case .invalidArgument: "invalid argument"
        case .sessionNotFound: "no such session"
        case .spawnFailed: "the session could not be started"
        case .io: "I/O error"
        case .panic: "the core panicked (details are in the log)"
        case .gpu: "the renderer failed (GPU or font)"
        case .noSurface: "the session has no surface"
        case .unknown(let rawValue): "unknown core error \(rawValue)"
        }
    }
}
