import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Selection, Copy and link gestures through the real `TerminalSurfaceView`
/// (#30), driven by `Fixtures/selection_gestures.txt`: each row is run with
/// synthesized `NSEvent`s on a view backed by a real session and core.
///
/// The fixture's `###` layout has its own parser here; a row with a key,
/// event or value it cannot read fails the test, and so does a file with
/// another number of rows than the 40 it holds.
///
/// The view is not in a window: it has no surface and draws nothing, so the
/// core's grid is the size the row asks for, and the metrics are set to 10 x
/// 20 px cells with no padding. The child records its standard input (so a
/// row can say nothing reached the program), prints the row's `feed` text in
/// raw mode and then a title that tells the test the core has parsed it; a
/// `feed` after the first event is printed once the test creates a trigger
/// file. The autoscroll's timer is replaced by a clock the row advances.
///
/// What a row's `sel:` says is checked through what the core makes of it:
/// whether there is a selection and its `text:`. The coordinates themselves
/// are the core's rows in `selection_spec.txt`; ``TerminalSelectionCallTests``
/// holds the calls each gesture makes.
@MainActor
final class TerminalSelectionTests: XCTestCase {
    private static let rowCount = 40

    private var app: TesseraApp!
    private let delegate = TitleRecorder()

    override func setUp() async throws {
        app = try TesseraApp()
        app.delegate = delegate
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    func testEveryGestureRowPasses() async throws {
        let rows = try Self.loadRows()
        XCTAssertEqual(rows.count, Self.rowCount, "the fixture holds \(Self.rowCount) rows")
        var failures: [String] = []
        for row in rows {
            do {
                try await run(row)
            } catch let error as RowFailure {
                failures.append("\(row.id): \(error.message)")
            }
        }
        XCTAssertTrue(failures.isEmpty, "\(failures.count) of \(rows.count) rows failed:\n" + failures.joined(separator: "\n"))
    }

    // MARK: Running a row

    private struct RowFailure: Error {
        var message: String
    }

    private func run(_ row: Row) async throws {
        let rig = try await Rig(app: app, delegate: delegate, row: row)
        defer { rig.close() }
        // The feeds before the first event were printed by the child; the
        // later ones, after the events before them.
        let started = row.steps.firstIndex { if case .event = $0 { true } else { false } } ?? row.steps.count
        for step in row.steps[started...] {
            switch step {
            case .feed(let bytes):
                try await rig.feedLater(bytes)
            case .event(let text):
                try rig.perform(text)
            }
        }
        try await rig.check(row.expectations)
    }

    // MARK: Fixture

    struct Row {
        var id: String
        var rows = 24
        var cols = 80
        var steps: [Step] = []
        var expectations: [(key: String, value: String)] = []
        /// Payloads of `feed` before the first event.
        var initialFeed: [UInt8] {
            var bytes: [UInt8] = []
            for case .feed(let chunk) in steps.prefix(while: { if case .feed = $0 { true } else { false } }) {
                bytes += chunk
            }
            return bytes
        }
    }

    enum Step {
        case feed([UInt8])
        case event(String)
    }

    private static let expectationKeys: Set<String> = [
        "sel", "text", "pasteboard", "cursor", "hover", "opened", "pty_input", "scroll", "copy_enabled",
        "selectall_enabled",
    ]
    private static let metaKeys: Set<String> = ["cite", "note", "decision"]

    static func loadRows(file: StaticString = #filePath) throws -> [Row] {
        let url = URL(fileURLWithPath: "\(file)").deletingLastPathComponent().appendingPathComponent(
            "Fixtures/selection_gestures.txt")
        let text = try String(contentsOf: url, encoding: .utf8)
        var rows: [Row] = []
        for line in text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init) {
            if line.hasPrefix("### ") {
                rows.append(Row(id: String(line.dropFirst(4))))
                continue
            }
            guard !line.hasPrefix("#"), !line.isEmpty, rows.indices.last != nil else { continue }
            guard let colon = line.firstIndex(of: ":") else {
                throw FixtureError("\(rows.last?.id ?? "?"): line without a key: \(line)")
            }
            let key = String(line[..<colon])
            var value = String(line[line.index(after: colon)...])
            if value.hasPrefix(" ") { value.removeFirst() }
            try rows[rows.count - 1].add(key: key, value: value)
        }
        return rows
    }

    struct FixtureError: Error, CustomStringConvertible {
        var description: String
        init(_ description: String) { self.description = description }
    }

    // MARK: Test rig

    /// One row's session, view and recording child.
    @MainActor
    final class Rig {
        let session: TesseraSession
        let view: TerminalSurfaceView
        let delegate: TitleRecorder
        let scheduler = FakeAutoscrollScheduler()
        let opener = RecordingLinkOpener()
        let pasteboard: NSPasteboard
        private let directory: URL
        private var lastPoint = NSPoint.zero
        private var inactive = false
        private var swallowing = false
        private var sentinel: (count: Int, string: String)
        private var stage2: URL { directory.appendingPathComponent("stage2") }
        private var trigger: URL { directory.appendingPathComponent("trigger") }
        private var recording: URL { directory.appendingPathComponent("input") }

        init(app: TesseraApp, delegate: TitleRecorder, row: Row) async throws {
            self.delegate = delegate
            directory = FileManager.default.temporaryDirectory.appendingPathComponent(
                "tessera-selection-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let stage1 = directory.appendingPathComponent("stage1")
            try Data(row.initialFeed).write(to: stage1)
            session = try app.spawn(
                .init(
                    program: "/bin/sh",
                    arguments: [
                        "-c",
                        #"stty raw -echo; cat <&0 > "$4" & cat "$1"; printf '\033]2;sync-1\007'; while [ ! -e "$2" ]; do sleep 0.05; done; cat "$3"; printf '\033]2;sync-2\007'; wait"#,
                        "sh", stage1.path, directory.appendingPathComponent("trigger").path,
                        directory.appendingPathComponent("stage2").path, directory.appendingPathComponent("input").path,
                    ],
                    rows: UInt16(row.rows), cols: UInt16(row.cols)))
            view = TerminalSurfaceView(
                session: session, frame: NSRect(x: 0, y: 0, width: row.cols * 10, height: row.rows * 20))
            view.scroller.preferredStyleOverride = .overlay
            view.metrics = .init(
                widthPx: UInt32(row.cols * 10), heightPx: UInt32(row.rows * 20), scale: 1, cellWidthPx: 10,
                cellHeightPx: 20, baselinePx: 15, paddingPx: 0, rows: UInt16(row.rows), cols: UInt16(row.cols),
                fontSizePt: 13, cursorRow: 0, cursorCol: 0, cursorOnScreen: true, cursorWide: false)
            view.linkOpener = opener
            view.autoscroller.scheduler = scheduler
            pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.\(UUID().uuidString)"))
            pasteboard.clearContents()
            pasteboard.setString("sentinel", forType: .string)
            view.pasteboard = pasteboard
            sentinel = (pasteboard.changeCount, "sentinel")
            try await waitForTitle("sync-1", "the child printed the row's text")
        }

        func close() {
            try? session.close()
            pasteboard.releaseGlobally()
            try? FileManager.default.removeItem(at: directory)
        }

        func waitForTitle(_ title: String, _ what: String) async throws {
            let deadline = ContinuousClock.now + .seconds(10)
            while delegate.titles[session.id] != title {
                guard ContinuousClock.now < deadline else {
                    throw RowFailure(message: "timed out waiting for \(what)")
                }
                try await Task.sleep(for: .milliseconds(10))
            }
        }

        /// Output after the row's first event (the child prints it once the
        /// trigger exists).
        func feedLater(_ bytes: [UInt8]) async throws {
            try Data(bytes).write(to: stage2)
            FileManager.default.createFile(atPath: trigger.path, contents: Data())
            try await waitForTitle("sync-2", "the later output")
        }

        // MARK: Events

        private struct Place {
            var col: Int
            var row: Int?
            var above = false
            var below = false
        }

        private static func flags(_ text: String) throws -> NSEvent.ModifierFlags {
            var flags: NSEvent.ModifierFlags = []
            for name in text.split(separator: ",") {
                switch name {
                case "cmd": flags.insert(.command)
                case "opt": flags.insert(.option)
                case "shift": flags.insert(.shift)
                case "ctrl": flags.insert(.control)
                case "none": break
                default: throw RowFailure(message: "unknown modifier \(name)")
                }
            }
            return flags
        }

        private func place(_ text: String) throws -> Place {
            let parts = text.split(separator: ",")
            guard parts.count == 2, let col = Int(parts[1]) else { throw RowFailure(message: "bad place \(text)") }
            switch parts[0] {
            case "above": return Place(col: col, row: nil, above: true)
            case "below": return Place(col: col, row: nil, below: true)
            default:
                guard let row = Int(parts[0]) else { throw RowFailure(message: "bad place \(text)") }
                return Place(col: col, row: row)
            }
        }

        private func point(_ place: Place) -> NSPoint {
            let x = CGFloat(place.col) * 10 + 5
            if place.above { return NSPoint(x: x, y: view.bounds.height + 5) }
            if place.below { return NSPoint(x: x, y: -5) }
            return NSPoint(x: x, y: view.bounds.height - (CGFloat(place.row ?? 0) * 20 + 10))
        }

        private func mouse(
            _ type: NSEvent.EventType, at point: NSPoint, _ flags: NSEvent.ModifierFlags, clicks: Int = 1
        ) throws -> NSEvent {
            lastPoint = point
            return try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: point, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil,
                    eventNumber: 0, clickCount: clicks, pressure: 1))
        }

        private func flagsChanged(_ flags: NSEvent.ModifierFlags) throws -> NSEvent {
            let cg = try XCTUnwrap(CGEvent(keyboardEventSource: nil, virtualKey: 55, keyDown: true))
            cg.type = .flagsChanged
            var mask = CGEventFlags()
            if flags.contains(.command) { mask.insert(.maskCommand) }
            if flags.contains(.option) { mask.insert(.maskAlternate) }
            if flags.contains(.shift) { mask.insert(.maskShift) }
            if flags.contains(.control) { mask.insert(.maskControl) }
            cg.flags = mask
            return try XCTUnwrap(NSEvent(cgEvent: cg))
        }

        private func key(_ name: String) throws -> NSEvent {
            let (character, code): (String, UInt16)
            switch name {
            case "cmd-c": (character, code) = ("c", 8)
            case "cmd-a": (character, code) = ("a", 0)
            default: throw RowFailure(message: "unknown key \(name)")
            }
            return try XCTUnwrap(
                NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: .command, timestamp: 0, windowNumber: 0,
                    context: nil, characters: character, charactersIgnoringModifiers: character, isARepeat: false,
                    keyCode: code))
        }

        /// Runs one `event:` line.
        func perform(_ text: String) throws {
            var words = text.split(separator: " ", omittingEmptySubsequences: true).map(String.init)
            guard let verb = words.first else { throw RowFailure(message: "empty event") }
            words.removeFirst()
            var flags: NSEvent.ModifierFlags = []
            var clicks = 1
            var arguments: [String] = []
            for word in words {
                if word.hasPrefix("mods=") {
                    flags = try Self.flags(String(word.dropFirst(5)))
                } else if word.hasPrefix("clicks=") {
                    clicks = Int(word.dropFirst(7)) ?? 1
                } else {
                    arguments.append(word)
                }
            }
            switch verb {
            case "down":
                if inactive, !view.acceptsFirstMouse(for: nil) {
                    // The activating click only activates: AppKit delivers
                    // neither it nor its drags and release.
                    swallowing = true
                    inactive = false
                    return
                }
                view.mouseDown(with: try mouse(.leftMouseDown, at: point(place(arguments[0])), flags, clicks: clicks))
            case "drag":
                guard !swallowing else { return }
                view.mouseDragged(with: try mouse(.leftMouseDragged, at: point(place(arguments[0])), flags))
            case "up":
                if swallowing {
                    swallowing = false
                    return
                }
                let at = arguments.first.map { point(try! place($0)) } ?? lastPoint
                view.mouseUp(with: try mouse(.leftMouseUp, at: at, flags))
            case "move":
                view.mouseMoved(with: try mouse(.mouseMoved, at: point(place(arguments[0])), flags))
            case "flags":
                view.flagsChanged(with: try flagsChanged(Self.flags(arguments.first ?? "none")))
            case "wait":
                guard let milliseconds = Int(arguments.first?.dropLast(2) ?? "") else {
                    throw RowFailure(message: "bad wait \(text)")
                }
                scheduler.advance(by: .milliseconds(milliseconds))
            case "key":
                try pressKey(try key(arguments[0]))
            case "ime_marked_text":
                let marked = words.joined(separator: " ")
                view.setMarkedText(
                    marked, selectedRange: NSRange(location: marked.utf16.count, length: 0),
                    replacementRange: NSRange(location: NSNotFound, length: 0))
                if !view.hasMarkedText() { throw RowFailure(message: "the view did not take the marked text") }
            case "window_inactive":
                inactive = true
            default:
                throw RowFailure(message: "unknown event \(verb)")
            }
        }

        /// A key through the menu bar's key equivalents, aimed at the view
        /// (no window is key in the test host), and the view's own `keyDown`
        /// when no item takes it, as AppKit would.
        private func pressKey(_ event: NSEvent) throws {
            let bar = try XCTUnwrap(NSApp.mainMenu)
            let edit = try XCTUnwrap(bar.items.first { $0.submenu?.title == "Edit" }?.submenu)
            for item in edit.items { item.target = view }
            defer { for item in edit.items { item.target = nil } }
            if !bar.performKeyEquivalent(with: event) {
                view.keyDown(with: event)
            }
        }

        // MARK: Expectations

        private func quoted(_ value: String) throws -> String {
            guard value.hasPrefix("\""), value.hasSuffix("\""), value.count >= 2 else {
                throw RowFailure(message: "not a quoted string: \(value)")
            }
            return String(decoding: unescape(String(value.dropFirst().dropLast())), as: UTF8.self)
        }

        private func enabled(_ action: Selector) -> Bool {
            view.validateMenuItem(NSMenuItem(title: "", action: action, keyEquivalent: ""))
        }

        func check(_ expectations: [(key: String, value: String)]) async throws {
            var problems: [String] = []
            func expect<T: Equatable>(_ key: String, _ actual: T, _ expected: T) {
                if actual != expected { problems.append("\(key): want \(expected), got \(actual)") }
            }
            for (key, value) in expectations {
                switch key {
                case "sel":
                    let selected = try session.hasSelection
                    expect("sel", selected, value != "none")
                case "text":
                    let want: String? = value == "none" ? nil : try quoted(value)
                    expect("text", try session.selectionText(), want)
                case "pasteboard":
                    if value == "unchanged" {
                        expect("pasteboard", pasteboard.string(forType: .string), sentinel.string)
                        expect("pasteboard changes", pasteboard.changeCount, sentinel.count)
                    } else {
                        expect("pasteboard", pasteboard.string(forType: .string), try quoted(value))
                    }
                case "cursor":
                    let cursors: [String: NSCursor] = ["arrow": .arrow, "ibeam": .iBeam, "pointing": .pointingHand]
                    guard let want = cursors[value] else { throw RowFailure(message: "unknown cursor \(value)") }
                    expect("cursor", view.textCursor, want)
                case "hover":
                    expect("hover", view.hoveredLink ?? "none", value)
                case "opened":
                    let list = value.dropFirst().dropLast().split(separator: ",").map {
                        $0.trimmingCharacters(in: .whitespaces)
                    }
                    expect("opened", opener.opened.map(\.absoluteString), list)
                case "pty_input":
                    try await expectInput(value, &problems)
                case "scroll":
                    expect("scroll", Int(try session.scrollPosition.offset), Int(value) ?? -1)
                case "copy_enabled":
                    expect("copy_enabled", enabled(#selector(TerminalSurfaceView.copy(_:))), value == "true")
                case "selectall_enabled":
                    expect("selectall_enabled", enabled(#selector(TerminalSurfaceView.selectAll(_:))), value == "true")
                default:
                    throw RowFailure(message: "unknown expectation \(key)")
                }
            }
            if !problems.isEmpty { throw RowFailure(message: problems.joined(separator: "; ")) }
        }

        /// Everything the child received so far: a key sent now arrives after
        /// whatever was sent before, so the recorder holds it all once it
        /// ends with that key.
        func receivedInput() async throws -> [UInt8] {
            try session.send(key: .init(key: .char, codepoint: "~", text: "~"))
            let deadline = ContinuousClock.now + .seconds(10)
            while ContinuousClock.now < deadline {
                let received = (try? Array(Data(contentsOf: recording))) ?? []
                if received.last == UInt8(ascii: "~") { return received.dropLast() }
                try await Task.sleep(for: .milliseconds(10))
            }
            throw RowFailure(message: "the sentinel key never arrived")
        }

        /// `none`: nothing the gesture did reached the child. A key sent
        /// afterwards must be the first and only input the recorder sees.
        private func expectInput(_ value: String, _ problems: inout [String]) async throws {
            #if DEBUG
                if value == "none", !view.sentKeys.isEmpty { problems.append("pty_input: the view sent keys") }
            #endif
            try session.send(key: .init(key: .char, codepoint: "~", text: "~"))
            let deadline = ContinuousClock.now + .seconds(10)
            var received: [UInt8] = []
            repeat {
                received = (try? Array(Data(contentsOf: recording))) ?? []
                if received.last == UInt8(ascii: "~") { break }
                try await Task.sleep(for: .milliseconds(10))
            } while ContinuousClock.now < deadline
            if value == "none", received != [UInt8(ascii: "~")] {
                problems.append("pty_input: the child received \(received)")
            }
            if value == "any", received.count < 2 { problems.append("pty_input: the child received nothing") }
        }
    }

    @MainActor
    final class TitleRecorder: TesseraAppDelegate {
        var titles: [UInt64: String] = [:]
        func sessionDidOutput(_ session: TesseraSession) {}
        func session(_ session: TesseraSession, didSetTitle title: String) { titles[session.id] = title }
        func sessionDidRingBell(_ session: TesseraSession) {}
        func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {}
        func appearanceDidChange(opaque: Bool) {}
    }
}

extension TerminalSelectionTests.Row {
    fileprivate mutating func add(key: String, value: String) throws {
        switch key {
        case "setup":
            if value.hasPrefix("size ") {
                let parts = value.dropFirst(5).split(separator: "x")
                guard parts.count == 2, let rows = Int(parts[0]), let cols = Int(parts[1]) else {
                    throw TerminalSelectionTests.FixtureError("\(id): bad size \(value)")
                }
                self.rows = rows
                self.cols = cols
            } else if value.hasPrefix("feed ") {
                steps.append(.feed(unescape(String(value.dropFirst(5)))))
            } else if value.hasPrefix("scrollback:") {
                // The core's default history (100 lines) holds every row's output.
            } else {
                throw TerminalSelectionTests.FixtureError("\(id): unknown setup \(value)")
            }
        case "event":
            steps.append(.event(value))
        default:
            if ["cite", "note", "decision"].contains(key) { return }
            guard ["sel", "text", "pasteboard", "cursor", "hover", "opened", "pty_input", "scroll", "copy_enabled", "selectall_enabled"].contains(key) else {
                throw TerminalSelectionTests.FixtureError("\(id): unknown key \(key)")
            }
            expectations.append((key, value))
        }
    }
}

/// Bytes of a fixture string: `\xNN`, `\r`, `\n`, `\t`, `\\`, `\"` and
/// `\u{H}` are escapes, everything else is itself.
private func unescape(_ text: String) -> [UInt8] {
    var bytes: [UInt8] = []
    let characters = Array(text.unicodeScalars)
    var i = 0
    while i < characters.count {
        let c = characters[i]
        guard c == "\\", i + 1 < characters.count else {
            bytes += Array(String(c).utf8)
            i += 1
            continue
        }
        let next = characters[i + 1]
        switch next {
        case "r": bytes.append(13); i += 2
        case "n": bytes.append(10); i += 2
        case "t": bytes.append(9); i += 2
        case "\\": bytes.append(92); i += 2
        case "\"": bytes.append(34); i += 2
        case "x" where i + 3 < characters.count:
            let hex = String(String.UnicodeScalarView(characters[(i + 2)...(i + 3)]))
            bytes.append(UInt8(hex, radix: 16) ?? 0)
            i += 4
        case "u" where i + 2 < characters.count && characters[i + 2] == "{":
            let end = characters[(i + 3)...].firstIndex(of: "}") ?? characters.count - 1
            let hex = String(String.UnicodeScalarView(characters[(i + 3)..<end]))
            bytes += Array(String(Unicode.Scalar(UInt32(hex, radix: 16) ?? 0xFFFD) ?? "\u{FFFD}").utf8)
            i = end + 1
        default:
            bytes.append(92)
            i += 1
        }
    }
    return bytes
}

/// A clock and one-shot timer a test advances by hand.
@MainActor
final class FakeAutoscrollScheduler: AutoscrollScheduling {
    private(set) var now = ContinuousClock.Instant.now
    private var pending: (due: ContinuousClock.Instant, action: @MainActor () -> Void)?

    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) {
        pending = (now + delay, action)
    }

    func cancel() { pending = nil }

    var hasPendingTick: Bool { pending != nil }

    /// Fires the pending timer `extra` after it was due.
    func fireLate(by extra: Duration) {
        guard let next = pending else { return }
        pending = nil
        now = next.due + extra
        next.action()
    }

    /// Moves the clock `duration` on, firing every timer that falls due on
    /// the way at its own time.
    func advance(by duration: Duration) {
        let target = now + duration
        while let next = pending, next.due <= target {
            pending = nil
            now = next.due
            next.action()
        }
        now = target
    }
}

@MainActor
final class RecordingLinkOpener: LinkOpening {
    private(set) var opened: [URL] = []
    private(set) var revealed: [URL] = []
    func open(_ url: URL) { opened.append(url) }
    func reveal(_ url: URL) { revealed.append(url) }
}
