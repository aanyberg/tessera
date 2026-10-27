import Foundation
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

    override func setUp() async throws {
        var raw: OpaquePointer?
        XCTAssertEqual(tessera_app_new(&raw), TESSERA_ERROR_OK)
        core = CoreHandle(try XCTUnwrap(raw))
        bridge = CallbackBridge()
        recorder = Recorder()
        bridge.delegate = recorder
        session = TesseraSession(id: 7, core: core, bridge: bridge)
        bridge.register(session)
    }

    override func tearDown() async throws {
        core?.free()
        bridge?.shutDown()
    }

    /// Runs `body` on a plain background thread and blocks the main thread
    /// until it returns, so every event it emits is queued before any can be
    /// delivered.
    private func onReaderThread(_ body: @escaping @Sendable (TesseraCallbacks) -> Void) {
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread { [bridge] in
            body(bridge!.callbacks())
            done.signal()
        }
        done.wait()
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

    func testEveryStatusMapsToItsCase() {
        let cases: [(TesseraError, TesseraCoreError?)] = [
            (TESSERA_ERROR_OK, nil),
            (TESSERA_ERROR_NULL_POINTER, .nullPointer),
            (TESSERA_ERROR_INVALID_ARGUMENT, .invalidArgument),
            (TESSERA_ERROR_SESSION_NOT_FOUND, .sessionNotFound),
            (TESSERA_ERROR_SPAWN_FAILED, .spawnFailed),
            (TESSERA_ERROR_IO, .io),
            (TESSERA_ERROR_PANIC, .panic),
            (TesseraError(rawValue: 42), .unknown(rawValue: 42)),
        ]
        for (status, expected) in cases {
            XCTAssertEqual(TesseraCoreError(status), expected, "\(status.rawValue)")
        }
    }
}
