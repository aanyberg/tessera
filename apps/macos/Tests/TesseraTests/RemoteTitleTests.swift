import AppKit
import Darwin
import TesseraKit
import XCTest

@testable import Tessera

/// The remote-host title prefix (#205, ADR 0022 D-HOST amendment): the core
/// decides a session's remote state from OSC 7 reports and the `ssh`
/// fallback; the window shows the focused pane's state as `⇢ <host>: ` in
/// front of the title, unless `general.remote_indicator` is `"off"`.
@MainActor
final class RemoteTitleTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var previous: String?
    private var dir: URL?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        XCTAssertTrue(appDelegate.showsRemoteHost, "a previous test left the indicator off")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        if dir != nil {
            try appDelegate.replaceCore(start: false)
        }
        appDelegate.showsRemoteHost = true
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        appDelegate = nil
    }

    // MARK: Helpers

    /// Waits up to 10 s for `condition`.
    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// A shell that sends `report` (printf escapes) and then echoes input.
    private func reporting(_ report: String) -> TesseraSession.SpawnOptions {
        .init(program: "/bin/sh", arguments: ["-c", "printf '\(report)'; exec cat"])
    }

    /// Gives the app a core of its own on `config` in a fresh absolute
    /// temporary directory.
    private func startCore(config: String) throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-remote-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        let file = dir.appendingPathComponent("config.toml")
        try config.write(to: file, atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        try appDelegate.replaceCore(start: true)
        return file
    }

    // MARK: Composition

    func testTheTitleIsPrefixedWithTheHostOrSsh() {
        typealias C = TerminalWindowController
        XCTAssertEqual(C.title("vim", remote: .local), "vim", "a local title is unchanged")
        XCTAssertEqual(C.title("", remote: .local), "Terminal")
        XCTAssertEqual(C.title("~/app", remote: .host("prod-db")), "⇢ prod-db: ~/app")
        XCTAssertEqual(C.title("", remote: .host("prod-db")), "⇢ prod-db")
        XCTAssertEqual(C.title("~/app", remote: .unnamed), "⇢ ssh: ~/app", "no host we have not confirmed")
        XCTAssertEqual(C.title("", remote: .unnamed), "⇢ ssh")
    }

    func testAReportedHostPrefixesTheCustomTitleUntilALocalReport() throws {
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        let session = controller.focusedPane.session
        controller.session(session, didSetTitle: "vim notes.txt")
        controller.session(session, didChangeRemote: .host("prod-db"))
        XCTAssertEqual(window.title, "⇢ prod-db: vim notes.txt", "the custom title keeps its text")
        controller.session(session, didSetTitle: "top")
        XCTAssertEqual(window.title, "⇢ prod-db: top", "a new title keeps the prefix")
        controller.session(session, didChangeRemote: .local)
        XCTAssertEqual(window.title, "top", "the next local report clears it")
        controller.session(session, didChangeRemote: .unnamed)
        XCTAssertEqual(window.title, "⇢ ssh: top", "the ssh fallback")
        controller.session(session, didSetTitle: "")
        XCTAssertEqual(window.title, "⇢ ssh")
        controller.session(session, didChangeRemote: .local)
        XCTAssertEqual(window.title, "Terminal")
    }

    func testTurningTheIndicatorOffShowsTheTitleAloneInEveryWindow() throws {
        let first = try appDelegate.newWindow()
        let second = try appDelegate.newWindow()
        first.session(first.focusedPane.session, didChangeRemote: .host("far"))
        second.session(second.focusedPane.session, didSetTitle: "make")
        second.session(second.focusedPane.session, didChangeRemote: .unnamed)
        appDelegate.showsRemoteHost = false
        XCTAssertEqual(first.window?.title, "Terminal")
        XCTAssertEqual(second.window?.title, "make")
        let third = try appDelegate.newWindow()
        XCTAssertFalse(third.showsRemoteHost, "a new window follows the setting")
        third.session(third.focusedPane.session, didChangeRemote: .host("far"))
        XCTAssertEqual(third.window?.title, "Terminal")
        appDelegate.showsRemoteHost = true
        XCTAssertEqual(first.window?.title, "⇢ far", "the state was kept")
        XCTAssertEqual(second.window?.title, "⇢ ssh: make")
        XCTAssertEqual(third.window?.title, "⇢ far")
    }

    func testTheTabShowsTheFocusedPanesRemoteState() throws {
        let controller = try appDelegate.newWindow()
        controller.window?.setContentSize(NSSize(width: 1200, height: 800))
        controller.paneContainer.layoutSubtreeIfNeeded()
        let window = try XCTUnwrap(controller.window)
        let left = controller.focusedPane
        let right = try XCTUnwrap(controller.splitFocusedPane(.right), "the split is refused")
        controller.paneContainer.layoutSubtreeIfNeeded()
        controller.session(left.session, didChangeRemote: .host("far"))
        XCTAssertEqual(window.title, "Terminal", "the right pane has the focus and is local")
        controller.focus(left)
        XCTAssertEqual(window.title, "⇢ far")
        controller.session(right.session, didChangeRemote: .unnamed)
        XCTAssertEqual(window.title, "⇢ far", "an unfocused pane's state stays its own")
        controller.focus(right)
        XCTAssertEqual(window.title, "⇢ ssh")
        controller.closeFocusedPane(nil)
        XCTAssertEqual(window.title, "⇢ far", "the remaining pane's state")
    }

    // MARK: Through the core

    func testAHostReportFromTheSessionArrivesSanitised() async throws {
        // `far` + U+202E (right-to-left override) + `.example`: the override
        // is removed before the title is built (S3).
        appDelegate.spawnOptions = reporting(#"\033]7;file://far\342\200\256.example/srv\007"#)
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        try await waitUntil("the remote host reaches the title") { window.title != "Terminal" }
        XCTAssertEqual(window.title, "⇢ far.example")
        XCTAssertEqual(controller.focusedPane.remote, .host("far.example"))
        // The next local report clears it: an empty host is this machine.
        try controller.focusedPane.session.write(Array("\u{1b}]7;file:///tmp\u{07}\n".utf8))
        try await waitUntil("the local report clears the prefix") { window.title == "Terminal" }
    }

    func testAHostLongerThan253BytesShowsAsSsh() async throws {
        // One byte over the core's cap (TESSERA_MAX_REMOTE_HOST_LEN, 253).
        let host = String(repeating: "a", count: 254)
        appDelegate.spawnOptions = reporting(#"\033]7;file://"# + host + #"/srv\007"#)
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        try await waitUntil("the remote state reaches the title") { window.title != "Terminal" }
        XCTAssertEqual(window.title, "⇢ ssh", "dropped whole, never cut")
    }

    func testAFloodOfHostReportsKeepsMemoryAndCloseBounded() async throws {
        // Warm-up: the first window of the process sets up the renderer,
        // Metal and fonts (about 90 MB and a third of a second cold under
        // TSan), which is not the flood's cost. A plain `cat` window that
        // has shown output, then closed, takes that before `start`.
        let warmUp = try appDelegate.newWindow()
        let warmUpWindow = try XCTUnwrap(warmUp.window)
        try warmUp.focusedPane.session.write(Array("\u{1b}]0;warm\u{07}\n".utf8))
        try await waitUntil("the warm-up window shows output") { warmUpWindow.title == "warm" }
        try await Task.sleep(for: .milliseconds(200))
        warmUpWindow.close()
        // 200 000 reports alternating between two hosts: at most one change
        // per chunk reaches the bridge, which keeps the last per session.
        appDelegate.spawnOptions = .init(
            program: "/bin/sh",
            arguments: [
                "-c",
                #"i=0; while [ $i -lt 100000 ]; do printf '\033]7;file://one/\007\033]7;file://two/\007'; i=$((i+1)); done; printf '\033]7;file://end/\007'; exec cat"#,
            ])
        let start = Self.physFootprint()
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        let deadline = ContinuousClock.now + .seconds(120)
        var worstLag = Duration.zero
        while window.title != "⇢ end", ContinuousClock.now < deadline {
            let before = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(10))
            worstLag = max(worstLag, ContinuousClock.now - before - .milliseconds(10))
        }
        XCTAssertEqual(window.title, "⇢ end", "the last report wins")
        let growthMB = Double(Int64(Self.physFootprint()) - Int64(start)) / 1_048_576
        let closeStart = ContinuousClock.now
        window.close()
        let closeTime = ContinuousClock.now - closeStart
        print("remote flood: growth \(Int(growthMB)) MB, worst lag \(worstLag), close \(closeTime)")
        XCTAssertLessThan(growthMB, 150, "footprint grew \(growthMB) MB")
        XCTAssertLessThan(worstLag, .milliseconds(500), "main actor lagged \(worstLag)")
        XCTAssertLessThan(closeTime, .seconds(3), "closing took \(closeTime)")
    }

    func testTheIndicatorFollowsTheConfigurationLive() async throws {
        let file = try startCore(config: "[general]\nremote_indicator = \"off\"\n")
        XCTAssertFalse(appDelegate.showsRemoteHost, "read at start")
        appDelegate.spawnOptions = reporting(#"\033]7;file://far/srv\007"#)
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        try await waitUntil("the report arrives") { controller.focusedPane.remote == .host("far") }
        XCTAssertEqual(window.title, "Terminal", "off")
        try "[general]\nremote_indicator = \"title\"\n".write(to: file, atomically: true, encoding: .utf8)
        try await waitUntil("the edit is applied") { window.title == "⇢ far" }
        XCTAssertTrue(appDelegate.showsRemoteHost)
        try "[general]\nremote_indicator = \"off\"\n".write(to: file, atomically: true, encoding: .utf8)
        try await waitUntil("off again") { window.title == "Terminal" }
        // An unknown value shows the prefix (the core warns and uses "title").
        try "[general]\nremote_indicator = \"badge\"\n".write(to: file, atomically: true, encoding: .utf8)
        try await waitUntil("an unknown value falls back to title") { window.title == "⇢ far" }
    }

    // MARK: Lifetime

    func testClosingARemoteTabReleasesItsControllerViewAndSession() async throws {
        weak var weakController: TerminalWindowController?
        weak var weakView: TerminalSurfaceView?
        weak var weakSession: TesseraSession?
        appDelegate.spawnOptions = reporting(#"\033]7;file://far/srv\007"#)
        try autoreleasepool {
            let controller = try appDelegate.newWindow()
            weakController = controller
            weakView = controller.focusedPane.view
            weakSession = controller.focusedPane.session
        }
        try await waitUntil("the report arrives") { weakController?.window?.title == "⇢ far" }
        autoreleasepool { weakController?.window?.close() }
        let deadline = ContinuousClock.now + .seconds(10)
        while (weakController != nil || weakView != nil || weakSession != nil) && ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakSession, "session")
    }

    /// The process's physical footprint in bytes.
    private static func physFootprint() -> UInt64 {
        var usage = rusage_info_v4()
        let status = withUnsafeMutablePointer(to: &usage) {
            $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
                proc_pid_rusage(getpid(), RUSAGE_INFO_V4, $0)
            }
        }
        return status == 0 ? usage.ri_phys_footprint : 0
    }
}
