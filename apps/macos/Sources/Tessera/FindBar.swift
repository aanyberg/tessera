import AppKit
import TesseraKit
import os

/// One pane's search (ADR 0038, #46b): the query and its toggles, what the
/// core reports, and the stepping its view's display link drives. Owned by
/// the pane's ``TerminalSurfaceView``; the ``FindBarView`` shows it while
/// the bar is open.
///
/// The search lives in the core while the bar is open and is cleared when
/// it closes, so a closed bar makes no search call per frame (its view asks
/// ``needsStep`` only). The query, the toggles and the last query stay for
/// the next opening (⌘G with the bar closed opens it with the last query).
///
/// Neither the query nor match text is logged (S8).
@MainActor
final class FindController {
    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "find")

    /// The step budget per display refresh, in microseconds: a quarter of
    /// a 60 Hz frame, so typing stays smooth while a long history is
    /// scanned (docs/security.md: a step returns within four budgets plus
    /// 250 ms plus one regex call, which a pathological pattern can make
    /// last many seconds on a query's first call, 7.3 s measured on an 8 KB
    /// line; ADR 0038).
    static let stepBudgetMicros: UInt32 = 4_000

    /// How long typing must pause before the query reaches the core
    /// (tests set it).
    static var debounce: Duration = .milliseconds(120)

    let session: TesseraSession

    /// Whether the bar is open (the search is in the core).
    private(set) var isOpen = false
    /// The query as typed.
    private(set) var query = ""
    /// The query when the bar last closed, for ⌘G with the bar closed.
    private(set) var lastQuery = ""
    /// Read the query as a regex.
    var regex = false {
        didSet { if regex != oldValue { restartNow() } }
    }
    /// How case is compared; smart case by default.
    var caseMode: TesseraSession.SearchCase = .auto {
        didSet { if caseMode != oldValue { restartNow() } }
    }
    /// What the core last reported.
    private(set) var status = TesseraSession.SearchStatus()
    /// Why the core refused the query, or `nil`.
    private(set) var refusal: TesseraSession.SearchRefusal?
    /// A note on the last Copy All Matches, until the query changes.
    private(set) var notice: String?
    /// The search has work for the next display refresh: matches still to
    /// find, or output to search. False whenever the bar is closed.
    private(set) var needsStep = false {
        didSet { if needsStep != oldValue { onNeedsStepChange?() } }
    }

    /// The typed query has not reached the core yet (debounced).
    private(set) var hasPendingQuery = false
    /// A move asked for before the search found anything: made by the
    /// step that finds the first match. A new query asks for one too, so
    /// its newest match becomes current (and is scrolled to) as soon as it
    /// is found.
    private(set) var pendingMove: Direction?
    enum Direction { case older, newer }
    private var debounceGeneration = 0

    /// Called after anything shown changed: the bar's text, the
    /// highlights (the view redraws).
    var onChange: (() -> Void)?
    /// Called when ``needsStep`` changes (the view wakes or idles its
    /// display link).
    var onNeedsStepChange: (() -> Void)?

    // Test seams.

    /// Search calls into the core, of every kind.
    private(set) var searchCallCount = 0
    /// Steps made.
    private(set) var stepCount = 0

    init(session: TesseraSession) {
        self.session = session
    }

    // MARK: Opening and closing

    /// Opens the bar: the query (or `query`, when given) goes to the core
    /// at once. Opening an open bar changes nothing but the query.
    func open(query newQuery: String? = nil) {
        if let newQuery, newQuery != query {
            query = newQuery
            notice = nil
        } else if isOpen {
            return
        }
        isOpen = true
        restartNow()
    }

    /// Use Selection for Find (⌘E): `text` becomes the query, searched at
    /// once with the bar open, and the one ⌘G searches for with it closed
    /// (as in other Mac apps, the bar is not opened).
    func use(query text: String) {
        if isOpen {
            open(query: text)
        } else {
            query = text
            lastQuery = text
        }
        onChange?()
    }

    /// Closes the bar: the core's search and its highlights go, and the
    /// query is kept as the last one.
    func close() {
        guard isOpen else { return }
        isOpen = false
        lastQuery = query
        hasPendingQuery = false
        debounceGeneration += 1
        pendingMove = nil
        needsStep = false
        status = .init()
        refusal = nil
        notice = nil
        call("clear") { try session.searchClear() }
        onChange?()
    }

    // MARK: Query

    /// The field's text changed: it reaches the core after
    /// ``FindController/debounce``, or at once when emptied (clearing costs
    /// nothing to compile).
    func setQuery(_ text: String) {
        guard text != query else { return }
        query = text
        notice = nil
        guard isOpen else { return }
        if text.isEmpty || Self.debounce <= .zero {
            restartNow()
            return
        }
        hasPendingQuery = true
        debounceGeneration += 1
        let generation = debounceGeneration
        let delay = Self.debounce
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: delay)
            guard let self, generation == debounceGeneration else { return }
            flushQuery()
        }
    }

    /// Sends a debounced query now (Return, ⌘G).
    func flushQuery() {
        guard hasPendingQuery else { return }
        restartNow()
    }

    /// Hands the query and toggles to the core: the same ones keep the
    /// search's matches and current match, others start afresh.
    private func restartNow() {
        hasPendingQuery = false
        debounceGeneration += 1
        guard isOpen else { return }
        notice = nil
        refusal = nil
        pendingMove = nil
        let sensitive = caseMode.isSensitive(for: query, regex: regex)
        do {
            searchCallCount += 1
            try session.searchStart(query, regex: regex, caseSensitive: sensitive)
        } catch let refused as TesseraSession.SearchRefusal {
            refusal = refused
            Self.log.debug("query refused: \(Self.kind(of: refused), privacy: .public)")
        } catch {
            Self.log.debug("search not started: \(String(describing: error), privacy: .public)")
        }
        refreshStatus()
        if status.active, status.current == nil {
            pendingMove = .older
        }
        needsStep = status.active && !status.done
        onChange?()
    }

    // MARK: Stepping

    /// One display refresh: one step with ``stepBudgetMicros`` when the
    /// search has work, and the move waiting for its first match. Returns
    /// whether it stepped.
    @discardableResult
    func tick() -> Bool {
        guard isOpen, needsStep else { return false }
        stepCount += 1
        searchCallCount += 1
        let before = status
        do {
            status = try session.searchStep(budgetMicros: Self.stepBudgetMicros)
        } catch {
            // The session closed: nothing more to find.
            status = .init()
        }
        needsStep = status.active && !status.done
        if let move = pendingMove, status.matches > 0 {
            pendingMove = nil
            revealFirstMatch(move)
            return true
        } else if status.done || !status.active {
            pendingMove = nil
        }
        // A step that found nothing new leaves the highlights as drawn.
        if status != before { onChange?() }
        return true
    }

    /// Output arrived: the next refresh searches it. Only a flag, whatever
    /// the output's rate.
    func outputArrived() {
        guard isOpen, status.active else { return }
        needsStep = true
    }

    // MARK: Navigation

    /// The match before the current one, toward older output (Return, ⌘G),
    /// wrapping to the newest. Returns whether there was one to move to.
    /// Before the search found anything the move waits for the first
    /// match.
    @discardableResult
    func older() -> Bool { request(.older) }

    /// The match after the current one, toward newer output (⇧Return,
    /// ⌘⇧G), wrapping to the oldest.
    @discardableResult
    func newer() -> Bool { request(.newer) }

    private func request(_ direction: Direction) -> Bool {
        flushQuery()
        guard isOpen, status.active else { return false }
        if status.matches == 0 {
            guard !status.done else { return false }
            pendingMove = direction
            return true
        }
        pendingMove = nil
        return move(direction)
    }

    /// The first match found: made current by `direction` and scrolled to.
    /// The core makes its newest match current by itself (ADR 0038, D16)
    /// but scrolls only on a move or a reveal: one reveal call scrolls to
    /// it, the view moved only if that match was out of it.
    private func revealFirstMatch(_ direction: Direction) {
        guard status.current != nil else {
            _ = move(direction)
            return
        }
        searchCallCount += 1
        do {
            status = try session.searchReveal()
        } catch {
            Self.log.debug("search reveal failed: \(String(describing: error), privacy: .public)")
        }
        onChange?()
    }

    private func move(_ direction: Direction) -> Bool {
        searchCallCount += 1
        do {
            status = try direction == .older ? session.searchOlder() : session.searchNewer()
        } catch {
            Self.log.debug("search move failed: \(String(describing: error), privacy: .public)")
        }
        onChange?()
        return true
    }

    // MARK: Copy All Matches

    /// Puts the text of every match found on `pasteboard`, joined by
    /// newlines (at most 1 MiB, ADR 0038), and says so in ``notice``,
    /// flagging a cut. Returns `false`, leaving the pasteboard alone,
    /// without matches.
    @discardableResult
    func copyAllMatches(to pasteboard: NSPasteboard) -> Bool {
        flushQuery()
        guard isOpen, status.matches > 0 else { return false }
        searchCallCount += 1
        let copied: (text: String, truncated: Bool)
        do {
            copied = try session.searchMatchesText()
        } catch {
            Self.log.debug("matches not copied: \(String(describing: error), privacy: .public)")
            return false
        }
        guard !copied.text.isEmpty else { return false }
        pasteboard.clearContents()
        pasteboard.setString(copied.text, forType: .string)
        notice = Self.copyNotice(copyTruncated: copied.truncated, status: status)
        onChange?()
        return true
    }

    /// What Copy All Matches says, from whether the copy stopped at 1 MiB
    /// (`copyTruncated`) and the search's `status`: it says "all" only when
    /// the copy is whole, no match gave way at the cap and no line was
    /// skipped (#331).
    static func copyNotice(copyTruncated: Bool, status: TesseraSession.SearchStatus) -> String {
        if copyTruncated {
            return "Copied the matches that fit in 1 MB"
        }
        if status.truncated {
            // Only the matches the search kept (the newest
            // ``TesseraSession/searchMaxMatches``) were copied.
            let copied = "Copied \(format(status.matches)) of more than \(format(TesseraSession.searchMaxMatches))"
            return status.linesSkipped ? "\(copied); some lines were skipped" : copied
        }
        if status.linesSkipped {
            // The skipped lines' matches are not in the copy.
            return "Copied the matches found; some lines were skipped"
        }
        return "Copied all matches"
    }

    // MARK: Shown text

    /// The count: "3 of 12", "12 matches" without a current one, "more
    /// than 100 000" at the cap, "No matches", or empty without a query
    /// (or with a refused one: ``errorText`` says why); with ", some lines
    /// skipped" when lines too slow for the pattern were left out or cut
    /// (their matches may be missing).
    var countText: String {
        guard isOpen, !query.isEmpty, refusal == nil, status.active else { return "" }
        return Self.countText(for: status)
    }

    /// ``countText`` for an open bar with an accepted query and `status`.
    static func countText(for status: TesseraSession.SearchStatus) -> String {
        let count = baseCountText(for: status)
        return status.linesSkipped ? "\(count), some lines skipped" : count
    }

    private static func baseCountText(for status: TesseraSession.SearchStatus) -> String {
        if status.truncated {
            return "more than \(format(TesseraSession.searchMaxMatches))"
        }
        if status.matches == 0 {
            return status.done ? "No matches" : "Searching…"
        }
        if let current = status.current, current < status.matches {
            return "\(format(current + 1)) of \(format(status.matches))"
        }
        return status.matches == 1 ? "1 match" : "\(format(status.matches)) matches"
    }

    /// Why the query was refused, short enough for the bar, or `nil`.
    var errorText: String? {
        switch refusal {
        case nil: nil
        case .tooLong: "Too long (at most \(Self.format(TesseraSession.searchMaxQueryBytes)) bytes)"
        case .tooComplex: "Too complex to search"
        case .notUTF8: "Not valid text"
        case .syntax(let message): Self.syntaxSummary(message)
        }
    }

    /// The core's full message for a refused regex, for the tooltip, made
    /// safe to show (the message quotes the pattern), or `nil`.
    var errorDetail: String? {
        guard case .syntax(let message) = refusal else { return nil }
        return message.split(separator: "\n", omittingEmptySubsequences: false)
            .map { FindController.displayText(String($0)) }.joined(separator: "\n")
    }

    /// The last line of the regex crate's message ("error: unclosed
    /// group"), as "Invalid regex: unclosed group".
    static func syntaxSummary(_ message: String) -> String {
        let last = message.split(separator: "\n").last.map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
        let reason = last.hasPrefix("error:") ? String(last.dropFirst("error:".count)).trimmingCharacters(in: .whitespaces) : last
        let shown = displayText(reason)
        return shown.isEmpty ? "Invalid regex" : "Invalid regex: \(shown)"
    }

    static func format(_ count: Int) -> String {
        count.formatted(.number.grouping(.automatic))
    }

    /// The kind of a refusal, for the log: never its message (S8).
    private static func kind(of refusal: TesseraSession.SearchRefusal) -> String {
        switch refusal {
        case .tooLong: "too long"
        case .syntax: "syntax"
        case .tooComplex: "too complex"
        case .notUTF8: "not UTF-8"
        }
    }

    private func refreshStatus() {
        searchCallCount += 1
        status = (try? session.searchStatus) ?? .init()
    }

    private func call(_ what: StaticString, _ body: () throws -> Void) {
        searchCallCount += 1
        do {
            try body()
        } catch {
            Self.log.debug("search \(what, privacy: .public) failed: \(String(describing: error), privacy: .public)")
        }
    }

    // MARK: Child text

    /// `text` as the UI may show it (S3): controls (C0, DEL, C1),
    /// bidirectional controls (Bidi_Control: ALM, LRM, RLM, the embeddings,
    /// overrides and isolates), line and paragraph separators and
    /// unassigned characters are dropped. Other format characters (ZWJ,
    /// ZWNJ, a soft hyphen) are text and stay, so a selection still finds
    /// its own source.
    static func displayText(_ text: String) -> String {
        var shown = String.UnicodeScalarView()
        for scalar in text.unicodeScalars {
            switch scalar.properties.generalCategory {
            case .control, .lineSeparator, .paragraphSeparator, .unassigned:
                continue
            default:
                if scalar.properties.isBidiControl { continue }
                shown.append(scalar)
            }
        }
        return String(shown)
    }


    /// The query Use Selection for Find (⌘E) makes of selected terminal
    /// text (child text): its first line, made safe to show
    /// (``displayText(_:)``), at most ``TesseraSession/searchMaxQueryBytes``
    /// bytes cut at a character, and escaped for a regex when `regex` is on
    /// so it still finds itself. `nil` when nothing is left.
    static func query(fromSelection selection: String, regex: Bool) -> String? {
        let line = selection.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline).first.map(String.init) ?? ""
        var query = displayText(line)
        if regex {
            query = escapedForRegex(query)
        }
        while query.utf8.count > TesseraSession.searchMaxQueryBytes {
            query.removeLast()
            // An escape cut in half would be a different pattern.
            if regex, query.hasSuffix("\\"), !query.hasSuffix("\\\\") || isOddRunOfBackslashes(query) {
                query.removeLast()
            }
        }
        return query.isEmpty ? nil : query
    }

    /// `text` with every regex metacharacter escaped (the `regex` crate's
    /// syntax: escaping ASCII punctuation is always allowed; `<` and `>`
    /// are left, since `\<` and `\>` are word boundaries). Escaped per
    /// unicode scalar, as the regex reads it: a `.` with a combining mark
    /// is one Swift character but still a metacharacter.
    static func escapedForRegex(_ text: String) -> String {
        var escaped = String.UnicodeScalarView()
        for scalar in text.unicodeScalars {
            if "\\.+*?()|[]{}^$#&-~".unicodeScalars.contains(scalar) { escaped.append("\\") }
            escaped.append(scalar)
        }
        return String(escaped)
    }

    private static func isOddRunOfBackslashes(_ text: String) -> Bool {
        text.reversed().prefix { $0 == "\\" }.count % 2 == 1
    }
}

/// The find bar: a small panel floating in the top trailing corner of its
/// pane's terminal (E2): the query field, the count or the error, the
/// older and newer buttons, the Case and Regex toggles and Close. A
/// subview of the pane's ``TerminalSurfaceView``; it takes no layout slot
/// and never resizes the grid.
///
/// In the field Return moves to the older match, Shift-Return to the
/// newer (D16a), and Escape closes the bar.
@MainActor
final class FindBarView: NSView, NSTextFieldDelegate {
    /// Distance from the pane's top and trailing edges.
    static let margin: CGFloat = 8
    /// The widest the bar gets; a narrow pane gets a narrower one.
    static let preferredWidth: CGFloat = 420

    private weak var controller: FindController?

    /// What the bar's controls ask of its pane (the terminal view acts).
    var onClose: (() -> Void)?
    var onOlder: (() -> Void)?
    var onNewer: (() -> Void)?

    let field = FindField()
    let countLabel = NSTextField(labelWithString: "")
    let olderButton = NSButton()
    let newerButton = NSButton()
    let caseButton = NSPopUpButton(frame: .zero, pullsDown: true)
    let regexButton = NSButton()
    let closeButton = NSButton()
    private let background = NSVisualEffectView()

    init(controller: FindController) {
        self.controller = controller
        super.init(frame: NSRect(x: 0, y: 0, width: Self.preferredWidth, height: 32))
        setUp()
        refresh()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // A click on the bar outside its controls (its background, the count)
    // stops here: up the responder chain the terminal would take the
    // keyboard from the field and send the press to the program.
    override func mouseDown(with event: NSEvent) {}
    override func mouseDragged(with event: NSEvent) {}
    override func mouseUp(with event: NSEvent) {}
    override func rightMouseDown(with event: NSEvent) {}
    override func rightMouseDragged(with event: NSEvent) {}
    override func rightMouseUp(with event: NSEvent) {}
    override func otherMouseDown(with event: NSEvent) {}
    override func otherMouseDragged(with event: NSEvent) {}
    override func otherMouseUp(with event: NSEvent) {}

    private func setUp() {
        wantsLayer = true
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel("Find")
        shadow = {
            let shadow = NSShadow()
            shadow.shadowBlurRadius = 6
            shadow.shadowOffset = NSSize(width: 0, height: -1)
            shadow.shadowColor = NSColor.black.withAlphaComponent(0.25)
            return shadow
        }()

        background.material = .menu
        background.blendingMode = .withinWindow
        background.state = .active
        background.wantsLayer = true
        background.layer?.cornerRadius = 8
        background.layer?.masksToBounds = true
        background.layer?.borderWidth = 0.5
        background.layer?.borderColor = NSColor.separatorColor.cgColor
        background.translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)

        field.placeholderString = "Find"
        field.delegate = self
        field.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        field.controlSize = .small
        field.bezelStyle = .roundedBezel
        field.lineBreakMode = .byTruncatingTail
        field.usesSingleLineMode = true
        field.cell?.isScrollable = true
        field.setAccessibilityLabel("Find")
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        countLabel.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        countLabel.textColor = .secondaryLabelColor
        countLabel.lineBreakMode = .byTruncatingTail
        countLabel.alignment = .right
        countLabel.setContentHuggingPriority(.defaultHigh, for: .horizontal)
        countLabel.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)

        configure(olderButton, symbol: "chevron.up", label: "Previous Match (older)", action: #selector(olderClicked))
        configure(newerButton, symbol: "chevron.down", label: "Next Match (newer)", action: #selector(newerClicked))
        configure(closeButton, symbol: "xmark", label: "Close Find (Esc)", action: #selector(closeClicked))

        regexButton.title = ".*"
        regexButton.font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
        regexButton.setButtonType(.pushOnPushOff)
        regexButton.bezelStyle = .accessoryBarAction
        regexButton.controlSize = .small
        regexButton.toolTip = "Regular Expression"
        regexButton.setAccessibilityLabel("Regular Expression")
        regexButton.target = self
        regexButton.action = #selector(regexClicked)

        caseButton.controlSize = .small
        caseButton.bezelStyle = .accessoryBarAction
        caseButton.isBordered = false
        caseButton.toolTip = "Match Case"
        caseButton.setAccessibilityLabel("Match Case")
        let menu = NSMenu()
        let title = NSMenuItem(title: "Aa", action: nil, keyEquivalent: "")
        menu.addItem(title)
        for (mode, name) in Self.caseModes {
            let item = NSMenuItem(title: name, action: #selector(caseChosen(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = Self.caseModes.firstIndex { $0.0 == mode }
            menu.addItem(item)
        }
        caseButton.menu = menu
        caseButton.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .medium)

        let stack = NSStackView(views: [field, countLabel, olderButton, newerButton, caseButton, regexButton, closeButton])
        stack.orientation = .horizontal
        stack.spacing = 4
        stack.setCustomSpacing(8, after: countLabel)
        stack.edgeInsets = NSEdgeInsets(top: 4, left: 6, bottom: 4, right: 4)
        stack.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(stack)
        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: background.trailingAnchor),
            stack.topAnchor.constraint(equalTo: background.topAnchor),
            stack.bottomAnchor.constraint(equalTo: background.bottomAnchor),
            field.widthAnchor.constraint(greaterThanOrEqualToConstant: 60),
        ])
    }

    static let caseModes: [(TesseraSession.SearchCase, String)] = [
        (.auto, "Smart Case"), (.sensitive, "Match Case"), (.insensitive, "Ignore Case"),
    ]

    private func configure(_ button: NSButton, symbol: String, label: String, action: Selector) {
        button.image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        button.imagePosition = .imageOnly
        button.bezelStyle = .accessoryBarAction
        button.isBordered = false
        button.controlSize = .small
        button.toolTip = label
        button.setAccessibilityLabel(label)
        button.target = self
        button.action = action
    }

    override var isFlipped: Bool { true }

    /// The bar's height at its controls' size.
    var preferredHeight: CGFloat { max(28, fittingSize.height) }

    /// Shows what the controller holds: the count or the error, the
    /// toggles, and whether there is anything to move to.
    func refresh() {
        guard let controller else { return }
        // Typing keeps the two equal; a difference is a query set from
        // elsewhere (Use Selection, the last query), shown even mid-edit.
        if field.stringValue != controller.query {
            field.stringValue = controller.query
        }
        if let error = controller.errorText {
            countLabel.stringValue = error
            countLabel.textColor = .systemRed
            countLabel.toolTip = controller.errorDetail
        } else {
            countLabel.stringValue = controller.notice ?? controller.countText
            countLabel.textColor = .secondaryLabelColor
            countLabel.toolTip = nil
        }
        let canMove = controller.status.matches > 0 || controller.hasPendingQuery
        olderButton.isEnabled = canMove
        newerButton.isEnabled = canMove
        regexButton.state = controller.regex ? .on : .off
        let index = Self.caseModes.firstIndex { $0.0 == controller.caseMode } ?? 0
        for (offset, item) in (caseButton.menu?.items.dropFirst() ?? []).enumerated() {
            item.state = offset == index ? .on : .off
        }
        let (shortTitle, help) =
            switch controller.caseMode {
            case .auto: ("Aa", "Smart Case: matches case when the query has a capital letter")
            case .sensitive: ("Aa", "Match Case")
            case .insensitive: ("aa", "Ignore Case")
            }
        caseButton.menu?.items.first?.title = shortTitle
        caseButton.contentTintColor = controller.caseMode == .sensitive ? .controlAccentColor : nil
        caseButton.toolTip = help
        caseButton.setAccessibilityValue(Self.caseModes[index].1)
    }

    /// Puts the keyboard in the field with its text selected (⌘F).
    func focusField() {
        guard let window else { return }
        if field.currentEditor() == nil {
            window.makeFirstResponder(field)
        }
        field.currentEditor()?.selectAll(nil)
    }

    /// Whether the field has the keyboard.
    var fieldHasFocus: Bool { field.currentEditor() != nil && window?.firstResponder === field.currentEditor() }

    // MARK: Actions

    @objc private func olderClicked() { onOlder?() }
    @objc private func newerClicked() { onNewer?() }
    @objc private func closeClicked() { onClose?() }

    @objc private func regexClicked() {
        controller?.regex = regexButton.state == .on
    }

    @objc private func caseChosen(_ sender: NSMenuItem) {
        guard let index = sender.representedObject as? Int, Self.caseModes.indices.contains(index) else { return }
        controller?.caseMode = Self.caseModes[index].0
    }

    // MARK: NSTextFieldDelegate

    func controlTextDidChange(_ notification: Notification) {
        controller?.setQuery(field.stringValue)
    }

    /// Return and Shift-Return move, Escape closes.
    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertLineBreak(_:)):
            let shift = NSApp.currentEvent?.modifierFlags.contains(.shift) ?? false
            if shift { onNewer?() } else { onOlder?() }
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            onClose?()
            return true
        default:
            return false
        }
    }
}

/// The find bar's query field: tells its pane when it takes the keyboard,
/// so the tab's focused pane follows (ADR 0035).
@MainActor
final class FindField: NSTextField {
    var onFocus: (() -> Void)?

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became { onFocus?() }
        return became
    }
}
