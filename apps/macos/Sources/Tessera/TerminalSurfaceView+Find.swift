import AppKit
import TesseraKit

/// The find bar (#46b, ADR 0038): Edit > Find's actions, sent to the
/// focused pane's terminal view, and the bar floating in its top trailing
/// corner (E2).
extension TerminalSurfaceView {
    /// This pane's search, made on first use.
    var find: FindController {
        if let findController { return findController }
        let controller = FindController(session: session)
        controller.onChange = { [weak self] in self?.findChanged() }
        controller.onNeedsStepChange = { [weak self] in self?.searchNeedsStepChanged() }
        findController = controller
        return controller
    }

    /// The bar while it is shown.
    var findBar: FindBarView? { subviews.lazy.compactMap { $0 as? FindBarView }.first }

    /// Whether the find bar is open.
    var isFindBarOpen: Bool { findController?.isOpen ?? false }

    // MARK: Menu actions

    /// Find… (⌘F): opens the bar (with the query it had) and puts the
    /// keyboard in its field, the text selected.
    @objc func showFindBar(_ sender: Any?) {
        find.open()
        showBar().focusField()
    }

    /// Find Next (⌘G): the match before the current one, toward older
    /// output (D16a), wrapping to the newest. With the bar closed it opens
    /// with the last query, its newest match current.
    @objc func findNextMatch(_ sender: Any?) {
        if !reopenFindBar() { find.older() }
    }

    /// Find Previous (⌘⇧G): the match after the current one, toward newer
    /// output, wrapping to the oldest. With the bar closed it opens as Find
    /// Next does.
    @objc func findPreviousMatch(_ sender: Any?) {
        if !reopenFindBar() { find.newer() }
    }

    /// Use Selection for Find (⌘E): the selection's first line becomes the
    /// query (made safe to show, S3; escaped in regex mode). Beeps without
    /// a selection.
    @objc func useSelectionForFind(_ sender: Any?) {
        guard let text = try? session.selectionText(),
            let query = FindController.query(fromSelection: text, regex: find.regex)
        else {
            NSSound.beep()
            return
        }
        find.use(query: query)
    }

    /// Copy All Matches: the text of every match found goes to the
    /// pasteboard, one per line; the bar says when it was cut at 1 MB.
    /// Beeps without matches.
    @objc func copyAllFindMatches(_ sender: Any?) {
        if !find.copyAllMatches(to: pasteboard) { NSSound.beep() }
    }

    /// Closes the bar (Escape, its close button): the search and its
    /// highlights go, and the keyboard returns to the terminal.
    func closeFindBar() {
        let hadFocus = findBar?.fieldHasFocus ?? false
        findController?.close()
        findBar?.removeFromSuperview()
        if hadFocus || window?.firstResponder == nil || window?.firstResponder === window {
            window?.makeFirstResponder(self)
        }
    }

    // MARK: Bar

    /// Opens the bar with the last query when it is closed (⌘G, ⌘⇧G),
    /// leaving the keyboard in the terminal. Returns whether it opened it.
    private func reopenFindBar() -> Bool {
        guard !find.isOpen else { return false }
        find.open(query: find.lastQuery)
        showBar()
        return true
    }

    /// The bar, added if it is not shown: below a dimmed pane's shade,
    /// which stays the pane's top subview.
    @discardableResult
    private func showBar() -> FindBarView {
        if let bar = findBar { return bar }
        let bar = FindBarView(controller: find)
        bar.onClose = { [weak self] in self?.closeFindBar() }
        bar.onOlder = { [weak self] in self?.findController?.older() }
        bar.onNewer = { [weak self] in self?.findController?.newer() }
        bar.field.onFocus = { [weak self] in self?.didBecomeFirstResponder?() }
        if let shade = subviews.last(where: { $0 is PaneDimView }) {
            addSubview(bar, positioned: .below, relativeTo: shade)
        } else {
            addSubview(bar)
        }
        layoutFindBar()
        return bar
    }

    /// Places the bar in the top trailing corner, clear of the scroll bar,
    /// as wide as fits. Never resizes the grid.
    func layoutFindBar() {
        guard let bar = findBar else { return }
        let margin = FindBarView.margin
        let trailing = margin + TerminalScroller.width(for: scroller.effectiveStyle)
        let width = max(0, min(FindBarView.preferredWidth, bounds.width - margin - trailing))
        let height = bar.preferredHeight
        let x = bounds.width - trailing - width
        let y = isFlipped ? margin : bounds.height - margin - height
        let frame = NSRect(x: x, y: y, width: width, height: height).integral
        if bar.frame != frame { bar.frame = frame }
    }

    /// The search changed: the bar's text and the highlights follow.
    private func findChanged() {
        findBar?.refresh()
        searchDidChange()
    }

    /// One refresh's search step (the display link calls it).
    func stepSearch() {
        findController?.tick()
    }
}
