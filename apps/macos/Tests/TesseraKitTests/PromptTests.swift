import Foundation
@testable import TesseraKit
import XCTest

/// `TesseraSession.scrollToPrompt(previous:)`, `selectLastOutput()` and
/// `hasCommandOutput` round-tripped through the C API (ADR 0039): a 24-row
/// session whose child prints two commands with their OSC 133 marks around
/// 30 plain lines (12 lines of history), and one that prints no marks.
@MainActor
final class PromptTests: XCTestCase {
    private var app: TesseraApp!

    private static let rows: UInt16 = 24

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    /// Line 0 is the first prompt, 1 and 2 its output, 3 to 32 plain lines,
    /// 33 the second prompt and 34 its output (`hi`), 35 the live prompt.
    private func sessionWithMarks() async throws -> TesseraSession {
        let mark = { (payload: String) in "\\033]133;\(payload)\\007" }
        let first = "\(mark("A"))$ \(mark("B"))ls\\n\(mark("C"))out1\\nout2\\n\(mark("D;0"))"
        let second = "\(mark("A"))$ \(mark("B"))echo\\n\(mark("C"))hi\\n\(mark("D;0"))\(mark("A"))$ "
        let script = "printf '\(first)'; seq 1 30; printf '\(second)'; exec cat"
        let session = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", script], rows: Self.rows))
        try await waitUntil("the marked output is complete") {
            (try? session.scrollPosition)?.history == 12 && (try? session.selectLastOutput()) == true
        }
        return session
    }

    func testSelectLastOutputSelectsTheLastCommandsOutput() async throws {
        let session = try await sessionWithMarks()
        XCTAssertEqual(try session.selectionText(), "hi")
        XCTAssertTrue(try session.hasCommandOutput)
    }

    func testThePromptsStopTheViewAndItStopsAtTheEnds() async throws {
        let session = try await sessionWithMarks()
        let steps: [(Bool, Bool, UInt64)] = [(true, true, 12), (true, false, 12), (false, true, 0), (false, false, 0)]
        for (previous, moved, offset) in steps {
            XCTAssertEqual(try session.scrollToPrompt(previous: previous), moved, "previous: \(previous)")
            XCTAssertEqual(try session.scrollPosition.offset, offset, "previous: \(previous)")
        }
    }

    /// Without marks the prompt commands page (D4), and there is no output
    /// to select.
    func testWithoutMarksTheyPage() async throws {
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "seq 1 100; exec cat"], rows: Self.rows))
        try await waitUntil("the history is complete") { (try? session.scrollPosition)?.history ?? 0 >= 77 }
        XCTAssertTrue(try session.scrollToPrompt(previous: true))
        XCTAssertEqual(try session.scrollPosition.offset, UInt64(Self.rows - 1))
        XCTAssertTrue(try session.scrollToPrompt(previous: false))
        XCTAssertEqual(try session.scrollPosition.offset, 0)
        XCTAssertFalse(try session.hasCommandOutput)
        XCTAssertFalse(try session.selectLastOutput())
        XCTAssertNil(try session.selectionText())
    }

    func testAClosedSessionThrows() async throws {
        let session = try await sessionWithMarks()
        try session.close()
        XCTAssertThrowsError(try session.scrollToPrompt(previous: true)) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.selectLastOutput()) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.hasCommandOutput) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
    }

    /// Polls `condition` on the main actor until it holds or five seconds
    /// pass.
    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
