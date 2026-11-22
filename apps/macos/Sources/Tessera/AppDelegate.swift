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
    /// The closed Settings window's model while an add or delete of a
    /// profile it made waits for the watch to apply it (#455), or an edit
    /// of `[plugins] enabled` does (#489 48b m1): Settings opened again
    /// before then edits on with it, still locked, instead of a model of
    /// the configuration from before the edit (whose indices are the old
    /// ones, whose toggles would write a list without it). Dropped with the
    /// next configuration applied once nothing waits (applied, or failed),
    /// ``settingsWaitLimit`` after the window closed, or with the core.
    /// Internal for tests, which may give Settings a model of their own
    /// through it.
    var waitingSettingsModel: SettingsModel?
    /// The one timer of ``settingsWaitLimit``, the last close's: it goes
    /// with the waiting model, so a reopened and again closed model waits
    /// its whole limit (#455).
    private var waitingSettingsTimer: SettingsTimer?
    /// An Option-click on a close button is waiting to run Close All
    /// (``terminalWindowControllerTakesOptionClickOnCloseButton()``).
    private var closeAllIsScheduled = false
    /// How long a closed Settings window's model waits for its add or
    /// delete at most. The watch applies an edit within about a second
    /// (ADR 0044); a model that waits longer waits for a watch that will
    /// not apply it, and Settings opened after it starts afresh from the
    /// configuration applied. Internal for tests.
    var settingsWaitLimit: Duration = .seconds(2)
    /// The clock of the Settings window's model, which also times
    /// ``settingsWaitLimit``. Internal for tests, which move a manual one.
    var settingsClock: any SettingsClock = SystemSettingsClock()
    /// Where the Settings window's frame is saved: its autosave name, or
    /// `nil` when hosting tests, so tests never write the user's defaults.
    var settingsAutosaveName: String? = AppDelegate.isTestHost ? nil : SettingsWindowController.autosaveName
    /// Internal so tests can see which sessions it routes.
    let router = SessionRouter()
    /// Marks and posts finished commands (#470, ADR 0048). Internal so
    /// tests can give it a focus and a poster of their own.
    let commandNotifier = CommandNotifier(poster: UserNotificationPoster(center: SystemNotificationCenter()))
    /// The notification centre's delegate, installed at launch and held
    /// here (the centre holds it weakly). Internal for tests.
    private(set) lazy var notificationDelegate = NotificationDelegate { [weak self] sessionID in
        self?.commandNotifier.open(sessionID: sessionID)
    }
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
    /// Every window's blur radius (ADR 0045). 0 in 0.2.0, which keeps each
    /// pane's material blur: the app does not read `[appearance]
    /// blur_radius` (#490) until the rework in 0.3.0 (#491).
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
    /// Set by a termination signal, or by the system's quit Apple event (a
    /// logout) with a confirmation up: quit without asking.
    var terminatesWithoutConfirmation = false
    /// Reads macOS's "Close windows when quitting an application"
    /// (``WindowStateStore/quitKeepsWindows(_:)``) whenever it is needed,
    /// so a change while the app runs applies: at launch, and for the
    /// Quit alternate's title and choice (#464). Tests replace it.
    var quitKeepsWindows: @MainActor () -> Bool = { WindowStateStore.quitKeepsWindows(.standard) }
    /// The choice the Quit alternate (⌥⌘Q) made for this quit alone:
    /// whether the next launch restores the windows. Set from the
    /// alternate's action until its `terminate` returns (the quit was
    /// cancelled or refused), and taken by the quit's save
    /// (``saveWindowState(quitting:)``).
    private var quitRestoresWindowsChoice: Bool?
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
    /// The application's windows front to back as they were when it last
    /// hid, from its hiding to its unhiding: a hidden application's
    /// `NSApp.orderedWindows` lists its windows in the order they opened
    /// (#414). Weak, so a window closed meanwhile is not kept.
    private var windowsStackedAtHide: [WeakWindow]?
    /// Takes and clears ``windowsStackedAtHide``.
    private var hideObservers: [any NSObjectProtocol] = []
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
    /// its `app_appearance` read (``readAppAppearance()``). The quit Apple
    /// event's handler replaces AppKit's here, before the first event is
    /// dispatched (``handleQuitEvent(_:withReplyEvent:)``).
    func applicationWillFinishLaunching(_ notification: Notification) {
        Self.applyAppAppearance(.dark)
        NSAppleEventManager.shared().setEventHandler(
            self, andSelector: #selector(handleQuitEvent(_:withReplyEvent:)),
            forEventClass: AEEventClass(kCoreEventClass), andEventID: AEEventID(kAEQuitApplication))
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
        installNotificationDelegate()
        terminateOnSignals()
        observeDisplayOptions()
        observeHiding()
        applyDisplayOptions()
        guard !Self.isTestHost else { return }
        let store = WindowStateStore(defaults: .standard)
        windowStateStore = store
        if !restoreWindowsAtLaunch(from: store) {
            openNewWindow(nil)
        }
        NSApp.activate(ignoringOtherApps: true)
    }

    /// Cmd-Q, the Dock's Quit, logging out: when any session runs a job, one
    /// alert lists them all (D4b) and Cancel keeps everything open. A
    /// termination signal quits without asking, also when it arrives while
    /// that alert is up: it ends the alert (``terminateWithoutAsking()``),
    /// and the quit goes on rather than being cancelled; so does the
    /// system's quit Apple event (a logout) that arrives while it is up.
    /// Once the app quits, the windows are saved for the next launch, with
    /// the Quit alternate's choice (⌥⌘Q) if it asked to quit
    /// (``quitWithTheOtherWindowChoice(_:)`` drops that choice on Cancel).
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if !terminatesWithoutConfirmation, let presenter = closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.quit, sessions: windowControllers.flatMap(\.sessions)),
            !confirmQuit(confirmation, with: presenter), !terminatesWithoutConfirmation
        {
            return .terminateCancel
        }
        saveWindowState(quitting: true)
        return .terminateNow
    }

    /// Quit and Keep Windows, or Quit and Close All Windows while macOS
    /// keeps windows on quit (⌥⌘Q, Quit's alternate; AppKit's titles):
    /// quits as ⌘Q does, the one alert included (D4b), and the next launch
    /// restores the windows if the setting says not to, or restores none if
    /// it says to; the launch after follows the setting again. `terminate`
    /// returns only if the app does not quit (Cancel, or a refusal while a
    /// sheet is up), and the choice is dropped then, so a later ⌘Q makes
    /// none.
    @objc func quitWithTheOtherWindowChoice(_ sender: Any?) {
        quitRestoresWindowsChoice = !quitKeepsWindows()
        defer { quitRestoresWindowsChoice = nil }
        NSApp.terminate(sender)
    }

    /// The Quit alternate's title for the setting
    /// (``WindowStateStore/quitKeepsWindows(_:)``): AppKit's own wording.
    static func quitAlternateTitle(quitKeepsWindows: Bool) -> String {
        quitKeepsWindows ? "Quit and Close All Windows" : "Quit and Keep Windows"
    }

    /// Restores the windows saved at the last quit, if this launch restores
    /// them (``WindowStateStore/takeRestoresAtLaunch(setting:)``, which
    /// takes the Quit alternate's choice); whether any window opened.
    func restoreWindowsAtLaunch(from store: WindowStateStore) -> Bool {
        store.takeRestoresAtLaunch(setting: quitKeepsWindows()) && restoreWindows(from: store.load())
    }

    /// The quit alert asks (``confirmQuit(_:with:)``): a quit Apple event
    /// that arrives meanwhile leaves it asking.
    private var asksToQuit = false

    private func confirmQuit(_ confirmation: CloseConfirmation, with presenter: any CloseConfirmationPresenter) -> Bool {
        asksToQuit = true
        defer { asksToQuit = false }
        return presenter.confirmApplicationModally(confirmation)
    }

    /// The app came to the front (AppKit posts
    /// `NSApplication.didBecomeActiveNotification`, which reaches its
    /// delegate here): a marked pane that has the keyboard now loses its
    /// mark and its notification (D10), also when the key window's
    /// `windowDidBecomeKey` ran before the app read as active. Only the key
    /// window's panes can have it, so asking every window is the same.
    func applicationDidBecomeActive(_ notification: Notification) {
        for controller in windowControllers {
            commandNotifier.focusMayHaveChanged(in: controller)
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        commandNotifier.removeAllDelivered()
        core?.shutdown()
    }

    /// Wires the command notifier and installs the notification centre's
    /// delegate before launching finishes, so a click on a notification
    /// that launched the app reaches it (U02). Asks for no permission.
    private func installNotificationDelegate() {
        commandNotifier.controllerForSession = { [weak self] id in self?.router.controller(forSessionID: id) }
        router.onCommandFinished = { [weak self] session, finish, controller in
            guard let self, let core else { return }
            commandNotifier.commandDidFinish(finish, of: session, in: controller) { duration, focused in
                core.commandFinishedDecision(durationMilliseconds: duration, focused: focused)
            }
        }
        (commandNotifier.poster as? UserNotificationPoster)?.install(notificationDelegate)
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

    /// Close All (⌥⌘W, Close Tab's alternate, which takes the place of
    /// AppKit's own, #464): closes every terminal window with all its tabs,
    /// and the Settings window. If any session runs a job, one
    /// application-modal alert lists them all first (D4b; the Settings
    /// window asks nothing), and Cancel closes nothing. Ignored while a tab
    /// asks about itself, a pane or its window, as ⇧⌘W is. A termination
    /// signal or the system's quit while the alert asks ends it, and the
    /// app quits without asking (``terminateWithoutAsking()``). An
    /// Option-click on a window's close button comes here too
    /// (``terminalWindowControllerTakesOptionClickOnCloseButton()``).
    @objc func closeAllWindows(_ sender: Any?) {
        guard !windowControllers.contains(where: { $0.pendingClose != nil }) else { return }
        if let presenter = closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.allWindows, sessions: windowControllers.flatMap(\.sessions))
        {
            let confirmed = presenter.confirmApplicationModally(confirmation)
            // The alert was aborted for a quit that asks nothing: the
            // windows stay, to be saved, and the app quits.
            if terminatesWithoutConfirmation {
                NSApp.terminate(sender)
                return
            }
            guard confirmed else { return }
        }
        // `close()`, not `performClose(_:)`: the user has been asked once
        // for all of them, or nothing in them runs.
        for window in windowControllers.compactMap(\.window) {
            window.close()
        }
        closeSettingsWindow()
    }

    /// Closes the Settings window, a sheet on it (an open panel) ended as
    /// Cancel first, as a quit ends one (``handleQuitEvent(_:withReplyEvent:)``):
    /// it would otherwise outlive the window it asks for.
    private func closeSettingsWindow() {
        guard let controller = settingsWindowController else { return }
        if let window = controller.window {
            for sheet in window.sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
        }
        controller.close()
    }

    /// Whether `event` lets go of an Option-click on a window's close
    /// button. In full screen the button is in a window of AppKit's own, so
    /// every window's button is compared with where the event happened.
    /// The close buttons of the tab bar's tabs are other buttons: Option
    /// there still means AppKit's Close Other Tabs.
    static func isOptionClickOnCloseButton(_ event: NSEvent?) -> Bool {
        guard let event, event.type == .leftMouseUp, event.modifierFlags.contains(.option),
            let clicked = event.window
        else { return false }
        return NSApp.windows.contains { window in
            guard let button = window.standardWindowButton(.closeButton), button.window === clicked else {
                return false
            }
            return button.bounds.contains(button.convert(event.locationInWindow, from: nil))
        }
    }

    /// Runs the Close All an Option-click on a close button asked for.
    @objc private func runScheduledCloseAll() {
        closeAllIsScheduled = false
        closeAllWindows(nil)
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
            // The host may have started or stopped a plugin since the pane
            // last read them (#489 48b m6).
            controller.model.readPluginsAgain()
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
        let waiting = takeWaitingSettingsModel()
        let controller = SettingsWindowController(
            model: waiting ?? SettingsModel(source: core, clock: settingsClock), actions: actions,
            autosaveName: settingsAutosaveName)
        controller.onClose = { [weak self, weak controller] in
            guard let self else { return }
            self.settingsWindowController = nil
            guard let model = controller?.model, Self.waitsForApply(model) else { return }
            self.waitingSettingsModel = model
            // No other timer runs: this window's model was taken, with
            // its timer, when the window opened.
            self.waitingSettingsTimer = model.clock.schedule(after: self.settingsWaitLimit) { [weak self] in
                _ = self?.takeWaitingSettingsModel()
            }
        }
        controller.takesOptionClickOnCloseButton = { [weak self] in
            self?.terminalWindowControllerTakesOptionClickOnCloseButton() ?? false
        }
        settingsWindowController = controller
        controller.showWindow(sender)
    }

    /// A configuration was applied: the closed Settings window's waiting
    /// model reads it, and goes once its add or delete no longer waits.
    private func reloadWaitingSettingsModel() {
        guard let model = waitingSettingsModel else { return }
        model.reload()
        if !Self.waitsForApply(model) { _ = takeWaitingSettingsModel() }
    }

    /// Whether `model` has an edit that a closed window keeps it for: an
    /// add or delete of a profile, or an edit of `[plugins] enabled`, not
    /// yet applied, or an edit of a profile not yet written (one that
    /// meets a hand edit is written again later by this model, so Settings
    /// opened again must edit on with it, #549).
    private static func waitsForApply(_ model: SettingsModel) -> Bool {
        model.profilesChanging || model.pluginsChanging || model.profileEditsUnwritten
    }

    /// The closed Settings window's waiting model, which the app delegate
    /// lets go of, with its timer.
    private func takeWaitingSettingsModel() -> SettingsModel? {
        waitingSettingsTimer?.cancel()
        waitingSettingsTimer = nil
        defer { waitingSettingsModel = nil }
        return waitingSettingsModel
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
    /// number of tabs (D4c), each tab's panes (ADR 0035) and its selected
    /// tab, stacked as they were: the state lists them front to back, so the
    /// back one opens first and the front one last, which leaves it in front
    /// and key (#414). Returns whether any window opened; `false` for no
    /// state or no window in it, so the caller opens the usual one.
    @discardableResult
    func restoreWindows(from state: WindowState?, screens: [NSRect] = NSScreen.screens.map(\.visibleFrame)) -> Bool {
        guard let state else { return false }
        var opened = false
        do {
            for saved in state.restorePlan(screens: screens).reversed() {
                let first = try newWindow(frame: saved.frame)
                opened = true
                if let layout = saved.layout(ofTab: 0) {
                    try first.restoreLayout(layout)
                }
                guard let window = first.window as? TerminalWindow else { continue }
                for index in 1..<saved.tabCount {
                    let tab = try newTab(in: window.tabGroup?.windows.last ?? window)
                    if let layout = saved.layout(ofTab: index) {
                        try tab.restoreLayout(layout)
                    }
                }
                // Each new tab was selected as it opened, so the last one is.
                if let selected = saved.selectedTab {
                    window.selectTab(at: selected)
                }
            }
        } catch {
            presentOpenError(error)
        }
        return opened
    }

    /// Each window's frame, tab count, tabs' pane layouts (in tab order)
    /// and selected tab now, front to back by `orderedWindows`, by default
    /// the application's windows front to back (``windowsFrontToBack()``,
    /// which lists minimized ones after those on screen): a window whose
    /// selected tab is not among them (ordered out, or opened while the
    /// application was hidden) comes after those that are, in the order the
    /// windows opened. Every tab of a window counts once, whichever is
    /// selected.
    func currentWindowState(orderedWindows: [NSWindow]? = nil) -> WindowState {
        let orderedWindows = orderedWindows ?? windowsFrontToBack()
        var groups: [(front: NSWindow, tabs: [TerminalWindowController])] = []
        var indexByGroup: [ObjectIdentifier: Int] = [:]
        for controller in windowControllers {
            guard let window = controller.window else { continue }
            let group = window.tabGroup.map(ObjectIdentifier.init) ?? ObjectIdentifier(window)
            if let index = indexByGroup[group] {
                groups[index].tabs.append(controller)
            } else {
                indexByGroup[group] = groups.count
                groups.append((window.tabGroup?.selectedWindow ?? window, [controller]))
            }
        }
        var depth: [ObjectIdentifier: Int] = [:]
        for (index, window) in orderedWindows.enumerated() where depth[ObjectIdentifier(window)] == nil {
            depth[ObjectIdentifier(window)] = index
        }
        let stacked = groups.enumerated().sorted { lhs, rhs in
            let left = depth[ObjectIdentifier(lhs.element.front)] ?? Int.max
            let right = depth[ObjectIdentifier(rhs.element.front)] ?? Int.max
            return (left, lhs.offset) < (right, rhs.offset)
        }
        let windows = stacked.map(\.element).map { group in
            // Tab bar order, which a drag may have changed; a tab not (yet)
            // in the bar keeps its opening place.
            let order = group.tabs.first?.window?.tabGroup?.windows ?? []
            let tabs = group.tabs.enumerated().sorted { lhs, rhs in
                let left = order.firstIndex { $0 === lhs.element.window } ?? Int.max
                let right = order.firstIndex { $0 === rhs.element.window } ?? Int.max
                return (left, lhs.offset) < (right, rhs.offset)
            }
            return WindowState.Window(
                frame: group.front.frame, tabCount: tabs.count,
                tabs: tabs.map { WindowState.Tab(tree: $0.element.paneLayout) },
                selectedTab: tabs.firstIndex { $0.element.window === group.front })
        }
        return WindowState(windows: windows)
    }

    /// The application's windows front to back: `NSApp.orderedWindows`,
    /// or while the application is hidden, the order they had when it hid.
    private func windowsFrontToBack() -> [NSWindow] {
        windowsStackedAtHide?.compactMap(\.window) ?? NSApp.orderedWindows
    }

    /// At `willHide` the window list still has the windows' stacking; from
    /// `didUnhide` on it has it again.
    private func observeHiding() {
        guard hideObservers.isEmpty else { return }
        let center = NotificationCenter.default
        hideObservers = [
            center.addObserver(forName: NSApplication.willHideNotification, object: NSApp, queue: .main) {
                [weak self] _ in
                MainActor.assumeIsolated { self?.windowsStackedAtHide = NSApp.orderedWindows.map { WeakWindow(window: $0) } }
            },
            center.addObserver(forName: NSApplication.didUnhideNotification, object: NSApp, queue: .main) {
                [weak self] _ in
                MainActor.assumeIsolated { self?.windowsStackedAtHide = nil }
            },
        ]
    }

    /// Saves ``currentWindowState()`` to ``windowStateStore``, if there is
    /// one. `quitting`, for the save of a quit, also takes the Quit
    /// alternate's choice and records it for the next launch; no other save
    /// does, so a save while the quit alert asks cannot leave it behind.
    func saveWindowState(quitting: Bool = false) {
        windowStateSavePending = false
        windowStateStore?.save(currentWindowState())
        guard quitting, let choice = quitRestoresWindowsChoice else { return }
        quitRestoresWindowsChoice = nil
        windowStateStore?.setRestoresAtNextLaunch(choice)
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
        _ = takeWaitingSettingsModel()
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
            self?.readRemoteIndicator()
            self?.applyProfiles()
            self?.settingsWindowController?.model.reload()
            self?.reloadWaitingSettingsModel()
        }
        router.onConfigurationInvalid = { [weak self] in
            // Nothing else changed: the configuration in effect stays.
            self?.settingsWindowController?.model.reload()
            self?.reloadWaitingSettingsModel()
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

    /// How long ``terminateWithoutAsking()`` waits for `NSApp.terminate` to
    /// end the app before ``exitAfterTerminationGrace()`` ends it itself.
    static let terminationGrace: DispatchTimeInterval = .seconds(1)

    /// `kill`, Ctrl-C and closing the launching terminal (`SIGTERM`, `SIGINT`,
    /// `SIGHUP`) end the process without `applicationWillTerminate`, which
    /// would orphan a child that ignores `SIGHUP`. Route them through
    /// ``terminateWithoutAsking()`` so `shutdown()` runs.
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
                    self?.terminateWithoutAsking()
                }
            }
            source.resume()
            terminationSignals.append(source)
            signal(signalNumber, SIG_IGN)
        }
    }

    /// Quits for a termination signal (nobody may be there to answer an
    /// alert, #441) or the system's quit Apple event that arrives with a
    /// confirmation up (#458). `NSApp.terminate` does nothing while a window has a
    /// sheet (a close confirmation: `applicationShouldTerminate` is never
    /// asked), so every sheet is ended first as Cancel. An app-modal alert
    /// (the quit confirmation, an error) is aborted instead: when it is the
    /// quit confirmation, the `terminate` that showed it goes on and quits,
    /// so this call does not start a second, nested one (that one would end
    /// the app without saving the windows). Should the app still run after
    /// ``terminationGrace`` (another alert was aborted, or something else
    /// refused), it saves the windows, shuts the core down and exits on its
    /// own.
    func terminateWithoutAsking() {
        terminatesWithoutConfirmation = true
        for window in NSApp.windows {
            for sheet in window.sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
        }
        if NSApp.modalWindow != nil {
            NSApp.abortModal()
        } else {
            NSApp.terminate(nil)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.terminationGrace) { [weak self] in
            MainActor.assumeIsolated {
                self?.exitAfterTerminationGrace()
            }
        }
    }

    /// The fallback of ``terminateWithoutAsking()``: what quitting would
    /// have done (the window state, `shutdown()`, which ends the sessions'
    /// children), then the process ends.
    private func exitAfterTerminationGrace() {
        Self.log.error("terminate did not end the app in time; exiting")
        saveWindowState(quitting: true)
        core?.shutdown()
        exit(0)
    }
}

// MARK: Quit Apple event

extension AppDelegate {
    /// The quit Apple event (#458). AppKit's own handler calls `terminate`,
    /// which does nothing while a window has a sheet: with a close
    /// confirmation up the event was ignored, and a logout waited on the
    /// app. This one replaces it:
    ///
    /// - The system's quit (a logout, restart or shutdown, which the user
    ///   has already confirmed; ``isSystemQuit(_:)``) with a confirmation
    ///   up, a sheet or an application-modal alert such as the quit alert
    ///   itself: the question is left unanswered and the system wins. It is
    ///   ended and the app quits without asking again, the windows saved,
    ///   as a signal quits (``terminateWithoutAsking()``).
    /// - Any other quit (the Dock's Quit, `NSRunningApplication.terminate()`,
    ///   AppleScript's `quit`), or the system's with nothing up: every sheet
    ///   is ended as Cancel and `terminate` asks as ⌘Q does, one alert for
    ///   every busy session (D4b), since quitting ends more than the sheet
    ///   asked about; with idle sessions it quits. That holds with another
    ///   application-modal alert up too (an error), as with AppKit's
    ///   handler. Only while the quit alert itself asks is nothing more
    ///   done.
    ///
    /// When the app is still running afterwards, the event is answered with
    /// `userCanceledErr`, as AppKit's handler answers a cancelled quit, so a
    /// logout stops rather than waiting on the app.
    @objc func handleQuitEvent(_ event: NSAppleEventDescriptor, withReplyEvent reply: NSAppleEventDescriptor) {
        let modal = NSApp.modalWindow != nil
        let sheets = NSApp.windows.flatMap { window in window.sheets.map { (window, $0) } }
        if Self.isSystemQuit(event), modal || !sheets.isEmpty {
            return terminateWithoutAsking()
        }
        if !asksToQuit {
            for (window, sheet) in sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
            NSApp.terminate(nil)
        }
        if reply.descriptorType != typeNull {
            reply.setParam(NSAppleEventDescriptor(int32: Int32(userCanceledErr)), forKeyword: keyErrorNumber)
        }
    }

    /// Whether a quit Apple event is the system's: a logout, restart or
    /// shutdown names its reason (`kAEQuitReason`, a parameter, though
    /// commonly read as an attribute; both are read), which the Dock's
    /// Quit, `NSRunningApplication.terminate()` and AppleScript's `quit`
    /// do not.
    static func isSystemQuit(_ event: NSAppleEventDescriptor) -> Bool {
        let reason = AEKeyword(kAEQuitReason)
        return event.paramDescriptor(forKeyword: reason) != nil || event.attributeDescriptor(forKeyword: reason) != nil
    }
}

extension AppDelegate: NSMenuItemValidation {
    /// The Quit alternate's title follows the setting each time the menu
    /// opens or a key equivalent is matched, so a change while the app runs
    /// (System Settings, `defaults write`) shows at once. Every item is
    /// enabled, as without this.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(quitWithTheOtherWindowChoice(_:)) {
            menuItem.title = Self.quitAlternateTitle(quitKeepsWindows: quitKeepsWindows())
        }
        return true
    }
}

extension AppDelegate: TerminalWindowControllerDelegate {
    /// An Option-click on a close button is Close All: one question for
    /// every window (ADR 0031). AppKit answers the click by offering each
    /// window its close in turn, every tab and Settings through
    /// `windowShouldClose`, which asks here: each is refused, and Close All
    /// runs once, after AppKit's round. Later, by a timer rather than the
    /// main queue: under a block of that queue the alert's modal loop would
    /// run nothing else of it, signals and session output included.
    func terminalWindowControllerTakesOptionClickOnCloseButton() -> Bool {
        guard Self.isOptionClickOnCloseButton(NSApp.currentEvent) else { return false }
        if !closeAllIsScheduled {
            closeAllIsScheduled = true
            perform(#selector(runScheduledCloseAll), with: nil, afterDelay: 0)
        }
        return true
    }

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

    func terminalWindowControllerFocusMayHaveChanged(_ controller: TerminalWindowController) {
        commandNotifier.focusMayHaveChanged(in: controller)
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

/// A window not kept alive by being listed.
private struct WeakWindow {
    weak var window: NSWindow?
}
