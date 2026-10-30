import AppKit
import Synchronization
internal import TesseraCore

/// Receives the core's callbacks on its reader threads and delivers them to the
/// ``TesseraAppDelegate`` on the main actor.
///
/// The bridge is the `user_data` of the installed `TesseraCallbacks`, passed
/// unretained: ``TesseraApp`` keeps it alive until `tessera_app_free` has
/// returned, after which the core never uses `user_data` again.
///
/// Each C callback copies its arguments (the title string before it returns),
/// then hops to the main actor with an unstructured `Task`. It never waits for
/// the main thread: closing a session and freeing the application wait for a
/// running callback, so a synchronous hop would deadlock.
///
/// Events are coalesced per session, because the core calls back once per PTY
/// read and once per BEL byte, which a program can emit far faster than any
/// delegate runs (`yes $'\a'`): one `Task` per event would grow the main
/// queue without bound. Each callback records its event in the session's
/// pending set (output: a flag; title and clipboard text: the latest value;
/// bell: a flag; exit: the code and signal) and schedules a delivery only if
/// none is queued for that session. The delivery takes the whole set and
/// reports it in a fixed order: output, title, clipboard, bell, exit. The
/// clipboard text (OSC 52) goes to ``pasteboard``, not to the delegate. So
/// at most one delivery per session is queued at any time; a delivery
/// reports what happened since the previous one; and the exit, the core's
/// last callback for a session, is always delivered last.
///
/// `on_appearance_changed` (from the core's configuration watch thread) is
/// coalesced the same way, app-wide: at most one delivery is queued, and it
/// reports the latest `opaque`.
@MainActor
final class CallbackBridge {
    weak var delegate: (any TesseraAppDelegate)?
    /// Where programs' clipboard writes go; tests inject a named pasteboard.
    var pasteboard = NSPasteboard.general

    /// Sessions this application owns, by id: open sessions and exited ones
    /// not yet closed.
    private(set) var sessions: [UInt64: TesseraSession] = [:]
    private var isShutDown = false

    /// Events received and not yet delivered, by session. A session has an
    /// entry exactly while a delivery for it is queued on the main actor.
    private nonisolated let pending = Mutex<[UInt64: Pending]>([:])
    /// The latest `opaque` received and not yet delivered; non-`nil` exactly
    /// while an appearance delivery is queued.
    private nonisolated let pendingAppearance = Mutex<Bool?>(nil)
    /// Appearance deliveries scheduled so far (tests: a burst schedules one).
    nonisolated let scheduledAppearanceDeliveries = Mutex<Int>(0)

    func register(_ session: TesseraSession) {
        guard !isShutDown else { return }
        sessions[session.id] = session
    }

    func unregister(_ id: UInt64) {
        sessions[id] = nil
    }

    /// Called after `tessera_app_free` returned: drops the sessions (breaking
    /// their reference back to the bridge). Deliveries already queued still
    /// run; each takes its pending record and drops it because `isShutDown` is
    /// set.
    func shutDown() {
        isShutDown = true
        sessions.removeAll()
    }

    /// The C callback table for this bridge. The function pointers capture
    /// nothing; they find the bridge through `user_data`.
    nonisolated func callbacks() -> TesseraCallbacks {
        TesseraCallbacks(
            user_data: Unmanaged.passUnretained(self).toOpaque(),
            on_session_output: { userData, id in
                CallbackBridge.from(userData)?.receive(for: id) { $0.output = true }
            },
            on_session_title: { userData, id, title in
                // `title` is valid only during this call: copy it now.
                let copy = title.map { String(cString: $0) } ?? ""
                CallbackBridge.from(userData)?.receive(for: id) { $0.title = copy }
            },
            on_session_bell: { userData, id in
                CallbackBridge.from(userData)?.receive(for: id) { $0.bell = true }
            },
            on_session_exit: { userData, id, code, signal in
                CallbackBridge.from(userData)?.receive(for: id) { $0.exit = (code, signal) }
            },
            on_appearance_changed: { userData, opaque in
                CallbackBridge.from(userData)?.receiveAppearance(opaque: opaque)
            },
            on_session_clipboard: { userData, id, text, len in
                // `text` (UTF-8, may hold NUL) is valid only during this call.
                // Empty text never touches the clipboard (ADR 0022).
                guard let text, len > 0 else { return }
                let copy = String(decoding: UnsafeBufferPointer(start: text, count: len), as: UTF8.self)
                CallbackBridge.from(userData)?.receive(for: id) { $0.clipboard = copy }
            }
        )
    }

    private nonisolated static func from(_ userData: UnsafeMutableRawPointer?) -> CallbackBridge? {
        guard let userData else { return nil }
        return Unmanaged<CallbackBridge>.fromOpaque(userData).takeUnretainedValue()
    }

    // MARK: Reader threads

    /// What one session did since its last delivery.
    private struct Pending: Sendable {
        var output = false
        var title: String?
        var clipboard: String?
        var bell = false
        var exit: (code: Int32, signal: Int32)?
    }

    private nonisolated func receive(for id: UInt64, _ record: (inout Pending) -> Void) {
        let schedule = pending.withLock { pending in
            var isNew = false
            if pending[id] == nil {
                pending[id] = Pending()
                isNew = true
            }
            record(&pending[id]!)
            return isNew
        }
        guard schedule else { return }
        Task { @MainActor in
            self.deliver(id)
        }
    }

    private nonisolated func receiveAppearance(opaque: Bool) {
        let schedule = pendingAppearance.withLock { pending in
            defer { pending = opaque }
            return pending == nil
        }
        guard schedule else { return }
        scheduledAppearanceDeliveries.withLock { $0 += 1 }
        Task { @MainActor in
            self.deliverAppearance()
        }
    }

    // MARK: Main actor

    private func deliverAppearance() {
        guard let opaque = pendingAppearance.withLock({ pending in
            defer { pending = nil }
            return pending
        }) else { return }
        guard !isShutDown, let delegate else { return }
        delegate.appearanceDidChange(opaque: opaque)
    }

    private func deliver(_ id: UInt64) {
        // Taken before the delegate runs, so events arriving meanwhile
        // schedule the next delivery.
        guard let events = pending.withLock({ $0.removeValue(forKey: id) }) else { return }
        // The delegate may close the session, shut the app down or replace
        // itself in any call, so look both up again before each one.
        func target() -> (any TesseraAppDelegate, TesseraSession)? {
            guard !isShutDown, let session = sessions[id], let delegate else { return nil }
            return (delegate, session)
        }
        if events.output, let (delegate, session) = target() {
            delegate.sessionDidOutput(session)
        }
        if let title = events.title, let (delegate, session) = target() {
            delegate.session(session, didSetTitle: title)
        }
        if let text = events.clipboard, !isShutDown, sessions[id] != nil {
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
        }
        if events.bell, let (delegate, session) = target() {
            delegate.sessionDidRingBell(session)
        }
        if let exit = events.exit, let (delegate, session) = target() {
            delegate.session(session, didExitWithCode: exit.code, signal: exit.signal)
        }
    }
}
