import AppKit
import TesseraKit
import os

/// A pane's view: the window controller lays it out in the tab's
/// ``PaneContainerView`` and reports its session's events to it
/// (``TerminalSurfaceView``).
@MainActor
protocol TerminalContentView: NSView {
    /// The session wrote output and the terminal has applied it.
    func sessionDidOutput()
    /// The session's child exited with `code`, or was killed by `signal`
    /// (non-zero); the pane stays open showing it.
    func sessionDidExit(code: Int32, signal: Int32)
    /// The core re-styled the views: read the session's appearance again,
    /// make the window opaque or translucent to match its panes and redraw.
    func appearanceDidChange()
}

/// What a ``TerminalWindowController`` asks of the application.
@MainActor
protocol TerminalWindowControllerDelegate: AnyObject {
    /// The tab bar's "+" button: open a new tab in `window`'s tab group.
    func terminalWindowControllerWantsNewTab(_ controller: TerminalWindowController)
    /// A new pane of `controller`'s tab needs a session: spawn one and route
    /// its events to `controller` before the main actor runs again.
    func terminalWindowControllerSpawnSession(_ controller: TerminalWindowController) throws -> TesseraSession
    /// A pane of `controller`'s tab closed with its session; stop routing
    /// the session's events. The tab stays open.
    func terminalWindowController(_ controller: TerminalWindowController, didClosePaneWith session: TesseraSession)
    /// The controller's window closed and every session of its panes is
    /// closed; forget it.
    func terminalWindowControllerDidClose(_ controller: TerminalWindowController)
    /// The controller's window moved, was resized or was selected (which is
    /// how a tab moving to another window or a merge shows): the windows'
    /// frames or tab counts may have changed.
    func terminalWindowControllerDidChangeLayout(_ controller: TerminalWindowController)
    /// Shows that a terminal could not be opened (a new pane's session).
    func presentOpenError(_ error: any Error)
    /// Asks before a pane, tab or window whose sessions run a job closes;
    /// `nil` closes without asking.
    var closeConfirmationPresenter: (any CloseConfirmationPresenter)? { get }
}

/// One terminal tab: one `NSWindow` whose panes each run a
/// ``TesseraSession``.
///
/// Native macOS tabs are windows: every tab is its own window with its own
/// controller, and AppKit groups the windows that share
/// ``tabbingIdentifier`` into one tab bar, ordering, dragging and merging
/// them itself (ADR 0015). Within a tab, a ``PaneContainerView`` lays out the
/// panes (ADR 0035): ⌘D and ⌘⇧D split the focused pane, ⌘⌥ and an arrow (or
/// ⌘[ and ⌘]) move the focus, ⌘⇧↩ maximizes the focused pane and back, ⌘W
/// closes the focused pane and the last pane's ⌘W closes the tab. A split,
/// a close, a focus move or a new tab shows every pane again first. The
/// window's title is the focused pane's.
@MainActor
final class TerminalWindowController: NSWindowController, NSWindowDelegate, NSMenuItemValidation {
    /// Shared by every terminal window, so AppKit tabs them together.
    static let tabbingIdentifier = "com.aanyberg.tessera.terminal"
    /// The window title while the focused pane's session has not set one.
    static let defaultTitle = "Terminal"

    /// Whether a remote focused pane marks the title (`[general]
    /// remote_indicator`, #205); the application delegate sets it from the
    /// configuration in effect.
    var showsRemoteHost = true {
        didSet {
            if showsRemoteHost != oldValue { updateTitle() }
        }
    }

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window")

    /// The Shell menu's ⌘W item, which ``PaneContainerView`` retitles.
    private static let closeTabCommand = "close_tab"

    /// The tab's content: its panes, laid out.
    let paneContainer: PaneContainerView
    weak var delegate: (any TerminalWindowControllerDelegate)?

    /// The pane that has (or last had) the keyboard: its view is the
    /// window's first responder while the window is key and nothing else
    /// took it (a sheet's field).
    private(set) var focusedPane: Pane

    /// The close confirmation up on this tab's sheet, if any: `.pane` and
    /// `.tab` hold back further closes of this tab only, `.window` (⇧⌘W) of
    /// every tab of its window, rather than asking twice. If what it asks
    /// about closes underneath its sheet (the shell exits, or ⇧⌘W on another
    /// tab closes the tab), the sheet ends as Cancel.
    private(set) var pendingClose: CloseConfirmation.Scope?
    /// The pane a `.pane` confirmation asks about.
    private weak var pendingPane: Pane?

    /// - Parameters:
    ///   - session: The first pane's session.
    ///   - title: The initial window title, until the session sets one.
    init(session: TesseraSession, title: String) {
        let pane = Pane(session: session)
        focusedPane = pane
        let window = TerminalWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        paneContainer = PaneContainerView(pane: pane, frame: NSRect(x: 0, y: 0, width: 480, height: 200))
        // The controller owns the window; AppKit must not release it again.
        window.isReleasedWhenClosed = false
        window.tabbingMode = .preferred
        window.tabbingIdentifier = Self.tabbingIdentifier
        window.title = title.isEmpty ? Self.defaultTitle : title
        window.contentView = paneContainer
        super.init(window: window)
        window.delegate = self
        watchFocus(of: pane)
        window.initialFirstResponder = pane.view
        paneContainer.activePane = pane
        updateMinimumSize()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// The tab's panes in reading order.
    var panes: [Pane] { paneContainer.panes }

    /// Every pane's session, in reading order.
    var sessions: [TesseraSession] { panes.map(\.session) }

    /// The pane running `session`, if it is one of this tab's.
    func pane(for session: TesseraSession) -> Pane? {
        panes.first { $0.session.id == session.id }
    }

    // MARK: Session events (from SessionRouter)

    func sessionDidOutput(_ session: TesseraSession) {
        pane(for: session)?.view.sessionDidOutput()
    }

    /// The focused pane's title is the window's.
    func session(_ session: TesseraSession, didSetTitle title: String) {
        guard let pane = pane(for: session) else { return }
        pane.title = title
        if pane === focusedPane {
            updateTitle()
        }
    }

    /// The focused pane's remote state marks the window's title.
    func session(_ session: TesseraSession, didChangeRemote remote: TesseraRemote) {
        guard let pane = pane(for: session) else { return }
        pane.remote = remote
        if pane === focusedPane {
            updateTitle()
        }
    }

    /// A clean exit (code 0) closes the pane, and with the last pane the
    /// tab; any other exit keeps it open, showing the status, like
    /// Terminal.app.
    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {
        guard let pane = pane(for: session) else { return }
        if code == 0 && signal == 0 {
            close(pane)
        } else {
            pane.view.sessionDidExit(code: code, signal: signal)
        }
    }

    // MARK: Panes

    /// Splits the focused pane toward `direction` with a new session, and
    /// focuses the new pane. Returns it, or `nil` when the split is refused
    /// (``PaneTree/maxLeaves`` panes already, or a half would be smaller
    /// than ``PaneContainerView/minimumColumns`` ×
    /// ``PaneContainerView/minimumRows`` cells).
    ///
    /// - Throws: The core's error if the session cannot be started.
    @discardableResult
    func splitFocusedPane(_ direction: PaneSplitDirection) throws -> Pane? {
        unzoom()
        guard let delegate, paneContainer.canSplit(focusedPane, direction) else { return nil }
        let session = try delegate.terminalWindowControllerSpawnSession(self)
        let pane = Pane(session: session)
        guard paneContainer.split(focusedPane, direction, adding: pane) else {
            // Not reached: `canSplit` said yes and nothing ran since.
            closeSession(of: pane)
            return nil
        }
        watchFocus(of: pane)
        updateMinimumSize()
        focus(pane)
        paneLayoutDidChange()
        return pane
    }

    /// Moves the focus to the pane next to the focused one toward
    /// `direction`; does nothing at the edge. Shows every pane first.
    func moveFocus(_ direction: PaneDirection) {
        unzoom()
        guard let neighbour = paneContainer.neighbour(of: focusedPane, toward: direction) else { return }
        focus(neighbour)
    }

    /// Moves the focus to the next pane in reading order (`offset` 1) or
    /// the previous one (-1), wrapping around. Shows every pane first.
    func cycleFocus(by offset: Int) {
        unzoom()
        let panes = panes
        guard panes.count > 1, let index = panes.firstIndex(of: focusedPane) else { return }
        focus(panes[(index + offset % panes.count + panes.count) % panes.count])
    }

    /// Gives `pane` the keyboard; another pane than a maximized one shows
    /// every pane first.
    func focus(_ pane: Pane) {
        guard panes.contains(pane) else { return }
        if let zoomed = paneContainer.zoomedPane, zoomed !== pane {
            unzoom()
        }
        if window?.makeFirstResponder(pane.view) != true {
            // The window keeps its first responder (a sheet's field holds
            // it); the pane still becomes the focused one.
            paneDidBecomeFocused(pane)
        }
    }

    /// Whether a pane is maximized.
    var isZoomed: Bool { paneContainer.zoomedPane != nil }

    /// Maximizes the focused pane (`zoom_pane`), or shows every pane again
    /// while one is maximized. Returns `false` for a lone pane.
    @discardableResult
    func toggleZoom() -> Bool {
        let changed = isZoomed ? paneContainer.unzoom() : paneContainer.zoom(focusedPane)
        if changed {
            refocusAfterMovingViews()
        }
        return changed
    }

    /// Shows every pane again if one is maximized.
    func unzoom() {
        if paneContainer.unzoom() {
            refocusAfterMovingViews()
        }
    }

    /// Gives every pane an equal share (`equalize_panes`), every pane shown.
    func equalizePanes() {
        unzoom()
        paneContainer.equalize()
    }

    /// Moving the views between split views can leave the window itself
    /// first responder; gives the keyboard back to the focused pane.
    private func refocusAfterMovingViews() {
        guard let window, window.firstResponder === window || window.firstResponder == nil else { return }
        window.makeFirstResponder(focusedPane.view)
    }

    /// The tab's pane layout to save (ADR 0031), or `nil` for one pane.
    var paneLayout: PaneLayout? {
        panes.count > 1 ? PaneLayout(paneContainer.tree) : nil
    }

    /// Lays the tab out as `layout`, a saved one, the first pane being
    /// this tab's one pane and each other a new session's. Does nothing
    /// (logged) for a layout that is not ``PaneLayout/isRestorable``, or
    /// whose panes at their smallest size do not fit the window, or once
    /// the tab has been split.
    ///
    /// - Throws: The core's error if a session cannot be started; the tab
    ///   then keeps its one pane and every session started for it is closed.
    func restoreLayout(_ layout: PaneLayout) throws {
        guard let delegate, panes.count == 1, layout.leafCount > 1 else { return }
        guard layout.isRestorable, let shape = layout.tree(leaves: Array(0..<layout.leafCount)) else {
            Self.log.error("a saved pane layout is not usable; the tab opens with one pane")
            return
        }
        let first = focusedPane
        let paneMinimum = PaneContainerView.minimumSize(of: first.view)
        let minimum = shape.minimumSize(divider: PaneSplitView.thickness) { _ in paneMinimum }
        let space = paneContainer.bounds.size
        guard minimum.width <= space.width, minimum.height <= space.height else {
            Self.log.info("a saved pane layout does not fit its window; the tab opens with one pane")
            return
        }
        var added: [Pane] = []
        do {
            for _ in 1..<layout.leafCount {
                added.append(Pane(session: try delegate.terminalWindowControllerSpawnSession(self)))
            }
        } catch {
            for pane in added {
                closeSession(of: pane)
                delegate.terminalWindowController(self, didClosePaneWith: pane.session)
            }
            throw error
        }
        guard let tree = layout.tree(leaves: [first] + added), paneContainer.restore(tree) else {
            // Not reached: the layout was checked above.
            for pane in added {
                closeSession(of: pane)
                delegate.terminalWindowController(self, didClosePaneWith: pane.session)
            }
            return
        }
        for pane in added {
            watchFocus(of: pane)
        }
        updateMinimumSize()
        focus(first)
        paneLayoutDidChange()
    }

    /// The panes' shape or a divider changed: the window state is saved.
    func paneLayoutDidChange() {
        delegate?.terminalWindowControllerDidChangeLayout(self)
    }

    /// ⌘W: closes the focused pane, asking first while its session runs a
    /// job; the last pane's ⌘W is the tab's close (``windowShouldClose(_:)``).
    /// Ignored while this tab asks, or its window asks for all its tabs.
    func closeFocusedPane(_ sender: Any?) {
        guard panes.count > 1 else {
            window?.performClose(sender)
            return
        }
        guard pendingClose == nil, !windowAsks else { return }
        let pane = focusedPane
        guard let presenter = delegate?.closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.pane, sessions: [pane.session])
        else {
            close(pane)
            return
        }
        pendingPane = pane
        ask(confirmation, presenter: presenter) { [weak self, weak pane] in
            guard let self, let pane else { return }
            close(pane)
        }
    }

    /// Closes `pane` and its session: its sibling takes its space, and the
    /// focus moves to the pane before it in reading order if it had it. The
    /// last pane closes the tab instead.
    private func close(_ pane: Pane) {
        let panes = panes
        guard let index = panes.firstIndex(of: pane) else { return }
        guard panes.count > 1 else {
            window?.close()
            return
        }
        // Asked about and gone meanwhile (its shell exited): the sheet
        // answers Cancel.
        if pendingPane === pane, let window {
            for sheet in window.sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
        }
        // An active input context (the key window's first responder's)
        // retains its view until it is deactivated.
        pane.view.inputContext?.deactivate()
        let wasFocused = pane === focusedPane
        unzoom()
        paneContainer.remove(pane)
        closeSession(of: pane)
        delegate?.terminalWindowController(self, didClosePaneWith: pane.session)
        updateMinimumSize()
        paneLayoutDidChange()
        // Moving the views between split views can leave the window itself
        // first responder; give the keyboard back to the focused pane.
        if wasFocused {
            let remaining = self.panes
            focus(remaining[max(0, min(index, remaining.count) - 1)])
        } else if window?.firstResponder === window {
            focus(focusedPane)
        }
    }

    private func closeSession(of pane: Pane) {
        do {
            try pane.session.close()
        } catch TesseraCoreError.sessionNotFound {
            // Already closed: nothing to do.
        } catch {
            // The core logged the details; the pane closes regardless.
            Self.log.error(
                "closing session \(pane.session.id) failed: \(String(describing: error), privacy: .public)")
        }
    }

    /// Follows `pane`'s view becoming the first responder: a click, a focus
    /// move, the window making it first.
    private func watchFocus(of pane: Pane) {
        pane.view.didBecomeFirstResponder = { [weak self, weak pane] in
            guard let self, let pane else { return }
            paneDidBecomeFocused(pane)
        }
    }

    private func paneDidBecomeFocused(_ pane: Pane) {
        guard panes.contains(pane) else { return }
        focusedPane = pane
        window?.initialFirstResponder = pane.view
        paneContainer.activePane = pane
        updateTitle()
    }

    private func updateTitle() {
        window?.title = Self.title(focusedPane.title, remote: showsRemoteHost ? focusedPane.remote : .local)
    }

    /// The window title for a pane titled `title` (empty: none set) whose
    /// shell runs at `remote`: `⇢ host: title`, or `⇢ ssh: title` when the
    /// host is not known; a pane with no title shows `⇢ host` or `⇢ ssh`
    /// alone. The host and the title both come sanitised from the core
    /// (docs/security.md S3).
    static func title(_ title: String, remote: TesseraRemote) -> String {
        let prefix: String
        switch remote {
        case .local: return title.isEmpty ? defaultTitle : title
        case .host(let host): prefix = "⇢ \(host)"
        case .unnamed: prefix = "⇢ ssh"
        }
        return title.isEmpty ? prefix : "\(prefix): \(title)"
    }

    /// The window cannot be made smaller than its panes' smallest sizes.
    private func updateMinimumSize() {
        window?.contentMinSize = paneContainer.minimumSize
    }

    /// Whether the window asks for all its tabs (⇧⌘W).
    private var windowAsks: Bool {
        let group = (window as? TerminalWindow)?.tabs ?? []
        return group.contains { ($0.windowController as? TerminalWindowController)?.pendingClose == .window }
    }

    // MARK: Actions

    /// ⌘D: splits the focused pane, the new pane on the right; a refused
    /// split beeps.
    @objc func splitPaneRight(_ sender: Any?) {
        split(.right)
    }

    /// ⌘⇧D: splits the focused pane, the new pane below.
    @objc func splitPaneBelow(_ sender: Any?) {
        split(.below)
    }

    private func split(_ direction: PaneSplitDirection) {
        do {
            if try splitFocusedPane(direction) == nil {
                NSSound.beep()
            }
        } catch {
            delegate?.presentOpenError(error)
        }
    }

    /// ⌘⌥←, ⌘⌥→, ⌘⌥↑, ⌘⌥↓: the focus moves to the neighbouring pane.
    @objc func selectPaneLeft(_ sender: Any?) { moveFocus(.left) }
    @objc func selectPaneRight(_ sender: Any?) { moveFocus(.right) }
    @objc func selectPaneAbove(_ sender: Any?) { moveFocus(.up) }
    @objc func selectPaneBelow(_ sender: Any?) { moveFocus(.down) }

    /// ⌘[ and ⌘]: the focus moves to the previous or next pane in reading
    /// order.
    @objc func selectPreviousPane(_ sender: Any?) { cycleFocus(by: -1) }
    @objc func selectNextPane(_ sender: Any?) { cycleFocus(by: 1) }

    /// ⌘⇧↩: maximizes the focused pane, or shows every pane again.
    @objc func toggleZoomPane(_ sender: Any?) {
        if !toggleZoom() {
            NSSound.beep()
        }
    }

    /// Shell > Equalize Panes.
    @objc func equalizePanes(_ sender: Any?) {
        equalizePanes()
    }

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
    /// window. If any pane of any of the tabs runs a job, one sheet on this
    /// tab lists them all first (D4b, D2), and Cancel keeps every tab.
    /// Ignored while this tab asks or the window already asks; another tab's
    /// question about itself or a pane stays up beside this one, and Close
    /// closes that tab too.
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
            let confirmation = CloseConfirmation(.window, sessions: controllers.flatMap(\.sessions))
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
            self?.pendingPane = nil
            if confirmed {
                close()
            }
        }
    }

    /// ⌘1 to ⌘9 are enabled only for tabs that exist, the pane moves,
    /// Maximize Pane (checked while a pane is) and Equalize Panes only with
    /// more than one pane.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(selectTabAtIndex(_:)):
            guard let window = window as? TerminalWindow else { return false }
            return window.tabs.indices.contains(menuItem.tag)
        case #selector(toggleZoomPane(_:)):
            menuItem.state = isZoomed ? .on : .off
            return panes.count > 1
        case #selector(selectPaneLeft(_:)), #selector(selectPaneRight(_:)), #selector(selectPaneAbove(_:)),
            #selector(selectPaneBelow(_:)), #selector(selectPreviousPane(_:)), #selector(selectNextPane(_:)),
            #selector(equalizePanes(_:)):
            return panes.count > 1
        default:
            return true
        }
    }

    // MARK: NSWindowDelegate

    /// The tab's close button, AppKit's own multi-tab closes (Close Other
    /// Tabs, Close All), which ask each tab, and ⌘W on the last pane:
    /// closes at once while every pane's shell is at its prompt; while any
    /// pane runs a job, asks once on a sheet, naming every busy pane's job
    /// (D2), and closes only if the user confirms. A request is ignored
    /// while this tab is already asking, or while its window asks for all
    /// its tabs (⇧⌘W); another tab's own question holds back nothing here.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if pendingClose != nil || windowAsks {
            return false
        }
        guard let presenter = delegate?.closeConfirmationPresenter,
            let confirmation = CloseConfirmation(.tab, sessions: sessions)
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

    /// ⌘W's title is this tab's while its window is key: another window's
    /// menu (one that is not a terminal's) says Close Tab again.
    func windowDidResignKey(_ notification: Notification) {
        Self.resetCloseTitle()
    }

    private static func resetCloseTitle() {
        MainMenu.item(forCommand: closeTabCommand, in: NSApp.mainMenu)?.title = PaneContainerView.closeTabTitle
    }

    func windowWillClose(_ notification: Notification) {
        Self.resetCloseTitle()
        // A tab closing under its close sheet (its shell exited, or ⇧⌘W on
        // another tab closed it): the sheet answers Cancel, or the alert and
        // its modal session would outlive the tab.
        if let window {
            for sheet in window.sheets {
                window.endSheet(sheet, returnCode: .cancel)
            }
        }
        for pane in panes {
            closeSession(of: pane)
        }
        // AppKit may keep the closed window (the macOS 15 runner does); the
        // panes must not live on in it. On the next turn, like each pane's
        // view leaving it, once AppKit is done with the close.
        let closedWindow = window
        Task { @MainActor [weak paneContainer, weak closedWindow] in
            guard let paneContainer, let closedWindow, closedWindow.contentView === paneContainer else { return }
            closedWindow.contentView = nil
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

    /// A new tab beside this one: this tab shows every pane again first.
    override func addTabbedWindow(_ window: NSWindow, ordered: NSWindow.OrderingMode) {
        (windowController as? TerminalWindowController)?.unzoom()
        super.addTabbedWindow(window, ordered: ordered)
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
