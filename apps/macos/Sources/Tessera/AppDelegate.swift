import AppKit
import TesseraKit
import UniformTypeIdentifiers
import os

/// The application object: `NSApplication` but for AppKit's own Close All,
/// `closeAll:`. `NSApplication` answers that action itself, ahead of its
/// delegate in the responder chain, by offering every window its close in
/// turn, so each busy tab asked on a sheet of its own. Here it is
/// ``AppDelegate/closeAllWindows(_:)``, the single Close All (#526, ADR 0031):
/// one question for every window, and a sheet on Settings ended first.
/// The close button sends it on an Option-click, as would AppKit's own
/// Close All item (Tessera's ⌥⌘W takes its place, #464), a script or an
/// accessibility client; AppKit sends it neither at launch nor on quit.
/// `closeAll:` is not in the SDK's headers, so this is an Objective-C
/// override by selector.
@MainActor
final class TesseraApplication: NSApplication {
    @objc(closeAll:) func closeAllWindows(_ sender: Any?) {
        (delegate as? AppDelegate)?.closeAllFromAppKit(sender)
    }
}

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
    /// Whether sessions start off the main thread (#722,
    /// ``TesseraApp/spawnInBackground(_:completion:)``): a new window or tab shows its
    /// placeholder until its session runs. Tests that need a terminal open
    /// when the call returns turn it off; the sessions then start on the
    /// main thread, as ``newWindow(frame:profile:)`` and
    /// ``newTab(in:profile:)`` always start theirs.
    var spawnsInBackground = true
    /// Windows and tabs whose first session is starting in the background,
    /// in the order they opened (#722). Each shows ``SpawnPlaceholderView``
    /// and becomes a ``TerminalWindowController``'s once its session runs.
    private(set) var pendingTerminals: [PendingTerminal] = []
    /// Sessions starting in the background (tests).
    private(set) var spawnsInFlight = 0
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
    /// (``closeAllFromAppKit(_:)``).
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
    /// The saved layouts of restored tabs whose other panes' sessions are
    /// still starting (``TerminalWindowController/beginRestoringLayout(_:completion:)``),
    /// by controller: the window state saves these, not the one pane such a
    /// tab has meanwhile.
    private var restoringLayouts: [ObjectIdentifier: PaneLayout] = [:]
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
    /// Transparency is on: every window's blur (#491, ADR 0045).
    var windowBlurRadius: UInt32 = WindowBlurRadius.defaultRadius {
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
    /// Where programs' clipboard writes (OSC 52) go: the general pasteboard.
    /// Tests set a named one, so the user's clipboard is never changed.
    var programPasteboard: NSPasteboard = .general {
        didSet { core?.pasteboard = programPasteboard }
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

    /// Creates the application object as a ``TesseraApplication``, before
    /// anything else asks for `NSApp`; `NSPrincipalClass` is not read, as
    /// nothing calls `NSApplicationMain`.
    static func main() {
        let app = TesseraApplication.shared
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
        guard windowControllers.isEmpty, pendingTerminals.isEmpty else { return true }
        openNewWindow(nil)
        return false
    }

    // MARK: Windows and tabs

    /// Opens a standalone window with a new session and returns its
    /// controller, the session started on the main thread (tests; the app
    /// opens its windows with ``openWindow(frame:profile:completion:)``).
    ///
    /// - Parameter frame: Where the window goes, as restored at launch;
    ///   `nil` cascades it from the previous standalone window.
    /// - Throws: The core's error if it or the session cannot be started.
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    @discardableResult
    func newWindow(frame: NSRect? = nil, profile: String? = nil) throws -> TerminalWindowController {
        try onMainThread { openWindow(frame: frame, profile: profile, completion: $0) }
    }

    /// Opens a new tab with a new session right after `window` in its tab
    /// bar, as ``openTab(in:profile:completion:)``, and returns its
    /// controller, the session started on the main thread (tests).
    ///
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    /// - Throws: The core's error if it or the session cannot be started.
    @discardableResult
    func newTab(in window: NSWindow?, profile: String? = nil) throws -> TerminalWindowController {
        try onMainThread { openTab(in: window, profile: profile, completion: $0) }
    }

    /// Runs `open` with ``spawnsInBackground`` off and returns the
    /// controller its completion got.
    private func onMainThread(
        _ open: (@escaping @MainActor (Result<TerminalWindowController, any Error>) -> Void) -> Void
    ) throws -> TerminalWindowController {
        let background = spawnsInBackground
        spawnsInBackground = false
        defer { spawnsInBackground = background }
        let outcome = SpawnOutcome<TerminalWindowController>()
        open { outcome.value = $0 }
        guard let result = outcome.value else {
            // Not reached: a spawn on the main thread completes at once.
            throw TesseraCoreError.spawnFailed
        }
        return try result.get()
    }

    /// Opens a standalone window with a new session. The session starts off
    /// the main thread (``spawnsInBackground``): the window shows its
    /// placeholder until then, and `completion` gets its controller, or the
    /// error once the window has closed again (`CancellationError` if it was
    /// closed first, its session closed too). Returns the window, or `nil`
    /// if it never opened (a main-thread spawn that failed).
    ///
    /// - Parameter frame: Where the window goes, as restored at launch;
    ///   `nil` cascades it from the previous standalone window.
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    @discardableResult
    func openWindow(
        frame: NSRect? = nil,
        profile: String? = nil,
        completion: @escaping @MainActor (Result<TerminalWindowController, any Error>) -> Void = { _ in }
    ) -> TerminalWindow? {
        openTerminal(profile: profile, completion: completion) { [weak self] window in
            guard let self else { return }
            // `.preferred` would put a window that is ordered front into the
            // key window's tab bar; a new window must stand alone.
            window.tabbingMode = .disallowed
            if let frame {
                window.setFrame(frame, display: false)
                // From the zero point the window stays put; the next new window
                // cascades from it.
                cascadePoint = window.cascadeTopLeft(from: .zero)
            } else {
                if windowControllers.count + pendingTerminals.count == 1 {
                    window.center()
                    cascadePoint = .zero
                }
                // Each standalone window opens down and to the right of the
                // previous.
                cascadePoint = window.cascadeTopLeft(from: cascadePoint)
            }
            window.makeKeyAndOrderFront(nil)
            window.tabbingMode = .preferred
        }
    }

    /// Opens a new tab with a new session right after `window` in its tab
    /// bar (for ⌘T, the selected tab), or a standalone window if `window` is
    /// `nil` or not a terminal window, as
    /// ``openWindow(frame:profile:completion:)`` does: the tab shows its
    /// placeholder until its session runs.
    ///
    /// - Parameter profile: The profile the session starts with; `nil` for
    ///   `[general] default_profile`.
    @discardableResult
    func openTab(
        in window: NSWindow?,
        profile: String? = nil,
        completion: @escaping @MainActor (Result<TerminalWindowController, any Error>) -> Void = { _ in }
    ) -> TerminalWindow? {
        guard let window, isTerminalWindow(window) else {
            return openWindow(profile: profile, completion: completion)
        }
        return openTerminal(profile: profile, completion: completion) { [weak window] tab in
            guard let window else { return }
            window.addTabbedWindow(tab, ordered: .above)
            tab.makeKeyAndOrderFront(nil)
        }
    }

    /// Whether `window` is a terminal tab: one with its session, or one
    /// whose session is starting.
    func isTerminalWindow(_ window: NSWindow) -> Bool {
        windowControllers.contains { $0.window === window } || pendingTerminals.contains { $0.window === window }
    }

    /// A terminal window whose first session starts with `profile`: placed
    /// (`place`) at once, showing its placeholder, when the session starts
    /// in the background, or once the session runs when it starts on the
    /// main thread, so a failure there shows nothing.
    private func openTerminal(
        profile: String?,
        completion: @escaping @MainActor (Result<TerminalWindowController, any Error>) -> Void,
        place: @escaping @MainActor (TerminalWindow) -> Void
    ) -> TerminalWindow? {
        let pending = PendingTerminal(window: TerminalWindowController.makeWindow())
        pending.onClose = { [weak self, weak pending] in
            self?.pendingTerminals.removeAll { $0 === pending }
            // The saved state lists the tab while its session starts.
            self?.scheduleWindowStateSave()
            if let pending { self?.passOnClipboardNotice(of: pending) }
        }
        pendingTerminals.append(pending)
        spawn(profile: profile) { [weak self] result in
            pending.isSettled = true
            guard let self else { return }
            pendingTerminals.removeAll { $0 === pending }
            let window = pending.window
            switch result {
            case .failure(let error):
                pending.dropTyped()
                // Held clipboard writes (#763) pass on only from a placed
                // tab. An unplaced one cannot hold any today: a session
                // started on the main thread settles before this call
                // returns, and one started in the background is placed as
                // soon as it does, before a closed tab's writes can move.
                if pending.isPlaced, !pending.isClosed {
                    window.delegate = nil
                    passOnClipboardNotice(of: pending)
                    window.close()
                    scheduleWindowStateSave()
                }
                completion(.failure(error))
            case .success(let session):
                guard !pending.isClosed else {
                    // Closed while its session started: nothing attaches.
                    profileBySession[session.id] = nil
                    try? session.close()
                    completion(.failure(CancellationError()))
                    return
                }
                let controller = makeController(session: session, window: window)
                if pending.isPlaced {
                    let typed = pending.takeTyped()
                    if let view = controller.panes.first?.view {
                        window.makeFirstResponder(view)
                        // What was typed at the placeholder, as if typed now.
                        for event in typed {
                            view.keyDown(with: event)
                        }
                    }
                    if pending.droppedKeys > 0 {
                        Self.log.info("\(pending.droppedKeys) keys typed while a terminal started were dropped")
                    }
                } else {
                    place(window)
                }
                if let notice = pending.takeClipboardNotice() {
                    controller.adoptClipboardNotice(notice)
                }
                completion(.success(controller))
            }
        }
        guard !pending.isSettled else { return liveWindow(pending) }
        pending.showPlaceholder()
        place(pending.window)
        pending.isPlaced = true
        return pending.window
    }

    /// A starting tab that holds a closed tab's unseen clipboard writes
    /// closes before its session runs (#763): they go to the tab the user
    /// is left with instead, once AppKit has picked it.
    private func passOnClipboardNotice(of pending: PendingTerminal) {
        guard let notice = pending.takeClipboardNotice() else { return }
        let siblings = pending.window.tabBarAsItClosed.map { WeakWindow(window: $0) }
        Task { @MainActor [weak self] in
            self?.adoptClipboardNotice(notice, fromTabsOf: siblings)
        }
    }

    /// `pending`'s window if a controller took it over.
    private func liveWindow(_ pending: PendingTerminal) -> TerminalWindow? {
        windowControllers.contains { $0.window === pending.window } ? pending.window : nil
    }

    /// ⌘N.
    @objc func openNewWindow(_ sender: Any?) {
        openWindow(completion: presentOpenFailure)
    }

    /// ⌘T: a tab in the key window's tab bar, or in the main window's while a
    /// panel (About, an alert) is key.
    @objc func openNewTab(_ sender: Any?) {
        openTab(profile: nil)
    }

    /// Shell > New Window with Profile > a profile: the item's
    /// `representedObject` is the profile's name (``MainMenu/setProfiles(_:in:)``).
    @objc func openNewWindowWithProfile(_ sender: Any?) {
        openWindow(profile: Self.profileName(of: sender), completion: presentOpenFailure)
    }

    /// Shell > New Tab with Profile > a profile, as ⌘T with that profile.
    @objc func openNewTabWithProfile(_ sender: Any?) {
        openTab(profile: Self.profileName(of: sender))
    }

    private func openTab(profile: String?) {
        let terminal = [NSApp.keyWindow, NSApp.mainWindow].lazy.compactMap { $0 }.first { window in
            self.isTerminalWindow(window)
        }
        openTab(in: terminal, profile: profile, completion: presentOpenFailure)
    }

    /// Shows a new window's or tab's failure to open, unless it was closed
    /// while its session started.
    private func presentOpenFailure(_ result: Result<TerminalWindowController, any Error>) {
        guard case .failure(let error) = result, !(error is CancellationError) else { return }
        presentOpenError(error)
    }

    /// Close All (⌥⌘W, Close Tab's alternate, which takes the place of
    /// AppKit's own, #464): closes every terminal window with all its tabs,
    /// and the Settings window. If any session runs a job, one
    /// application-modal alert lists them all first (D4b; the Settings
    /// window asks nothing), and Cancel closes nothing. Ignored while a tab
    /// asks about itself, a pane or its window, as ⇧⌘W is. A termination
    /// signal or the system's quit while the alert asks ends it, and the
    /// app quits without asking (``terminateWithoutAsking()``). AppKit's
    /// own `closeAll:` comes here too, the Option-click on a window's close
    /// button included (``closeAllFromAppKit(_:)``).
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
        for window in windowControllers.compactMap(\.window) + pendingTerminals.map(\.window) {
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

    /// AppKit's `closeAll:` (``TesseraApplication``): Close All. For an
    /// Option-click on a window's close button it runs once, after AppKit's
    /// round: a window with tabs first offers every other tab its close
    /// (each refuses, ``isOptionClickOnCloseButton(_:)``) and sends
    /// `closeAll:` at the end of that loop (in place of the clicked tab's
    /// own close), so nothing may close under
    /// it. By a timer rather than the main queue: under a block of that
    /// queue the alert's modal loop would run nothing else of it, signals
    /// and session output included.
    func closeAllFromAppKit(_ sender: Any?) {
        guard Self.isOptionClickOnCloseButton(NSApp.currentEvent) else {
            closeAllWindows(sender)
            return
        }
        if !closeAllIsScheduled {
            closeAllIsScheduled = true
            perform(#selector(runScheduledCloseAll), with: nil, afterDelay: 0)
        }
    }

    /// Runs the Close All an Option-click on a close button asked for.
    @objc private func runScheduledCloseAll() {
        closeAllIsScheduled = false
        closeAllWindows(nil)
    }

    /// Whether `event` lets go of an Option-click on a window's close
    /// button, which is Close All: every terminal tab refuses to close for
    /// it in `windowShouldClose`, where AppKit's close of a window with tabs
    /// offers each other tab its close. In full
    /// screen the button is in a window of AppKit's own, so every window's
    /// button is compared with where the event happened. The close buttons
    /// of the tab bar's tabs are other buttons: Option there still means
    /// AppKit's Close Other Tabs.
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
    ///
    /// Each tab opens at once, showing its placeholder while its session
    /// starts in the background (#722), so the tabs keep their order
    /// whatever order the sessions start in; a tab whose session fails
    /// closes, and only the first failure is shown. With sessions started
    /// on the main thread the first failure ends the restore.
    @discardableResult
    func restoreWindows(from state: WindowState?, screens: [NSRect] = NSScreen.screens.map(\.visibleFrame)) -> Bool {
        guard let state else { return false }
        let run = RestoreRun()
        let report: @MainActor (any Error) -> Void = { [weak self] error in
            guard !run.failed, !(error is CancellationError) else { return }
            run.failed = true
            self?.presentOpenError(error)
        }
        func restore(_ layout: PaneLayout?) -> @MainActor (Result<TerminalWindowController, any Error>) -> Void {
            { [weak self] result in
                switch result {
                case .success(let controller):
                    guard let layout, let self else { return }
                    // Saved as the tab's layout until its panes are there
                    // (or one fails); a save meanwhile keeps it.
                    let id = ObjectIdentifier(controller)
                    restoringLayouts[id] = layout
                    controller.beginRestoringLayout(layout) { [weak self] error in
                        self?.restoringLayouts[id] = nil
                        self?.scheduleWindowStateSave()
                        error.map(report)
                    }
                case .failure(let error):
                    report(error)
                }
            }
        }
        /// `window`, opened for a tab of `layout`, carries it while its
        /// session starts, so a save meanwhile keeps the tab's layout.
        func opening(_ window: TerminalWindow?, with layout: PaneLayout?) -> TerminalWindow? {
            pendingTerminals.last { $0.window === window }?.layout = layout
            return window
        }
        var opened = false
        for saved in state.restorePlan(screens: screens).reversed() {
            guard !run.failed,
                let window = opening(
                    openWindow(frame: saved.frame, completion: restore(saved.layout(ofTab: 0))),
                    with: saved.layout(ofTab: 0))
            else { break }
            opened = true
            for index in 1..<saved.tabCount {
                let layout = saved.layout(ofTab: index)
                guard !run.failed,
                    opening(openTab(in: window.tabGroup?.windows.last ?? window, completion: restore(layout)), with: layout)
                        != nil
                else { break }
            }
            // Each new tab was selected as it opened, so the last one is.
            if let selected = saved.selectedTab {
                window.selectTab(at: selected)
            }
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
    /// selected. A tab whose session is starting (``pendingTerminals``)
    /// counts with the layout it opens with, as does a restored tab whose
    /// other panes' sessions are starting, so a save during a restore keeps
    /// every tab of it.
    func currentWindowState(orderedWindows: [NSWindow]? = nil) -> WindowState {
        let orderedWindows = orderedWindows ?? windowsFrontToBack()
        let terminals: [(window: NSWindow, layout: PaneLayout?)] =
            windowControllers.compactMap { controller in
                controller.window.map { window in
                    let restoring = controller.panes.count == 1 ? restoringLayouts[ObjectIdentifier(controller)] : nil
                    return (window, restoring ?? controller.paneLayout)
                }
            } + pendingTerminals.filter(\.isPlaced).map { ($0.window, $0.layout) }
        var groups: [(front: NSWindow, tabs: [(window: NSWindow, layout: PaneLayout?)])] = []
        var indexByGroup: [ObjectIdentifier: Int] = [:]
        for terminal in terminals {
            let window = terminal.window
            let group = window.tabGroup.map(ObjectIdentifier.init) ?? ObjectIdentifier(window)
            if let index = indexByGroup[group] {
                groups[index].tabs.append(terminal)
            } else {
                indexByGroup[group] = groups.count
                groups.append((window.tabGroup?.selectedWindow ?? window, [terminal]))
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
            let order = group.tabs.first?.window.tabGroup?.windows ?? []
            let tabs = group.tabs.enumerated().sorted { lhs, rhs in
                let left = order.firstIndex { $0 === lhs.element.window } ?? Int.max
                let right = order.firstIndex { $0 === rhs.element.window } ?? Int.max
                return (left, lhs.offset) < (right, rhs.offset)
            }
            return WindowState.Window(
                frame: group.front.frame, tabCount: tabs.count,
                tabs: tabs.map { WindowState.Tab(tree: $0.element.layout) },
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

    private func makeController(session: TesseraSession, window: TerminalWindow) -> TerminalWindowController {
        let controller = TerminalWindowController(
            session: session, title: TerminalWindowController.defaultTitle, window: window)
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

    /// Starts a session of ``spawnOptions`` with `profile`, records its
    /// profile and gives it to `completion` on the main actor, or the error:
    /// off the main thread while ``spawnsInBackground`` (#722), before this
    /// returns otherwise.
    private func spawn(
        profile: String?,
        completion: @escaping @MainActor (Result<TesseraSession, any Error>) -> Void
    ) {
        let core: TesseraApp
        do {
            core = try self.core ?? startCore()
        } catch {
            completion(.failure(error))
            return
        }
        var options = spawnOptions
        options.profile = profile
        guard spawnsInBackground else {
            completion(Result { try core.spawn(options) }.map { record(profile, of: $0) })
            return
        }
        spawnsInFlight += 1
        core.spawnInBackground(options) { [weak self] result in
            guard let self else {
                if case .success(let session) = result { try? session.close() }
                return
            }
            spawnsInFlight -= 1
            completion(result.map { record(profile, of: $0) })
        }
    }

    private func record(_ profile: String?, of session: TesseraSession) -> TesseraSession {
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
        core.pasteboard = programPasteboard
        self.core = core
        router.onConfigurationApplied = { [weak self] in
            self?.readAppAppearance()
            self?.applyKeymap()
            self?.readPaneDimming()
            self?.readBlurRadius()
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
        readBlurRadius()
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
    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController) {
        openTab(in: controller.window, completion: presentOpenFailure)
    }

    /// A split's session, of the focused pane's profile; closed if the tab
    /// closed while it started.
    func terminalWindowControllerSpawnSession(
        _ controller: TerminalWindowController,
        completion: @escaping @MainActor (Result<TesseraSession, any Error>) -> Void
    ) {
        spawn(profile: profile(of: controller.focusedPane.session)) { [weak self, weak controller] result in
            guard case .success(let session) = result else {
                completion(result)
                return
            }
            guard let self, let controller, windowControllers.contains(where: { $0 === controller }) else {
                self?.profileBySession[session.id] = nil
                try? session.close()
                completion(.failure(CancellationError()))
                return
            }
            // Registered before the main actor can run the session's first
            // delivery, so no event is dropped.
            router.register(controller, session: session)
            completion(.success(session))
        }
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
        // A clipboard write the tab did not show for long enough (its last
        // pane's program wrote and exited, #763) shows on the tab the user
        // is left with, once AppKit has picked it.
        if let unseen = controller.takeUnseenClipboardNotice() {
            let tabBar = (controller.window as? TerminalWindow)?.tabBarAsItClosed ?? []
            let siblings = tabBar.map { WeakWindow(window: $0) }
            Task { @MainActor [weak self] in
                self?.adoptClipboardNotice(unseen, fromTabsOf: siblings)
            }
        }
    }

    /// Shows a closed tab's unseen clipboard writes (#763) on the tab the
    /// user is left with: the selected tab of its tab bar, else the selected
    /// tab of the frontmost window that is not minimised, else of any
    /// terminal window, where it waits until that pane is on screen. A tab
    /// whose session is still starting (its placeholder) keeps them for its
    /// pane, or passes them on if it closes first. With no terminal window
    /// left nothing shows (ADR 0022 D-NOTICE52).
    private func adoptClipboardNotice(_ notice: ClipboardNoticeThrottle.Notice, fromTabsOf siblings: [WeakWindow]) {
        let controllers = windowControllers
        let starting = pendingTerminals.filter { !$0.isClosed }
        /// Gives the writes to `window`'s selected tab, running or starting.
        func adopt(into window: NSWindow) -> Bool {
            let shown = window.tabGroup?.selectedWindow ?? window
            if let controller = controllers.first(where: { $0.window === shown }) {
                controller.adoptClipboardNotice(notice)
                return true
            }
            if let pending = starting.first(where: { $0.window === shown }) {
                pending.holdClipboardNotice(notice)
                return true
            }
            return false
        }
        let windows = windowsFrontToBack()
        // The first candidate that takes them.
        let candidates = siblings.compactMap(\.window) + windows.filter { !$0.isMiniaturized } + windows
        guard !candidates.contains(where: adopt(into:)) else { return }
        if let controller = controllers.first {
            controller.adoptClipboardNotice(notice)
        } else {
            starting.first?.holdClipboardNotice(notice)
        }
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
struct WeakWindow {
    weak var window: NSWindow?
}
