import CoreGraphics

/// Which way a split divides a pane (D1, iTerm2's commands).
enum PaneSplitDirection: Equatable, Sendable {
    /// ⌘D: the new pane goes to the right, behind a vertical divider.
    case right
    /// ⌘⇧D: the new pane goes below, behind a horizontal divider.
    case below
}

/// Which neighbour ⌘⌥ and an arrow key move the focus to.
enum PaneDirection: Equatable, Sendable, CaseIterable {
    case left
    case right
    case up
    case down
}

/// A tab's panes: a binary tree whose leaves are the panes and whose inner
/// nodes are splits, each with a direction and the share of its space its
/// first child takes (ADR 0035).
///
/// A value type over any leaf, so the tree's rules (splitting, closing,
/// sizes, spatial neighbours) are tested without AppKit; the shell's leaves
/// are ``Pane``s. Every leaf is distinct (`==`). Geometry is in a top-left
/// coordinate space: y grows downwards.
struct PaneTree<Leaf: Equatable> {
    /// A pane, or a split of two subtrees.
    indirect enum Node {
        case leaf(Leaf)
        case split(Split)
    }

    /// Two subtrees side by side (``PaneSplitDirection/right``: `first` on
    /// the left) or stacked (``PaneSplitDirection/below``: `first` on top).
    struct Split {
        var direction: PaneSplitDirection
        /// The share of the space, dividers excluded, that `first` takes:
        /// between 0 and 1.
        var ratio: CGFloat
        var first: Node
        var second: Node
    }

    /// The most panes a tab holds (D1).
    static var maxLeaves: Int { 16 }

    private(set) var root: Node

    init(_ leaf: Leaf) {
        root = .leaf(leaf)
    }

    init(root: Node) {
        self.root = root
    }

    /// The panes in reading order: left to right, then top to bottom within
    /// each split.
    var leaves: [Leaf] {
        root.leaves
    }

    func contains(_ leaf: Leaf) -> Bool {
        leaves.contains(leaf)
    }

    /// Splits `leaf` in two halves, `leaf` first and `new` second, and
    /// returns whether it did: not when `leaf` is not in the tree, `new`
    /// already is, or the tree is full.
    @discardableResult
    mutating func split(_ leaf: Leaf, _ direction: PaneSplitDirection, adding new: Leaf) -> Bool {
        let leaves = leaves
        guard leaves.contains(leaf), !leaves.contains(new), leaves.count < Self.maxLeaves else { return false }
        root = root.replacing(leaf) {
            .split(Split(direction: direction, ratio: 0.5, first: .leaf(leaf), second: .leaf(new)))
        }
        return true
    }

    /// Takes `leaf` out of the tree: its sibling takes the place of their
    /// split. Returns whether it did; the last leaf is never removed.
    @discardableResult
    mutating func remove(_ leaf: Leaf) -> Bool {
        guard case .split = root, contains(leaf), let removed = root.removing(leaf) else { return false }
        root = removed
        return true
    }

    /// Replaces the splits' ratios, in the order ``Node/splits`` lists them
    /// (the shares measured from the views); each is clamped to 0...1, and
    /// splits past the end of `ratios` keep theirs.
    mutating func setRatios(_ ratios: [CGFloat]) {
        var next = ratios[...]
        root = root.withRatios(&next)
    }

    // MARK: Geometry

    /// Each leaf's frame when the tree fills `bounds`, the splits keeping
    /// their ratios and every divider `divider` thick. A split's first child
    /// gets its share rounded to the nearest whole point.
    func frames(in bounds: CGRect, divider: CGFloat) -> [(leaf: Leaf, frame: CGRect)] {
        var frames: [(leaf: Leaf, frame: CGRect)] = []
        root.collectFrames(in: bounds, divider: divider, into: &frames)
        return frames
    }

    /// The smallest size the tree fits in, with each leaf at least `minimum`
    /// of it and every divider `divider` thick.
    func minimumSize(divider: CGFloat, _ minimum: (Leaf) -> CGSize) -> CGSize {
        root.minimumSize(divider: divider, minimum)
    }

    /// The leaf to move the focus to from the one at `focused`, toward
    /// `direction`, among `frames` (top-left coordinates): of the leaves
    /// wholly on that side whose span across the direction overlaps
    /// `focused`'s, the nearest; among equally near ones the one that
    /// overlaps it most, then the one whose centre is nearest its centre,
    /// then the first in `frames`. `nil` at the edge.
    static func neighbour(
        of focused: CGRect, toward direction: PaneDirection, among frames: [(leaf: Leaf, frame: CGRect)]
    ) -> Leaf? {
        // Frames come from views laid out in points; a divider's rounding
        // must not hide a neighbour.
        let slack: CGFloat = 0.5
        // The best so far: its index, and its rank (smaller is better).
        var best: (index: Int, rank: (CGFloat, CGFloat, CGFloat))?
        for (index, entry) in frames.enumerated() {
            let frame = entry.frame
            let gap: CGFloat
            let overlap: CGFloat
            let offset: CGFloat
            switch direction {
            case .left:
                gap = focused.minX - frame.maxX
                overlap = min(focused.maxY, frame.maxY) - max(focused.minY, frame.minY)
                offset = abs(frame.midY - focused.midY)
            case .right:
                gap = frame.minX - focused.maxX
                overlap = min(focused.maxY, frame.maxY) - max(focused.minY, frame.minY)
                offset = abs(frame.midY - focused.midY)
            case .up:
                gap = focused.minY - frame.maxY
                overlap = min(focused.maxX, frame.maxX) - max(focused.minX, frame.minX)
                offset = abs(frame.midX - focused.midX)
            case .down:
                gap = frame.minY - focused.maxY
                overlap = min(focused.maxX, frame.maxX) - max(focused.minX, frame.minX)
                offset = abs(frame.midX - focused.midX)
            }
            guard gap >= -slack, overlap > slack else { continue }
            let rank = (gap, -overlap, offset)
            if best.map({ rank < $0.rank }) ?? true {
                best = (index, rank)
            }
        }
        return best.map { frames[$0.index].leaf }
    }
}

extension PaneTree.Node {
    var leaves: [Leaf] {
        switch self {
        case .leaf(let leaf): [leaf]
        case .split(let split): split.first.leaves + split.second.leaves
        }
    }

    /// The splits in pre-order: a split, then those of its first child, then
    /// those of its second.
    var splits: [PaneTree.Split] {
        switch self {
        case .leaf: []
        case .split(let split): [split] + split.first.splits + split.second.splits
        }
    }

    fileprivate func replacing(_ leaf: Leaf, with node: () -> Self) -> Self {
        switch self {
        case .leaf(let other):
            return other == leaf ? node() : self
        case .split(var split):
            split.first = split.first.replacing(leaf, with: node)
            split.second = split.second.replacing(leaf, with: node)
            return .split(split)
        }
    }

    /// This subtree without `leaf`, or `nil` when it is the leaf itself.
    fileprivate func removing(_ leaf: Leaf) -> Self? {
        switch self {
        case .leaf(let other):
            return other == leaf ? nil : self
        case .split(var split):
            guard let first = split.first.removing(leaf) else { return split.second }
            guard let second = split.second.removing(leaf) else { return split.first }
            split.first = first
            split.second = second
            return .split(split)
        }
    }

    fileprivate func withRatios(_ ratios: inout ArraySlice<CGFloat>) -> Self {
        switch self {
        case .leaf:
            return self
        case .split(var split):
            if let ratio = ratios.popFirst() {
                split.ratio = min(max(ratio, 0), 1)
            }
            split.first = split.first.withRatios(&ratios)
            split.second = split.second.withRatios(&ratios)
            return .split(split)
        }
    }

    fileprivate func collectFrames(
        in bounds: CGRect, divider: CGFloat, into frames: inout [(leaf: Leaf, frame: CGRect)]
    ) {
        switch self {
        case .leaf(let leaf):
            frames.append((leaf, bounds))
        case .split(let split):
            let (first, second) = PaneTree.divide(bounds, split.direction, ratio: split.ratio, divider: divider)
            split.first.collectFrames(in: first, divider: divider, into: &frames)
            split.second.collectFrames(in: second, divider: divider, into: &frames)
        }
    }

    fileprivate func minimumSize(divider: CGFloat, _ minimum: (Leaf) -> CGSize) -> CGSize {
        switch self {
        case .leaf(let leaf):
            return minimum(leaf)
        case .split(let split):
            let first = split.first.minimumSize(divider: divider, minimum)
            let second = split.second.minimumSize(divider: divider, minimum)
            return PaneTree.combine(first, second, split.direction, divider: divider)
        }
    }
}

extension PaneTree {
    /// `bounds` divided for a split: the first part takes `ratio` of the
    /// space the divider leaves, rounded to the nearest whole point.
    static func divide(
        _ bounds: CGRect, _ direction: PaneSplitDirection, ratio: CGFloat, divider: CGFloat
    ) -> (first: CGRect, second: CGRect) {
        switch direction {
        case .right:
            let space = max(0, bounds.width - divider)
            let width = (space * ratio).rounded()
            return (
                CGRect(x: bounds.minX, y: bounds.minY, width: width, height: bounds.height),
                CGRect(x: bounds.minX + width + divider, y: bounds.minY, width: space - width, height: bounds.height)
            )
        case .below:
            let space = max(0, bounds.height - divider)
            let height = (space * ratio).rounded()
            return (
                CGRect(x: bounds.minX, y: bounds.minY, width: bounds.width, height: height),
                CGRect(x: bounds.minX, y: bounds.minY + height + divider, width: bounds.width, height: space - height)
            )
        }
    }

    /// The smallest size of a split whose children need `first` and
    /// `second`.
    static func combine(_ first: CGSize, _ second: CGSize, _ direction: PaneSplitDirection, divider: CGFloat) -> CGSize {
        switch direction {
        case .right:
            CGSize(width: first.width + divider + second.width, height: max(first.height, second.height))
        case .below:
            CGSize(width: max(first.width, second.width), height: first.height + divider + second.height)
        }
    }
}
