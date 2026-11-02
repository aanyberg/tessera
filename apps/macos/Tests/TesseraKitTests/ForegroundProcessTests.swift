import Foundation
import TesseraKit
import XCTest

/// `TesseraSession.foregroundProcess()` against a real interactive shell
/// (ADR 0031): idle at the prompt, the job's name while `sleep` runs, idle
/// again after Ctrl-C, and an error once the session is closed.
@MainActor
final class ForegroundProcessTests: XCTestCase {
    private var app: TesseraApp!

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    func testAnInteractiveShellReportsItsForegroundJob() async throws {
        let session = try app.spawn(.init(program: "/bin/sh", arguments: ["-i"]))
        XCTAssertNil(try session.foregroundProcess(), "a fresh shell runs no job")

        try session.write(Array("sleep 100\n".utf8))
        // Between fork and exec the job's process is still a copy of the shell, so wait for the name.
        let job = try await poll("sleep runs in the foreground") {
            try session.foregroundProcess().flatMap { $0.name == "sleep" ? $0 : nil }
        }
        XCTAssertGreaterThan(job.pid, 0)

        try session.write([0x03])
        try await waitUntil("Ctrl-C ends the job") { try session.foregroundProcess() == nil }

        try session.close()
        XCTAssertThrowsError(try session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
    }

    /// A program that is not an interactive shell never counts as busy.
    func testAProgramThatIsNotAShellReportsNoJob() throws {
        let session = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", "sleep 100"]))
        XCTAssertNil(try session.foregroundProcess())
        try session.close()
    }

    func testAfterShutdownTheCallFails() throws {
        let session = try app.spawn(.init(program: "/bin/cat"))
        app.shutdown()
        XCTAssertThrowsError(try session.foregroundProcess()) {
            XCTAssertEqual($0 as? TesseraCoreError, .nullPointer)
        }
    }

    // MARK: Helpers

    private func poll<T>(
        _ description: String, timeout: Duration = .seconds(5), _ value: () throws -> T?
    ) async throws -> T {
        let deadline = ContinuousClock.now + timeout
        while true {
            if let value = try value() { return value }
            guard ContinuousClock.now < deadline else {
                throw TimedOut(description: "timed out after \(timeout): \(description)")
            }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () throws -> Bool
    ) async throws {
        _ = try await poll(description, timeout: timeout) { try condition() ? true : nil }
    }
}

private struct TimedOut: Error, CustomStringConvertible {
    let description: String
}
