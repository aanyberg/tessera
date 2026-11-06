import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// What the selection gestures ask of the core (#30), which the fixture rows
/// cannot say (they see only the result), and the pieces of the gestures
/// that have rules of their own: the autoscroll's pace and the links a
/// command-click may open.
@MainActor
final class TerminalSelectionCallTests: XCTestCase {
    private var app: TesseraApp!
    private let delegate = TerminalSelectionTests.TitleRecorder()

    override func setUp() async throws {
        app = try TesseraApp()
        app.delegate = delegate
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    private func rig(rows: Int = 3, cols: Int = 30, feed: String = "hello world foo") async throws
        -> TerminalSelectionTests.Rig
    {
        var row = TerminalSelectionTests.Row(id: "calls")
        row.rows = rows
        row.cols = cols
        row.steps = [.feed(Array(feed.utf8))]
        return try await .init(app: app, delegate: delegate, row: row)
    }

    #if DEBUG
        func testEachGestureMakesTheCallsItsModifiersAndClicksAskFor() async throws {
            let rig = try await rig()
            defer { rig.close() }
            let view = rig.view
            func gesture(_ events: [String]) throws -> [SelectionCall] {
                let before = view.selectionCalls.count
                for event in events { try rig.perform(event) }
                return Array(view.selectionCalls.dropFirst(before))
            }
            XCTAssertEqual(try gesture(["down 0,3", "up"]), [.begin(col: 3, row: 0, granularity: .character, block: false)])
            XCTAssertEqual(try gesture(["down 0,3 clicks=2", "up"]), [.begin(col: 3, row: 0, granularity: .word, block: false)])
            XCTAssertEqual(try gesture(["down 0,3 clicks=3", "up"]), [.begin(col: 3, row: 0, granularity: .line, block: false)])
            XCTAssertEqual(
                try gesture(["down 0,3 clicks=5", "up"]), [.begin(col: 3, row: 0, granularity: .line, block: false)],
                "a fourth click and later are a line too")
            XCTAssertEqual(
                try gesture(["down 1,1 mods=opt", "up mods=opt"]),
                [.begin(col: 1, row: 1, granularity: .character, block: true)], "Option selects a block")
            XCTAssertEqual(
                try gesture(["down 1,1 mods=cmd,opt", "up mods=cmd,opt"]),
                [.begin(col: 1, row: 1, granularity: .character, block: true)], "⌥⌘ selects a block too")
            XCTAssertEqual(
                try gesture(["down 0,0", "drag 0,0", "drag 0,1", "drag 0,1", "drag 0,0", "up"]),
                [
                    .begin(col: 0, row: 0, granularity: .character, block: false), .extend(col: 1, row: 0),
                    .extend(col: 0, row: 0),
                ], "the core is asked to extend only when the pointer reaches another cell")
            XCTAssertEqual(
                try gesture(["down 0,0", "drag 1,3 mods=opt", "up"]),
                [.begin(col: 0, row: 0, granularity: .character, block: false), .extend(col: 3, row: 1)],
                "the mode is fixed at the press")
            // A selection exists now (a drag): shift adjusts it.
            XCTAssertTrue(view.hasSelection)
            XCTAssertEqual(try gesture(["down 0,9 mods=shift", "up mods=shift"]), [.adjust(col: 9, row: 0)])
            // Without one it is a plain click.
            try rig.session.selectClear()
            XCTAssertEqual(
                try gesture(["down 0,9 mods=shift", "up mods=shift"]),
                [.begin(col: 9, row: 0, granularity: .character, block: false)])
            XCTAssertEqual(try gesture(["flags cmd", "down 0,1 mods=cmd", "drag 0,5 mods=cmd", "up mods=cmd"]), [])
            XCTAssertEqual(
                try gesture(["down 0,1 mods=cmd,shift", "up mods=cmd,shift"]), [], "only Command alone is a link click")
        }
    #endif

    func testLosingKeyStatusStopsTheAutoscrollAndKeepsTheSelection() async throws {
        let rig = try await rig(rows: 3, cols: 10, feed: "l01\r\nl02\r\nl03\r\nl04\r\nl05\r\nl06")
        defer { rig.close() }
        try rig.perform("down 2,0")
        try rig.perform("drag above,0")
        try rig.perform("wait 100ms")
        let offset = try rig.session.scrollPosition.offset
        XCTAssertGreaterThan(offset, 0, "it was scrolling")
        rig.view.windowKeyChanged(isKey: false)
        XCTAssertNil(rig.view.autoscroller.direction)
        try rig.perform("wait 500ms")
        XCTAssertEqual(try rig.session.scrollPosition.offset, offset, "and stopped")
        XCTAssertTrue(rig.view.hasSelection, "the selection stays")
    }

    func testTheAutoscrollRunsWhileThePointerIsOutsideAndStopsWithTheButtonOrTheReturn() async throws {
        let rig = try await rig(rows: 3, cols: 10, feed: "l01\r\nl02\r\nl03\r\nl04\r\nl05\r\nl06")
        defer { rig.close() }
        let scroller = rig.view.autoscroller
        try rig.perform("down 2,0")
        XCTAssertNil(scroller.direction)
        try rig.perform("drag above,0")
        XCTAssertEqual(scroller.direction, .up)
        try rig.perform("drag 1,0")
        XCTAssertNil(scroller.direction, "the pointer came back")
        try rig.perform("drag below,0")
        XCTAssertEqual(scroller.direction, .down)
        try rig.perform("up")
        XCTAssertNil(scroller.direction, "the button went up")
        XCTAssertFalse(rig.scheduler.hasPendingTick, "and nothing is left to fire")
    }

    func testCommandClickOpensOnlyWhenItStaysInTheLinksCell() async throws {
        let rig = try await rig(cols: 40, feed: "see https://a.bc/x now")
        defer { rig.close() }
        try rig.perform("down 0,10 mods=cmd")
        try rig.perform("drag 0,11 mods=cmd")
        try rig.perform("drag 0,10 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened, [], "a drag, even back to the cell, is not a click")
        try rig.perform("down 0,10 mods=cmd clicks=2")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened, [], "a double-click does not open it twice")
        try rig.perform("down 0,10 mods=cmd")
        try rig.perform("up 0,11 mods=cmd")
        XCTAssertEqual(rig.opener.opened, [], "released in another cell")
        try rig.perform("down 0,10 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened.map(\.absoluteString), ["https://a.bc/x"])
    }

    func testCommandClickOpensTheLinkUnderThePressWhenOutputScrollsBeforeTheRelease() async throws {
        let rig = try await rig(cols: 40, feed: "x\r\nhttps://a.bc/one\r\nhttps://a.bc/two")
        defer { rig.close() }
        try rig.perform("down 1,5 mods=cmd")
        try await rig.feedLater(Array("\r\n".utf8))
        XCTAssertEqual(
            try rig.session.link(col: 5, row: 1), "https://a.bc/two",
            "the output scrolled the screen up a line: another link is under the pointer now")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened.map(\.absoluteString), ["https://a.bc/one"], "the link that was pressed opens")
    }

    func testNoSelectionOrLinkClickBeginsWhileTheCellSizeIsUnknown() async throws {
        // The link starts at cell 0,0, where every point lands with no
        // cell size.
        let rig = try await rig(cols: 40, feed: "https://a.bc/x now")
        defer { rig.close() }
        rig.view.metrics = nil
        try rig.perform("down 0,20 clicks=2")
        try rig.perform("drag 0,25")
        try rig.perform("up")
        XCTAssertFalse(try rig.session.hasSelection, "a double-click would select the word at 0,0")
        #if DEBUG
            XCTAssertEqual(rig.view.selectionCalls, [], "no selection call at all")
        #endif
        try rig.perform("flags cmd")
        try rig.perform("down 0,20 mods=cmd")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened, [], "a Command-click would open the link at 0,0")
    }

    func testCommandHoverIsOnlyOverTheTextWhileItsCellSizeIsKnown() async throws {
        // The link ends in the last column, next to a legacy scroll bar.
        let rig = try await rig(cols: 30, feed: "https://a.bc/" + String(repeating: "x", count: 17))
        defer { rig.close() }
        let view = rig.view
        view.scroller.preferredStyleOverride = .legacy
        XCTAssertNotNil(try rig.session.link(col: 29, row: 0), "the grid kept its link in the last column")
        let y = view.bounds.height - 10
        func hover(at point: NSPoint) throws -> String? {
            view.mouseMoved(
                with: try XCTUnwrap(
                    NSEvent.mouseEvent(
                        with: .mouseMoved, location: point, modifierFlags: .command, timestamp: 0, windowNumber: 0,
                        context: nil, eventNumber: 0, clickCount: 0, pressure: 0)))
            return view.hoveredLink
        }
        let text = view.renderView.frame
        let bar = view.scroller.frame
        XCTAssertGreaterThanOrEqual(bar.minX, text.maxX, "legacy: the scroll bar is beside the text")
        XCTAssertNotNil(try hover(at: NSPoint(x: text.maxX - 1, y: y)), "the text's last column hovers")
        XCTAssertNil(try hover(at: NSPoint(x: bar.midX, y: y)), "the scroll bar beside it does not")
        XCTAssertNotNil(try hover(at: NSPoint(x: 5, y: y)))
        view.metrics = nil
        try rig.perform("flags cmd")
        XCTAssertNil(view.hoveredLink, "no cell size: Command finds no link at 0,0")
        XCTAssertNil(try hover(at: NSPoint(x: 5, y: y)), "and neither does a move")
    }

    func testCommandHoverIsOnlyForLinksThePolicyAllows() async throws {
        let rig = try await rig(cols: 60, feed: "file://otherhost/etc/passwd ftp://h/f")
        defer { rig.close() }
        try rig.perform("flags cmd")
        try rig.perform("move 0,10 mods=cmd")
        XCTAssertNil(rig.view.hoveredLink, "a remote file URL would not open, so it is not underlined")
        try rig.perform("move 0,34 mods=cmd")
        XCTAssertEqual(rig.view.hoveredLink, "ftp://h/f")
        try rig.perform("down 0,34 mods=cmd")
        XCTAssertEqual(rig.opener.opened, [], "nothing opens until the button is released")
        try rig.perform("up mods=cmd")
        XCTAssertEqual(rig.opener.opened.map(\.absoluteString), ["ftp://h/f"])
    }

    func testLeavingTheViewEndsTheHover() async throws {
        let rig = try await rig(cols: 40, feed: "see https://a.bc/x now")
        defer { rig.close() }
        try rig.perform("flags cmd")
        try rig.perform("move 0,10 mods=cmd")
        XCTAssertNotNil(rig.view.hoveredLink)
        rig.view.mouseExited(with: NSEvent())
        XCTAssertNil(rig.view.hoveredLink)
        try rig.perform("flags none")
        try rig.perform("flags cmd")
        XCTAssertNil(rig.view.hoveredLink, "the pointer is gone: nothing to re-test")
    }

    // MARK: Autoscroll pace

    func testAutoscrollFollowsITerm2sPace() {
        let clock = FakeAutoscrollScheduler()
        let scroller = SelectionAutoscroller(scheduler: clock)
        var ticks: [(Duration, SelectionAutoscroller.Direction, Int)] = []
        let start = clock.now
        scroller.onTick = { direction, lines in ticks.append((clock.now - start, direction, lines)) }
        scroller.update(direction: .up)
        clock.advance(by: .milliseconds(99))
        XCTAssertTrue(ticks.isEmpty, "the first tick comes after 100 ms")
        clock.advance(by: .milliseconds(1))
        clock.advance(by: .milliseconds(300))
        XCTAssertEqual(ticks.map(\.2), [1, 1, 1, 1], "a tick on time scrolls one line")
        func milliseconds(_ duration: Duration) -> Double {
            Double(duration.components.seconds) * 1000 + Double(duration.components.attoseconds) / 1e15
        }
        for (tick, want) in zip(ticks, [100, 195, 285.25, 370.9875]) {
            XCTAssertEqual(
                milliseconds(tick.0), want, accuracy: 0.001,
                "ticks come 100 ms, then 5 % sooner each time (95, 90.25, 85.7 ms)")
        }
        // Never faster than 10 ms.
        ticks = []
        clock.advance(by: .seconds(2))
        let gaps = zip(ticks.map(\.0), ticks.dropFirst().map(\.0)).map { $1 - $0 }
        XCTAssertGreaterThanOrEqual(gaps.min() ?? .zero, .milliseconds(10))
        XCTAssertEqual(gaps.last, .milliseconds(10), "the pace settles at the 10 ms floor")
        // A late tick catches up: elapsed / wait lines, at least 1, at most 100.
        clock.fireLate(by: .milliseconds(65))
        XCTAssertEqual(ticks.last?.2, 7, "75 ms since the last tick on a 10 ms wait: 7 lines")
        clock.fireLate(by: .seconds(60))
        XCTAssertEqual(ticks.last?.2, 100, "capped")
        scroller.stop()
        // A timer that fires a little early still scrolls one line.
        scroller.update(direction: .down)
        clock.fireLate(by: .milliseconds(-3))
        XCTAssertEqual(ticks.last?.1, .down)
        XCTAssertEqual(ticks.last?.2, 1, "97 ms since the start on a 100 ms wait: 0 lines, but at least 1")
        scroller.stop()
        let count = ticks.count
        clock.advance(by: .seconds(1))
        XCTAssertEqual(ticks.count, count, "stopped")
    }

    func testAutoscrollKeepsItsPaceWhileThePointerStaysOutsideAndRestartsOnAnotherSide() {
        let clock = FakeAutoscrollScheduler()
        let scroller = SelectionAutoscroller(scheduler: clock)
        var ticks: [SelectionAutoscroller.Direction] = []
        scroller.onTick = { direction, _ in ticks.append(direction) }
        scroller.update(direction: .up)
        clock.advance(by: .milliseconds(100))
        scroller.update(direction: .up)
        clock.advance(by: .milliseconds(95))
        XCTAssertEqual(ticks, [.up, .up], "still outside: the tick after 95 ms is not delayed back to 100")
        scroller.update(direction: .down)
        clock.advance(by: .milliseconds(99))
        XCTAssertEqual(ticks.count, 2)
        clock.advance(by: .milliseconds(1))
        XCTAssertEqual(ticks, [.up, .up, .down], "the other side starts again at 100 ms")
        scroller.update(direction: nil)
        clock.advance(by: .seconds(1))
        XCTAssertEqual(ticks.count, 3, "back inside: no more ticks")
    }

    // MARK: Links

    func testCommandClickOpensOnlyWhatThePolicyAllows() {
        let host = "MyMac.local"
        func opens(_ string: String) -> Bool {
            if case .open? = LinkPolicy.action(for: string, localHostName: host) { return true }
            return false
        }
        for allowed in [
            "http://a.bc/x", "https://a.bc/x?y=1#z", "HTTPS://A.BC/", "ftp://h/f", "mailto:me@a.bc",
            "file:///etc/hosts", "file://localhost/etc/hosts", "file://LOCALHOST./etc/hosts",
            "file://mymac.local/etc/hosts", "file://MyMac.local./etc/hosts",
        ] {
            XCTAssertTrue(opens(allowed), allowed)
        }
        for refused in [
            "javascript:alert(1)", "vscode://x/y", "ssh://h", "tel:123", "data:text/html,hi", "x-man-page://ls",
            "file://otherhost/etc/passwd", "file://mymac.local.evil/x", "file://evil.com/x", "http://a b", "http://[bad", "",
            "https", "/etc/hosts",
        ] {
            XCTAssertNil(LinkPolicy.action(for: refused, localHostName: host), refused)
        }
        XCTAssertNil(LinkPolicy.action(for: "file://x/y", localHostName: ""), "no host name known: only localhost and none")
    }

    /// A directory with one of each kind of local file a link can name.
    private func makeFiles() throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-links-\(UUID().uuidString)")
        let manager = FileManager.default
        try manager.createDirectory(at: directory, withIntermediateDirectories: true)
        func file(_ name: String, mode: Int = 0o644, _ contents: String = "x") throws {
            let url = directory.appendingPathComponent(name)
            try Data(contents.utf8).write(to: url)
            try manager.setAttributes([.posixPermissions: mode], ofItemAtPath: url.path)
        }
        func folder(_ name: String) throws {
            try manager.createDirectory(at: directory.appendingPathComponent(name), withIntermediateDirectories: true)
        }
        try file("notes.txt")
        try file("picture.png")
        try file("run.command")
        try file("script.sh")
        try file("tool", mode: 0o755)
        try file("readme.txt", mode: 0o755)
        try file("link.webloc")
        try file("a.inetloc")
        try file("b.fileloc")
        try file("c.terminal")
        try file("d.tool")
        try file("e.pkg")
        try file("f.mpkg")
        try file("g.dmg")
        try file("h.jar")
        // Stored URLs, judged by type: the URL inside goes to its scheme's
        // handler, whatever the scheme.
        try file("j.url", "[InternetShortcut]\nURL=x-tessera-test://hello\n")
        try file("k.vncloc")
        try file("l.afploc")
        // Disk images under another name than `.dmg`.
        try file("m.img")
        try file("n.iso")
        try folder("folder")
        try folder("Foo.app")
        try folder("Doc.rtfd")
        try folder("Plug.bundle")
        // A folder's type says nothing of its name: only the extension
        // rule shows it (a `.jar` file's type is executable already).
        try folder("i.jar")
        for (alias, target) in [("alias-to-app", "Foo.app"), ("alias-to-txt", "notes.txt")] {
            let data = try directory.appendingPathComponent(target).bookmarkData(options: .suitableForBookmarkFile)
            try URL.writeBookmarkData(data, to: directory.appendingPathComponent(alias))
        }
        try manager.createSymbolicLink(
            at: directory.appendingPathComponent("link-to-app"), withDestinationURL: directory.appendingPathComponent("Foo.app"))
        try manager.createSymbolicLink(
            at: directory.appendingPathComponent("link-to-webloc.txt"),
            withDestinationURL: directory.appendingPathComponent("link.webloc"))
        try manager.createSymbolicLink(
            at: directory.appendingPathComponent("link-to-tool.txt"), withDestinationURL: directory.appendingPathComponent("tool"))
        return directory
    }

    func testALocalFileThatWouldLaunchSomethingIsRevealedNotOpened() throws {
        let directory = try makeFiles()
        defer { try? FileManager.default.removeItem(at: directory) }
        func action(_ name: String) -> LinkPolicy.Action? {
            LinkPolicy.action(for: directory.appendingPathComponent(name).absoluteString, localHostName: "mymac")
        }
        let url = { directory.appendingPathComponent($0) }
        for opens in ["notes.txt", "picture.png", "folder"] {
            XCTAssertEqual(action(opens), .open(url(opens)), "\(opens) is an ordinary file or folder")
        }
        for revealed in [
            "run.command", "script.sh", "tool", "link.webloc", "a.inetloc", "b.fileloc", "c.terminal", "d.tool",
            "e.pkg", "f.mpkg", "g.dmg", "h.jar", "i.jar", "j.url", "k.vncloc", "l.afploc", "m.img", "n.iso",
            "Foo.app", "Doc.rtfd", "Plug.bundle", "link-to-app", "link-to-tool.txt", "link-to-webloc.txt", "readme.txt",
            "alias-to-app", "alias-to-txt",
        ] {
            XCTAssertEqual(action(revealed), .reveal(url(revealed)), "\(revealed) could launch something")
        }
        XCTAssertNil(action("missing.txt"), "a path that does not exist does nothing")
        XCTAssertNil(action("missing.command"), "and neither does a missing script")
        XCTAssertEqual(
            LinkPolicy.action(for: "file://LOCALHOST." + url("run.command").path, localHostName: "x"),
            .reveal(URL(string: "file://LOCALHOST." + url("run.command").path)!), "the host rule is the same")
    }

    func testCommandClickRevealsWhatWouldLaunchAndOpensTheRest() async throws {
        let directory = try makeFiles()
        defer { try? FileManager.default.removeItem(at: directory) }
        let names = ["notes.txt", "run.command", "Foo.app", "missing.txt"]
        let feed = names.map { directory.appendingPathComponent($0).absoluteString }.joined(separator: "\r\n")
        let rig = try await rig(rows: 6, cols: 200, feed: feed)
        defer { rig.close() }
        for row in 0..<names.count {
            try rig.perform("down \(row),10 mods=cmd")
            try rig.perform("up mods=cmd")
        }
        XCTAssertEqual(rig.opener.opened.map(\.lastPathComponent), ["notes.txt"])
        XCTAssertEqual(rig.opener.revealed.map(\.lastPathComponent), ["run.command", "Foo.app"])
    }

    func testCommandHoverOverFileLinksNeverLooksThemUpAndAClickDoes() async throws {
        let directory = try makeFiles()
        defer { try? FileManager.default.removeItem(at: directory) }
        let names = ["notes.txt", "run.command", "missing.txt"]
        let feed = names.map { directory.appendingPathComponent($0).absoluteString }.joined(separator: "\r\n")
        let rig = try await rig(rows: 4, cols: 200, feed: feed)
        defer { rig.close() }
        let files = CountingFileManager()
        rig.view.linkFileManager = files
        try rig.perform("flags cmd")
        for row in 0..<names.count {
            for col in [8, 10, 20, 30, 40] {
                try rig.perform("move \(row),\(col) mods=cmd")
                XCTAssertNotNil(rig.view.hoveredLink, "\(names[row]) at column \(col) is underlined")
            }
        }
        try rig.perform("flags none")
        try rig.perform("flags cmd")
        XCTAssertNotNil(rig.view.hoveredLink, "Command pressed again over the link")
        XCTAssertEqual(files.lookups, 0, "a hover never touches the file system")
        for row in 0..<names.count {
            try rig.perform("down \(row),10 mods=cmd")
            try rig.perform("up mods=cmd")
        }
        XCTAssertGreaterThan(files.lookups, 0, "a click looks the file up, through the view's file manager")
        XCTAssertEqual(rig.opener.opened.map(\.lastPathComponent), ["notes.txt"])
        XCTAssertEqual(rig.opener.revealed.map(\.lastPathComponent), ["run.command"])
    }

    // MARK: Click during a composition

    #if DEBUG
        func testAClickWhileTextIsMarkedCommitsItAndThenSelects() async throws {
            let rig = try await rig()
            defer { rig.close() }
            try rig.perform("ime_marked_text あ")
            try rig.perform("down 0,2")
            try rig.perform("drag 0,5")
            try rig.perform("up")
            XCTAssertFalse(rig.view.hasMarkedText(), "the composition ended")
            XCTAssertEqual(
                rig.view.selectionCalls,
                [.begin(col: 2, row: 0, granularity: .character, block: false), .extend(col: 5, row: 0)],
                "and the click went on as a selection")
            let received = try await rig.receivedInput()
            XCTAssertEqual(received, Array("あ".utf8), "the marked text was committed, not dropped")
        }

        func testAClickTheInputMethodUsesGoesNoFurther() async throws {
            let rig = try await rig()
            defer { rig.close() }
            var offered = 0
            rig.view.offersMouseDownToInputContext = { _ in
                offered += 1
                return true
            }
            try rig.perform("ime_marked_text あ")
            try rig.perform("down 0,2")
            try rig.perform("up")
            XCTAssertEqual(offered, 1)
            XCTAssertTrue(rig.view.hasMarkedText(), "the input method keeps its composition")
            XCTAssertEqual(rig.view.selectionCalls, [], "and the click selects nothing")
            XCTAssertEqual(rig.view.sentMouseCount, 0, "nor reaches the core")
            let received = try await rig.receivedInput()
            XCTAssertEqual(received, [])
        }
    #endif
}

/// Counts the file system lookups a link's policy makes through it.
final class CountingFileManager: FileManager, @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    /// `fileExists` and `isExecutableFile` calls so far.
    var lookups: Int { lock.withLock { count } }

    private func counted<T>(_ body: () -> T) -> T {
        lock.withLock { count += 1 }
        return body()
    }

    override func fileExists(atPath path: String) -> Bool {
        counted { super.fileExists(atPath: path) }
    }

    override func fileExists(atPath path: String, isDirectory: UnsafeMutablePointer<ObjCBool>?) -> Bool {
        counted { super.fileExists(atPath: path, isDirectory: isDirectory) }
    }

    override func isExecutableFile(atPath path: String) -> Bool {
        counted { super.isExecutableFile(atPath: path) }
    }
}
