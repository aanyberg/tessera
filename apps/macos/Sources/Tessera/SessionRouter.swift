import AppKit
import TesseraKit

/// The core's delegate: routes each session's events to the window controller
/// of the tab that runs it.
///
/// This is the seam between the core's sessions and the Swift window and tab
/// model (ADR 0015). When the core grows its own windows, tabs and panes
/// model, that model's events arrive here and the router maps them onto
/// AppKit's windows instead.
@MainActor
final class SessionRouter: TesseraAppDelegate {
    private struct WeakController {
        weak var controller: TerminalWindowController?
    }

    /// Controllers by session id: one entry per pane. Weak: the application
    /// delegate's registry owns the controllers.
    private var controllers: [UInt64: WeakController] = [:]
    private var lastBeep: ContinuousClock.Instant?
    /// Runs when the core applied a configuration: the application delegate
    /// gives the menus the new key map.
    var onConfigurationApplied: (() -> Void)?
    /// Runs when `config.toml` changed and does not load: the application
    /// delegate has Settings read the error.
    var onConfigurationInvalid: (() -> Void)?
    /// Runs when a command of a session finished, with its tab: the
    /// application delegate's notifier decides what it shows (#470).
    var onCommandFinished: ((TesseraSession, TesseraCommandFinish, TerminalWindowController) -> Void)?

    /// Routes `session`'s events to `controller`, the tab whose pane runs it.
    func register(_ controller: TerminalWindowController, session: TesseraSession) {
        controllers[session.id] = WeakController(controller: controller)
    }

    /// Stops routing `session`'s events.
    func unregister(_ session: TesseraSession) {
        controllers[session.id] = nil
    }

    /// The controller of the tab whose pane runs `session`, if it is open.
    func controller(for session: TesseraSession) -> TerminalWindowController? {
        controllers[session.id]?.controller
    }

    /// The controller of the tab whose pane runs the session `id`, if it is
    /// open (a notification's click carries the id alone).
    func controller(forSessionID id: UInt64) -> TerminalWindowController? {
        controllers[id]?.controller
    }

    // MARK: TesseraAppDelegate

    func sessionDidOutput(_ session: TesseraSession) {
        controller(for: session)?.sessionDidOutput(session)
    }

    func session(_ session: TesseraSession, didSetTitle title: String) {
        controller(for: session)?.session(session, didSetTitle: title)
    }

    func session(_ session: TesseraSession, didChangeRemote remote: TesseraRemote) {
        controller(for: session)?.session(session, didChangeRemote: remote)
    }

    func sessionDidRingBell(_ session: TesseraSession) {
        // Bells are coalesced per delivery, but a program ringing continuously
        // still gets one delivery after another: beep at most every 100 ms.
        let now = ContinuousClock.now
        if let lastBeep, now - lastBeep < .milliseconds(100) { return }
        lastBeep = now
        NSSound.beep()
    }

    func session(_ session: TesseraSession, didFinishCommand finish: TesseraCommandFinish) {
        guard let controller = controller(for: session) else { return }
        onCommandFinished?(session, finish, controller)
    }

    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {
        controller(for: session)?.session(session, didExitWithCode: code, signal: signal)
    }

    /// The configuration, the theme in use or the display options changed:
    /// every pane of every tab reads its own session's appearance (ADR 0043),
    /// so `opaque`, which is every view's at once, is not needed.
    func appearanceDidChange(opaque: Bool) {
        for (id, entry) in controllers {
            entry.controller?.panes.first { $0.session.id == id }?.view.appearanceDidChange()
        }
    }

    /// The font fallback index landed: every view draws a frame, which
    /// redraws the rows it showed with a provisional face (#594).
    func fontIndexDidLand() {
        for (id, entry) in controllers {
            entry.controller?.panes.first { $0.session.id == id }?.view.fontIndexDidLand()
        }
    }

    /// A configuration was applied: its `[keys]` may move menu shortcuts.
    func configurationDidApply(generation: UInt64) {
        onConfigurationApplied?()
    }

    /// `config.toml` does not load: nothing was applied, but Settings
    /// shows the error and stops waiting for its edits (#547).
    func configurationDidNotLoad() {
        onConfigurationInvalid?()
    }
}
