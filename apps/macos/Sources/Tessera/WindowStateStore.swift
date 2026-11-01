import AppKit
import os

/// The windows to bring back at the next launch: each window's frame and
/// its number of tabs, in the order the windows opened (ADR 0031, D4c).
/// Restored tabs start fresh shells in the default directory; their working
/// directories are #61.
struct WindowState: Codable, Equatable {
    struct Window: Codable, Equatable {
        /// The window's frame in screen coordinates.
        var frame: CGRect
        /// How many tabs it has, at least 1.
        var tabCount: Int
    }

    var windows: [Window]

    /// At most this many sessions are restored, however many were saved: a
    /// damaged or hand-edited value must not start thousands of shells.
    static let maxRestoredSessions = 64
    /// A restored window is at least this big, whatever was saved.
    static let minimumWindowSize = CGSize(width: 160, height: 80)

    /// The windows to open: windows without a usable frame (not finite,
    /// empty) are dropped, tab counts are at least 1, the total is capped at
    /// ``maxRestoredSessions``, and each frame is moved onto `screens` (see
    /// ``clamp(_:to:)``).
    func restorePlan(screens: [CGRect]) -> [Window] {
        var plan: [Window] = []
        var sessions = 0
        for window in windows {
            let frame = window.frame.standardized
            guard sessions < Self.maxRestoredSessions,
                [frame.minX, frame.minY, frame.width, frame.height].allSatisfy(\.isFinite),
                frame.width > 0, frame.height > 0
            else { continue }
            let tabs = min(max(window.tabCount, 1), Self.maxRestoredSessions - sessions)
            sessions += tabs
            plan.append(Window(frame: Self.clamp(frame, to: screens), tabCount: tabs))
        }
        return plan
    }

    /// `frame` moved and shrunk to lie wholly inside the screen it overlaps
    /// most, or inside the first screen (the main one) if it overlaps none,
    /// as when the display it was on is gone. At least
    /// ``minimumWindowSize``, unless the screen is smaller. Unchanged when
    /// there is no screen.
    static func clamp(_ frame: CGRect, to screens: [CGRect]) -> CGRect {
        guard let first = screens.first else { return frame }
        // `max(by:)` keeps the first of equals: the main screen when the
        // frame overlaps none.
        let screen = screens.max { overlap(frame, $0) < overlap(frame, $1) } ?? first
        let width = min(max(frame.width, minimumWindowSize.width), screen.width)
        let height = min(max(frame.height, minimumWindowSize.height), screen.height)
        let x = min(max(frame.minX, screen.minX), screen.maxX - width)
        let y = min(max(frame.minY, screen.minY), screen.maxY - height)
        return CGRect(x: x, y: y, width: width, height: height)
    }

    private static func overlap(_ a: CGRect, _ b: CGRect) -> CGFloat {
        let common = a.intersection(b)
        return common.isNull ? 0 : common.width * common.height
    }
}

/// Saves and loads ``WindowState`` as JSON in user defaults, under
/// ``key``.
@MainActor
final class WindowStateStore {
    /// The defaults key. A later format gets a new key, so an older
    /// Tessera never misreads it.
    nonisolated static let key = "TesseraWindowState.v1"

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window-state")

    let defaults: UserDefaults

    init(defaults: UserDefaults) {
        self.defaults = defaults
    }

    func save(_ state: WindowState) {
        do {
            defaults.set(try JSONEncoder().encode(state), forKey: Self.key)
        } catch {
            Self.log.error("could not save the window state: \(String(describing: error), privacy: .public)")
        }
    }

    /// The saved state, or `nil` if there is none or it cannot be read.
    func load() -> WindowState? {
        guard let data = defaults.data(forKey: Self.key) else { return nil }
        do {
            return try JSONDecoder().decode(WindowState.self, from: data)
        } catch {
            Self.log.error("ignoring an unreadable window state: \(String(describing: error), privacy: .public)")
            return nil
        }
    }

    /// Whether windows come back after a quit: false when macOS's "Close
    /// windows when quitting an application" is on, as it is by default
    /// (`NSQuitAlwaysKeepsWindows` false or unset), as AppKit's own
    /// restoration does. Set per application with
    /// `defaults write com.aanyberg.tessera NSQuitAlwaysKeepsWindows -bool true`.
    static func quitKeepsWindows(_ defaults: UserDefaults) -> Bool {
        defaults.bool(forKey: "NSQuitAlwaysKeepsWindows")
    }
}
