import AppKit
import TesseraKit
import UniformTypeIdentifiers
import os

/// Entry point and application lifecycle: owns the core, the window
/// registry and the signal handling. Each tab is a ``TerminalWindowController``
/// running one session; ``SessionRouter`` delivers the core's events to them.
///
/// The app keeps running with no windows open, like other macOS apps; ⌘N or
/// clicking the Dock icon opens a new one.
@main
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Every open tab, in the order they were opened. Each tab is a window;
    /// AppKit owns their order within a tab bar.
    private(set) var windowControllers: [TerminalWindowController] = []
    /// What new windows and tabs run. Tests replace the login shell here.
    var spawnOptions = TesseraSession.SpawnOptions()
    /// Opens the configuration file and the help page. Tests record instead,
    /// so no editor or browser starts.
    var opener: any ExternalOpener = WorkspaceOpener()
    /// Creates `config.toml` where it is missing and returns its path, for
    /// Settings…: the core's ``TesseraApp/createConfig()`` and
    /// ``TesseraApp/configPath`` unless a test replaces it, so hosted tests
    /// never create a configuration directory or start the core's watch of
    /// it.
    var configFileCreator: (@MainActor () throws -> URL)?

    /// Where Tessera Help goes: the README until there is a help book.
    static let helpURL = URL(string: "https://github.com/aanyberg/tessera#readme")!

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "app")

    private var core: TesseraApp?
    private let router = SessionRouter()
    private var terminationSignals: [any DispatchSourceSignal] = []
    /// Where the next standalone window's top-left corner goes.
    private var cascadePoint = NSPoint.zero

    /// True when the app is the host of an XCTest bundle: no window is opened
    /// at launch, the tests open their own.
    static let isTestHost = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil

    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        MainMenu.install(in: NSApp)
        terminateOnSignals()
        guard !Self.isTestHost else { return }
        openNewWindow(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationWillTerminate(_ notification: Notification) {
        core?.shutdown()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Clicking the Dock icon with no window open opens one. With windows
    /// open (minimized ones included) AppKit's default applies.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        guard windowControllers.isEmpty else { return true }
        openNewWindow(nil)
        return false
    }

    // MARK: Windows and tabs

    /// Opens a standalone window with a new session and returns its
    /// controller.
    ///
    /// - Throws: The core's error if it or the session cannot be started.
    @discardableResult
    func newWindow() throws -> TerminalWindowController {
        let controller = try makeController()
        guard let window = controller.window else { return controller }
        // `.preferred` would put a window that is ordered front into the
        // key window's tab bar; a new window must stand alone.
        window.tabbingMode = .disallowed
        if windowControllers.count == 1 {
            window.center()
            cascadePoint = .zero
        }
        // Each standalone window opens down and to the right of the previous.
        cascadePoint = window.cascadeTopLeft(from: cascadePoint)
        controller.showWindow(nil)
        window.tabbingMode = .preferred
        return controller
    }

    /// Opens a new tab with a new session right after `window` in its tab
    /// bar (for ⌘T, the selected tab), or a standalone window if `window` is
    /// `nil` or not a terminal window, and returns its controller.
    ///
    /// - Throws: The core's error if it or the session cannot be started.
    @discardableResult
    func newTab(in window: NSWindow?) throws -> TerminalWindowController {
        guard let window, windowControllers.contains(where: { $0.window === window }) else {
            return try newWindow()
        }
        let controller = try makeController()
        guard let tab = controller.window else { return controller }
        window.addTabbedWindow(tab, ordered: .above)
        tab.makeKeyAndOrderFront(nil)
        return controller
    }

    /// ⌘N.
    @objc func openNewWindow(_ sender: Any?) {
        do {
            try newWindow()
        } catch {
            presentOpenError(error)
        }
    }

    /// ⌘T: a tab in the key window's tab bar, or in the main window's while a
    /// panel (About, an alert) is key.
    @objc func openNewTab(_ sender: Any?) {
        let terminal = [NSApp.keyWindow, NSApp.mainWindow].lazy.compactMap { $0 }.first { window in
            self.windowControllers.contains { $0.window === window }
        }
        do {
            try newTab(in: terminal)
        } catch {
            presentOpenError(error)
        }
    }

    // MARK: Configuration and help

    /// Settings… (⌘,): opens `config.toml` in the default editor, after the
    /// core has written its commented defaults if the file is missing (and
    /// started applying edits of it live). Replaced by the Preferences window
    /// in #40 (D6). When no application opens it, or opening fails, Finder
    /// shows the file instead, so the user can still find it.
    @objc func openConfigFile(_ sender: Any?) {
        let file: URL
        do {
            file = try configFileCreator?() ?? createCoreConfigFile()
        } catch {
            presentError("Could not open the configuration file", error)
            return
        }
        guard let editor = opener.editor(for: file) else {
            Self.log.error("no application opens \(file.path, privacy: .public); showing it in Finder")
            opener.reveal(file)
            return
        }
        opener.open(file, withApplicationAt: editor) { [weak self] error in
            guard let error, let self else { return }
            Self.log.error("could not open \(file.path, privacy: .public): \(String(describing: error), privacy: .public)")
            self.opener.reveal(file)
        }
    }

    private func createCoreConfigFile() throws -> URL {
        let core = try self.core ?? startCore()
        try core.createConfig()
        return core.configPath
    }

    /// Tessera Help (⌘?): the README, in the default browser.
    @objc func openHelp(_ sender: Any?) {
        if !opener.openURL(Self.helpURL) {
            Self.log.error("could not open \(Self.helpURL.absoluteString, privacy: .public)")
        }
    }

    private func makeController() throws -> TerminalWindowController {
        let core = try self.core ?? startCore()
        let session = try core.spawn(spawnOptions)
        let controller = TerminalWindowController(session: session, title: TerminalWindowController.defaultTitle)
        controller.delegate = self
        // Registered before the main actor can run the session's first
        // delivery, so no event is dropped.
        router.register(controller)
        windowControllers.append(controller)
        return controller
    }

    private func startCore() throws -> TesseraApp {
        let core = try TesseraApp()
        core.delegate = router
        self.core = core
        return core
    }

    private func presentOpenError(_ error: any Error) {
        presentError("Could not open a terminal", error)
    }

    private func presentError(_ message: String, _ error: any Error) {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = message
        alert.informativeText = "\(error)"
        alert.runModal()
    }

    // MARK: Signals

    /// `kill`, Ctrl-C and closing the launching terminal (`SIGTERM`, `SIGINT`,
    /// `SIGHUP`) end the process without `applicationWillTerminate`, which
    /// would orphan a child that ignores `SIGHUP`. Route them through
    /// `NSApp.terminate` so `shutdown()` runs.
    ///
    /// The default action would kill the process before the source's handler
    /// runs, so the process ignores the signal once the source is armed (the
    /// source still sees ignored signals, through kqueue). Ignored
    /// dispositions survive `exec`; sessions still get the defaults because the
    /// core's PTY spawn (portable-pty's `pre_exec`) resets `SIGTERM`, `SIGINT`
    /// and `SIGHUP`, among others, to `SIG_DFL` in the child.
    private func terminateOnSignals() {
        for signalNumber in [SIGTERM, SIGINT, SIGHUP] {
            let source = DispatchSource.makeSignalSource(signal: signalNumber, queue: .main)
            source.setEventHandler {
                MainActor.assumeIsolated { NSApp.terminate(nil) }
            }
            source.resume()
            terminationSignals.append(source)
            signal(signalNumber, SIG_IGN)
        }
    }
}

extension AppDelegate: TerminalWindowControllerDelegate {
    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController) {
        do {
            try newTab(in: controller.window)
        } catch {
            presentOpenError(error)
        }
    }

    func terminalWindowControllerDidClose(_ controller: TerminalWindowController) {
        router.unregister(controller.session)
        windowControllers.removeAll { $0 === controller }
    }
}

/// Opens things outside the app: the configuration file and the help page.
/// What to do when that fails is ``AppDelegate``'s; this only reaches
/// `NSWorkspace`, so tests can stand in for it.
@MainActor
protocol ExternalOpener {
    /// Opens a web page in the default browser; `false` if it could not.
    func openURL(_ url: URL) -> Bool
    /// The application that opens `file`: the one registered for its type,
    /// else the default plain-text editor, as nothing claims `.toml` on a
    /// stock system. `nil` if there is none.
    func editor(for file: URL) -> URL?
    /// Opens `file` with `application`; `completion` runs on the main actor
    /// with the error, if any.
    func open(
        _ file: URL, withApplicationAt application: URL,
        completion: @escaping @MainActor @Sendable ((any Error)?) -> Void)
    /// Shows `file` selected in a Finder window.
    func reveal(_ file: URL)
}

/// ``ExternalOpener`` through `NSWorkspace`.
struct WorkspaceOpener: ExternalOpener {
    func openURL(_ url: URL) -> Bool {
        NSWorkspace.shared.open(url)
    }

    func editor(for file: URL) -> URL? {
        let workspace = NSWorkspace.shared
        return workspace.urlForApplication(toOpen: file) ?? workspace.urlForApplication(toOpen: .plainText)
    }

    func open(
        _ file: URL, withApplicationAt application: URL,
        completion: @escaping @MainActor @Sendable ((any Error)?) -> Void
    ) {
        let configuration = NSWorkspace.OpenConfiguration()
        NSWorkspace.shared.open([file], withApplicationAt: application, configuration: configuration) { _, error in
            Task { @MainActor in completion(error) }
        }
    }

    func reveal(_ file: URL) {
        NSWorkspace.shared.activateFileViewerSelecting([file])
    }
}

