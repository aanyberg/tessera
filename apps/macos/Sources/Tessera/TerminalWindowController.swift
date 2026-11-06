import AppKit
import TesseraKit
import os

/// A tab's content view: the window controller installs it as the window's
/// content view and reports its session's events to it
/// (``TerminalSurfaceView``).
@MainActor
protocol TerminalContentView: NSView {
    /// The session wrote output and the terminal has applied it.
    func sessionDidOutput()
    /// The session's child exited with `code`, or was killed by `signal`
    /// (non-zero); the tab stays open showing it.
    func sessionDidExit(code: Int32, signal: Int32)
    /// The core re-themed every view: make the window opaque or translucent
    /// as `opaque` says and redraw.
    func applyLayerOpacity(opaque: Bool)
}

/// What a ``TerminalWindowController`` asks of the application.
@MainActor
protocol TerminalWindowControllerDelegate: AnyObject {
    /// The tab bar's "+" button: open a new tab in `window`'s tab group.
    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController)
    /// The controller's window closed and its session is closed; forget it.
    func terminalWindowControllerDidClose(_ controller: TerminalWindowController)
    /// The controller's window moved, was resized or was selected (which is
    /// how a tab moving to another window or a merge shows): the windows'
    /// frames or tab counts may have changed.
    func terminalWindowControllerDidChangeLayout(_ controller: TerminalWindowController)
    /// Asks before a tab or window whose session runs a job closes; `nil`
    /// closes without asking.
    var closeConfirmationPresenter: (any CloseConfirmationPresenter)? { get }
}

/// One terminal tab: one `NSWindow` running one ``TesseraSession``.
///
/// Native macOS tabs are windows: every tab is its own window with its own
/// controller, and AppKit groups the windows that share
/// ``tabbingIdentifier`` into one tab bar, ordering, dragging and merging
/// them itself (ADR 0015).
@MainActor
final class TerminalWindowController: NSWindowController, NSWindowDelegate, NSMenuItemValidation {
    /// Shared by every terminal window, so AppKit tabs them together.
    static let tabbingIdentifier = "com.aanyberg.tessera.terminal"
    /// The window title while the session has not set one.
    static let defaultTitle = "Terminal"

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window")

    let session: TesseraSession
    /// The tab's content, installed as the window's content view.
    let contentView: any TerminalContentView
    weak var delegate: (any TerminalWindowControllerDelegate)?

    /// The close confirmation up on this tab's sheet, if any: `.tab` holds
    /// back further closes of this tab only, `.window` (⇧⌘W) of every tab of
    /// its window, rather than asking twice. If the tab closes underneath its
    /// sheet (its shell exits, or ⇧⌘W on another tab closes it), the sheet
    /// ends as Cancel (``windowWillClose(_:)``).
    private(set) var pendingClose: CloseConfirmation.Scope?

    /// - Parameters:
    ///   - title: The initial window title, until the session sets one.
    ///   - contentView: The tab's content; the session's terminal surface by
    ///     default.
    init(
        session: TesseraSession,
        title: String,
        contentView: (any TerminalContentView)? = nil
    ) {
        self.session = session
        let contentView = contentView ?? TerminalSurfaceView(session: session)
        self.contentView = contentView
        let window = TerminalWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        // The controller owns the window; AppKit must not release it again.
        window.isReleasedWhenClosed = false
        window.tabbingMode = .preferred
        window.tabbingIdentifier = Self.tabbingIdentifier
        window.title = title.isEmpty ? Self.defaultTitle : title
        window.contentView = contentView
        super.init(window: window)
        window.delegate = self
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Session events (from SessionRouter)

    func sessionDidOutput() {
        contentView.sessionDidOutput()
    }

    func sessionDidSetTitle(_ title: String) {
        window?.title = title.isEmpty ? Self.defaultTitle : title
    }

    /// A clean exit (code 0) closes the tab; any other exit keeps it open,
    /// showing the status, like Terminal.app.
    func sessionDidExit(code: Int32, signal: Int32) {
        if code == 0 && signal == 0 {
            window?.close()
        } else {
            contentView.sessionDidExit(code: code, signal: signal)
        }
    }

    // MARK: Actions

    /// The tab bar's "+" button. AppKit shows it only when the responder
    /// chain answers this action.
    override func newWindowForTab(_ sender: Any?) {
        delegate?.terminalWindowControllerWantsNewTab(self)
    }

    /// ⌘1 to ⌘9: selects the tab at the sender's tag (0-based) in this
    /// window's tab bar.
    @objc func selectTabAtIndex(_ sender: NSMenuItem) {
        (window as? TerminalWindow)?.selectTab(at: sender.tag)
    }

    /// ⇧⌘W: closes every tab of this window's tab bar, and with them the
    /// window. If any of the tabs runs a job, one sheet on this tab lists
    /// them all first (D4b), and Cancel keeps every tab. Ignored while this
    /// tab asks or the window already asks; another tab's question about
    /// itself stays up beside this one, and Close closes that tab too.
    @objc func closeTabGroup(_ sender: Any?) {
        guard let window = window as? TerminalWindow else { return }
        let tabs = window.tabs
        let controllers = tabs.compactMap { $0.windowController as? TerminalWindowController }
        guard pendingClose == nil, !controllers.contains(where: { $0.pendingClose == .window }) else { return }
        // `close()`, not `performClose(_:)`: the user has been asked once
        // for all of them, or nothing in them runs.
        let closeAll = { @MainActor in
            for tab in tabs {
                tab.close()
            }
        }
        guard let presenter = delegate?.closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.window, sessions: controllers.map(\.session))
        else {
            closeAll()
            return
        }
        ask(confirmation, presenter: presenter, close: closeAll)
    }

    /// Shows `confirmation` on a sheet of this tab, which it selects first,
    /// and runs `close` if the user confirms.
    private func ask(
        _ confirmation: CloseConfirmation, presenter: any CloseConfirmationPresenter,
        close: @escaping @MainActor () -> Void
    ) {
        guard let window else { return }
        pendingClose = confirmation.scope
        window.tabGroup?.selectedWindow = window
        presenter.confirm(confirmation, sheetFor: window) { [weak self] confirmed in
            self?.pendingClose = nil
            if confirmed {
                close()
            }
        }
    }

    /// ⌘1 to ⌘9 are enabled only for tabs that exist.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        guard menuItem.action == #selector(selectTabAtIndex(_:)) else { return true }
        guard let window = window as? TerminalWindow else { return false }
        return window.tabs.indices.contains(menuItem.tag)
    }

    // MARK: NSWindowDelegate

    /// ⌘W, the tab's close button and AppKit's own multi-tab closes (Close
    /// Other Tabs, Close All), which ask each tab: closes at once at the
    /// shell's prompt; while the session runs a job, asks on a sheet first
    /// and closes only if the user confirms. A request is ignored while this
    /// tab is already asking, or while its window asks for all its tabs
    /// (⇧⌘W); another tab's own question holds back nothing here.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        let group = (window as? TerminalWindow)?.tabs ?? []
        let windowAsks = group.contains { ($0.windowController as? TerminalWindowController)?.pendingClose == .window }
        if pendingClose != nil || windowAsks {
            return false
        }
        guard let presenter = delegate?.closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.tab, sessions: [session])
        else { return true }
        ask(confirmation, presenter: presenter) { [weak self] in
            self?.window?.close()
        }
        return false
    }

    func windowDidMove(_ notification: Notification) {
        delegate?.terminalWindowControllerDidChangeLayout(self)
    }

    func windowDidResize(_ notification: Notification) {
        delegate?.terminalWindowControllerDidChangeLayout(self)
    }

    func windowDidBecomeMain(_ notification: Notification) {
        delegate?.terminalWindowControllerDidChangeLayout(self)
    }

    func windowWillClose(_ notification: Notification) {
        // A tab closing under its close sheet (its shell exited, or ⇧⌘W on
        // another tab closed it): the sheet answers Cancel, or the alert and
        // its modal session would outlive the tab.
        if let window {
            for sheet in window.sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
        }
        do {
            try session.close()
        } catch TesseraCoreError.sessionNotFound {
            // Already closed: nothing to do.
        } catch {
            // The core logged the details; the tab closes regardless.
            Self.log.error("closing session \(self.session.id) failed: \(String(describing: error), privacy: .public)")
        }
        delegate?.terminalWindowControllerDidClose(self)
    }
}

/// A terminal tab's window.
///
/// Selects tabs itself instead of through AppKit's implementation of the tab
/// actions, which does nothing unless the application is active and the
/// window is key; selecting by index works in every state (and in tests) and
/// behaves the same: the selected tab moves, wrapping around at either end.
@MainActor
final class TerminalWindow: NSWindow {
    /// The windows of this window's tab bar in tab order, or just this window.
    var tabs: [NSWindow] {
        tabGroup?.windows ?? [self]
    }

    /// Selects the tab at `index` of this window's tab bar; does nothing if
    /// there is no such tab.
    func selectTab(at index: Int) {
        let tabs = tabs
        guard tabs.indices.contains(index) else { return }
        let target = tabs[index]
        tabGroup?.selectedWindow = target
        target.makeKeyAndOrderFront(nil)
    }

    /// ⇧⌘]: the next tab, the first after the last.
    override func selectNextTab(_ sender: Any?) {
        selectTab(offsetBy: 1)
    }

    /// ⇧⌘[: the previous tab, the last before the first.
    override func selectPreviousTab(_ sender: Any?) {
        selectTab(offsetBy: -1)
    }

    private func selectTab(offsetBy offset: Int) {
        let tabs = tabs
        let selected = tabGroup?.selectedWindow ?? self
        guard tabs.count > 1, let index = tabs.firstIndex(where: { $0 === selected }) else { return }
        selectTab(at: (index + offset + tabs.count) % tabs.count)
    }
}
