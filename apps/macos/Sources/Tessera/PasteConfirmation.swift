import AppKit
import TesseraKit

/// The question a guarded paste asks (#471, ADR 0022 D-PASTE): the core
/// decided to ask (``TesseraSession/pasteCheck(_:)``) and made the preview
/// (``TesseraSession/pastePreview(_:)``), escaped and bounded, so the sheet
/// shows only what the core gave and the line count. Cancel is the default
/// button: a Return typed right after ⌘V must not run the lines.
struct PasteConfirmation: Equatable {
    let preview: TesseraSession.PastePreview
    /// Whether the program had bracketed paste on.
    let bracketed: Bool

    var messageText: String {
        preview.lines == 1 ? "Paste 1 line?" : "Paste \(preview.lines.formatted()) lines?"
    }

    /// Why it asks: each line runs, the text holds controls, or neither
    /// (`paste_guard = "always"`).
    var informativeText: String {
        var reasons: [String] = []
        if preview.hasLineBreaks && !bracketed {
            reasons.append(
                "The program has not turned on bracketed paste, so each line break runs what comes before it as a command.")
        }
        if preview.hasControls {
            reasons.append("The text holds control characters, shown escaped (^[ is Escape); pasting removes them.")
        }
        if reasons.isEmpty {
            reasons.append("Check the text before it is sent to the program.")
        }
        return reasons.joined(separator: " ")
    }

    /// The preview with a count of the lines it leaves out.
    var shownText: String {
        guard preview.moreLines > 0 else { return preview.text }
        let more = preview.moreLines == 1 ? "1 more line" : "\(preview.moreLines.formatted()) more lines"
        return preview.text + "\n… " + more
    }
}

/// Shows a ``PasteConfirmation`` and reports the answer. Each terminal view
/// holds one; tests record instead of showing sheets.
@MainActor
protocol PasteConfirmationPresenter: AnyObject {
    /// Asks on a sheet of `window`; calls `completion` once, with `true` to
    /// paste.
    func confirm(
        _ confirmation: PasteConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void)
    /// Ends the open confirmation as Cancel (its view left the window).
    func dismiss()
}

/// ``PasteConfirmationPresenter`` with a ``PasteConfirmationSheet``.
@MainActor
final class SheetPasteConfirmationPresenter: PasteConfirmationPresenter {
    /// The sheet up now (the window holds it while it is).
    private(set) weak var sheet: PasteConfirmationSheet?

    func confirm(
        _ confirmation: PasteConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void
    ) {
        let sheet = PasteConfirmationSheet(confirmation)
        self.sheet = sheet
        window.beginSheet(sheet) { response in
            MainActor.assumeIsolated { completion(response == .OK) }
        }
        // `beginSheet` clears the sheet's default button key equivalent
        // (seen on macOS 27), so Cancel takes Return only from here on.
        sheet.makeCancelTheDefault()
    }

    func dismiss() {
        sheet?.cancelOperation(nil)
    }
}

/// The sheet: the question, why it is asked, the escaped preview in a
/// scroll view, and Paste and Cancel, Cancel the default (Return). Escape
/// and ⌘. answer Cancel too, whatever has the focus.
@MainActor
final class PasteConfirmationSheet: NSPanel {
    let messageLabel: NSTextField
    let informativeLabel: NSTextField
    let previewView = NSTextView()
    let pasteButton: NSButton
    let cancelButton: NSButton

    /// The preview's size on screen: about twelve lines of 11 pt text.
    static let previewSize = NSSize(width: 460, height: 180)

    init(_ confirmation: PasteConfirmation) {
        messageLabel = NSTextField(wrappingLabelWithString: confirmation.messageText)
        informativeLabel = NSTextField(wrappingLabelWithString: confirmation.informativeText)
        pasteButton = NSButton(title: "Paste", target: nil, action: nil)
        cancelButton = NSButton(title: "Cancel", target: nil, action: nil)
        super.init(
            contentRect: NSRect(x: 0, y: 0, width: 500, height: 320), styleMask: [.titled, .docModalWindow],
            backing: .buffered, defer: true)
        isReleasedWhenClosed = false
        messageLabel.font = .boldSystemFont(ofSize: NSFont.systemFontSize)
        informativeLabel.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        for label in [messageLabel, informativeLabel] {
            label.preferredMaxLayoutWidth = Self.previewSize.width
            label.isSelectable = false
        }

        // Left to right whatever the text's first strong character, so a
        // right-to-left word does not reorder the line around it (text
        // after a `#` shown before the command).
        let paragraph = NSMutableParagraphStyle()
        paragraph.baseWritingDirection = .leftToRight
        paragraph.alignment = .left
        previewView.defaultParagraphStyle = paragraph
        previewView.typingAttributes[.paragraphStyle] = paragraph
        previewView.string = confirmation.shownText
        previewView.textStorage?.addAttribute(
            .paragraphStyle, value: paragraph, range: NSRange(location: 0, length: previewView.textStorage?.length ?? 0))
        previewView.isEditable = false
        previewView.isSelectable = true
        previewView.isRichText = false
        previewView.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        previewView.textContainerInset = NSSize(width: 4, height: 4)
        previewView.isVerticallyResizable = true
        previewView.autoresizingMask = [.width]
        previewView.setAccessibilityLabel("Text to paste")
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.documentView = previewView
        scroll.translatesAutoresizingMaskIntoConstraints = false

        pasteButton.target = self
        pasteButton.action = #selector(pasteClicked(_:))
        cancelButton.target = self
        cancelButton.action = #selector(cancelClicked(_:))
        let buttons = NSStackView(views: [pasteButton, cancelButton])
        buttons.orientation = .horizontal
        buttons.spacing = 12

        let stack = NSStackView(views: [messageLabel, informativeLabel, scroll, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        stack.setCustomSpacing(16, after: scroll)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(stack)
        contentView = content
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            scroll.widthAnchor.constraint(equalToConstant: Self.previewSize.width),
            scroll.heightAnchor.constraint(equalToConstant: Self.previewSize.height),
            buttons.trailingAnchor.constraint(equalTo: stack.trailingAnchor, constant: -20),
        ])
        makeCancelTheDefault()
        initialFirstResponder = cancelButton
    }

    /// Cancel answers Return.
    func makeCancelTheDefault() {
        cancelButton.keyEquivalent = "\r"
        defaultButtonCell = cancelButton.cell as? NSButtonCell
    }

    @objc private func pasteClicked(_ sender: Any?) { answer(.OK) }
    @objc private func cancelClicked(_ sender: Any?) { answer(.cancel) }

    override func cancelOperation(_ sender: Any?) { answer(.cancel) }

    /// Escape or ⌘., without other modifiers. A focused button does not pass
    /// Escape on as `cancelOperation(_:)`, so the sheet reads the keys
    /// itself.
    static func isCancelKey(_ event: NSEvent) -> Bool {
        guard event.type == .keyDown else { return false }
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            .subtracting([.capsLock, .numericPad, .function])
        if event.keyCode == 53 { return modifiers.isEmpty }
        return modifiers == .command && event.charactersIgnoringModifiers == "."
    }

    /// The application's key equivalent pass, before the key window gets
    /// the key.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if Self.isCancelKey(event) {
            answer(.cancel)
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func sendEvent(_ event: NSEvent) {
        if Self.isCancelKey(event) {
            answer(.cancel)
            return
        }
        super.sendEvent(event)
    }

    private func answer(_ response: NSApplication.ModalResponse) {
        if let parent = sheetParent {
            parent.endSheet(self, returnCode: response)
        } else {
            orderOut(nil)
        }
    }
}
