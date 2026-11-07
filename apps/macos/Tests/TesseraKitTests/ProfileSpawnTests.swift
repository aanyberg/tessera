import Foundation
import TesseraKit
import XCTest

/// Profiles through the wrapper (#39), on a core of the test's own with a
/// `config.toml` of its own: ``TesseraApp/profileNames()`` lists them in
/// file order, and ``TesseraSession/SpawnOptions/profile`` starts the
/// profile's command in its working directory.
@MainActor
final class ProfileSpawnTests: XCTestCase {
    private var dir: URL!
    private var work: URL!
    private var previous: String?
    private var core: TesseraApp?

    override func setUp() async throws {
        let unique = UUID().uuidString
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-profiles-\(unique)")
        work = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-profile-work-\(unique)")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
        try? FileManager.default.removeItem(at: work)
    }

    private func start(config: String) throws -> TesseraApp {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let started = try TesseraApp()
        core = started
        return started
    }

    func testProfileNamesAreListedInFileOrderAsWritten() throws {
        let core = try start(
            config: """
                [[profiles]]
                name = "zsh"
                [[profiles]]
                name = "Build \\u202Eevil"
                [[profiles]]
                name = "Admin"
                """)
        XCTAssertEqual(try core.profileNames(), ["zsh", "Build \u{202E}evil", "Admin"])
        core.shutdown()
        XCTAssertThrowsError(try core.profileNames()) { XCTAssertEqual($0 as? TesseraCoreError, .nullPointer) }
    }

    func testNoProfilesIsAnEmptyList() throws {
        XCTAssertEqual(try start(config: "").profileNames(), [])
    }

    /// The profile's command runs, in its working directory: it writes
    /// that directory to a file there.
    func testSpawningWithAProfileRunsItsCommandInItsDirectory() async throws {
        let core = try start(
            config: """
                [[profiles]]
                name = "build"
                command = "pwd -P > ran; exec cat"
                working_directory = '\(work.path)'
                """)
        let session = try core.spawn(.init(profile: "build"))
        defer { try? session.close() }
        let ran = work.appendingPathComponent("ran")
        let deadline = ContinuousClock.now + .seconds(10)
        var written = ""
        while !written.hasSuffix("\n"), ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
            written = (try? String(contentsOf: ran, encoding: .utf8)) ?? ""
        }
        // `pwd -P`'s physical path (`/private/var/...`), which Foundation's
        // `resolvingSymlinksInPath()` would shorten again.
        let physical = try XCTUnwrap(realpath(work.path, nil))
        defer { free(physical) }
        XCTAssertEqual(written.trimmingCharacters(in: .newlines), String(cString: physical))
    }

    func testAProfileNameWithANulOrPastTheBoundIsRefused() throws {
        let core = try start(config: "")
        XCTAssertThrowsError(try core.spawn(.init(program: "/bin/cat", profile: "a\u{0}b"))) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
        XCTAssertThrowsError(try core.spawn(.init(program: "/bin/cat", profile: String(repeating: "p", count: 129)))) {
            XCTAssertEqual($0 as? TesseraCoreError, .invalidArgument)
        }
        // An unknown name of the longest length falls back to the default.
        let session = try core.spawn(.init(program: "/bin/cat", profile: String(repeating: "p", count: 128)))
        try session.close()
    }
}
