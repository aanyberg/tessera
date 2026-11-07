import AppKit
import TesseraKit

/// What closing a pane, a tab or a window, or quitting, would terminate: the
/// sessions among those it closes whose shell runs a job in the foreground,
/// and the alert that asks first (ADR 0031, D4; panes ADR 0035).
///
/// Every job counts, `tmux` and `ssh` included; there is no list of programs
/// that close without asking. A window or the application asks once for all
/// its busy sessions, never once per tab or pane.
struct CloseConfirmation: Equatable {
    /// What is being closed.
    enum Scope: Equatable {
        /// ⌘W on one pane of several.
        case pane
        case tab
        case window
        case quit
    }

    let scope: Scope
    /// The busy sessions' processes as the alert shows them, in the order of
    /// the sessions: filtered by ``displayName(_:)``.
    let processNames: [String]

    /// At most this many processes are listed; the rest are counted.
    static let maxListedProcesses = 10

    /// Shown for a job whose name is empty or nothing but filtered
    /// characters.
    static let unnamedProcess = "unnamed process"

    init(scope: Scope, processNames: [String]) {
        self.scope = scope
        self.processNames = processNames
    }

    /// The confirmation for closing `sessions`, or `nil` when none of them
    /// runs a job, so they close without asking. Asks the core once per
    /// session; a session it no longer knows (closed, or its application
    /// shut down) counts as idle.
    @MainActor
    init?(_ scope: Scope, sessions: [TesseraSession]) {
        let jobs = sessions.compactMap { try? $0.foregroundProcess() }
        guard !jobs.isEmpty else { return nil }
        self.init(scope: scope, processNames: jobs.map { Self.displayName($0.name) })
    }

    /// `name` as it can be shown: a process names itself, so its name may
    /// hold escape sequences, line breaks or bidirectional overrides. Each
    /// control (C0, DEL, C1), format (bidirectional controls, zero-width
    /// characters), line or paragraph separator and unassigned character
    /// becomes U+FFFD, surrounding spaces are trimmed, and an empty result
    /// becomes ``unnamedProcess``.
    static func displayName(_ name: String) -> String {
        var shown = String.UnicodeScalarView()
        for scalar in name.unicodeScalars {
            switch scalar.properties.generalCategory {
            case .control, .format, .lineSeparator, .paragraphSeparator, .unassigned:
                shown.append("\u{FFFD}")
            default:
                shown.append(scalar)
            }
        }
        let trimmed = String(shown).trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? unnamedProcess : trimmed
    }

    // MARK: Alert text

    var messageText: String {
        switch scope {
        case .pane: "Close this pane?"
        case .tab: "Close this tab?"
        case .window: "Close this window?"
        case .quit: "Quit Tessera?"
        }
    }

    /// Names every process (up to ``maxListedProcesses``), one per line.
    var informativeText: String {
        let what = processNames.count == 1 ? "the running process" : "the running processes"
        let lead =
            switch scope {
            case .pane: "Closing this pane will terminate \(what):"
            case .tab: "Closing this tab will terminate \(what):"
            case .window: "Closing this window will terminate \(what):"
            case .quit: "Quitting will terminate \(what):"
            }
        var lines = processNames.prefix(Self.maxListedProcesses).map { "• \($0)" }
        let more = processNames.count - Self.maxListedProcesses
        if more > 0 {
            lines.append("• and \(more) more")
        }
        return ([lead] + lines).joined(separator: "\n")
    }

    /// The button that closes; the other one is Cancel.
    var confirmButtonTitle: String {
        scope == .quit ? "Quit" : "Close"
    }
}

/// Shows a ``CloseConfirmation`` and reports the answer. ``AppDelegate``
/// holds one; tests record instead of showing alerts.
@MainActor
protocol CloseConfirmationPresenter: AnyObject {
    /// Asks on a sheet of `window` (a tab or a window closing); calls
    /// `completion` once, with `true` to close.
    func confirm(
        _ confirmation: CloseConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void)
    /// Asks before quitting, application-modally; `true` to quit.
    func confirmQuit(_ confirmation: CloseConfirmation) -> Bool
}

/// ``CloseConfirmationPresenter`` with `NSAlert`: Close (or Quit) is the
/// default button, as in Terminal.app, and Cancel answers Escape.
@MainActor
final class AlertCloseConfirmationPresenter: CloseConfirmationPresenter {
    func confirm(
        _ confirmation: CloseConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void
    ) {
        makeAlert(confirmation).beginSheetModal(for: window) { response in
            MainActor.assumeIsolated { completion(response == .alertFirstButtonReturn) }
        }
    }

    func confirmQuit(_ confirmation: CloseConfirmation) -> Bool {
        NSApp.activate()
        return makeAlert(confirmation).runModal() == .alertFirstButtonReturn
    }

    private func makeAlert(_ confirmation: CloseConfirmation) -> NSAlert {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = confirmation.messageText
        alert.informativeText = confirmation.informativeText
        alert.addButton(withTitle: confirmation.confirmButtonTitle).hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        return alert
    }
}
