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
    /// Open config.toml: the core's ``TesseraApp/createConfig()`` and
    /// ``TesseraApp/configPath`` unless a test replaces it, so hosted tests
    /// never create a configuration directory or start the core's watch of
    /// it.
    var configFileCreator: (@MainActor () throws -> URL)?
    /// Asks before a busy tab or window closes and before quitting with busy
    /// sessions (ADR 0031). Tests record instead of showing alerts.
    var closeConfirmationPresenter: (any CloseConfirmationPresenter)? = AlertCloseConfirmationPresenter()
    /// Where each window's frame and tab count are saved for the next launch:
    /// the user's defaults, or `nil` when hosting tests, so tests never
    /// overwrite the user's windows (they set a store of their own).
    var windowStateStore: WindowStateStore?

    /// Where Tessera Help goes: the README until there is a help book.
    static let helpURL = URL(string: "https://github.com/aanyberg/tessera#readme")!

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "app")

    private var core: TesseraApp?
    /// The Settings window while it is open (ADR 0044); dropped when it
    /// closes. Internal for tests.
    private(set) var settingsWindowController: SettingsWindowController?
    /// Where the Settings window's frame is saved: its autosave name, or
    /// `nil` when hosting tests, so tests never write the user's defaults.
    var settingsAutosaveName: String? = AppDelegate.isTestHost ? nil : SettingsWindowController.autosaveName
    /// Internal so tests can see which sessions it routes.
    let router = SessionRouter()
    private var terminationSignals: [any DispatchSourceSignal] = []
    /// Where the next standalone window's top-left corner goes.
    private var cascadePoint = NSPoint.zero
    /// A coalesced save of the window state is queued.
    private var windowStateSavePending = false
    /// `[appearance] dim_inactive_panes` as last read from the core, given to
    /// every tab (internal so tests can set it).
    var paneDimming: CGFloat = 0 {
        didSet {
            for controller in windowControllers {
                controller.paneContainer.inactivePaneDimming = paneDimming
            }
        }
    }
    /// `[appearance] blur_radius` as last read from the core, 0 while Reduce
    /// Transparency is on: every window's blur radius (ADR 0045).
    var windowBlurRadius: UInt32 = 0 {
        didSet {
            for controller in windowControllers {
                controller.windowBlurRadius = windowBlurRadius
            }
        }
    }
    /// `[general] remote_indicator` as last read from the core: whether every
    /// tab marks a remote shell in its title (#205).
    var showsRemoteHost = true {
        didSet {
            for controller in windowControllers {
                controller.showsRemoteHost = showsRemoteHost
            }
        }
    }
    /// Set by a termination signal: quit without asking.
    var terminatesWithoutConfirmation = false
    /// Reads the system's Reduce Transparency setting (Accessibility,
    /// Display), which ``applyDisplayOptions()`` hands the core. Tests
    /// replace it.
    var reducesTransparency: @MainActor () -> Bool = {
        NSWorkspace.shared.accessibilityDisplayShouldReduceTransparency
    }
    /// Reads the system's Increase Contrast setting, which
    /// ``applyDisplayOptions()`` hands the core with Reduce Transparency
    /// (ADR 0041). Tests replace it.
    var increasesContrast: @MainActor () -> Bool = {
        NSWorkspace.shared.accessibilityDisplayShouldIncreaseContrast
    }
    /// Reads the system's Reduce Motion setting, which
    /// ``applyDisplayOptions()`` gives every scroll bar (it stays on the
    /// Swift side, ADR 0041). Tests replace it.
    var reducesMotion: @MainActor () -> Bool = {
        NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
    }
    /// The options last handed to the current core, or `nil` before the
    /// first: a notification that changes neither setting makes no call.
    private var appliedDisplayOptions: TesseraApp.DisplayOptions?
    /// How many times the core was handed display options (tests).
    private(set) var displayOptionsCallCount = 0
    /// Follows changes of the system's display settings.
    private var displayOptionsObserver: (any NSObjectProtocol)?
    /// The profile each session was started with, by session id, for the
    /// sessions started with one; the rest have `[general]
    /// default_profile`'s. A split pane starts with its focused pane's
    /// profile. Not saved with the window state (#39).
    private var profileBySession: [UInt64: String] = [:]

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

    /// Runs before any window exists, so every window, sheet and panel the
    /// app shows starts out dark, the default, until the core is started and
    /// its `app_appearance` read (``readAppAppearance()``).
    func applicationWillFinishLaunching(_ notification: Notification) {
        Self.applyAppAppearance(.dark)
    }

    /// The one place that sets the app's appearance (#365, ADR 0043):
    /// `NSApp.appearance` dark (`.darkAqua`), light (`.aqua`) or `nil` for
    /// the system's, which AppKit then follows live with no code here. Every
    /// terminal window, its title and tab bars and blur, the find bar,
    /// Settings, alerts and sheets inherit it; nothing sets a window's own.
    /// A value equal to the current one changes nothing, so a reload that
    /// keeps it redraws no window.
    static func applyAppAppearance(_ value: TesseraApp.AppAppearance) {
        let appearance: NSAppearance? =
            switch value {
            case .dark: NSAppearance(named: .darkAqua)
            case .light: NSAppearance(named: .aqua)
            case .system: nil
            }
        guard NSApp.appearance?.name != appearance?.name else { return }
        NSApp.appearance = appearance
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        MainMenu.install(in: NSApp)
        terminateOnSignals()
        observeDisplayOptions()
        applyDisplayOptions()
        guard !Self.isTestHost else { return }
        let store = WindowStateStore(defaults: .standard)
        windowStateStore = store
        let restored = WindowStateStore.quitKeepsWindows(.standard) && restoreWindows(from: store.load())
        if !restored {
            openNewWindow(nil)
        }
        NSApp.activate(ignoringOtherApps: true)
    }

    /// Cmd-Q, the Dock's Quit, logging out: when any session runs a job, one
    /// alert lists them all (D4b) and Cancel keeps everything open. A
    /// termination signal quits without asking. Once the app quits, the
    /// windows are saved for the next launch.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if !terminatesWithoutConfirmation, let presenter = closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.quit, sessions: windowControllers.flatMap(\.sessions)),
            !presenter.confirmQuit(confirmation)
        {
            return .terminateCancel
        }
        saveWindowState()
        return .terminateNow
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
    /// - Parameter frame: Where the window goes, as restored at launch;
    ///   `nil` cascades it from the previous standalone window.
    /// - Throws: The core's error if it or the session cannot be started.
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    @discardableResult
    func newWindow(frame: NSRect? = nil, profile: String? = nil) throws -> TerminalWindowController {
        let controller = try makeController(profile: profile)
        guard let window = controller.window else { return controller }
        // `.preferred` would put a window that is ordered front into the
        // key window's tab bar; a new window must stand alone.
        window.tabbingMode = .disallowed
        if let frame {
            window.setFrame(frame, display: false)
            // From the zero point the window stays put; the next new window
            // cascades from it.
            cascadePoint = window.cascadeTopLeft(from: .zero)
        } else {
            if windowControllers.count == 1 {
                window.center()
                cascadePoint = .zero
            }
            // Each standalone window opens down and to the right of the
            // previous.
            cascadePoint = window.cascadeTopLeft(from: cascadePoint)
        }
        controller.showWindow(nil)
        window.tabbingMode = .preferred
        return controller
    }

    /// Opens a new tab with a new session right after `window` in its tab
    /// bar (for ⌘T, the selected tab), or a standalone window if `window` is
    /// `nil` or not a terminal window, and returns its controller.
    ///
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    /// - Throws: The core's error if it or the session cannot be started.
    @discardableResult
    func newTab(in window: NSWindow?, profile: String? = nil) throws -> TerminalWindowController {
        guard let window, windowControllers.contains(where: { $0.window === window }) else {
            return try newWindow(profile: profile)
        }
        let controller = try makeController(profile: profile)
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
        openTab(profile: nil)
    }

    /// Shell > New Window with Profile > a profile: the item's
    /// `representedObject` is the profile's name (``MainMenu/setProfiles(_:in:)``).
    @objc func openNewWindowWithProfile(_ sender: Any?) {
        do {
            try newWindow(profile: Self.profileName(of: sender))
        } catch {
            presentOpenError(error)
        }
    }

    /// Shell > New Tab with Profile > a profile, as ⌘T with that profile.
    @objc func openNewTabWithProfile(_ sender: Any?) {
        openTab(profile: Self.profileName(of: sender))
    }

    private func openTab(profile: String?) {
        let terminal = [NSApp.keyWindow, NSApp.mainWindow].lazy.compactMap { $0 }.first { window in
            self.windowControllers.contains { $0.window === window }
        }
        do {
            try newTab(in: terminal, profile: profile)
        } catch {
            presentOpenError(error)
        }
    }

    private static func profileName(of sender: Any?) -> String? {
        (sender as? NSMenuItem)?.representedObject as? String
    }

    /// The profile `session` was started with, `nil` for `[general]
    /// default_profile`'s.
    func profile(of session: TesseraSession) -> String? {
        profileBySession[session.id]
    }

    // MARK: Configuration and help

    /// Settings (⌘,): opens the Settings window (ADR 0044), or brings the
    /// open one to the front. It edits the configuration through the core,
    /// which it starts if no session has yet.
    @objc func openSettings(_ sender: Any?) {
        if let controller = settingsWindowController {
            controller.showWindow(sender)
            controller.window?.makeKeyAndOrderFront(sender)
            return
        }
        if Self.isTestHost, !(ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]?.hasPrefix("/") ?? false) {
            // Never edit the user's own configuration from a test.
            Self.log.error("Settings not opened: a test host needs an absolute TESSERA_CONFIG_DIR")
            return
        }
        let core: TesseraApp
        do {
            core = try self.core ?? startCore()
        } catch {
            presentError("Could not open Settings", error)
            return
        }
        let actions = SettingsActions { [weak self] in self?.openConfigFile(nil) }
        let controller = SettingsWindowController(
            model: SettingsModel(source: core), actions: actions, autosaveName: settingsAutosaveName)
        controller.onClose = { [weak self] in self?.settingsWindowController = nil }
        settingsWindowController = controller
        controller.showWindow(sender)
    }

    /// Open config.toml (Advanced in Settings): opens
    /// `config.toml` in the default editor, after the core has written its
    /// commented defaults if the file is missing (and started applying edits
    /// of it live). When no application opens it, or opening fails, Finder
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

    // MARK: Window state

    /// Recreates the windows of `state` (the one saved at the last quit) with
    /// fresh shells: each at its saved frame, moved onto `screens` (the
    /// visible frames of the screens, the main one first), with its saved
    /// number of tabs (D4c) and each tab's panes (ADR 0035). Returns whether
    /// any window opened; `false` for no state or no window in it, so the
    /// caller opens the usual one.
    @discardableResult
    func restoreWindows(from state: WindowState?, screens: [NSRect] = NSScreen.screens.map(\.visibleFrame)) -> Bool {
        guard let state else { return false }
        var opened = false
        do {
            for saved in state.restorePlan(screens: screens) {
                let first = try newWindow(frame: saved.frame)
                opened = true
                if let layout = saved.layout(ofTab: 0) {
                    try first.restoreLayout(layout)
                }
                guard let window = first.window else { continue }
                for index in 1..<saved.tabCount {
                    let tab = try newTab(in: window.tabGroup?.windows.last ?? window)
                    if let layout = saved.layout(ofTab: index) {
                        try tab.restoreLayout(layout)
                    }
                }
            }
        } catch {
            presentOpenError(error)
        }
        return opened
    }

    /// Each window's frame, tab count and tabs' pane layouts (in tab
    /// order) now, in the order the windows opened. Every tab of a window
    /// counts once, whichever is selected.
    func currentWindowState() -> WindowState {
        var groups: [(frame: CGRect, tabs: [TerminalWindowController])] = []
        var indexByGroup: [ObjectIdentifier: Int] = [:]
        for controller in windowControllers {
            guard let window = controller.window else { continue }
            let group = window.tabGroup.map(ObjectIdentifier.init) ?? ObjectIdentifier(window)
            if let index = indexByGroup[group] {
                groups[index].tabs.append(controller)
            } else {
                indexByGroup[group] = groups.count
                groups.append(((window.tabGroup?.selectedWindow ?? window).frame, [controller]))
            }
        }
        let windows = groups.map { group in
            // Tab bar order, which a drag may have changed; a tab not (yet)
            // in the bar keeps its opening place.
            let order = group.tabs.first?.window?.tabGroup?.windows ?? []
            let tabs = group.tabs.enumerated().sorted { lhs, rhs in
                let left = order.firstIndex { $0 === lhs.element.window } ?? Int.max
                let right = order.firstIndex { $0 === rhs.element.window } ?? Int.max
                return (left, lhs.offset) < (right, rhs.offset)
            }
            return WindowState.Window(
                frame: group.frame, tabCount: tabs.count,
                tabs: tabs.map { WindowState.Tab(tree: $0.element.paneLayout) })
        }
        return WindowState(windows: windows)
    }

    /// Saves ``currentWindowState()`` to ``windowStateStore``, if there is one.
    func saveWindowState() {
        windowStateSavePending = false
        windowStateStore?.save(currentWindowState())
    }

    /// Saves the window state once the main actor is next free: a burst of
    /// tab changes costs one save. A live resize saves once per resize
    /// event, as the main queue runs between them while the mouse tracks.
    private func scheduleWindowStateSave() {
        guard windowStateStore != nil, !windowStateSavePending else { return }
        windowStateSavePending = true
        Task { @MainActor [weak self] in
            self?.saveWindowState()
        }
    }

    private func makeController(profile: String?) throws -> TerminalWindowController {
        let session = try spawn(profile: profile)
        let controller = TerminalWindowController(session: session, title: TerminalWindowController.defaultTitle)
        controller.delegate = self
        controller.paneContainer.inactivePaneDimming = paneDimming
        controller.showsRemoteHost = showsRemoteHost
        controller.windowBlurRadius = windowBlurRadius
        // Registered before the main actor can run the session's first
        // delivery, so no event is dropped.
        router.register(controller, session: session)
        windowControllers.append(controller)
        scheduleWindowStateSave()
        return controller
    }

    /// Starts a session of ``spawnOptions`` with `profile` and records its
    /// profile.
    private func spawn(profile: String?) throws -> TesseraSession {
        let core = try self.core ?? startCore()
        var options = spawnOptions
        options.profile = profile
        let session = try core.spawn(options)
        if let profile {
            profileBySession[session.id] = profile
        }
        return session
    }

    /// Shuts the core down and, with `start`, starts another, which reads
    /// `TESSERA_CONFIG_DIR` as it is now; without, the next session starts
    /// one. Internal for tests, which call it with no session open so the
    /// app's own spawn and reload paths run on a configuration of theirs.
    func replaceCore(start: Bool) throws {
        // The Settings window edits through this core.
        settingsWindowController?.close()
        core?.shutdown()
        core = nil
        if start {
            _ = try startCore()
        }
    }

    private func startCore() throws -> TesseraApp {
        let core = try TesseraApp()
        core.delegate = router
        self.core = core
        router.onConfigurationApplied = { [weak self] in
            self?.readAppAppearance()
            self?.applyKeymap()
            self?.readPaneDimming()
            self?.readBlurRadius()
            self?.readRemoteIndicator()
            self?.applyProfiles()
            self?.settingsWindowController?.model.reload()
        }
        // Before the caller opens the window the core was started for.
        readAppAppearance()
        applyKeymap()
        readPaneDimming()
        readRemoteIndicator()
        applyProfiles()
        appliedDisplayOptions = nil
        applyDisplayOptions()
        return core
    }

    /// Applies the system's display settings: at start and whenever they
    /// change, each read once. Reduce Motion goes to every scroll bar.
    /// Reduce Transparency (ADR 0043) and Increase Contrast (ADR 0041) go
    /// to the core, only when one of them changed: it re-styles the
    /// sessions and reports the change through the router, so every pane
    /// and window follows, and the pane dimming is read again (Increase
    /// Contrast turns it off).
    func applyDisplayOptions() {
        let motion = reducesMotion()
        if TerminalScroller.systemReducesMotion != motion {
            TerminalScroller.systemReducesMotion = motion
            for controller in windowControllers {
                for pane in controller.paneContainer.panes {
                    pane.view.scroller.reducesMotion = motion
                }
            }
        }
        guard let core else { return }
        let options = TesseraApp.DisplayOptions(
            reduceTransparency: reducesTransparency(), increaseContrast: increasesContrast())
        guard options != appliedDisplayOptions else { return }
        do {
            try core.setDisplayOptions(options)
            appliedDisplayOptions = options
            displayOptionsCallCount += 1
        } catch {
            Self.log.error("display options not applied: \(String(describing: error), privacy: .public)")
        }
        readPaneDimming()
        readBlurRadius()
    }

    /// AppKit posts this on the workspace's centre, on the main thread, when
    /// an Accessibility display setting changes.
    private func observeDisplayOptions() {
        guard displayOptionsObserver == nil else { return }
        displayOptionsObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.applyDisplayOptions() }
        }
    }

    /// Gives the profile submenus the core's profiles: at start and after
    /// every configuration the core applies. They keep their previous items
    /// if the profiles cannot be read.
    func applyProfiles() {
        guard let core, let bar = NSApp.mainMenu else { return }
        do {
            MainMenu.setProfiles(try core.profileNames(), in: bar)
        } catch {
            Self.log.error("profiles not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Reads `app_appearance` from the core and applies it to the app: at
    /// start and after every configuration it applies (live reload, a
    /// Settings edit). Keeps the previous appearance if it cannot be read.
    private func readAppAppearance() {
        guard let core else { return }
        do {
            Self.applyAppAppearance(try core.appAppearance())
        } catch {
            Self.log.error("app_appearance not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Reads `dim_inactive_panes` from the core: at start, after every
    /// configuration it applies (live reload) and after the display options
    /// change (0 under Increase Contrast). Keeps the previous amount if it
    /// cannot be read.
    private func readPaneDimming() {
        guard let core else { return }
        do {
            paneDimming = CGFloat(try core.dimInactivePanes())
        } catch {
            Self.log.error("dim_inactive_panes not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Reads `blur_radius` from the core: after the display options are
    /// applied (at start and whenever they change) and after every
    /// configuration it applies (live reload). 0 while Reduce Transparency is
    /// on: the core reports the configured value whatever the display
    /// options, and every window is opaque then anyway (ADR 0043 E8). Keeps
    /// the previous radius if it cannot be read.
    private func readBlurRadius() {
        guard let core else { return }
        do {
            let radius = try core.blurRadius()
            let reduced = appliedDisplayOptions?.reduceTransparency ?? reducesTransparency()
            windowBlurRadius = reduced ? 0 : radius
        } catch {
            Self.log.error("blur_radius not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Reads `[general] remote_indicator` from the configuration in effect: at
    /// start and after every configuration the core applies (live reload).
    /// The core has already replaced an unknown value with `"title"`, with a
    /// warning. Keeps the previous setting if it cannot be read.
    private func readRemoteIndicator() {
        guard let core else { return }
        do {
            showsRemoteHost = try core.configSnapshot()["general.remote_indicator"]?.string != "off"
        } catch {
            Self.log.error("remote_indicator not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Gives the menu bar the core's key map (ADR 0036): at start and after
    /// every configuration the core applies. The menus keep their previous
    /// key equivalents if it cannot be read.
    func applyKeymap() {
        guard let core, let bar = NSApp.mainMenu else { return }
        do {
            MainMenu.apply(try core.keymap(), to: bar)
        } catch {
            Self.log.error("key map not read: \(String(describing: error), privacy: .public)")
        }
    }

    func presentOpenError(_ error: any Error) {
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
            source.setEventHandler { [weak self] in
                MainActor.assumeIsolated {
                    // Nobody may be there to answer an alert.
                    self?.terminatesWithoutConfirmation = true
                    NSApp.terminate(nil)
                }
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

    /// A split's session, of the focused pane's profile.
    func terminalWindowControllerSpawnSession(_ controller: TerminalWindowController) throws -> TesseraSession {
        let session = try spawn(profile: profile(of: controller.focusedPane.session))
        // Registered before the main actor can run the session's first
        // delivery, so no event is dropped.
        router.register(controller, session: session)
        return session
    }

    func terminalWindowController(_ controller: TerminalWindowController, didClosePaneWith session: TesseraSession) {
        router.unregister(session)
        profileBySession[session.id] = nil
    }

    func terminalWindowControllerDidClose(_ controller: TerminalWindowController) {
        for session in controller.sessions {
            router.unregister(session)
            profileBySession[session.id] = nil
        }
        windowControllers.removeAll { $0 === controller }
        scheduleWindowStateSave()
    }

    func terminalWindowControllerDidChangeLayout(_ controller: TerminalWindowController) {
        scheduleWindowStateSave()
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

