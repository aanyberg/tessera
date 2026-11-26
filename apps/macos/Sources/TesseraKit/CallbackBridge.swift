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
/// pending set (output: a flag; title, remote state and clipboard text: the
/// latest value; bell: a flag; command finished: the longest, the latest of equals, so a
/// long command is not hidden by a short one after it (ADR 0048); exit: the code
/// and signal) and schedules a delivery only if none is queued for that
/// session. The delivery takes the whole set and reports it in a fixed
/// order: output, title, remote state, clipboard, bell, command finished,
/// exit. The
/// clipboard text (OSC 52) goes to ``pasteboard``, not to the delegate,
/// through one app-wide ``ClipboardThrottle`` (at most 10 writes a second,
/// the latest held; ADR 0022 D-RATE52). So at most one delivery per session
/// is queued at any time; a delivery reports what happened since the
/// previous one; and the exit, the core's last callback for a session, is
/// always delivered last.
///
/// `on_appearance_changed` (from the core's configuration watch thread) is
/// coalesced the same way, app-wide: at most one delivery is queued, and it
/// reports the latest `opaque`. `on_config_applied` (same thread) likewise
/// reports the latest generation, and `on_config_invalid` (same thread) is a
/// flag. `on_font_index_ready` (the core's font index thread, once per font
/// database the core loads) is a flag too (#594).
@MainActor
final class CallbackBridge {
    weak var delegate: (any TesseraAppDelegate)?
    /// Where programs' clipboard writes go; tests inject a named pasteboard.
    var pasteboard = NSPasteboard.general

    /// Sessions this application owns, by id: open sessions and exited ones
    /// not yet closed.
    private(set) var sessions: [UInt64: TesseraSession] = [:]
    private var isShutDown = false
    /// Spaces the pasteboard writes of every session (one pasteboard).
    private(set) var clipboardThrottle = ClipboardThrottle()
    /// Writes the held clipboard text when it falls due; at most one runs.
    /// It holds the bridge weakly, and ``shutDown()`` cancels it.
    private var clipboardRelease: Task<Void, Never>?
    /// Whether a clipboard release is scheduled (tests).
    var isClipboardReleaseScheduled: Bool { clipboardRelease != nil }

    /// Events received and not yet delivered, by session. A session has an
    /// entry exactly while a delivery for it is queued on the main actor.
    private nonisolated let pending = Mutex<[UInt64: Pending]>([:])
    /// The latest `opaque` received and not yet delivered; non-`nil` exactly
    /// while an appearance delivery is queued.
    private nonisolated let pendingAppearance = Mutex<Bool?>(nil)
    /// Appearance deliveries scheduled so far (tests: a burst schedules one).
    nonisolated let scheduledAppearanceDeliveries = Mutex<Int>(0)
    /// The latest generation `on_config_applied` reported and not yet
    /// delivered; non-`nil` exactly while a delivery is queued.
    private nonisolated let pendingConfiguration = Mutex<UInt64?>(nil)
    /// Configuration deliveries scheduled so far (tests: a burst schedules one).
    nonisolated let scheduledConfigurationDeliveries = Mutex<Int>(0)
    /// Whether `on_config_invalid` was received and not yet delivered;
    /// `true` exactly while a delivery is queued.
    private nonisolated let pendingInvalid = Mutex<Bool>(false)
    /// Load-failure deliveries scheduled so far (tests: a burst schedules one).
    nonisolated let scheduledInvalidDeliveries = Mutex<Int>(0)
    /// Whether `on_font_index_ready` was received and not yet delivered;
    /// `true` exactly while a delivery is queued.
    private nonisolated let pendingFontIndex = Mutex<Bool>(false)
    /// The registry's command names, by id (``TesseraKeymap/Command/command``),
    /// read once at start: the registry never changes while the core runs.
    var commandNames: [String] = []

    func register(_ session: TesseraSession) {
        guard !isShutDown else { return }
        sessions[session.id] = session
        guard let events = parked.removeValue(forKey: session.id) else { return }
        let schedule = pending.withLock { pending in
            let isNew = pending[session.id] == nil
            pending[session.id] = events.merged(with: pending[session.id])
            return isNew
        }
        guard schedule else { return }
        Task { @MainActor in
            self.deliver(session.id)
        }
    }

    /// Spawns running in the background (``TesseraApp/spawnInBackground(_:completion:)``),
    /// not yet returned: while any does, a delivery for a session not yet
    /// registered is held (``parked``) for its registration, since the
    /// session's first events may come before its spawn returns (#722).
    var spawnsInFlight = 0 {
        didSet {
            // A held delivery left once no spawn runs belongs to no session
            // that will register (a closed one's last events).
            if spawnsInFlight == 0 { parked.removeAll() }
        }
    }
    /// Deliveries held for sessions whose spawn has not returned, by id: at
    /// most one record per session that a spawn in flight started, plus a
    /// closed session's last events, until no spawn runs.
    private var parked: [UInt64: Pending] = [:]

    /// How many deliveries are held for unregistered sessions (tests).
    var parkedCount: Int { parked.count }

    /// Forgets a closed session. Its own held clipboard write, the last one
    /// its program made, is written now rather than lost (a clean exit closes
    /// the tab before the write would fall due).
    func unregister(_ id: UInt64) {
        sessions[id] = nil
        guard !isShutDown, let write = clipboardThrottle.takeHeld(from: id, at: .now) else { return }
        pasteboard.clearContents()
        pasteboard.setString(write.text, forType: .string)
    }

    /// Called after `tessera_app_free` returned: drops the sessions (breaking
    /// their reference back to the bridge). Deliveries already queued still
    /// run; each takes its pending record and drops it because `isShutDown` is
    /// set. A held clipboard write is dropped.
    func shutDown() {
        isShutDown = true
        sessions.removeAll()
        parked.removeAll()
        clipboardRelease?.cancel()
        clipboardRelease = nil
        clipboardThrottle.dropHeld()
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
            },
            on_config_applied: { userData, generation in
                CallbackBridge.from(userData)?.receiveConfiguration(generation: generation)
            },
            on_session_remote: { userData, id, kind, host, len in
                // `host` (UTF-8, already sanitised by the core) is valid only
                // during this call.
                let remote = CallbackBridge.remote(kind: kind, host: host, len: len)
                CallbackBridge.from(userData)?.receive(for: id) { $0.remote = remote }
            },
            on_config_invalid: { userData in
                CallbackBridge.from(userData)?.receiveInvalid()
            },
            on_session_command_finished: { userData, id, exitKnown, exitCode, durationMs in
                let finish = TesseraCommandFinish(
                    exitCode: exitKnown ? exitCode : nil, durationMilliseconds: durationMs)
                CallbackBridge.from(userData)?.receive(for: id) { pending in
                    if pending.commandFinished.map({ $0.durationMilliseconds <= durationMs }) ?? true {
                        pending.commandFinished = finish
                    }
                }
            },
            on_font_index_ready: { userData in
                CallbackBridge.from(userData)?.receiveFontIndexReady()
            }
        )
    }

    /// The remote state an `on_session_remote` call describes. A host kind
    /// without a host, or longer than the core's cap, reads as unnamed; an
    /// unknown kind as local.
    nonisolated static func remote(kind: UInt8, host: UnsafePointer<UInt8>?, len: Int) -> TesseraRemote {
        switch TesseraRemoteKind(rawValue: UInt32(kind)) {
        case TESSERA_REMOTE_KIND_HOST:
            guard let host, len > 0, len <= Int(TESSERA_MAX_REMOTE_HOST_LEN) else { return .unnamed }
            return .host(String(decoding: UnsafeBufferPointer(start: host, count: len), as: UTF8.self))
        case TESSERA_REMOTE_KIND_UNNAMED:
            return .unnamed
        default:
            return .local
        }
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
        var remote: TesseraRemote?
        var clipboard: String?
        var bell = false
        var commandFinished: TesseraCommandFinish?
        var exit: (code: Int32, signal: Int32)?

        /// These events, then `later`'s on top of them: a flag set in either
        /// stays set, a value is the later one's if it has one, a finished
        /// command the longer one's.
        func merged(with later: Pending?) -> Pending {
            guard let later else { return self }
            var merged = self
            merged.output = output || later.output
            merged.title = later.title ?? title
            merged.remote = later.remote ?? remote
            merged.clipboard = later.clipboard ?? clipboard
            merged.bell = bell || later.bell
            // The longest finish, as ``receive(for:_:)`` keeps it (ADR 0048).
            if let finish = later.commandFinished,
                commandFinished.map({ $0.durationMilliseconds <= finish.durationMilliseconds }) ?? true
            {
                merged.commandFinished = finish
            }
            merged.exit = later.exit ?? exit
            return merged
        }
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

    private nonisolated func receiveConfiguration(generation: UInt64) {
        let schedule = pendingConfiguration.withLock { pending in
            defer { pending = generation }
            return pending == nil
        }
        guard schedule else { return }
        scheduledConfigurationDeliveries.withLock { $0 += 1 }
        Task { @MainActor in
            self.deliverConfiguration()
        }
    }

    private nonisolated func receiveInvalid() {
        let schedule = pendingInvalid.withLock { pending in
            defer { pending = true }
            return !pending
        }
        guard schedule else { return }
        scheduledInvalidDeliveries.withLock { $0 += 1 }
        Task { @MainActor in
            self.deliverInvalid()
        }
    }

    private nonisolated func receiveFontIndexReady() {
        let schedule = pendingFontIndex.withLock { pending in
            defer { pending = true }
            return !pending
        }
        guard schedule else { return }
        Task { @MainActor in
            self.deliverFontIndexReady()
        }
    }

    // MARK: Main actor

    private func deliverFontIndexReady() {
        guard pendingFontIndex.withLock({ pending in
            defer { pending = false }
            return pending
        }) else { return }
        guard !isShutDown, let delegate else { return }
        delegate.fontIndexDidLand()
    }

    private func deliverInvalid() {
        guard pendingInvalid.withLock({ pending in
            defer { pending = false }
            return pending
        }) else { return }
        guard !isShutDown, let delegate else { return }
        delegate.configurationDidNotLoad()
    }

    private func deliverConfiguration() {
        guard let generation = pendingConfiguration.withLock({ pending in
            defer { pending = nil }
            return pending
        }) else { return }
        guard !isShutDown, let delegate else { return }
        delegate.configurationDidApply(generation: generation)
    }

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
        if !isShutDown, sessions[id] == nil, spawnsInFlight > 0 {
            parked[id] = parked[id]?.merged(with: events) ?? events
            return
        }
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
        if let remote = events.remote, let (delegate, session) = target() {
            delegate.session(session, didChangeRemote: remote)
        }
        if let text = events.clipboard, !isShutDown, sessions[id] != nil {
            offerClipboard(.init(session: id, text: text))
        }
        if events.bell, let (delegate, session) = target() {
            delegate.sessionDidRingBell(session)
        }
        if let finish = events.commandFinished, let (delegate, session) = target() {
            delegate.session(session, didFinishCommand: finish)
        }
        if let exit = events.exit, let (delegate, session) = target() {
            delegate.session(session, didExitWithCode: exit.code, signal: exit.signal)
        }
    }

    // MARK: Clipboard

    private func offerClipboard(_ write: ClipboardThrottle.Write) {
        if let write = clipboardThrottle.offer(write, at: .now) {
            writePasteboard(write)
        }
        scheduleClipboardRelease()
    }

    /// Starts the timer for a held write, unless one is already running.
    private func scheduleClipboardRelease() {
        guard clipboardRelease == nil, let due = clipboardThrottle.dueAt else { return }
        clipboardRelease = Task { [weak self] in
            try? await Task.sleep(until: due, clock: .continuous)
            guard !Task.isCancelled else { return }
            self?.releaseClipboard()
        }
    }

    private func releaseClipboard() {
        clipboardRelease = nil
        if let write = clipboardThrottle.release(at: .now) {
            writePasteboard(write)
        }
        scheduleClipboardRelease()
    }

    /// Writes a program's clipboard text, unless the bridge shut down or the
    /// session it came from was closed in the meantime.
    private func writePasteboard(_ write: ClipboardThrottle.Write) {
        guard !isShutDown, sessions[write.session] != nil else { return }
        pasteboard.clearContents()
        pasteboard.setString(write.text, forType: .string)
    }
}
