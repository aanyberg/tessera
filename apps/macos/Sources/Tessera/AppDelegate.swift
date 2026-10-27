import AppKit
import TesseraKit

/// Entry point. Still minimal: one window showing the core version and the
/// state of one session started on launch, which exercises the wrapper's
/// callback bridge end to end. Real window management arrives in M3.
@main
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var label: NSTextField?
    private var core: TesseraApp?
    private var session: TesseraSession?
    private var outputEvents = 0
    private var lastBeep: ContinuousClock.Instant?
    private var terminationSignals: [any DispatchSourceSignal] = []

    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        installMainMenu()
        terminateOnSignals()

        let label = NSTextField(wrappingLabelWithString: "")
        label.font = .monospacedSystemFont(ofSize: 15, weight: .medium)
        label.alignment = .center
        self.label = label

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Tessera"
        window.center()
        window.contentView = label
        window.makeKeyAndOrderFront(nil)
        self.window = window

        startCore()
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationWillTerminate(_ notification: Notification) {
        core?.shutdown()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    private func startCore() {
        do {
            let core = try TesseraApp()
            core.delegate = self
            self.core = core
            session = try core.spawn()
            showStatus("login shell running")
        } catch {
            showStatus("could not start a session: \(error)")
        }
    }

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

    private func showStatus(_ status: String) {
        label?.stringValue = "Tessera core v\(TesseraApp.coreVersion)\n\(status)"
    }

    private func installMainMenu() {
        let mainMenu = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "About Tessera", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Tessera", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        mainMenu.addItem(appItem)
        NSApp.mainMenu = mainMenu
    }
}

extension AppDelegate: TesseraAppDelegate {
    func sessionDidOutput(_ session: TesseraSession) {
        outputEvents += 1
        showStatus("login shell running (\(outputEvents) output events)")
    }

    func session(_ session: TesseraSession, didSetTitle title: String) {
        window?.subtitle = title
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
        showStatus(signal == 0 ? "shell exited with code \(code)" : "shell killed by signal \(signal)")
        try? session.close()
        if self.session === session {
            self.session = nil
        }
    }
}
