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

    /// Controllers by session id. Weak: the application delegate's registry
    /// owns the controllers.
    private var controllers: [UInt64: WeakController] = [:]
    private var lastBeep: ContinuousClock.Instant?

    /// Routes `controller.session`'s events to `controller`.
    func register(_ controller: TerminalWindowController) {
        controllers[controller.session.id] = WeakController(controller: controller)
    }

    /// Stops routing `session`'s events.
    func unregister(_ session: TesseraSession) {
        controllers[session.id] = nil
    }

    /// The controller of the tab that runs `session`, if it is open.
    func controller(for session: TesseraSession) -> TerminalWindowController? {
        controllers[session.id]?.controller
    }

    // MARK: TesseraAppDelegate

    func sessionDidOutput(_ session: TesseraSession) {
        controller(for: session)?.sessionDidOutput()
    }

    func session(_ session: TesseraSession, didSetTitle title: String) {
        controller(for: session)?.sessionDidSetTitle(title)
    }

    func sessionDidRingBell(_ session: TesseraSession) {
        // Bells are coalesced per delivery, but a program ringing continuously
        // still gets one delivery after another: beep at most every 100 ms.
        let now = ContinuousClock.now
        if let lastBeep, now - lastBeep < .milliseconds(100) { return }
        lastBeep = now
        NSSound.beep()
    }

    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {
        controller(for: session)?.sessionDidExit(code: code, signal: signal)
    }

    /// The configuration or the theme in use changed: every tab follows.
    func appearanceDidChange(opaque: Bool) {
        for entry in controllers.values {
            entry.controller?.contentView.applyLayerOpacity(opaque: opaque)
        }
    }
}
