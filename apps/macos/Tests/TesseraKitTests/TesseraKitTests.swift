import Foundation
import Synchronization
import TesseraCore
import TesseraKit
import XCTest

/// Records delegate events and fulfils expectations for them. Every method
/// asserts it runs on the main thread: the bridge must deliver there.
@MainActor
final class Recorder: TesseraAppDelegate {
    enum Event: Equatable {
        case output
        case title(String)
        case bell
        case exit(code: Int32, signal: Int32)
    }

    private(set) var events: [UInt64: [Event]] = [:]
    /// Runs after an output event is recorded.
    var onOutput: ((TesseraSession) -> Void)?
    private var waiters: [(session: UInt64, matches: (Event) -> Bool, expectation: XCTestExpectation)] = []

    /// An expectation fulfilled by the first matching event of `session`.
    func expect(_ session: TesseraSession, _ description: String, where matches: @escaping (Event) -> Bool)
        -> XCTestExpectation
    {
        let expectation = XCTestExpectation(description: description)
        if events[session.id, default: []].contains(where: matches) {
            expectation.fulfill()
        } else {
            waiters.append((session.id, matches, expectation))
        }
        return expectation
    }

    func expectExit(_ session: TesseraSession) -> XCTestExpectation {
        expect(session, "session \(session.id) exits") {
            if case .exit = $0 { true } else { false }
        }
    }

    private func record(_ event: Event, _ session: TesseraSession) {
        XCTAssertTrue(Thread.isMainThread, "delegate called off the main thread")
        MainActor.assertIsolated()
        events[session.id, default: []].append(event)
        waiters.removeAll { waiter in
            guard waiter.session == session.id, waiter.matches(event) else { return false }
            waiter.expectation.fulfill()
            return true
        }
    }

    func sessionDidOutput(_ session: TesseraSession) {
        record(.output, session)
        onOutput?(session)
    }
    func session(_ session: TesseraSession, didSetTitle title: String) { record(.title(title), session) }
    func sessionDidRingBell(_ session: TesseraSession) { record(.bell, session) }
    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {
        record(.exit(code: code, signal: signal), session)
    }

    /// Every `appearanceDidChange(opaque:)`, in order.
    private(set) var appearances: [Bool] = []

    func appearanceDidChange(opaque: Bool) {
        XCTAssertTrue(Thread.isMainThread, "delegate called off the main thread")
        appearances.append(opaque)
    }
}

@MainActor
final class TesseraKitTests: XCTestCase {
    private var app: TesseraApp!
    private var recorder: Recorder!

    override func setUp() async throws {
        app = try TesseraApp()
        recorder = Recorder()
        app.delegate = recorder
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
        recorder = nil
    }

    private func sh(_ script: String) throws -> TesseraSession {
        try app.spawn(.init(program: "/bin/sh", arguments: ["-c", script]))
    }

    // MARK: Version and lifecycle

    func testCoreVersionMatchesTheCHeader() {
        XCTAssertFalse(TesseraApp.coreVersion.isEmpty)
        XCTAssertEqual(TesseraApp.coreVersion, String(cString: tessera_version()))
    }

    func testCoreVersionIsReadableOffTheMainActor() async {
        let version = await Task.detached { TesseraApp.coreVersion }.value
        XCTAssertEqual(version, TesseraApp.coreVersion)
    }

    func testStartingCreatesNothingAndCreateConfigWritesTheDefaultFileOnce() throws {
        // A directory of this test's own: the scheme's scratch directory must
        // stay absent, so the other tests run without the configuration watch.
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-config-\(UUID().uuidString)")
        let previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        defer {
            if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
            try? FileManager.default.removeItem(at: dir)
        }
        let own = try TesseraApp()
        defer { own.shutdown() }
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.path), "starting creates nothing")
        try own.createConfig()
        let first = try String(contentsOf: own.configPath, encoding: .utf8)
        XCTAssertTrue(first.contains("[appearance]"), first)
        try own.createConfig()
        XCTAssertEqual(try String(contentsOf: own.configPath, encoding: .utf8), first)
    }

    func testInitReportsTheConfigPath() {
        XCTAssertEqual(app.configPath.lastPathComponent, "config.toml")
        XCTAssertTrue(app.configPath.isFileURL)
        XCTAssertTrue(app.configPath.path.contains("Tessera"), app.configPath.path)
    }

    func testShutdownIsIdempotentAndDisablesTheApp() throws {
        let session = try sh("sleep 30")
        app.shutdown()
        app.shutdown()
        XCTAssertThrowsError(try app.spawn()) { XCTAssertEqual($0 as? TesseraCoreError, .nullPointer) }
        XCTAssertThrowsError(try session.write(Array("x".utf8))) {
            XCTAssertEqual($0 as? TesseraCoreError, .nullPointer)
        }
        XCTAssertThrowsError(try session.close()) { XCTAssertEqual($0 as? TesseraCoreError, .nullPointer) }
    }

    // MARK: Callbacks (mirrors crates/tessera-ffi/tests/c/lifecycle.c)

    func testShellExitsWithCodeZeroOnTheMainActor() async throws {
        let session = try app.spawn(.init(program: "/bin/sh"))
        try session.resize(rows: 30, cols: 100)
        let exited = recorder.expectExit(session)
        try session.write(Array("exit\n".utf8))
        await fulfillment(of: [exited], timeout: 5)
        XCTAssertEqual(recorder.events[session.id]?.last, .exit(code: 0, signal: 0))
        try session.close()
    }

    func testExitCodeAndSignalAreReported() async throws {
        let failing = try sh("exit 3")
        let killed = try sh("kill -TERM $$")
        let both = [recorder.expectExit(failing), recorder.expectExit(killed)]
        await fulfillment(of: both, timeout: 5)
        XCTAssertEqual(recorder.events[failing.id]?.last, .exit(code: 3, signal: 0))
        XCTAssertEqual(recorder.events[killed.id]?.last, .exit(code: -1, signal: SIGTERM))
    }

    /// The app ignores SIGTERM, SIGINT and SIGHUP (it handles them with
    /// dispatch sources); the PTY spawn must reset them so sessions still die
    /// of them.
    func testChildGetsDefaultSignalsWhenTheHostIgnoresThem() async throws {
        let previousTerm = signal(SIGTERM, SIG_IGN)
        let previousInt = signal(SIGINT, SIG_IGN)
        let previousHup = signal(SIGHUP, SIG_IGN)
        defer {
            signal(SIGTERM, previousTerm)
            signal(SIGINT, previousInt)
            signal(SIGHUP, previousHup)
        }
        let terminated = try sh("kill -TERM $$")
        let interrupted = try sh("kill -INT $$")
        let hungUp = try sh("kill -HUP $$")
        await fulfillment(
            of: [terminated, interrupted, hungUp].map { recorder.expectExit($0) }, timeout: 5)
        XCTAssertEqual(recorder.events[terminated.id]?.last, .exit(code: -1, signal: SIGTERM))
        XCTAssertEqual(recorder.events[interrupted.id]?.last, .exit(code: -1, signal: SIGINT))
        XCTAssertEqual(recorder.events[hungUp.id]?.last, .exit(code: -1, signal: SIGHUP))
    }

    func testTitleAndBellArriveBeforeExitInOrder() async throws {
        let session = try sh(#"printf '\033]0;hello\007'; printf '\a'"#)
        let exited = recorder.expectExit(session)
        await fulfillment(of: [exited], timeout: 5)
        let events = recorder.events[session.id, default: []].filter { $0 != .output }
        // The core ignores OSC until #9 (term: OSC handling) lands, so no title
        // arrives yet. Once one does this fails on purpose: expect
        // [.title("hello"), .bell, .exit(code: 0, signal: 0)] instead. The
        // bridge's title path is covered without the core in CallbackBridgeTests.
        XCTAssertEqual(events, [.bell, .exit(code: 0, signal: 0)], "titles arrive now (#9): update this test")
    }

    func testOutputIsDelivered() async throws {
        let session = try sh("echo hello")
        let output = recorder.expect(session, "output") { $0 == .output }
        await fulfillment(of: [output, recorder.expectExit(session)], timeout: 5)
    }

    func testEventsOfAClosedSessionAreDropped() async throws {
        let closed = try sh(#"printf '\033]0;never\007'; exit 0"#)
        try closed.close()
        // A second session's exit, emitted after, proves the queue drained.
        let other = try sh("exit 0")
        await fulfillment(of: [recorder.expectExit(other)], timeout: 5)
        XCTAssertNil(recorder.events[closed.id])
    }

    /// Close Window closes its tabs one after another on the main actor; with
    /// children that ignore SIGHUP each close used to wait out the hangup grace,
    /// about a second each. The core now reaps them in the background (#79).
    func testClosingManySessionsThatIgnoreSighupReturnsAtOnce() async throws {
        let sessions = try (0..<20).map { _ in try sh("trap '' HUP; printf ready; exec sleep 30") }
        let ready = sessions.map { recorder.expect($0, "trap installed") { $0 == .output } }
        await fulfillment(of: ready, timeout: 10)
        let start = ContinuousClock.now
        for session in sessions {
            try session.close()
        }
        let closeTime = ContinuousClock.now - start
        // Well under a millisecond in release; generous for Thread Sanitizer.
        XCTAssertLessThan(closeTime, .milliseconds(500))
        // Freeing waits for the background closes, whose grace periods overlap.
        let freeStart = ContinuousClock.now
        app.shutdown()
        let shutdownTime = ContinuousClock.now - freeStart
        XCTAssertLessThan(shutdownTime, .milliseconds(2500))
        print("closing 20 SIGHUP-ignoring sessions: close loop \(closeTime), shutdown \(shutdownTime)")
        for session in sessions {
            let exited = recorder.events[session.id, default: []].contains { if case .exit = $0 { true } else { false } }
            XCTAssertFalse(exited, "no exit for a closed session")
        }
    }

    // MARK: Errors

    func testClosingAStaleSessionThrowsSessionNotFound() throws {
        let session = try sh("sleep 30")
        try session.close()
        XCTAssertThrowsError(try session.close()) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
        XCTAssertThrowsError(try session.resize(rows: 10, cols: 10)) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.clearScrollback()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.write(Array("x".utf8))) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
    }

    func testInvalidSpawnOptionsThrow() {
        let cases: [(TesseraSession.SpawnOptions, TesseraCoreError)] = [
            (.init(program: "/bin/sh", rows: 0), .invalidArgument),
            (.init(program: "/bin/sh", cols: 0), .invalidArgument),
            (.init(program: ""), .invalidArgument),
            (.init(program: "/bin/sh\0-x"), .invalidArgument),
            (.init(program: "/bin/sh", arguments: ["-c", "true\0false"]), .invalidArgument),
            (.init(program: "/bin/sh", workingDirectory: URL(string: "https://example.com/tmp")), .invalidArgument),
            (.init(program: "/nonexistent/tessera-test-program"), .spawnFailed),
        ]
        for (options, expected) in cases {
            XCTAssertThrowsError(try app.spawn(options), "\(options)") {
                XCTAssertEqual($0 as? TesseraCoreError, expected, "\(options)")
            }
        }
    }

    func testResizeRejectsZeroCells() throws {
        let session = try sh("sleep 30")
        XCTAssertThrowsError(try session.resize(rows: 0, cols: 80)) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
        try session.resize(rows: 50, cols: 132, widthPx: 1000, heightPx: 800)
        try session.clearScrollback()
    }

    func testArgumentsAndWorkingDirectoryReachTheChild() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("tessera kit \(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        // Exits 0 only when cwd is `directory` and both arguments arrived intact.
        let script = #"[ "$(pwd -P)" = "$(cd "$0" && pwd -P)" ] && [ "$1" = "a b" ] && [ "$2" = "é" ]"#
        let session = try app.spawn(
            .init(
                program: "/bin/sh", arguments: ["-c", script, directory.path, "a b", "é"],
                workingDirectory: directory))
        await fulfillment(of: [recorder.expectExit(session)], timeout: 5)
        XCTAssertEqual(recorder.events[session.id]?.last, .exit(code: 0, signal: 0))
    }

    // MARK: Threads

    func testWriteFromBackgroundThreads() async throws {
        let session = try app.spawn(.init(program: "/bin/sh"))
        let exited = recorder.expectExit(session)
        // Plain threads (no Swift concurrency context) and a detached task.
        let results = Mutex<[Result<Bool, any Error>]>([])
        let written = XCTestExpectation(description: "background writes returned")
        written.expectedFulfillmentCount = 4
        for _ in 0..<3 {
            Thread.detachNewThread {
                let result = Result { try session.write(Data(": noop\n".utf8)) }.map { Thread.isMainThread }
                results.withLock { $0.append(result) }
                written.fulfill()
            }
        }
        Task.detached {
            let result = Result { try session.write(Array(": noop\n".utf8)) }.map { Thread.isMainThread }
            results.withLock { $0.append(result) }
            written.fulfill()
        }
        await fulfillment(of: [written], timeout: 5)
        for result in results.withLock({ $0 }) {
            XCTAssertEqual(try result.get(), false, "write ran on the main thread")
        }
        try await Task.detached { try session.write(Array("exit\n".utf8)) }.value
        await fulfillment(of: [exited], timeout: 5)
        XCTAssertEqual(recorder.events[session.id]?.last, .exit(code: 0, signal: 0))
    }

    func testShutdownWakesAWriteBlockedOnAChildThatDoesNotRead() async throws {
        // Raw mode so the terminal queues the input instead of discarding it;
        // the child never reads, so the write blocks on the full input queue.
        let session = try sh("stty raw -echo; printf ready; exec sleep 30")
        let ready = recorder.expect(session, "terminal in raw mode") { $0 == .output }
        await fulfillment(of: [ready], timeout: 5)
        let returned = XCTestExpectation(description: "blocked write returned")
        let large = Data(repeating: UInt8(ascii: "x"), count: 8 << 20)
        let outcome = Mutex<Result<Void, any Error>?>(nil)
        Thread.detachNewThread {
            let result = Result { try session.write(large) }
            outcome.withLock { $0 = result }
            returned.fulfill()
        }
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertNil(outcome.withLock { $0 }, "the write did not block; the test proves nothing")
        let start = ContinuousClock.now
        app.shutdown()
        XCTAssertLessThan(ContinuousClock.now - start, .seconds(5))
        await fulfillment(of: [returned], timeout: 5)
        XCTAssertThrowsError(try outcome.withLock { $0 }?.get(), "the woken write must fail") {
            XCTAssertEqual($0 as? TesseraCoreError, .io)
        }
    }
}
