import AppKit
import OSLog
import TesseraKit
@preconcurrency import UserNotifications

/// A system notification for a finished command (#470, ADR 0048): plain
/// text only, and nothing from the terminal's output or grid (S2). Its
/// subtitle is the pane's tab title, sanitised and capped (S3).
struct CommandNotification: Sendable, Equatable {
    /// One per session: a new finish replaces the session's last one in
    /// Notification Center, so a pane leaves at most one there (S5).
    var identifier: String
    var sessionID: UInt64
    var title: String
    /// `nil`: no subtitle (the tab has no title of its own, T16).
    var subtitle: String?
    var body: String

    /// The notification content: the text, a thread per session (U12) and
    /// a `userInfo` of the session id alone, never any text (T24): it
    /// outlives the app in Notification Center.
    func content() -> UNMutableNotificationContent {
        let content = UNMutableNotificationContent()
        content.title = title
        if let subtitle { content.subtitle = subtitle }
        content.body = body
        content.threadIdentifier = "tessera.session.\(sessionID)"
        content.userInfo = [CommandNotification.sessionKey: NSNumber(value: sessionID)]
        return content
    }

    /// The one `userInfo` key: the session id, as a number.
    static let sessionKey = "session"

    /// The session id a notification's `userInfo` carries, if it carries a
    /// valid one (U07).
    static func sessionID(from userInfo: [AnyHashable: Any]) -> UInt64? {
        (userInfo[sessionKey] as? NSNumber)?.uint64Value
    }

    /// The notification identifier of `sessionID`'s finishes.
    static func identifier(for sessionID: UInt64) -> String {
        "tessera.command-finished.\(sessionID)"
    }
}

/// The text of a command notification (rows T01-T21).
enum CommandNotificationText {
    /// "Command succeeded" for exit status 0, "Command failed" for any
    /// other, "Command finished" when the shell reported none.
    static func title(exitCode: Int32?) -> String {
        switch exitCode {
        case nil: "Command finished"
        case 0: "Command succeeded"
        default: "Command failed"
        }
    }

    /// "Took <duration>", then ", exit status <n>" when the status is
    /// known.
    static func body(durationMilliseconds: UInt64, exitCode: Int32?) -> String {
        let took = "Took \(duration(milliseconds: durationMilliseconds))"
        guard let exitCode else { return took }
        return "\(took), exit status \(exitCode)"
    }

    /// Whole seconds, rounded down, in hours, minutes and seconds with no
    /// zero part: "59s", "1m", "1h 2m 5s", "25h", and "0s" for nothing.
    static func duration(milliseconds: UInt64) -> String {
        let seconds = milliseconds / 1000
        let parts = [(seconds / 3600, "h"), (seconds % 3600 / 60, "m"), (seconds % 60, "s")]
            .filter { $0.0 > 0 }
            .map { "\($0.0)\($0.1)" }
        return parts.isEmpty ? "0s" : parts.joined(separator: " ")
    }

    /// The subtitle for a tab titled `title`: without C0 controls (line
    /// feeds included), DEL, C1 controls, bidirectional formatting
    /// characters and line or paragraph separators; `nil` when nothing
    /// visible is left (only white space, default-ignorable characters or
    /// combining marks); cut at ``TesseraApp/maxNotificationTextBytes``
    /// bytes of UTF-8 on a character boundary (S3).
    static func subtitle(_ title: String) -> String? {
        var kept = String.UnicodeScalarView()
        kept.append(contentsOf: title.unicodeScalars.filter { !isRemoved($0) })
        let shown = String(kept)
        guard shown.unicodeScalars.contains(where: isVisible) else { return nil }
        var capped = ""
        var bytes = 0
        for character in shown {
            let length = character.utf8.count
            guard bytes + length <= TesseraApp.maxNotificationTextBytes else { break }
            capped.append(character)
            bytes += length
        }
        return capped.isEmpty ? nil : capped
    }

    private static func isRemoved(_ scalar: Unicode.Scalar) -> Bool {
        switch scalar.value {
        case 0x00...0x1F, 0x7F...0x9F: true
        case 0x061C, 0x200E, 0x200F, 0x202A...0x202E, 0x2066...0x2069: true
        case 0x2028, 0x2029: true
        default: false
        }
    }

    private static func isVisible(_ scalar: Unicode.Scalar) -> Bool {
        let properties = scalar.properties
        switch properties.generalCategory {
        case .nonspacingMark, .enclosingMark, .spacingMark, .format, .control: return false
        default: return !properties.isWhitespace && !properties.isDefaultIgnorableCodePoint
        }
    }
}

/// Posts command notifications. ``UserNotificationPoster`` posts through
/// macOS; tests record.
@MainActor
protocol CommandNotificationPosting: AnyObject {
    func post(_ notification: CommandNotification)
    /// Removes delivered notifications from Notification Center.
    func removeDelivered(identifiers: [String])
    /// Removes every delivered notification of this app.
    func removeAllDelivered()
}

/// The calls ``UserNotificationPoster`` makes of macOS's notification
/// centre, so its own logic is tested over a fake (U13).
protocol UserNotificationCentering: Sendable {
    func authorizationStatus() async -> UNAuthorizationStatus
    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool
    func add(_ notification: CommandNotification) async throws
    func removeDeliveredNotifications(withIdentifiers identifiers: [String])
    func removeAllDeliveredNotifications()
    func setDelegate(_ delegate: NotificationDelegate)
}

/// `UNUserNotificationCenter.current()`, resolved at each call and never
/// before (U01): a process without an app bundle raises an exception
/// there, so a type that merely links this one must not.
struct SystemNotificationCenter: UserNotificationCentering {
    func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool {
        try await UNUserNotificationCenter.current().requestAuthorization(options: options)
    }

    func add(_ notification: CommandNotification) async throws {
        let request = UNNotificationRequest(
            identifier: notification.identifier, content: notification.content(), trigger: nil)
        try await UNUserNotificationCenter.current().add(request)
    }

    func removeDeliveredNotifications(withIdentifiers identifiers: [String]) {
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: identifiers)
    }

    func removeAllDeliveredNotifications() {
        UNUserNotificationCenter.current().removeAllDeliveredNotifications()
    }

    func setDelegate(_ delegate: NotificationDelegate) {
        UNUserNotificationCenter.current().delegate = delegate
    }
}

/// Posts through macOS's notification centre. Asks for permission (alerts
/// only, no sound: D9) at the first post that finds it undecided, never at
/// launch and never again (U02, U03); a post waiting for the answer posts
/// once it is given, unless its notification was removed meanwhile (its
/// pane was seen while the prompt was up: D10). Refused or failed posts log
/// their status or error code once, never the text (U04, S8).
@MainActor
final class UserNotificationPoster: CommandNotificationPosting {
    private let center: any UserNotificationCentering
    /// The one permission request of this process, once made.
    private var authorization: Task<Void, Never>?
    /// How many refusals and failures were logged: at most one (tests).
    private(set) var loggedProblems = 0
    /// Posts not yet added or given up (tests: they end).
    private(set) var postsInFlight = 0
    /// The newest post in flight per identifier; a removal forgets it, so
    /// that post is not added after all (at most one entry per post in
    /// flight).
    private var newestPost: [String: UInt64] = [:]
    private var nextPost: UInt64 = 0

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "notifications")

    init(center: any UserNotificationCentering) {
        self.center = center
    }

    /// Installs `delegate` as the centre's (before launching finishes, so a
    /// click that launched the app reaches it: U02).
    func install(_ delegate: NotificationDelegate) {
        center.setDelegate(delegate)
    }

    func post(_ notification: CommandNotification) {
        postsInFlight += 1
        nextPost += 1
        let token = nextPost
        newestPost[notification.identifier] = token
        Task {
            defer {
                postsInFlight -= 1
                if newestPost[notification.identifier] == token { newestPost[notification.identifier] = nil }
            }
            await deliver(notification, token: token)
        }
    }

    private func deliver(_ notification: CommandNotification, token: UInt64) async {
        var status = await center.authorizationStatus()
        if status == .notDetermined {
            let center = center
            let request = authorization ?? Task {
                do {
                    _ = try await center.requestAuthorization(options: [.alert])
                } catch {
                    self.logProblem("command notifications: permission request failed", code: (error as NSError).code)
                }
            }
            authorization = request
            await request.value
            status = await center.authorizationStatus()
        }
        switch status {
        case .authorized, .provisional, .ephemeral:
            // Removed (seen) or replaced by a newer post while this one waited.
            guard newestPost[notification.identifier] == token else { return }
            do {
                try await center.add(notification)
            } catch {
                logProblem("command notifications: posting failed", code: (error as NSError).code)
            }
        default:
            logProblem("command notifications: not allowed", code: status.rawValue)
        }
    }

    /// Logs the first refusal or failure, with its code alone.
    private func logProblem(_ message: StaticString, code: Int) {
        guard loggedProblems == 0 else { return }
        loggedProblems += 1
        Self.log.notice("\(message, privacy: .public) (code \(code, privacy: .public))")
    }

    func removeDelivered(identifiers: [String]) {
        for identifier in identifiers { newestPost[identifier] = nil }
        center.removeDeliveredNotifications(withIdentifiers: identifiers)
    }

    func removeAllDelivered() {
        center.removeAllDeliveredNotifications()
    }
}

/// The notification centre's delegate: not the main-actor application
/// delegate, because macOS calls it on a queue of its own (U10). It reads
/// the session id first and only that crosses to the main actor.
final class NotificationDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    /// Runs for a click on a notification, with the session id it carries
    /// (`nil` when it carries none).
    private let onOpen: @MainActor @Sendable (UInt64?) -> Void

    init(onOpen: @escaping @MainActor @Sendable (UInt64?) -> Void) {
        self.onOpen = onOpen
    }

    /// Shown while the app is frontmost too (a background tab: U06), as a
    /// banner and in Notification Center, without a sound (D9).
    func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        Self.presentationOptions
    }

    static let presentationOptions: UNNotificationPresentationOptions = [.banner, .list]

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        await receive(action: response.actionIdentifier, userInfo: response.notification.request.content.userInfo)
    }

    /// A response to a notification (tests call it): a click (the default
    /// action) opens the session it carries; any other action, a dismissal
    /// included, does nothing.
    func receive(action: String, userInfo: [AnyHashable: Any]) async {
        guard action == UNNotificationDefaultActionIdentifier else { return }
        await open(CommandNotification.sessionID(from: userInfo))
    }

    /// The main-actor half of a click (tests call it).
    func open(_ sessionID: UInt64?) async {
        await onOpen(sessionID)
    }
}

/// Decides, at each finished command, whether its pane is marked and a
/// notification posted (ADR 0048), and clears both when the pane gets the
/// keyboard. The core decides from the configuration and its rate limit;
/// this type only tells it whether the pane has the keyboard.
@MainActor
final class CommandNotifier {
    /// How the notifier sees the keyboard focus: the app's and each
    /// window's (F09). Tests replace it; none reads the real state.
    struct Focus {
        var isApplicationActive: @MainActor () -> Bool
        var isKeyWindow: @MainActor (NSWindow) -> Bool

        static let system = Focus(isApplicationActive: { NSApp.isActive }, isKeyWindow: { $0.isKeyWindow })
    }

    var focus = Focus.system
    var poster: any CommandNotificationPosting
    /// Brings the app to the front for a click (tests record).
    var activateApplication: @MainActor () -> Void = { NSApp.activate() }
    /// The tab running a session, if it is open (the router's).
    var controllerForSession: @MainActor (UInt64) -> TerminalWindowController? = { _ in nil }

    init(poster: any CommandNotificationPosting) {
        self.poster = poster
    }

    /// Whether `pane` has the keyboard (D1): the app is active, its window
    /// is key and its tab's selected one, and it is the tab's focused pane.
    func isFocused(_ pane: Pane, in controller: TerminalWindowController) -> Bool {
        guard let window = controller.window, focus.isApplicationActive(), focus.isKeyWindow(window) else {
            return false
        }
        if let selected = window.tabGroup?.selectedWindow, selected !== window { return false }
        return controller.focusedPane === pane
    }

    /// A command of `session` finished: asks `decide` (the core: the
    /// duration in milliseconds, whether the pane is focused now) and marks
    /// the pane, and posts, as it says.
    func commandDidFinish(
        _ finish: TesseraCommandFinish, of session: TesseraSession, in controller: TerminalWindowController,
        decide: (UInt64, Bool) -> TesseraCommandFinishedDecision
    ) {
        guard let pane = controller.pane(for: session) else { return }
        let decision = decide(finish.durationMilliseconds, isFocused(pane, in: controller))
        guard decision != .nothing else { return }
        controller.setCommandMark(true, for: pane)
        guard decision == .markAndPost else { return }
        poster.post(
            CommandNotification(
                identifier: CommandNotification.identifier(for: session.id),
                sessionID: session.id,
                title: CommandNotificationText.title(exitCode: finish.exitCode),
                subtitle: CommandNotificationText.subtitle(controller.tabTitle(of: pane)),
                body: CommandNotificationText.body(
                    durationMilliseconds: finish.durationMilliseconds, exitCode: finish.exitCode)))
    }

    /// A pane of `controller` may have got the keyboard (it was focused,
    /// its tab selected, its window made key): each marked pane that has
    /// it now loses its mark and its delivered notification (D10, F10).
    func focusMayHaveChanged(in controller: TerminalWindowController) {
        for pane in controller.panes where controller.isCommandMarked(pane) && isFocused(pane, in: controller) {
            controller.setCommandMark(false, for: pane)
            poster.removeDelivered(identifiers: [CommandNotification.identifier(for: pane.session.id)])
        }
    }

    /// A click on the notification of `sessionID`: brings the app to the
    /// front and selects that session's window, tab and pane; for a session
    /// not open in this process (a previous launch's, a closed tab's: U08,
    /// U09) it only brings the app to the front.
    func open(sessionID: UInt64?) {
        activateApplication()
        guard let sessionID, let controller = controllerForSession(sessionID), let window = controller.window,
            let pane = controller.panes.first(where: { $0.session.id == sessionID })
        else { return }
        if let group = window.tabGroup, group.windows.count > 1 {
            group.selectedWindow = window
        }
        window.makeKeyAndOrderFront(nil)
        controller.focus(pane)
        focusMayHaveChanged(in: controller)
    }

    /// The app quits: nothing it posted stays in Notification Center (D10).
    func removeAllDelivered() {
        poster.removeAllDelivered()
    }
}
