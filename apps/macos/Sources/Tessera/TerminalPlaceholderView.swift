import AppKit
import TesseraKit

/// A tab's content until the terminal surface lands (#27): the core version
/// and the session's state, centred in the window.
///
/// #27's `TerminalSurfaceView` replaces it as another ``TerminalContentView``.
@MainActor
final class TerminalPlaceholderView: NSView, TerminalContentView {
    private let label = NSTextField(wrappingLabelWithString: "")
    private var outputEvents = 0

    /// The status line under the core version, as shown.
    private(set) var status = "login shell running" {
        didSet { label.stringValue = "Tessera core v\(TesseraApp.coreVersion)\n\(status)" }
    }

    /// The label's full text: core version and status.
    var text: String { label.stringValue }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        label.font = .monospacedSystemFont(ofSize: 15, weight: .medium)
        label.alignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: centerXAnchor),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
            label.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 20),
            label.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -20),
        ])
        label.stringValue = "Tessera core v\(TesseraApp.coreVersion)\n\(status)"
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// The session wrote output.
    func sessionDidOutput() {
        outputEvents += 1
        status = "login shell running (\(outputEvents) output events)"
    }

    /// The session's child exited with `code`, or was killed by `signal`
    /// (non-zero).
    func sessionDidExit(code: Int32, signal: Int32) {
        status = signal == 0 ? "shell exited with code \(code)" : "shell killed by signal \(signal)"
    }
}
