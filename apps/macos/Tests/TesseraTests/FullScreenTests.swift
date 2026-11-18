import AppKit
import CoreGraphics
import IOKit
import Metal
import ObjectiveC
import os
import TesseraKit
import XCTest

@testable import Tessera

/// `[appearance]` (the default profile) is translucent and blurred at 0.8,
/// `clear` translucent without blur, `solid` opaque, `solidblur` opaque
/// with blur asked for (which an opaque session does not get).
private let fullScreenConfig = """
    [appearance]
    opacity = 0.8
    blur = true
    [[profiles]]
    name = "solid"
    [profiles.appearance]
    opacity = 1.0
    [[profiles]]
    name = "solidblur"
    [profiles.appearance]
    opacity = 1.0
    blur = true
    [[profiles]]
    name = "clear"
    [profiles.appearance]
    blur = false
    """

/// What a tab's window shows of its title bar, strip and panes, as the
/// tests compare them windowed and in full screen.
private struct WindowLayout: Equatable, Codable, CustomStringConvertible {
    var fullScreen: Bool
    var fullSizeContent: Bool
    var titlebarTransparent: Bool
    var isOpaque: Bool
    var frame: NSRect
    var bounds: NSRect
    var contentLayoutRect: NSRect
    var panes: NSRect
    /// The blur's and the tint's frames under the title bar, `nil` without.
    var backdrop: NSRect?
    var tint: NSRect?
    var titlebarInset: CGFloat
    var contentMinSize: NSSize

    @MainActor
    init(_ controller: TerminalWindowController) throws {
        let window = try XCTUnwrap(controller.window)
        let content = try XCTUnwrap(window.contentView as? TerminalWindowContentView)
        fullScreen = window.styleMask.contains(.fullScreen)
        fullSizeContent = window.styleMask.contains(.fullSizeContentView)
        titlebarTransparent = window.titlebarAppearsTransparent
        isOpaque = window.isOpaque
        frame = window.frame
        bounds = content.bounds
        contentLayoutRect = window.contentLayoutRect
        panes = controller.paneContainer.frame
        backdrop = content.titlebarBackdrop?.frame
        tint = content.titlebarTint?.frame
        titlebarInset = content.titlebarInset
        contentMinSize = window.contentMinSize
    }

    /// The strip's height, 0 without one.
    var stripHeight: CGFloat { backdrop?.height ?? 0 }

    var description: String {
        "fullScreen=\(fullScreen) fullSizeContent=\(fullSizeContent) titlebarAppearsTransparent=\(titlebarTransparent) "
            + "isOpaque=\(isOpaque) frame=\(frame) contentLayoutRect=\(contentLayoutRect) panes=\(panes) "
            + "strip=\(backdrop.map { "\($0)" } ?? "none") tint=\(tint.map { "\($0)" } ?? "none") "
            + "titlebarInset=\(titlebarInset) contentMinSize=\(contentMinSize)"
    }
}

/// Has chosen windows of ``TerminalWindow`` report `.fullScreen` in their
/// style mask, which AppKit sets only during a real transition (it raises
/// if anyone else sets it) and which a locked console never completes: so
/// the layout that reads it can be checked on every host. The getter is
/// added to ``TerminalWindow`` once and passes every other window's mask
/// through unchanged, on whatever thread AppKit reads it.
private enum ReportedFullScreen {
    static let windows = OSAllocatedUnfairLock<Set<ObjectIdentifier>>(initialState: [])

    static let installed: Bool = {
        let selector = #selector(getter: NSWindow.styleMask)
        guard let method = class_getInstanceMethod(NSWindow.self, selector) else { return false }
        typealias Getter = @convention(c) (AnyObject, Selector) -> UInt
        let original = unsafeBitCast(method_getImplementation(method), to: Getter.self)
        let getter: @convention(block) (AnyObject) -> UInt = { window in
            let mask = original(window, selector)
            let id = ObjectIdentifier(window)
            let reported = windows.withLock { $0.contains(id) }
            return reported ? mask | NSWindow.StyleMask.fullScreen.rawValue : mask
        }
        return class_addMethod(
            TerminalWindow.self, selector, imp_implementationWithBlock(getter), method_getTypeEncoding(method))
    }()

    static func set(_ window: NSWindow, _ fullScreen: Bool) {
        let id = ObjectIdentifier(window)
        windows.withLock {
            if fullScreen {
                $0.insert(id)
            } else {
                $0.remove(id)
            }
        }
    }
}

/// A translucent window's title bar in native full screen (#494, ADR 0043
/// D7): the decision, and the layout that follows it, on a core of the
/// test's own; the windows are never shown. The real transition is
/// ``FullScreenToggleTests``'.
@MainActor
final class FullScreenLayoutTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var host: TransparencyHost?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-fullscreen-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try fullScreenConfig.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        ReportedFullScreen.windows.withLock { $0.removeAll() }
        if let host {
            for controller in host.controllers {
                controller.window?.close()
            }
            host.core.shutdown()
        }
        host = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    func testTheDecisionMakesTheTitlebarOpaqueWithNoStripInFullScreen() {
        typealias Layout = TerminalWindowContentView.TitlebarLayout
        let layout = TerminalWindowContentView.titlebarLayout
        XCTAssertEqual(
            layout(false, true, 68), Layout(titlebarTransparent: true, stripHeight: 68),
            "windowed, translucent: a transparent title bar over the measured strip")
        XCTAssertEqual(
            layout(false, false, 68), Layout(titlebarTransparent: false, stripHeight: 0),
            "windowed, opaque: the standard title bar, no strip")
        XCTAssertEqual(
            layout(true, true, 68), Layout(titlebarTransparent: false, stripHeight: 0),
            "full screen, translucent: an opaque title bar, no strip (D7)")
        XCTAssertEqual(
            layout(true, false, 68), Layout(titlebarTransparent: false, stripHeight: 0),
            "full screen, opaque: the standard title bar, no strip")
        XCTAssertEqual(
            layout(false, true, -4), Layout(titlebarTransparent: true, stripHeight: 0),
            "a content no taller than the panes has no strip")
    }

    /// Two tabs of `profile` in one window (so it has a tab bar), the
    /// first selected.
    private func openTabs(profile: String?) throws -> (TerminalWindowController, TerminalWindowController) {
        let host = try host ?? TransparencyHost(core: TesseraApp())
        self.host = host
        let first = try host.open(profile: profile)
        let second = try host.open(profile: profile)
        let window = try XCTUnwrap(first.window)
        window.addTabbedWindow(try XCTUnwrap(second.window), ordered: .above)
        window.tabGroup?.selectedWindow = window
        guard MTLCreateSystemDefaultDevice() != nil, (first.panes + second.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw XCTSkip("the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        return (first, second)
    }

    /// The window's delegate as AppKit calls it at the end of a transition.
    private func delegateDid(_ entering: Bool, _ window: NSWindow) {
        let name = entering ? NSWindow.didEnterFullScreenNotification : NSWindow.didExitFullScreenNotification
        let notification = Notification(name: name, object: window)
        if entering {
            window.delegate?.windowDidEnterFullScreen?(notification)
        } else {
            window.delegate?.windowDidExitFullScreen?(notification)
        }
    }

    /// The end of a full-screen transition, as AppKit tells the window's
    /// delegate (`windowDidEnterFullScreen`), makes a translucent window's
    /// title bar opaque with no strip under it, in every tab, and its exit
    /// brings the windowed layout back: the window keeps its opacity, its
    /// panes their frames, its smallest size the windowed one.
    func testEnteringFullScreenMakesTheTitlebarOpaqueAndExitingRestoresIt() throws {
        XCTAssertTrue(ReportedFullScreen.installed)
        let (first, second) = try openTabs(profile: nil)
        let window = try XCTUnwrap(first.window)
        let tab = try XCTUnwrap(second.window)
        let windowed = try WindowLayout(first)
        print("FULLSCREEN layout windowed: \(windowed)")
        XCTAssertTrue(windowed.titlebarTransparent, "windowed: a transparent title bar")
        XCTAssertGreaterThan(windowed.stripHeight, 0, "windowed: a strip under the title bar and tab bar")
        XCTAssertFalse(windowed.isOpaque)

        for each in [window, tab] { ReportedFullScreen.set(each, true) }
        delegateDid(true, window)
        let full = try WindowLayout(first)
        print("FULLSCREEN layout full screen: \(full)")
        XCTAssertTrue(full.fullScreen)
        XCTAssertFalse(full.titlebarTransparent, "full screen: the title bar draws its own material (D7)")
        XCTAssertEqual(full.stripHeight, 0, "full screen: no strip under the title bar and tab bar")
        XCTAssertEqual(full.tint?.height, 0, "full screen: no tint either")
        XCTAssertFalse(full.isOpaque, "the panes keep their opacity")
        XCTAssertEqual(full.panes, windowed.panes, "the panes keep their frames")
        XCTAssertEqual(full.contentMinSize, windowed.contentMinSize, "the windowed smallest size is kept")
        XCTAssertFalse(tab.titlebarAppearsTransparent, "the other tab too")

        for each in [window, tab] { ReportedFullScreen.set(each, false) }
        delegateDid(false, window)
        let exited = try WindowLayout(first)
        print("FULLSCREEN layout exited: \(exited)")
        XCTAssertEqual(exited, windowed, "leaving full screen restores the windowed layout")
        XCTAssertTrue(tab.titlebarAppearsTransparent, "the other tab too")
    }

    /// A layout pass in full screen (the transition resizes the window and
    /// moves its layout rect) follows the style mask with no delegate call:
    /// a tab selected or resized in full screen gets the full-screen title
    /// bar, and one out of it the windowed one.
    func testALayoutPassFollowsTheFullScreenStyleMask() throws {
        XCTAssertTrue(ReportedFullScreen.installed)
        let (first, _) = try openTabs(profile: "clear")
        let window = try XCTUnwrap(first.window)
        let windowed = try WindowLayout(first)
        XCTAssertTrue(windowed.titlebarTransparent)
        XCTAssertGreaterThan(windowed.stripHeight, 0)

        ReportedFullScreen.set(window, true)
        window.setContentSize(NSSize(width: 900, height: 560))
        var full = try WindowLayout(first)
        XCTAssertFalse(full.titlebarTransparent, "a resize in full screen: opaque title bar")
        XCTAssertEqual(full.stripHeight, 0, "a resize in full screen: no strip")
        XCTAssertFalse(full.isOpaque)

        ReportedFullScreen.set(window, false)
        window.setContentSize(NSSize(width: 1000, height: 600))
        full = try WindowLayout(first)
        XCTAssertEqual(full, windowed, "a resize out of full screen: the windowed layout")
    }

    /// An opaque window is the standard one in and out of full screen.
    func testAnOpaqueWindowIsUnchangedByFullScreen() throws {
        XCTAssertTrue(ReportedFullScreen.installed)
        let (first, second) = try openTabs(profile: "solid")
        let window = try XCTUnwrap(first.window)
        let windowed = try WindowLayout(first)
        XCTAssertFalse(windowed.titlebarTransparent)
        XCTAssertNil(windowed.backdrop)
        XCTAssertTrue(windowed.isOpaque)
        for each in [window, try XCTUnwrap(second.window)] { ReportedFullScreen.set(each, true) }
        delegateDid(true, window)
        var full = try WindowLayout(first)
        full.fullScreen = false
        XCTAssertEqual(full, windowed, "full screen: the same opaque window")
        for each in [window, try XCTUnwrap(second.window)] { ReportedFullScreen.set(each, false) }
        delegateDid(false, window)
        XCTAssertEqual(try WindowLayout(first), windowed)
    }
}

/// The four layouts of one real transition: windowed, in full screen (the
/// first tab, and the other one once it is selected there), and after it.
private struct ToggleLayouts: Codable {
    var windowed: WindowLayout
    var full: WindowLayout
    var tab: WindowLayout
    var exited: WindowLayout
}

/// Native full screen for real (#494, ADR 0043 D7): a window and a tab
/// opened the way the app opens them, put in full screen with
/// `toggleFullScreen(_:)` as View > Enter Full Screen does, posting nothing,
/// one toggle in and one out per test. The toggles run in a child copy of
/// the test host (``FullScreenToggleChild``): a transition that does not
/// complete ends a few seconds later in an AppKit assertion
/// (`NSWindowStackController`) that kills the process, even after one
/// toggle (macOS 27.2, locked console), and only the child is lost. The
/// test skips, printing why, when the child reports a toggle that did not
/// finish within 5 s or crashes or stalls once started (and every later test
/// then skips too), and before any child on a locked console or a session
/// off the console, where no transition finishes.
@MainActor
final class FullScreenToggleTests: XCTestCase {
    private var dir: URL!

    /// A transition did not finish: no further child is started.
    private static var unavailable: String?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-fullscreen-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try fullScreenConfig.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: dir)
    }

    /// Skips, printing why, when this host cannot make a transition.
    private func skip(_ reason: String) throws -> Never {
        print("FULLSCREEN skipped: \(reason)")
        throw XCTSkip(reason)
    }

    private func requireTransitions() throws {
        if let unavailable = Self.unavailable {
            try skip(unavailable)
        }
        let session = CGSessionCopyCurrentDictionary() as? [String: Any]
        if (session?["CGSSessionScreenIsLocked"] as? Bool) == true || Self.consoleLocked() {
            try skip("the screen is locked: AppKit does not complete a full-screen transition")
        }
        // kCGSessionOnConsoleKey, a CFSTR macro.
        if (session?["kCGSSessionOnConsoleKey"] as? Bool) != true {
            try skip("this login session is not on the console: AppKit does not complete a full-screen transition")
        }
    }

    /// The IO registry's `IOConsoleLocked`: the console is locked.
    private static func consoleLocked() -> Bool {
        let root = IORegistryGetRootEntry(kIOMainPortDefault)
        defer { IOObjectRelease(root) }
        let locked = IORegistryEntryCreateCFProperty(root, "IOConsoleLocked" as CFString, kCFAllocatorDefault, 0)
        return (locked?.takeRetainedValue() as? Bool) == true
    }

    /// One transition of `profile` in a child test host; its layouts,
    /// printed. A child that never starts its work fails the test (XCTest
    /// was not injected into it): only a started child's end is the host's.
    private func enterAndExit(profile: String?, _ what: String) async throws -> ToggleLayouts {
        try requireTransitions()
        let suite = "com.aanyberg.tessera.tests.\(UUID().uuidString)"
        defer { UserDefaults.standard.removePersistentDomain(forName: suite) }
        let request = FullScreenToggleChild.Request(profile: profile, configDir: dir.path, suite: suite)
        try JSONEncoder().encode(request).write(to: dir.appendingPathComponent(FullScreenToggleChild.requestFile))
        let child = try TerminationSignalChild.launch(in: dir)
        defer {
            if child.isRunning { kill(child.processIdentifier, SIGKILL) }
            child.waitUntilExit()
        }
        let launched = ContinuousClock.now
        while child.isRunning, ContinuousClock.now - launched < FullScreenToggleChild.parentDeadline {
            try await Task.sleep(for: .milliseconds(50))
        }
        let stages =
            (try? String(contentsOf: dir.appendingPathComponent(TerminationSignalChild.stagesFile), encoding: .utf8))
            ?? ""
        let resultURL = dir.appendingPathComponent(FullScreenToggleChild.resultFile)
        guard
            let outcome = try? JSONDecoder().decode(
                FullScreenToggleChild.Outcome.self, from: Data(contentsOf: resultURL))
        else {
            let diagnosis = TerminationSignalChild.diagnose(child, in: dir)
            guard stages.contains(FullScreenToggleChild.startedStage) else {
                XCTFail("the child test host never started the transition\n\(diagnosis)")
                throw XCTSkip("no child test host")
            }
            let how = child.isRunning ? "stalled for \(FullScreenToggleChild.parentDeadline)" : "ended"
            let reason = "the child test host \(how) before it reported (\(what))"
            Self.unavailable = reason
            print("FULLSCREEN child diagnosis: \(diagnosis)")
            try skip(reason)
        }
        if let failure = outcome.failure {
            XCTFail("\(what): \(failure)")
            throw XCTSkip("the child test host failed")
        }
        if let reason = outcome.skip {
            if outcome.transitionStalled { Self.unavailable = reason }
            try skip(reason)
        }
        let layouts = try XCTUnwrap(outcome.layouts)
        print("FULLSCREEN \(what) windowed: \(layouts.windowed)")
        print("FULLSCREEN \(what) full screen: \(layouts.full)")
        print("FULLSCREEN \(what) full screen, other tab: \(layouts.tab)")
        print("FULLSCREEN \(what) exited: \(layouts.exited)")
        return layouts
    }

    private func assertFullScreen(
        _ layouts: ToggleLayouts, translucent: Bool, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        let (windowed, full, tab, exited) = (layouts.windowed, layouts.full, layouts.tab, layouts.exited)
        XCTAssertEqual(windowed.titlebarTransparent, translucent, "\(what) windowed", file: file, line: line)
        XCTAssertEqual(windowed.isOpaque, !translucent, "\(what) windowed", file: file, line: line)
        XCTAssertTrue(full.fullScreen, "\(what): in full screen", file: file, line: line)
        XCTAssertFalse(full.titlebarTransparent, "\(what) full screen: an opaque title bar", file: file, line: line)
        XCTAssertEqual(full.stripHeight, 0, "\(what) full screen: no strip", file: file, line: line)
        XCTAssertEqual(full.tint?.height ?? 0, 0, "\(what) full screen: no tint", file: file, line: line)
        XCTAssertEqual(
            full.isOpaque, windowed.isOpaque, "\(what): the panes keep their opacity", file: file, line: line)
        XCTAssertEqual(
            full.panes, full.bounds.intersection(full.contentLayoutRect),
            "\(what) full screen: the panes in the layout rect", file: file, line: line)
        XCTAssertTrue(tab.fullScreen, "\(what): the other tab, selected, is in full screen", file: file, line: line)
        XCTAssertFalse(tab.titlebarTransparent, "\(what) full screen: the other tab", file: file, line: line)
        XCTAssertEqual(tab.stripHeight, 0, "\(what) full screen: the other tab", file: file, line: line)
        XCTAssertEqual(tab.tint?.height ?? 0, 0, "\(what) full screen: the other tab", file: file, line: line)
        XCTAssertEqual(
            tab.isOpaque, windowed.isOpaque, "\(what): the other tab keeps its opacity", file: file, line: line)
        XCTAssertEqual(
            tab.panes, tab.bounds.intersection(tab.contentLayoutRect),
            "\(what) full screen: the other tab's panes in the layout rect", file: file, line: line)
        XCTAssertFalse(exited.fullScreen, "\(what): out of full screen", file: file, line: line)
        XCTAssertEqual(exited, windowed, "\(what): the windowed layout is back", file: file, line: line)
    }

    /// The case the issue names: opacity 0.8, blurred.
    func testATranslucentBlurredWindowHasAnOpaqueTitlebarInFullScreen() async throws {
        let layouts = try await enterAndExit(profile: nil, "opacity 0.8, blur on")
        assertFullScreen(layouts, translucent: true, "opacity 0.8, blur on")
    }

    func testATranslucentUnblurredWindowHasAnOpaqueTitlebarInFullScreen() async throws {
        let layouts = try await enterAndExit(profile: "clear", "opacity 0.8, blur off")
        assertFullScreen(layouts, translucent: true, "opacity 0.8, blur off")
    }

    func testAnOpaqueWindowKeepsItsTitlebarInFullScreen() async throws {
        let layouts = try await enterAndExit(profile: "solid", "opacity 1.0, blur off")
        assertFullScreen(layouts, translucent: false, "opacity 1.0, blur off")
    }

    func testAnOpaqueWindowAskingForBlurKeepsItsTitlebarInFullScreen() async throws {
        let layouts = try await enterAndExit(profile: "solidblur", "opacity 1.0, blur on")
        assertFullScreen(layouts, translucent: false, "opacity 1.0, blur on")
    }
}

/// Set by a notification observer on the main queue, read by the child.
@MainActor
private final class Flag {
    var value = false
}

/// The child side of ``FullScreenToggleTests``: a copy of the test host
/// started by ``TerminationSignalChild/launch(in:)`` with ``requestFile`` in
/// its directory runs this from the test bundle's start
/// (``HostDisplaySettings``) instead of any test, and never returns. It
/// opens a window and a tab as ⌘N and ⌘T do, toggles full screen in,
/// selects the tab and the window again, toggles out, and writes the
/// layouts or why it could not (``resultFile``).
@MainActor
enum FullScreenToggleChild {
    static let requestFile = "fullscreen-request.json"
    static let resultFile = "fullscreen-result.json"
    /// Marked (``TerminationSignalChild/mark(_:)``) once the child runs.
    static let startedStage = "the full-screen child started"
    /// The child's own cap, and the parent's wait for it.
    static let childDeadline: Duration = .seconds(40)
    static let parentDeadline: Duration = .seconds(50)

    struct Request: Codable {
        let profile: String?
        /// Holds the `config.toml` the child reads.
        let configDir: String
        /// The defaults suite the window state is saved to.
        let suite: String
    }

    fileprivate struct Outcome: Codable {
        var layouts: ToggleLayouts?
        /// Why the host cannot make the transition: a skip.
        var skip: String?
        /// The skip is a transition that did not finish.
        var transitionStalled = false
        /// What went wrong before any transition: a failure.
        var failure: String?
    }

    private struct Skip: Error {
        let reason: String
        var stalled = false
    }

    /// Runs the child if this process is one for a full-screen test; then
    /// never returns.
    static func runIfRequested() {
        guard let path = ProcessInfo.processInfo.environment[TerminationSignalChild.directoryVariable] else { return }
        let dir = URL(fileURLWithPath: path)
        let requestURL = dir.appendingPathComponent(requestFile)
        guard FileManager.default.fileExists(atPath: requestURL.path) else { return }
        TerminationSignalChild.mark(startedStage)
        // Never cancelled: the child exits right after it writes its
        // result, so the cap only ends a child that stalled, and it is the
        // child's own process the parent then reports on.
        let seconds = Int(childDeadline.components.seconds)
        DispatchQueue.global().asyncAfter(deadline: .now() + .seconds(seconds)) { _exit(3) }
        var outcome = Outcome()
        do {
            let request = try JSONDecoder().decode(Request.self, from: Data(contentsOf: requestURL))
            outcome.layouts = try run(request)
        } catch let skip as Skip {
            outcome.skip = skip.reason
            outcome.transitionStalled = skip.stalled
        } catch {
            outcome.failure = "\(error)"
        }
        try? JSONEncoder().encode(outcome).write(to: dir.appendingPathComponent(resultFile))
        TerminationSignalChild.mark("the full-screen child reported")
        exit(0)
    }

    private static func run(_ request: Request) throws -> ToggleLayouts {
        setenv("TESSERA_CONFIG_DIR", request.configDir, 1)
        guard let appDelegate = NSApp.delegate as? AppDelegate, let defaults = UserDefaults(suiteName: request.suite)
        else { throw TerminationSignalChild.Failure(description: "no app delegate or defaults suite") }
        appDelegate.windowStateStore = WindowStateStore(defaults: defaults)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        HostDisplaySettings.pin(appDelegate)
        // A fresh host can read overlay scroll bars at launch and legacy
        // ones a moment later (macOS 15 CI), which widens the window's
        // smallest size between the windowed and the exited layouts.
        TerminalScroller.systemStyle = { .legacy }
        try appDelegate.replaceCore(start: true)
        NSApp.activate()
        let first = try appDelegate.newWindow(profile: request.profile)
        guard let window = first.window else { throw TerminationSignalChild.Failure(description: "no window") }
        let second = try appDelegate.newTab(in: window, profile: request.profile)
        window.tabGroup?.selectedWindow = window
        window.makeKeyAndOrderFront(nil)
        guard MTLCreateSystemDefaultDevice() != nil, (first.panes + second.panes).allSatisfy(\.view.surface.isAttached)
        else {
            throw Skip(reason: "the core could not attach a renderer to the views (no GPU can present, or no font)")
        }
        guard window.isVisible, window.windowNumber > 0 else { throw Skip(reason: "the host puts no window on screen") }
        spin(for: .milliseconds(200))
        let windowed = try WindowLayout(first)
        try toggle(window, entering: true)
        TerminationSignalChild.mark("in full screen")
        let full = try WindowLayout(first)
        let tab = try layout(of: second, selectedIn: window)
        try toggle(window, entering: false)
        spin(for: .milliseconds(200))
        TerminationSignalChild.mark("out of full screen")
        return ToggleLayouts(windowed: windowed, full: full, tab: tab, exited: try WindowLayout(first))
    }

    /// `toggleFullScreen(_:)` on `window`, as the View menu sends it, then
    /// up to 5 s for AppKit to finish (observed, never posted), and a turn
    /// for every observer of the end (the window's delegate among them).
    private static func toggle(_ window: NSWindow, entering: Bool) throws {
        let name = entering ? NSWindow.didEnterFullScreenNotification : NSWindow.didExitFullScreenNotification
        let done = Flag()
        let token = NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { _ in
            MainActor.assumeIsolated { done.value = true }
        }
        defer { NotificationCenter.default.removeObserver(token) }
        window.toggleFullScreen(nil)
        let deadline = ContinuousClock.now + .seconds(5)
        while !done.value, ContinuousClock.now < deadline {
            spin(for: .milliseconds(20))
        }
        guard done.value else {
            let inactive = NSApp.isActive ? "" : " (the child was inactive after activate)"
            throw Skip(
                reason: "toggleFullScreen did not \(entering ? "enter" : "exit") full screen within 5 s\(inactive)",
                stalled: true)
        }
        spin(for: .milliseconds(100))
    }

    /// The layout of `controller`'s tab of full-screen `window`, selected as
    /// a click on it selects it; then the window's own tab again. A tab not
    /// shown keeps its windowed frame and style mask until it is selected
    /// (macOS 15), so up to 5 s for AppKit to put each in full screen.
    private static func layout(
        of controller: TerminalWindowController, selectedIn window: NSWindow
    ) throws -> WindowLayout {
        guard let tab = controller.window else { throw TerminationSignalChild.Failure(description: "no tab window") }
        let frame = window.frame
        select(tab, in: window, frame: frame)
        let layout = try WindowLayout(controller)
        select(window, in: window, frame: frame)
        return layout
    }

    /// Selects `tab` of `window`'s group and runs the app until it is in
    /// full screen at `frame` (at most 5 s), and a moment more.
    private static func select(_ tab: NSWindow, in window: NSWindow, frame: NSRect) {
        window.tabGroup?.selectedWindow = tab
        let deadline = ContinuousClock.now + .seconds(5)
        while !(tab.styleMask.contains(.fullScreen) && tab.frame == frame), ContinuousClock.now < deadline {
            spin(for: .milliseconds(20))
        }
        spin(for: .milliseconds(200))
    }

    /// Runs the app's events and main queue for `duration`.
    private static func spin(for duration: Duration) {
        let deadline = ContinuousClock.now + duration
        while ContinuousClock.now < deadline {
            if let event = NSApp.nextEvent(
                matching: .any, until: Date(timeIntervalSinceNow: 0.02), inMode: .default, dequeue: true)
            {
                NSApp.sendEvent(event)
            }
        }
    }
}
