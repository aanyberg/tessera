/// When programs' clipboard writes (OSC 52) reach the pasteboard: at most
/// ``maxWritesPerSecond`` writes per second, app-wide (ADR 0022 D-RATE52).
///
/// Every pasteboard write bumps `NSPasteboard.changeCount`, which
/// clipboard-history apps observe, and a `printf` loop makes thousands a
/// second. A write at least ``minimumInterval`` after the previous
/// pasteboard write goes out at once, so a single yank has no added
/// latency. A write within the interval is held, not dropped: one held
/// write for the whole application, the latest from any session replacing
/// the one before, released when the interval since the previous write has
/// passed. So the pasteboard ends up with the last write any program made:
/// a session that closes with its write held has it written at the close
/// (``takeHeld(from:at:)``), at most one extra write per closing session.
/// Two are dropped instead, and the pasteboard keeps what was written
/// before: a write that reaches ``CallbackBridge`` after its session closed
/// (its delivery was still queued when the tab closed), and the write held
/// at shutdown.
///
/// Memory: the held write is one string, at most the core's 1 MiB OSC 52
/// payload cap (ADR 0022).
///
/// A pure value driven by explicit instants: ``CallbackBridge`` owns one,
/// does the pasteboard writes and runs the timer that calls
/// ``release(at:)`` at ``dueAt``.
struct ClipboardThrottle {
    static let maxWritesPerSecond = 10
    /// The shortest time between two pasteboard writes.
    static let minimumInterval: Duration = .seconds(1) / maxWritesPerSecond

    /// A program's clipboard write: the session it came from and its text.
    struct Write: Equatable {
        var session: UInt64
        var text: String
    }

    /// The write waiting for the interval to pass, or `nil`.
    private(set) var held: Write?
    /// When the previous write went out, or `nil` before the first.
    private var lastWrite: ContinuousClock.Instant?

    /// When the held write falls due, or `nil` if none is held.
    var dueAt: ContinuousClock.Instant? {
        guard held != nil else { return nil }
        return lastWrite.map { $0 + Self.minimumInterval }
    }

    /// Offers `write` at `now`. Returns it if it goes to the pasteboard now;
    /// otherwise it is held, replacing any held write, until ``dueAt``.
    mutating func offer(_ write: Write, at now: ContinuousClock.Instant) -> Write? {
        if let lastWrite, now - lastWrite < Self.minimumInterval {
            held = write
            return nil
        }
        // The interval since the previous pasteboard write has passed; a
        // held write (its release came late) is superseded by this newer
        // one.
        held = nil
        lastWrite = now
        return write
    }

    /// Takes the held write if it is due at `now`: the write to make now.
    mutating func release(at now: ContinuousClock.Instant) -> Write? {
        guard let due = dueAt, now >= due, let write = held else { return nil }
        held = nil
        lastWrite = now
        return write
    }

    /// Takes the held write if it came from `session` (closing), as a write
    /// made at `now`; a held write from another session stays held.
    mutating func takeHeld(from session: UInt64, at now: ContinuousClock.Instant) -> Write? {
        guard let write = held, write.session == session else { return nil }
        held = nil
        lastWrite = now
        return write
    }

    /// Forgets the held write (the application shut down).
    mutating func dropHeld() {
        held = nil
    }
}
