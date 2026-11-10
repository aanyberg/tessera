import AppKit
import os

/// The windows to bring back at the next launch: each window's frame, its
/// number of tabs and the layout of each tab's panes, in the order the
/// windows opened (ADR 0031, D4c; ADR 0035). Restored panes start fresh
/// shells in the default directory; their working directories are #61,
/// their profiles #39. Which pane had the focus, and a maximized pane, are
/// not kept: a restored tab has every pane showing and its first one
/// focused.
struct WindowState: Codable, Equatable {
    struct Window: Codable, Equatable {
        /// The window's frame in screen coordinates.
        var frame: CGRect
        /// How many tabs it has, at least 1, at most ``maxTabs``. Kept
        /// beside ``tabs``, which an older Tessera does not read, so it
        /// still restores the tabs.
        var tabCount: Int
        /// Each tab's panes, in tab order; `nil` (as in the first format,
        /// and as saved while every tab has a single pane) or a missing
        /// entry is one pane. Saved only when some tab is split; at most
        /// ``maxTabs``.
        var tabs: [Tab]?

        /// The tabs past the first ``maxTabs`` are left out: a window
        /// with more could not be read back.
        init(frame: CGRect, tabCount: Int, tabs: [Tab]? = nil) {
            self.frame = frame
            self.tabCount = min(tabCount, WindowState.maxTabs)
            let kept = tabs.map { Array($0.prefix(WindowState.maxTabs)) }
            // One pane everywhere is the first format's window.
            self.tabs = kept?.contains { $0.tree != nil } == true ? kept : nil
        }

        /// As the first format: a `tabs` that cannot be read (not a list,
        /// a tree too deep to decode) leaves every tab one pane rather
        /// than losing the window. A window of more than ``maxTabs`` tabs,
        /// by its count or its list, which is counted before a tab is
        /// decoded, makes the whole state unreadable (S5).
        init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            let frame = try container.decode(CGRect.self, forKey: .frame)
            let tabCount = try container.decode(Int.self, forKey: .tabCount)
            let listed = (try? container.nestedUnkeyedContainer(forKey: .tabs))?.count ?? 0
            guard tabCount <= WindowState.maxTabs, listed <= WindowState.maxTabs else {
                throw DecodingError.dataCorruptedError(
                    forKey: .tabCount, in: container,
                    debugDescription: "a window of more than \(WindowState.maxTabs) tabs")
            }
            var tabs: [Tab]?
            do {
                tabs = try container.decodeIfPresent([Tab].self, forKey: .tabs)
            } catch {
                WindowState.log.error(
                    "a window's pane layouts are unreadable; one pane per tab: \(String(describing: error), privacy: .public)")
            }
            self.init(frame: frame, tabCount: tabCount, tabs: tabs)
        }

        /// The layout tab `index` restores: its tree, or `nil` for one pane.
        func layout(ofTab index: Int) -> PaneLayout? {
            guard let tabs, tabs.indices.contains(index) else { return nil }
            return tabs[index].tree
        }
    }

    /// One tab of a window.
    struct Tab: Codable, Equatable {
        /// The tab's panes, or `nil` for one pane.
        var tree: PaneLayout?

        init(tree: PaneLayout?) {
            // A lone pane is no layout.
            self.tree = tree == .leaf ? nil : tree
        }

        /// A tree that cannot be read is one pane; the window and its other
        /// tabs are kept.
        init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            do {
                self.init(tree: try container.decodeIfPresent(PaneLayout.self, forKey: .tree))
            } catch {
                WindowState.log.error(
                    "a tab's pane layout is unreadable; one pane: \(String(describing: error), privacy: .public)")
                self.init(tree: nil)
            }
        }
    }

    var windows: [Window]

    fileprivate static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window-state")

    /// At most this many sessions (panes) are restored, however many were
    /// saved: a damaged or hand-edited value must not start thousands of
    /// shells.
    static let maxRestoredSessions = 64
    /// A window has at most this many tabs in the state (S5): more than a
    /// restore could start (``maxRestoredSessions``).
    static let maxTabs = 64
    /// A restored window is at least this big, whatever was saved.
    static let minimumWindowSize = CGSize(width: 160, height: 80)

    /// The windows to open: windows without a usable frame (not finite,
    /// empty) are dropped, tab counts are at least 1, and each frame is
    /// moved onto `screens` (see ``clamp(_:to:)``). Every pane counts
    /// against ``maxRestoredSessions``: a tab whose tree is not
    /// ``PaneLayout/isRestorable`` or does not fit what is left restores as
    /// one pane (logged), and tabs past the cap are dropped.
    func restorePlan(screens: [CGRect]) -> [Window] {
        var plan: [Window] = []
        var sessions = 0
        for window in windows {
            let frame = window.frame.standardized
            guard sessions < Self.maxRestoredSessions,
                [frame.minX, frame.minY, frame.width, frame.height].allSatisfy(\.isFinite),
                frame.width > 0, frame.height > 0
            else { continue }
            let tabCount = min(max(window.tabCount, 1), Self.maxRestoredSessions - sessions)
            var tabs: [Tab] = []
            for index in 0..<tabCount {
                guard sessions < Self.maxRestoredSessions else { break }
                var tree = window.layout(ofTab: index)
                if let layout = tree, !layout.isRestorable {
                    Self.log.error("a saved pane layout is out of bounds; restoring one pane")
                    tree = nil
                }
                if let layout = tree, layout.leafCount > Self.maxRestoredSessions - sessions {
                    Self.log.error("the saved panes exceed \(Self.maxRestoredSessions) sessions; restoring one pane")
                    tree = nil
                }
                sessions += tree?.leafCount ?? 1
                tabs.append(Tab(tree: tree))
            }
            plan.append(Window(frame: Self.clamp(frame, to: screens), tabCount: tabs.count, tabs: tabs))
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

/// A tab's panes as the state file keeps them: the shape of its
/// ``PaneTree``, each split with its direction and ratio. In JSON a pane is
/// `{"leaf":{}}` (room for what a pane may keep later, #61) and a split
/// `{"split":{"direction":"right","ratio":0.5,"first":…,"second":…}}`.
indirect enum PaneLayout: Equatable, Sendable {
    case leaf
    case split(PaneSplitDirection, ratio: Double, first: PaneLayout, second: PaneLayout)

    /// A restored tree has at most this many splits from its root to a pane:
    /// as many as a tree of `PaneTree.maxLeaves` panes can have (one pane
    /// split, then the new pane, and so on, as repeated ⌘D does), so every
    /// layout a tab can hold is restored.
    static let maxDepth = PaneTree<Int>.maxLeaves - 1
    /// Nested deeper than this many coding keys, a layout is not decoded at
    /// all (it fails, and its tab is one pane), so a hostile file cannot
    /// make decoding recurse further. A window's tab's tree starts five
    /// keys deep and each split adds two.
    static let maxCodingPathLength = 5 + 2 * (maxDepth + 1)

    /// Whether the tree may be restored: at most `PaneTree.maxLeaves`
    /// panes and ``maxDepth`` splits deep, every ratio a number strictly
    /// between 0 and 1.
    var isRestorable: Bool {
        depth <= Self.maxDepth && leafCount <= PaneTree<Int>.maxLeaves && ratiosAreValid
    }

    var leafCount: Int {
        switch self {
        case .leaf: 1
        case .split(_, _, let first, let second): first.leafCount + second.leafCount
        }
    }

    var depth: Int {
        switch self {
        case .leaf: 0
        case .split(_, _, let first, let second): 1 + max(first.depth, second.depth)
        }
    }

    private var ratiosAreValid: Bool {
        switch self {
        case .leaf: true
        case .split(_, let ratio, let first, let second):
            ratio.isFinite && ratio > 0 && ratio < 1 && first.ratiosAreValid && second.ratiosAreValid
        }
    }

    /// The layout of `tree`.
    init<Leaf>(_ tree: PaneTree<Leaf>) {
        self.init(tree.root)
    }

    private init<Leaf>(_ node: PaneTree<Leaf>.Node) {
        switch node {
        case .leaf:
            self = .leaf
        case .split(let split):
            // JSON holds no NaN: an encoder would refuse the whole state.
            let ratio = split.ratio.isFinite ? Double(split.ratio) : 0.5
            self = .split(
                split.direction, ratio: ratio, first: PaneLayout(split.first), second: PaneLayout(split.second))
        }
    }

    /// A tree of this shape whose panes are `leaves` in reading order, or
    /// `nil` when their number is not ``leafCount``.
    func tree<Leaf>(leaves: [Leaf]) -> PaneTree<Leaf>? {
        guard leaves.count == leafCount else { return nil }
        var next = leaves[...]
        return PaneTree(root: node(taking: &next))
    }

    private func node<Leaf>(taking leaves: inout ArraySlice<Leaf>) -> PaneTree<Leaf>.Node {
        switch self {
        case .leaf:
            return .leaf(leaves.removeFirst())
        case .split(let direction, let ratio, let first, let second):
            let firstNode = first.node(taking: &leaves)
            let secondNode = second.node(taking: &leaves)
            return .split(
                PaneTree<Leaf>.Split(direction: direction, ratio: CGFloat(ratio), first: firstNode, second: secondNode))
        }
    }
}

extension PaneLayout: Codable {
    private enum CodingKeys: String, CodingKey {
        case leaf
        case split
    }

    private enum SplitKeys: String, CodingKey {
        case direction
        case ratio
        case first
        case second
    }

    /// A struct holding nothing, for a pane's `{}`.
    private struct Empty: Codable {}

    init(from decoder: any Decoder) throws {
        guard decoder.codingPath.count <= Self.maxCodingPathLength else {
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "pane layout nested too deeply"))
        }
        let container = try decoder.container(keyedBy: CodingKeys.self)
        if container.contains(.leaf) {
            _ = try container.decode(Empty.self, forKey: .leaf)
            self = .leaf
            return
        }
        let split = try container.nestedContainer(keyedBy: SplitKeys.self, forKey: .split)
        self = .split(
            try split.decode(PaneSplitDirection.self, forKey: .direction),
            ratio: try split.decode(Double.self, forKey: .ratio),
            first: try split.decode(PaneLayout.self, forKey: .first),
            second: try split.decode(PaneLayout.self, forKey: .second))
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .leaf:
            try container.encode(Empty(), forKey: .leaf)
        case .split(let direction, let ratio, let first, let second):
            var split = container.nestedContainer(keyedBy: SplitKeys.self, forKey: .split)
            try split.encode(direction, forKey: .direction)
            try split.encode(ratio, forKey: .ratio)
            try split.encode(first, forKey: .first)
            try split.encode(second, forKey: .second)
        }
    }
}

/// Saves and loads ``WindowState`` as JSON in user defaults, under
/// ``key``.
@MainActor
final class WindowStateStore {
    /// The defaults key. The pane layouts (ADR 0035) were added to this
    /// format as an optional field an older Tessera skips, so the key
    /// stayed; a format an older Tessera would misread gets a new key.
    nonisolated static let key = "TesseraWindowState.v1"

    /// A saved value larger than this is not decoded at all (S5): decoding
    /// is linear in the size (about 10 MB/s). A window's entry is a few
    /// kilobytes at most (64 tabs, 16 panes each), so only hundreds of
    /// windows reach it, and a restore starts 64 sessions in any case.
    nonisolated static let maxBytes = 1 << 20

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window-state")

    let defaults: UserDefaults

    init(defaults: UserDefaults) {
        self.defaults = defaults
    }

    /// Saves `state`; one larger than ``maxBytes``, which ``load()``
    /// would refuse, removes the saved state instead.
    func save(_ state: WindowState) {
        do {
            let data = try JSONEncoder().encode(state)
            guard data.count <= Self.maxBytes else {
                Self.log.error("the window state is \(data.count) bytes, over \(Self.maxBytes); not saved")
                defaults.removeObject(forKey: Self.key)
                return
            }
            defaults.set(data, forKey: Self.key)
        } catch {
            Self.log.error("could not save the window state: \(String(describing: error), privacy: .public)")
        }
    }

    /// The saved state, or `nil` if there is none or it cannot be read,
    /// is larger than ``maxBytes`` (not decoded) or has a window of more
    /// than ``WindowState/maxTabs`` tabs.
    func load() -> WindowState? {
        guard let data = defaults.data(forKey: Self.key) else { return nil }
        guard data.count <= Self.maxBytes else {
            Self.log.error("ignoring a window state of \(data.count) bytes, over \(Self.maxBytes)")
            return nil
        }
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
