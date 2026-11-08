import AppKit
import Synchronization
import TesseraCore
@testable import TesseraKit
import XCTest

/// Drives the bridge's C callbacks directly, from a background thread, the way
/// the core's reader threads do, to cover paths the core cannot trigger yet
/// (titles) or cannot trigger deterministically (coalescing, ordering).
@MainActor
final class CallbackBridgeTests: XCTestCase {
    private var core: CoreHandle!
    private var bridge: CallbackBridge!
    private var session: TesseraSession!
    private var recorder: Recorder!
    /// A name of its own per test: a fixed one is shared by every test run
    /// on the machine (another worktree's, say), which then writes it too.
    private let pasteboard = NSPasteboard(
        name: NSPasteboard.Name("com.aanyberg.TesseraKitTests.clipboard.\(UUID().uuidString)"))

    override func setUp() async throws {
        var raw: OpaquePointer?
        XCTAssertEqual(tessera_app_new(&raw), TESSERA_ERROR_OK)
        core = CoreHandle(try XCTUnwrap(raw))
        bridge = CallbackBridge()
        // Never the user's clipboard.
        bridge.pasteboard = pasteboard
        pasteboard.clearContents()
        recorder = Recorder()
        bridge.delegate = recorder
        session = TesseraSession(id: 7, core: core, bridge: bridge)
        bridge.register(session)
    }

    override func tearDown() async throws {
        pasteboard.releaseGlobally()
        core?.free()
        bridge?.shutDown()
    }

    /// Runs `body` on a new thread and blocks the calling (main) thread until
    /// it returns. The thread runs at the main thread's quality of service: a
    /// semaphore donates no priority, so a lower one is a priority inversion
    /// (the Thread Performance Checker reports it).
    private nonisolated static func blockingOnThread(_ body: @escaping @Sendable () -> Void) {
        let done = DispatchSemaphore(value: 0)
        let thread = Thread {
            body()
            done.signal()
        }
        thread.qualityOfService = .userInteractive
        thread.start()
        done.wait()
    }

    /// Runs `body` on a background thread (``blockingOnThread(_:)``) and
    /// blocks the main thread until it returns, so every event it emits is
    /// queued before any can be delivered.
    private func onReaderThread(_ body: @escaping @Sendable (TesseraCallbacks) -> Void) {
        nonisolated(unsafe) let callbacks = bridge.callbacks()
        Self.blockingOnThread { body(callbacks) }
    }

    /// Lets queued main-actor deliveries run, then returns the session's events.
    private func drained() async throws -> [Recorder.Event] {
        try await Task.sleep(for: .milliseconds(50))
        return recorder.events[session.id, default: []]
    }

    func testTitleIsCopiedBeforeTheCallbackReturns() async throws {
        onReaderThread { callbacks in
            var title = Array("hello".utf8CString)
            title.withUnsafeBufferPointer { callbacks.on_session_title!(callbacks.user_data, 7, $0.baseAddress) }
            // The core frees the string after the call; clobber it the same way.
            for index in title.indices { title[index] = 0x58 }
        }
        let events = try await drained()
        XCTAssertEqual(events, [.title("hello")])
    }

    func testQueuedEventsAreCoalescedIntoOneOrderedDelivery() async throws {
        onReaderThread { callbacks in
            callbacks.on_session_bell!(callbacks.user_data, 7)
            for index in 0..<50 {
                Array("t\(index)".utf8CString).withUnsafeBufferPointer {
                    callbacks.on_session_title!(callbacks.user_data, 7, $0.baseAddress)
                }
                callbacks.on_session_output!(callbacks.user_data, 7)
                callbacks.on_session_bell!(callbacks.user_data, 7)
            }
            callbacks.on_session_exit!(callbacks.user_data, 7, 0, 0)
        }
        let events = try await drained()
        // One delivery: output, the latest title, one bell, then the exit.
        XCTAssertEqual(events, [.output, .title("t49"), .bell, .exit(code: 0, signal: 0)])
    }

    func testDeliveriesKeepTheirOrder() async throws {
        for index in 0..<5 {
            onReaderThread { callbacks in
                Array("t\(index)".utf8CString).withUnsafeBufferPointer {
                    callbacks.on_session_title!(callbacks.user_data, 7, $0.baseAddress)
                }
                callbacks.on_session_bell!(callbacks.user_data, 7)
            }
            _ = try await drained()
        }
        onReaderThread { callbacks in callbacks.on_session_exit!(callbacks.user_data, 7, 3, 0) }
        let events = try await drained()
        XCTAssertEqual(
            events, (0..<5).flatMap { [Recorder.Event.title("t\($0)"), .bell] } + [.exit(code: 3, signal: 0)])
    }

    /// #205: the host is copied before the callback returns, a burst of
    /// changes is one delivery with the latest, after the title.
    func testRemoteStatesAreCopiedAndCoalescedAfterTheTitle() async throws {
        onReaderThread { callbacks in
            var host = Array("far.example".utf8)
            host.withUnsafeBufferPointer {
                callbacks.on_session_remote!(
                    callbacks.user_data, 7, UInt8(TESSERA_REMOTE_KIND_HOST.rawValue), $0.baseAddress, $0.count)
            }
            for index in host.indices { host[index] = 0x58 }
            Array("t".utf8CString).withUnsafeBufferPointer {
                callbacks.on_session_title!(callbacks.user_data, 7, $0.baseAddress)
            }
        }
        let first = try await drained()
        XCTAssertEqual(first, [.title("t"), .remote(.host("far.example"))])
        onReaderThread { callbacks in
            for index in 0..<100_000 {
                let kind = index.isMultiple(of: 2) ? TESSERA_REMOTE_KIND_UNNAMED : TESSERA_REMOTE_KIND_LOCAL
                callbacks.on_session_remote!(callbacks.user_data, 7, UInt8(kind.rawValue), nil, 0)
            }
        }
        let flooded = try await drained()
        XCTAssertEqual(
            flooded, [.title("t"), .remote(.host("far.example")), .remote(.local)],
            "a flood is one delivery, the latest state")
    }

    func testMalformedRemoteCallsReadSafely() {
        XCTAssertEqual(CallbackBridge.remote(kind: UInt8(TESSERA_REMOTE_KIND_HOST.rawValue), host: nil, len: 3), .unnamed)
        let long = Array(repeating: UInt8(ascii: "a"), count: Int(TESSERA_MAX_REMOTE_HOST_LEN) + 1)
        long.withUnsafeBufferPointer {
            XCTAssertEqual(
                CallbackBridge.remote(kind: UInt8(TESSERA_REMOTE_KIND_HOST.rawValue), host: $0.baseAddress, len: $0.count),
                .unnamed)
        }
        XCTAssertEqual(CallbackBridge.remote(kind: 9, host: nil, len: 0), .local)
    }

    func testBellBurstIsCoalescedIntoOneDelivery() async throws {
        onReaderThread { callbacks in
            for _ in 0..<100_000 {
                callbacks.on_session_bell!(callbacks.user_data, 7)
            }
        }
        let events = try await drained()
        XCTAssertEqual(events, [.bell])
    }

    func testDelegateClosingTheSessionStopsTheRestOfADelivery() async throws {
        recorder.onOutput = { [bridge] _ in bridge!.unregister(7) }
        onReaderThread { callbacks in
            callbacks.on_session_output!(callbacks.user_data, 7)
            callbacks.on_session_bell!(callbacks.user_data, 7)
            callbacks.on_session_exit!(callbacks.user_data, 7, 0, 0)
        }
        let events = try await drained()
        XCTAssertEqual(events, [.output])
    }

    func testOutputBurstIsCoalescedIntoOneDelivery() async throws {
        onReaderThread { callbacks in
            for _ in 0..<1000 {
                callbacks.on_session_output!(callbacks.user_data, 7)
            }
        }
        var events = try await drained()
        XCTAssertEqual(events, [.output])
        // Output after the delivery schedules a new one.
        onReaderThread { callbacks in callbacks.on_session_output!(callbacks.user_data, 7) }
        events = try await drained()
        XCTAssertEqual(events, [.output, .output])
    }

    func testEventsForUnknownOrClosedSessionsAreDropped() async throws {
        onReaderThread { callbacks in
            callbacks.on_session_bell!(callbacks.user_data, 99)
            callbacks.on_session_bell!(nil, 7)
        }
        var events = try await drained()
        XCTAssertEqual(events, [])
        XCTAssertNil(recorder.events[99])
        bridge.unregister(7)
        onReaderThread { callbacks in callbacks.on_session_bell!(callbacks.user_data, 7) }
        events = try await drained()
        XCTAssertEqual(events, [])
    }

    func testEventsQueuedBeforeShutdownAreDropped() async throws {
        onReaderThread { callbacks in
            callbacks.on_session_output!(callbacks.user_data, 7)
            callbacks.on_session_bell!(callbacks.user_data, 7)
        }
        bridge.shutDown()
        let events = try await drained()
        XCTAssertEqual(events, [])
        XCTAssertTrue(bridge.sessions.isEmpty)
    }

    /// Calls `on_session_clipboard` for `session` with `text`'s UTF-8 from a
    /// buffer the caller overwrites as soon as the call returns.
    private nonisolated static func writeClipboard(_ callbacks: TesseraCallbacks, _ text: String, session: UInt64 = 7) {
        var bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer {
            callbacks.on_session_clipboard!(callbacks.user_data, session, $0.baseAddress, $0.count)
        }
        for index in bytes.indices { bytes[index] = 0x58 }
    }

    func testClipboardLastWriteWinsAndIsCopiedBeforeTheCallbackReturns() async throws {
        // Kept alive and overwritten after the call: the bridge must have
        // copied it by then.
        nonisolated(unsafe) let buffer = UnsafeMutableBufferPointer<UInt8>.allocate(capacity: 5)
        defer { buffer.deallocate() }
        onReaderThread { callbacks in
            CallbackBridgeTests.writeClipboard(callbacks, "one")
            _ = buffer.initialize(from: "hello".utf8)
            callbacks.on_session_clipboard!(callbacks.user_data, 7, UnsafePointer(buffer.baseAddress), buffer.count)
            buffer.update(repeating: 0x58)
        }
        _ = try await drained()
        XCTAssertEqual(pasteboard.string(forType: .string), "hello")
        XCTAssertEqual(recorder.events[session.id, default: []], [], "not a delegate event")
    }

    func testClipboardWritesAfterCloseOrShutdownAreDropped() async throws {
        bridge.unregister(7)
        onReaderThread { callbacks in CallbackBridgeTests.writeClipboard(callbacks, "closed") }
        _ = try await drained()
        XCTAssertNil(pasteboard.string(forType: .string), "closed session")
        bridge.register(session)
        onReaderThread { callbacks in CallbackBridgeTests.writeClipboard(callbacks, "late") }
        bridge.shutDown()
        _ = try await drained()
        XCTAssertNil(pasteboard.string(forType: .string), "after shutdown")
    }

    func testEmptyClipboardTextNeverTouchesThePasteboard() async throws {
        pasteboard.setString("keep", forType: .string)
        onReaderThread { callbacks in
            callbacks.on_session_clipboard!(callbacks.user_data, 7, nil, 0)
            CallbackBridgeTests.writeClipboard(callbacks, "")
        }
        _ = try await drained()
        XCTAssertEqual(pasteboard.string(forType: .string), "keep")
    }

    // MARK: Clipboard write cap (ADR 0022 D-RATE52)

    /// Lets the main actor run until `condition` holds; fails the test if it
    /// does not within `limit`. The limit is a failure bound, not a timing
    /// claim: a passing run returns as soon as the condition holds, and a
    /// starved main actor (load average 70 and more) must not fail it.
    private func until(
        _ limit: Duration = .seconds(10), _ condition: () -> Bool,
        file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        let deadline = ContinuousClock.now + limit
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("condition not met within \(limit)", file: file, line: line)
                return
            }
            try await Task.sleep(for: .milliseconds(1))
        }
    }

    func testUntilFailsAtItsDeadline() async throws {
        let start = ContinuousClock.now
        try await until { true }
        // Strict: the test fails unless `until` records its failure; any
        // other failure is not expected.
        let options = XCTExpectedFailure.Options()
        options.issueMatcher = { $0.compactDescription.contains("condition not met within") }
        XCTExpectFailure("a condition that never holds fails at the deadline", options: options)
        try await until(.milliseconds(20)) { false }
        XCTAssertGreaterThanOrEqual(ContinuousClock.now - start, .milliseconds(20), "not before it")
    }

    /// ``writeClipboard(_:_:session:)`` on a background thread; returns once
    /// it has.
    private nonisolated static func writeClipboardOnReaderThread(
        _ callbacks: TesseraCallbacks, _ text: String, session: UInt64
    ) {
        nonisolated(unsafe) let callbacks = callbacks
        blockingOnThread { writeClipboard(callbacks, text, session: session) }
    }

    private var clipboard: String? { pasteboard.string(forType: .string) }

    /// Has `bridge` hold `text` from `session`: a write right after one that
    /// went to the pasteboard. Returns the text written before it. Retried
    /// when the machine is so slow that the second write came after the
    /// interval and went out at once.
    private func holdClipboardWrite(
        _ text: String, session: UInt64, on bridge: CallbackBridge, leadSession: UInt64 = 7
    ) async throws -> String {
        // The threads get the callback table, which holds the bridge
        // unretained, as the core's do: not the bridge.
        nonisolated(unsafe) let callbacks = bridge.callbacks()
        for attempt in 0..<10 {
            let lead = "lead \(attempt)"
            Self.writeClipboardOnReaderThread(callbacks, lead, session: leadSession)
            try await until { self.clipboard == lead }
            XCTAssertEqual(clipboard, lead)
            Self.writeClipboardOnReaderThread(callbacks, text, session: session)
            try await until { bridge.clipboardThrottle.held != nil || self.clipboard == text }
            if bridge.clipboardThrottle.held?.text == text {
                XCTAssertTrue(bridge.isClipboardReleaseScheduled)
                return lead
            }
            try await Task.sleep(for: ClipboardThrottle.minimumInterval * 2)
        }
        XCTFail("no write held in 10 attempts")
        return ""
    }

    /// Has `bridge` hold a write, lets the timer release it, and offers
    /// `text` from `session` right after the release: `text` must be held,
    /// since the release counts as a write. Retried when the machine is so
    /// slow that `text` may have come more than the interval after the
    /// release (then it goes out at once, rightly).
    private func offerRightAfterARelease(_ text: String, session: UInt64) async throws {
        nonisolated(unsafe) let callbacks = bridge.callbacks()
        for attempt in 0..<10 {
            let released = "released \(attempt)"
            _ = try await holdClipboardWrite(released, session: 7, on: bridge)
            // The last instant the write was seen held: the release came
            // after it.
            var stillHeld = ContinuousClock.now
            let deadline = stillHeld + .seconds(10)
            while bridge.clipboardThrottle.held != nil {
                guard ContinuousClock.now < deadline else {
                    XCTFail("the held write was not released within 10 s")
                    return
                }
                stillHeld = .now
                try await Task.sleep(for: .milliseconds(1))
            }
            XCTAssertEqual(clipboard, released)
            Self.writeClipboardOnReaderThread(callbacks, text, session: session)
            try await until { self.bridge.clipboardThrottle.held != nil || self.clipboard == text }
            if ContinuousClock.now - stillHeld < ClipboardThrottle.minimumInterval {
                // Offered within the interval after the release.
                XCTAssertEqual(bridge.clipboardThrottle.held?.text, text, "held right after a release")
                XCTAssertEqual(clipboard, released, "not written at once")
                return
            }
            try await Task.sleep(for: ClipboardThrottle.minimumInterval * 2)
        }
        XCTFail("no write offered within the interval after a release in 10 attempts")
    }

    func testAWriteRightAfterAnotherIsHeldThenWritten() async throws {
        let lead = try await holdClipboardWrite("held", session: 7, on: bridge)
        let changes = pasteboard.changeCount
        XCTAssertEqual(clipboard, lead, "held, not written yet")
        try await until { self.clipboard == "held" }
        XCTAssertEqual(clipboard, "held", "written once the interval passed")
        XCTAssertEqual(pasteboard.changeCount, changes + 1)
        XCTAssertNil(bridge.clipboardThrottle.held)
        XCTAssertFalse(bridge.isClipboardReleaseScheduled)
    }

    func testClosingAnotherSessionLeavesTheHeldWriteHeld() async throws {
        let other = TesseraSession(id: 8, core: core, bridge: bridge)
        bridge.register(other)
        let lead = try await holdClipboardWrite("kept", session: 8, on: bridge)
        bridge.unregister(7)
        XCTAssertEqual(clipboard, lead, "not written at another session's close")
        XCTAssertEqual(bridge.clipboardThrottle.held?.text, "kept")
        try await until { self.clipboard == "kept" }
        XCTAssertEqual(clipboard, "kept", "released when due")
    }

    /// A program's last write, held, then its session closes (a clean exit
    /// closes the tab): the write is made at the close, not lost.
    func testAClosingSessionsOwnHeldWriteIsWrittenAtTheClose() async throws {
        let other = TesseraSession(id: 8, core: core, bridge: bridge)
        bridge.register(other)
        _ = try await holdClipboardWrite("last", session: 8, on: bridge)
        bridge.unregister(8)
        XCTAssertEqual(clipboard, "last", "written at the close")
        XCTAssertNil(bridge.clipboardThrottle.held)
        // The release scheduled for it runs and finds nothing to write.
        let changes = pasteboard.changeCount
        try await until { !self.bridge.isClipboardReleaseScheduled }
        XCTAssertFalse(bridge.isClipboardReleaseScheduled)
        XCTAssertEqual(pasteboard.changeCount, changes, "nothing written after it")
        XCTAssertEqual(clipboard, "last")
    }

    func testAHeldWriteAtShutdownNeverReachesThePasteboard() async throws {
        let lead = try await holdClipboardWrite("late", session: 7, on: bridge)
        bridge.shutDown()
        XCTAssertFalse(bridge.isClipboardReleaseScheduled, "shutdown cancels the release")
        XCTAssertNil(bridge.clipboardThrottle.held, "and drops the held write")
        try await Task.sleep(for: ClipboardThrottle.minimumInterval * 3)
        XCTAssertEqual(clipboard, lead)
    }

    func testTheReleaseTimerDoesNotKeepAShutDownBridgeAlive() async throws {
        var other: CallbackBridge? = CallbackBridge()
        other!.pasteboard = pasteboard
        var session: TesseraSession? = TesseraSession(id: 9, core: core, bridge: other!)
        other!.register(session!)
        _ = try await holdClipboardWrite("held", session: 9, on: other!, leadSession: 9)
        weak var weakBridge: CallbackBridge?
        weakBridge = other
        other!.shutDown()
        session = nil
        other = nil
        XCTAssertNil(weakBridge, "the scheduled release does not hold the bridge")
        try await Task.sleep(for: ClipboardThrottle.minimumInterval * 2)
        XCTAssertNil(weakBridge, "nor after its interval")
        XCTAssertNotEqual(clipboard, "held")
    }

    /// Two reader threads flood OSC 52 writes for two sessions, small and
    /// 1 MiB, for over a second: every two consecutive pasteboard writes are
    /// at least 100 ms apart, so no 1 s window holds more than 11 (10 a
    /// second, both ends of the window included). The writes are timed by
    /// sampling the change count: each happened between the sample that saw
    /// it and the one before, and a gap or window counts as too short only
    /// when even those outer bounds are, so a loaded machine cannot make it
    /// fail. Then a write offered right after a timer release is held (the
    /// release counts as a write; in the flood a later offer usually beats
    /// the timer, so this is checked on its own), and released when due:
    /// the pasteboard ends with it.
    func testAClipboardFloodFromTwoSessionsIsCappedAndEndsWithTheLastWrite() async throws {
        let other = TesseraSession(id: 8, core: core, bridge: bridge)
        bridge.register(other)
        let start = ContinuousClock.now
        // When each write happened: after `after`, by `by`.
        var writes: [(after: ContinuousClock.Instant, by: ContinuousClock.Instant)] = []
        var previous = (at: start, count: pasteboard.changeCount)
        // The pasteboard is written on the main actor, as this runs: no
        // write falls between reading the clock and the count.
        func sample() {
            let now = ContinuousClock.now
            let count = pasteboard.changeCount
            for _ in 0..<max(0, count - previous.count) {
                writes.append((previous.at, now))
            }
            previous = (now, count)
        }
        let finished = Mutex(0)
        let perThread = 5_000
        let span = Duration.milliseconds(1_200)
        for sessionID: UInt64 in [7, 8] {
            Thread.detachNewThread { [bridge] in
                let callbacks = bridge!.callbacks()
                let big = String(repeating: "x", count: 1 << 20)
                for index in 0..<perThread {
                    let due = start + span * index / perThread
                    let wait = due - ContinuousClock.now
                    if wait > .zero {
                        Thread.sleep(forTimeInterval: Double(wait.components.attoseconds) / 1e18)
                    }
                    let text = index % 1_000 == 999 ? big + "\(sessionID) \(index)" : "\(sessionID) \(index)"
                    CallbackBridgeTests.writeClipboard(callbacks, text, session: sessionID)
                }
                finished.withLock { $0 += 1 }
            }
        }
        var heldWrites = 0
        while finished.withLock({ $0 }) < 2 {
            sample()
            if bridge.clipboardThrottle.held != nil { heldWrites += 1 }
            try await Task.sleep(for: .milliseconds(5))
        }
        // The flood's held write, released when due.
        try await until { self.bridge.clipboardThrottle.held == nil && !self.bridge.isClipboardReleaseScheduled }
        sample()
        XCTAssertGreaterThanOrEqual(ContinuousClock.now - start, .seconds(1), "the flood lasted a second")
        XCTAssertGreaterThan(heldWrites, 0, "writes were held during the flood")
        let window = ClipboardThrottle.maxWritesPerSecond + 1
        let crowded = zip(writes, writes.dropFirst(window)).filter { first, last in
            last.by - first.after <= .seconds(1)
        }
        XCTAssertEqual(crowded.count, 0, "\(window + 1) of \(writes.count) writes within a second")
        // The sampling runs on the main actor, as the throttle and the
        // pasteboard write do, so a write's throttle instant lies within its
        // sample bounds too.
        let close = zip(writes, writes.dropFirst()).filter { first, second in
            second.by - first.after < ClipboardThrottle.minimumInterval
        }
        XCTAssertEqual(close.count, 0, "\(close.count) of \(writes.count) writes under 100 ms after the one before")

        try await offerRightAfterARelease("the last", session: 8)
        try await until { self.clipboard == "the last" }
        XCTAssertEqual(clipboard, "the last", "the last write is not lost")
        XCTAssertNil(bridge.clipboardThrottle.held)
    }

    func testAppearanceChangesReachTheDelegateCoalesced() async throws {
        // From the core's configuration watch thread: a burst is one delivery
        // with the latest state.
        onReaderThread { callbacks in
            for index in 0..<10_000 {
                callbacks.on_appearance_changed!(callbacks.user_data, index % 2 == 0)
            }
            callbacks.on_appearance_changed!(callbacks.user_data, false)
        }
        _ = try await drained()
        XCTAssertEqual(recorder.appearances, [false], "10 001 callbacks, one delivery with the latest")
        XCTAssertEqual(bridge.scheduledAppearanceDeliveries.withLock { $0 }, 1, "and one scheduled")
        onReaderThread { callbacks in callbacks.on_appearance_changed!(callbacks.user_data, true) }
        _ = try await drained()
        XCTAssertEqual(recorder.appearances, [false, true], "a later change is a new delivery")
        // Session events are unaffected.
        XCTAssertEqual(recorder.events[session.id, default: []], [])
    }

    func testAppearanceChangesAfterShutdownAreDropped() async throws {
        onReaderThread { callbacks in callbacks.on_appearance_changed!(callbacks.user_data, false) }
        bridge.shutDown()
        _ = try await drained()
        XCTAssertEqual(recorder.appearances, [])
    }

    func testConfigurationAppliesReachTheDelegateCoalesced() async throws {
        // From the configuration watch thread: a burst is one delivery with
        // the latest generation, so the menus read the key map once.
        onReaderThread { callbacks in
            for generation in 1...10_000 {
                callbacks.on_config_applied!(callbacks.user_data, UInt64(generation))
            }
            callbacks.on_config_applied!(callbacks.user_data, 10_001)
        }
        _ = try await drained()
        XCTAssertEqual(recorder.configurations, [10_001], "10 001 callbacks, one delivery with the latest")
        XCTAssertEqual(bridge.scheduledConfigurationDeliveries.withLock { $0 }, 1, "and one scheduled")
        onReaderThread { callbacks in callbacks.on_config_applied!(callbacks.user_data, 10_002) }
        _ = try await drained()
        XCTAssertEqual(recorder.configurations, [10_001, 10_002], "a later apply is a new delivery")
        XCTAssertEqual(recorder.appearances, [], "appearance is a callback of its own")
        XCTAssertEqual(recorder.events[session.id, default: []], [])
    }

    func testConfigurationAppliesAfterShutdownAreDropped() async throws {
        onReaderThread { callbacks in callbacks.on_config_applied!(callbacks.user_data, 1) }
        bridge.shutDown()
        _ = try await drained()
        XCTAssertEqual(recorder.configurations, [])
    }

    func testEveryStatusMapsToItsCase() {
        let cases: [(TesseraError, TesseraCoreError?)] = [
            (TESSERA_ERROR_OK, nil),
            (TESSERA_ERROR_NULL_POINTER, .nullPointer),
            (TESSERA_ERROR_INVALID_ARGUMENT, .invalidArgument),
            (TESSERA_ERROR_SESSION_NOT_FOUND, .sessionNotFound),
            (TESSERA_ERROR_SPAWN_FAILED, .spawnFailed),
            (TESSERA_ERROR_IO, .io),
            (TESSERA_ERROR_PANIC, .panic),
            (TESSERA_ERROR_GPU, .gpu),
            (TESSERA_ERROR_NO_SURFACE, .noSurface),
            (TesseraError(rawValue: 42), .unknown(rawValue: 42)),
        ]
        for (status, expected) in cases {
            XCTAssertEqual(TesseraCoreError(status), expected, "\(status.rawValue)")
        }
    }
}
