internal import TesseraCore

// Swift mirrors of the input structs in TesseraCore.h (ADR 0011) and the
// `tessera_session_send_*`, `scroll_to`, `scroll_position`, `paste` and
// `set_focus` calls. The header is the
// contract: every type here converts to its C struct field for field, and the
// core validates, routes and encodes the events.

extension TesseraSession {
    /// A key, independent of AppKit's key codes: `TesseraKey`. Raw values
    /// are the C values, which are stable (new keys are only appended).
    public enum Key: UInt32, Sendable, CaseIterable {
        /// A text-only event with no identifiable key, such as an IME commit: only the text is sent.
        case unidentified = 0
        /// A key that produces a character: the event's `codepoint` holds its unshifted character in the active layout (`a`, not `A`; `4`, not `$`).
        case char = 1
        /// Escape.
        case escape = 2
        /// Enter / Return.
        case enter = 3
        /// Tab.
        case tab = 4
        /// Backspace.
        case backspace = 5
        /// Insert.
        case insert = 6
        /// Delete (forward delete).
        case delete = 7
        /// Left arrow.
        case left = 8
        /// Right arrow.
        case right = 9
        /// Up arrow.
        case up = 10
        /// Down arrow.
        case down = 11
        /// Page Up.
        case pageUp = 12
        /// Page Down.
        case pageDown = 13
        /// Home.
        case home = 14
        /// End.
        case end = 15
        /// Caps Lock.
        case capsLock = 16
        /// Scroll Lock.
        case scrollLock = 17
        /// Num Lock.
        case numLock = 18
        /// Print Screen.
        case printScreen = 19
        /// Pause.
        case pause = 20
        /// Menu.
        case menu = 21
        /// Function key F1.
        case f1 = 22
        /// Function key F2.
        case f2 = 23
        /// Function key F3.
        case f3 = 24
        /// Function key F4.
        case f4 = 25
        /// Function key F5.
        case f5 = 26
        /// Function key F6.
        case f6 = 27
        /// Function key F7.
        case f7 = 28
        /// Function key F8.
        case f8 = 29
        /// Function key F9.
        case f9 = 30
        /// Function key F10.
        case f10 = 31
        /// Function key F11.
        case f11 = 32
        /// Function key F12.
        case f12 = 33
        /// Function key F13.
        case f13 = 34
        /// Function key F14.
        case f14 = 35
        /// Function key F15.
        case f15 = 36
        /// Function key F16.
        case f16 = 37
        /// Function key F17.
        case f17 = 38
        /// Function key F18.
        case f18 = 39
        /// Function key F19.
        case f19 = 40
        /// Function key F20.
        case f20 = 41
        /// Function key F21.
        case f21 = 42
        /// Function key F22.
        case f22 = 43
        /// Function key F23.
        case f23 = 44
        /// Function key F24.
        case f24 = 45
        /// Function key F25.
        case f25 = 46
        /// Function key F26.
        case f26 = 47
        /// Function key F27.
        case f27 = 48
        /// Function key F28.
        case f28 = 49
        /// Function key F29.
        case f29 = 50
        /// Function key F30.
        case f30 = 51
        /// Function key F31.
        case f31 = 52
        /// Function key F32.
        case f32 = 53
        /// Function key F33.
        case f33 = 54
        /// Function key F34.
        case f34 = 55
        /// Function key F35.
        case f35 = 56
        /// Keypad 0.
        case kp0 = 57
        /// Keypad 1.
        case kp1 = 58
        /// Keypad 2.
        case kp2 = 59
        /// Keypad 3.
        case kp3 = 60
        /// Keypad 4.
        case kp4 = 61
        /// Keypad 5.
        case kp5 = 62
        /// Keypad 6.
        case kp6 = 63
        /// Keypad 7.
        case kp7 = 64
        /// Keypad 8.
        case kp8 = 65
        /// Keypad 9.
        case kp9 = 66
        /// Keypad decimal point.
        case kpDecimal = 67
        /// Keypad `/`.
        case kpDivide = 68
        /// Keypad `*`.
        case kpMultiply = 69
        /// Keypad `-`.
        case kpSubtract = 70
        /// Keypad `+`.
        case kpAdd = 71
        /// Keypad Enter.
        case kpEnter = 72
        /// Keypad `=`.
        case kpEqual = 73
        /// Keypad `,`.
        case kpSeparator = 74
        /// Keypad Left (Num Lock off).
        case kpLeft = 75
        /// Keypad Right (Num Lock off).
        case kpRight = 76
        /// Keypad Up (Num Lock off).
        case kpUp = 77
        /// Keypad Down (Num Lock off).
        case kpDown = 78
        /// Keypad Page Up (Num Lock off).
        case kpPageUp = 79
        /// Keypad Page Down (Num Lock off).
        case kpPageDown = 80
        /// Keypad Home (Num Lock off).
        case kpHome = 81
        /// Keypad End (Num Lock off).
        case kpEnd = 82
        /// Keypad Insert (Num Lock off).
        case kpInsert = 83
        /// Keypad Delete (Num Lock off).
        case kpDelete = 84
        /// Keypad Begin (the `5` key with Num Lock off).
        case kpBegin = 85
        /// Media: play.
        case mediaPlay = 86
        /// Media: pause.
        case mediaPause = 87
        /// Media: play/pause toggle.
        case mediaPlayPause = 88
        /// Media: reverse.
        case mediaReverse = 89
        /// Media: stop.
        case mediaStop = 90
        /// Media: fast forward.
        case mediaFastForward = 91
        /// Media: rewind.
        case mediaRewind = 92
        /// Media: next track.
        case mediaTrackNext = 93
        /// Media: previous track.
        case mediaTrackPrevious = 94
        /// Media: record.
        case mediaRecord = 95
        /// Volume down.
        case lowerVolume = 96
        /// Volume up.
        case raiseVolume = 97
        /// Mute.
        case muteVolume = 98
        /// Left Shift.
        case leftShift = 99
        /// Left Control.
        case leftControl = 100
        /// Left Alt.
        case leftAlt = 101
        /// Left Super.
        case leftSuper = 102
        /// Left Hyper.
        case leftHyper = 103
        /// Left Meta.
        case leftMeta = 104
        /// Right Shift.
        case rightShift = 105
        /// Right Control.
        case rightControl = 106
        /// Right Alt.
        case rightAlt = 107
        /// Right Super.
        case rightSuper = 108
        /// Right Hyper.
        case rightHyper = 109
        /// Right Meta.
        case rightMeta = 110
        /// ISO Level 3 Shift (`AltGr`).
        case isoLevel3Shift = 111
        /// ISO Level 5 Shift.
        case isoLevel5Shift = 112
    }

    /// What a key did: `TesseraKeyEventKind`.
    public enum KeyEventKind: UInt32, Sendable {
        /// The key went down.
        case press = 0
        /// The key is held and auto-repeating.
        case `repeat` = 1
        /// The key went up. Sends nothing unless the child asked for key
        /// release events.
        case release = 2
    }

    /// Modifier keys and lock states: the `TesseraModifiers` bits (the kitty
    /// keyboard protocol's values). Every bit is valid.
    public struct Modifiers: OptionSet, Sendable, Hashable {
        public let rawValue: UInt8
        public init(rawValue: UInt8) { self.rawValue = rawValue }

        public static let shift = Modifiers(rawValue: 1)
        /// Option. Set it to have the key sent with an `ESC` prefix; leave it
        /// out and pass the composed text to send what Option produced.
        public static let alt = Modifiers(rawValue: 2)
        public static let control = Modifiers(rawValue: 4)
        /// Command.
        public static let `super` = Modifiers(rawValue: 8)
        public static let hyper = Modifiers(rawValue: 16)
        public static let meta = Modifiers(rawValue: 32)
        public static let capsLock = Modifiers(rawValue: 64)
        public static let numLock = Modifiers(rawValue: 128)
    }

    /// A keyboard event: `TesseraKeyEvent`.
    public struct KeyEvent: Sendable, Equatable {
        public var key: Key
        /// For ``Key/char``: the key's unshifted character in the active
        /// layout. Ignored for every other key.
        public var codepoint: Unicode.Scalar?
        /// The key's shifted character in the active layout, or `nil` when
        /// unknown (a US layout is then assumed).
        public var shifted: Unicode.Scalar?
        /// The key at the same position in the US layout, or `nil` when
        /// unknown or the same as the key; lets Ctrl and Alt combinations
        /// work on non-Latin layouts.
        public var baseLayout: Unicode.Scalar?
        public var modifiers: Modifiers
        public var kind: KeyEventKind
        /// Text the OS composed for the event (layout, dead key, IME commit),
        /// or `nil`. It takes precedence over the key's character; text with
        /// a control character is ignored by the core.
        public var text: String?

        public init(
            key: Key,
            codepoint: Unicode.Scalar? = nil,
            shifted: Unicode.Scalar? = nil,
            baseLayout: Unicode.Scalar? = nil,
            modifiers: Modifiers = [],
            kind: KeyEventKind = .press,
            text: String? = nil
        ) {
            self.key = key
            self.codepoint = codepoint
            self.shifted = shifted
            self.baseLayout = baseLayout
            self.modifiers = modifiers
            self.kind = kind
            self.text = text
        }

        /// Text-only input (an IME commit, a dead-key result): the core sends
        /// `text` as typed, never as a paste.
        public static func text(_ text: String, modifiers: Modifiers = []) -> KeyEvent {
            KeyEvent(key: .unidentified, modifiers: modifiers, text: text)
        }
    }

    /// A mouse button: `TesseraMouseButton`.
    public enum MouseButton: UInt32, Sendable {
        /// No button: plain motion with every button up.
        case none = 0
        case left = 1
        case middle = 2
        case right = 3
        case back = 4
        case forward = 5
    }

    /// What happened to the mouse: `TesseraMouseEventKind`.
    public enum MouseEventKind: UInt32, Sendable {
        case press = 0
        case release = 1
        /// Moved, with `button` held (a drag) or ``MouseButton/none``.
        case motion = 2
    }

    /// A mouse event: `TesseraMouseEvent`.
    public struct MouseEvent: Sendable, Equatable {
        public var kind: MouseEventKind
        public var button: MouseButton
        /// Zero-based cell under the pointer, from the top left of the
        /// visible screen; values beyond the grid are clamped by the core.
        public var col: UInt16
        public var row: UInt16
        /// Pointer position in pixels from the view's top-left corner; may
        /// be negative or beyond the view during a drag. Must be finite.
        public var xPx: Float
        public var yPx: Float
        public var modifiers: Modifiers

        public init(
            kind: MouseEventKind, button: MouseButton, col: UInt16, row: UInt16,
            xPx: Float, yPx: Float, modifiers: Modifiers = []
        ) {
            self.kind = kind
            self.button = button
            self.col = col
            self.row = row
            self.xPx = xPx
            self.yPx = yPx
            self.modifiers = modifiers
        }
    }

    /// A scroll gesture: `TesseraScrollEvent`.
    public struct ScrollEvent: Sendable, Equatable {
        /// Lines; positive moves the view into the scrollback, negative
        /// towards the live screen. Fractions accumulate in the core.
        public var deltaLines: Float
        /// Distance in pixels as measured by the frontend, 0 when unknown.
        public var deltaXPx: Float
        public var deltaYPx: Float
        /// A trackpad or Magic Mouse rather than a notched wheel.
        public var precise: Bool
        public var col: UInt16
        public var row: UInt16
        public var modifiers: Modifiers

        public init(
            deltaLines: Float, deltaXPx: Float = 0, deltaYPx: Float = 0, precise: Bool = false,
            col: UInt16 = 0, row: UInt16 = 0, modifiers: Modifiers = []
        ) {
            self.deltaLines = deltaLines
            self.deltaXPx = deltaXPx
            self.deltaYPx = deltaYPx
            self.precise = precise
            self.col = col
            self.row = row
            self.modifiers = modifiers
        }
    }

    /// Where ``scroll(to:)`` moves the view: `TesseraScrollTarget`.
    /// Offsets count lines above the live screen.
    public enum ScrollTarget: Sendable, Equatable {
        /// The oldest line in the scrollback.
        case top
        /// The live screen (offset 0), which follows the output.
        case bottom
        /// One page towards older output: the screen height less one line
        /// (at least one line).
        case pageUp
        /// One page towards the live screen.
        case pageDown
        /// This many lines above the live screen (clamped to the history).
        case offset(UInt64)

        var cTarget: TesseraScrollTarget {
            switch self {
            case .top: TESSERA_SCROLL_TARGET_TOP
            case .bottom: TESSERA_SCROLL_TARGET_BOTTOM
            case .pageUp: TESSERA_SCROLL_TARGET_PAGE_UP
            case .pageDown: TESSERA_SCROLL_TARGET_PAGE_DOWN
            case .offset: TESSERA_SCROLL_TARGET_OFFSET
            }
        }

        /// The C call's `offset` argument, used by ``offset(_:)`` only.
        var cOffset: UInt64 {
            if case .offset(let lines) = self { lines } else { 0 }
        }
    }

    /// The view's place in the scrollback: `TesseraScrollPosition`.
    public struct ScrollPosition: Sendable, Equatable {
        /// Lines the view is scrolled back from the live screen: 0 shows the
        /// live screen, ``history`` the oldest line.
        public var offset: UInt64
        /// Lines of scrollback above the live screen; 0 on the alternate
        /// screen, which shows no scrollback.
        public var history: UInt64
        /// The screen's rows: the view shows this many lines.
        public var rows: UInt16

        public init(offset: UInt64, history: UInt64, rows: UInt16) {
            self.offset = offset
            self.history = history
            self.rows = rows
        }
    }

    /// Whether the core used an input event: `TesseraInputOutcome`.
    public enum InputOutcome: Sendable, Equatable {
        /// The core had no use for the event; the frontend may handle it
        /// (for example as a menu key equivalent).
        case ignored
        /// The core acted on it (wrote to the child, ran a binding, moved
        /// the view); the frontend must not handle it again.
        case consumed

        /// Maps the C value. A value this wrapper does not know (a newer
        /// header) reads as ``ignored``, which is also what the core reports
        /// after any failure.
        init(_ outcome: TesseraInputOutcome) {
            switch outcome {
            case TESSERA_INPUT_OUTCOME_CONSUMED: self = .consumed
            default: self = .ignored
            }
        }
    }

    // MARK: Calls

    /// Sends a key event. Never blocks: the bytes join the session's pending
    /// input, which a core thread writes to the child in order.
    ///
    /// - Returns: Whether the core used the key; ``InputOutcome/ignored``
    ///   for a key that encodes to nothing.
    /// - Throws: ``TesseraCoreError/invalidArgument`` for a malformed event,
    ///   ``TesseraCoreError/sessionNotFound`` once the session is closed,
    ///   ``TesseraCoreError/io`` after the child exited or when the pending
    ///   input is full (the key is dropped whole).
    @MainActor
    @discardableResult
    public func send(key event: KeyEvent) throws -> InputOutcome {
        var outcome = TESSERA_INPUT_OUTCOME_IGNORED
        let status: TesseraError
        if var text = event.text, !text.isEmpty {
            status = text.withUTF8 { bytes in
                bytes.withMemoryRebound(to: CChar.self) { chars in
                    var cEvent = event.cEvent(text: chars.baseAddress, length: chars.count)
                    return tessera_session_send_key(core.pointer, id, &cEvent, &outcome)
                }
            }
        } else {
            var cEvent = event.cEvent(text: nil, length: 0)
            status = tessera_session_send_key(core.pointer, id, &cEvent, &outcome)
        }
        try TesseraCoreError.check(status)
        return InputOutcome(outcome)
    }

    /// Sends a mouse event. Today the core only records the position
    /// (mouse reporting and selection come later) and returns
    /// ``InputOutcome/ignored``.
    ///
    /// - Throws: ``TesseraCoreError/invalidArgument`` for a non-finite
    ///   position, ``TesseraCoreError/sessionNotFound`` once closed.
    @MainActor
    @discardableResult
    public func send(mouse event: MouseEvent) throws -> InputOutcome {
        var outcome = TESSERA_INPUT_OUTCOME_IGNORED
        var cEvent = TesseraMouseEvent(
            kind: event.kind.rawValue, button: event.button.rawValue, col: event.col, row: event.row,
            x_px: event.xPx, y_px: event.yPx, modifiers: event.modifiers.rawValue)
        try TesseraCoreError.check(tessera_session_send_mouse(core.pointer, id, &cEvent, &outcome))
        return InputOutcome(outcome)
    }

    /// Sends a scroll gesture, which moves the session's view through the
    /// scrollback.
    ///
    /// - Returns: ``InputOutcome/consumed`` when the view moved by at least
    ///   one line (redraw), ``InputOutcome/ignored`` otherwise.
    /// - Throws: ``TesseraCoreError/invalidArgument`` for a non-finite
    ///   delta, ``TesseraCoreError/sessionNotFound`` once closed.
    @MainActor
    @discardableResult
    public func send(scroll event: ScrollEvent) throws -> InputOutcome {
        var outcome = TESSERA_INPUT_OUTCOME_IGNORED
        var cEvent = TesseraScrollEvent(
            delta_lines: event.deltaLines, delta_x_px: event.deltaXPx, delta_y_px: event.deltaYPx,
            precise: event.precise ? 1 : 0, col: event.col, row: event.row,
            modifiers: event.modifiers.rawValue)
        try TesseraCoreError.check(tessera_session_send_scroll(core.pointer, id, &cEvent, &outcome))
        return InputOutcome(outcome)
    }

    /// Moves the view in the scrollback. Every target is clamped to the
    /// history; on the alternate screen there is no scrollback and the view
    /// stays live. The next frame draws the new position. A view scrolled
    /// back keeps showing the same lines while output continues; typing,
    /// pasting and output that clears the scrollback return it to the live
    /// screen.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once closed.
    @MainActor
    public func scroll(to target: ScrollTarget) throws {
        try TesseraCoreError.check(
            tessera_session_scroll_to(core.pointer, id, target.cTarget.rawValue, target.cOffset))
    }

    /// The view's position in the scrollback, read in one look at the
    /// terminal: enough for a scroll bar. Read it once per frame at most,
    /// not per event. Nothing ties two reads together: output, and the
    /// resize of a configuration edit (made on a core thread), can change
    /// the history between them.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once closed.
    @MainActor
    public var scrollPosition: ScrollPosition {
        get throws {
            var position = TesseraScrollPosition()
            try TesseraCoreError.check(tessera_session_scroll_position(core.pointer, id, &position))
            return ScrollPosition(offset: position.offset, history: position.history, rows: position.rows)
        }
    }

    /// Pastes `text`. The core sanitises it (newlines become CR; ESC, other
    /// C0 and C1 controls and DEL are removed) and never blocks.
    ///
    /// For pasted text only: typed and IME-committed text goes through
    /// ``send(key:)`` with ``KeyEvent/text(_:modifiers:)``, so it is never
    /// bracketed as a paste.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once closed,
    ///   ``TesseraCoreError/io`` after the child exited or when the paste's
    ///   UTF-8 length, before sanitising, does not fit in the free pending
    ///   input (16 MiB at most; nothing is sent or copied).
    @MainActor
    public func paste(_ text: String) throws {
        var text = text
        let status = text.withUTF8 { bytes in
            bytes.withMemoryRebound(to: CChar.self) { chars in
                tessera_session_paste(core.pointer, id, chars.baseAddress, chars.count)
            }
        }
        try TesseraCoreError.check(status)
    }

    /// Tells the session whether its view has keyboard focus (a new session
    /// is unfocused). The renderer draws an unfocused cursor hollow.
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once closed.
    @MainActor
    public func setFocused(_ focused: Bool) throws {
        try TesseraCoreError.check(tessera_session_set_focus(core.pointer, id, focused))
    }
}

extension TesseraSession.KeyEvent {
    /// The C struct, borrowing `text` for the call.
    func cEvent(text: UnsafePointer<CChar>?, length: Int) -> TesseraKeyEvent {
        TesseraKeyEvent(
            key: key.rawValue,
            codepoint: key == .char ? codepoint?.value ?? 0 : 0,
            shifted: shifted?.value ?? 0,
            base_layout: baseLayout?.value ?? 0,
            modifiers: modifiers.rawValue,
            kind: kind.rawValue,
            text: text,
            text_len: length)
    }
}
