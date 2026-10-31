//! Device reports: the queries a program sends to learn what the terminal
//! is and what state it is in, and the replies the terminal queues for it
//! as [`Request::Reply`] (ADR 0024).
//!
//! | Query | Name | Reply |
//! |---|---|---|
//! | `CSI c`, `CSI 0 c`, `ESC Z` | DA1, DECID | `CSI ? 1 ; 2 c` at level 1, `CSI ? 6L ; 22 c` at level L = 2..=5 |
//! | `CSI > c`, `CSI > 0 c` | DA2 | `CSI > 1 ; Pv ; 0 c`, Pv = [`DA2_PV`] |
//! | `CSI = c`, `CSI = 0 c` | DA3 | `DCS ! \| 00000000 ST` at level 4 and up |
//! | `CSI > q`, `CSI > 0 q` | XTVERSION | `DCS > \| Tessera <version> ST` |
//! | `CSI 5 n` | DSR | `CSI 0 n` |
//! | `CSI 6 n` | CPR | `CSI row ; col R` |
//! | `CSI ? Ps n` | DEC DSR | `?6` DECXCPR, `?15`, `?25`, `?26`, `?55`, `?56`, `?75`, `?85` with xterm's level gates |
//! | `CSI Ps $ p`, `CSI ? Ps $ p` | DECRQM | `CSI Ps ; Pm $ y`, `CSI ? Ps ; Pm $ y`, one for every query |
//! | `CSI ? u` | kitty keyboard flags | `CSI ? flags u` from the active screen's stack |
//! | `DCS $ q Pt ST` | DECRQSS | `DCS 1 $ r ... ST` for `m`, `r`, ` q` and `" p`, `DCS 0 $ r ST` otherwise |
//! | `CSI 14 t`, `CSI 16 t` | XTWINOPS | text area and cell size in pixels, once [`Terminal::set_cell_pixels`] knows the cell |
//! | `CSI 18 t`, `CSI 19 t` | XTWINOPS | `CSI 8 ; rows ; cols t`, `CSI 9 ; rows ; cols t` |
//! | `CSI 22 ; Ps t`, `CSI 23 ; Ps t` | title stack | push / pop the window title; a pop sends [`Request::SetTitle`], nothing is replied |
//!
//! The level is [`Terminal::conformance_level`] (DECSCL). Every reply is
//! built by `State::reply_csi`, `State::reply_dcs` or `State::reply_osc`
//! and is always 7-bit (`ESC [`, `ESC P`, `ESC ]`, `ESC \`), also after
//! DECSCL selected 8-bit replies ([`Terminal::c1_8bit_replies`] is
//! recorded, not honoured, until #96). A reply is queued the moment its
//! query completes, in query order. Nothing here writes to the grid, moves
//! the cursor or disarms a pending wrap.
//!
//! # Identity
//!
//! Tessera does not claim to be xterm: XTVERSION names Tessera (a space
//! before the version and no parenthesis, so tmux matches none of its
//! known terminal prefixes), DA2 answers the VT220 class `1` with a
//! version above vim's "not an xterm" mark and Pc 0, and DA1 advertises
//! only what Tessera implements: 22 (ANSI colour). DA1 follows DECSCL's
//! level, as on DEC's VT510 (xterm keeps its `decTerminalID`); level 5
//! answers as a VT525. DA2 does not follow the level.
//!
//! # Silences
//!
//! A query whose answer would be a lie or a security hole gets none:
//! DA1/DA2/DA3/XTVERSION with a non-zero parameter, DSR other than 5 and
//! 6, DEC DSRs other than the ones above (`?53`, `?62` and `?63` among
//! them), the window reports `CSI 11 t`, `13 t`, `14 ; 2 t`, `15 t`, `20 t`
//! and `21 t` (a title is never echoed back), `CSI 14 t` and `16 t` while
//! the cell size is unknown, `CSI ? N u` with a non-zero parameter (the
//! terminal's own reply form; the parser reports `CSI ? 0 u` like the query
//! `CSI ? u`, so that one is answered, an accepted risk in ADR 0024), a
//! DECRQM with a parameter that saturated at 65535, DECRQSS `" p` at level
//! 1 and a malformed DECRQSS. A colon
//! sub-parameter voids every query, as it voids the mode sequences (ADR
//! 0020). XTGETTCAP (`DCS + q`) and every other DCS stay silent.
//!
//! # DECRQM
//!
//! Every DECRQM is answered, with 0 for a mode Tessera does not know:
//! programs that send several queries and then DA1 (neovim) match replies
//! by order. Modes Tessera can never change answer 3 (permanently set:
//! SRM 12, DECANM `?2`) or 4 (permanently reset: KAM 2, CRM 3, DECSCLM
//! `?4`, DECARM `?8` and the ANSI modes DEC lists as permanently reset).
//! Synchronized output (`?2026`) answers its real state, as the
//! synchronized-output specification asks; xterm does not know it.
//!
//! # Title stack
//!
//! `CSI 22 ; Ps t` pushes the title the terminal last emitted
//! ([`Request::SetTitle`], after sanitising; the empty title before any)
//! for Ps 0, 2 or absent; `CSI 23 ; Ps t` pops it and emits it again as a
//! [`Request::SetTitle`]. Ps 1 (the icon name, not modelled) and any
//! other value do nothing, and so does a third (slot) parameter. The
//! stack keeps [`TITLE_STACK_DEPTH`] titles: a push onto a full stack drops
//! the oldest, a pop on an empty one does nothing. DECSTR and RIS keep it.
//!
//! Expected behaviour per query is pinned by `tests/fixtures/reports_spec.txt`
//! (its notes list every deviation from xterm).
//!
//! [`Request::Reply`]: crate::Request::Reply
//! [`Request::SetTitle`]: crate::Request::SetTitle
//! [`Terminal::set_cell_pixels`]: crate::Terminal::set_cell_pixels
//! [`Terminal::conformance_level`]: crate::Terminal::conformance_level
//! [`Terminal::c1_8bit_replies`]: crate::Terminal::c1_8bit_replies

use std::fmt;
use std::io::Write as _;

use tessera_grid::{Color, Style, UnderlineStyle};
use tracing::trace;

use crate::{CursorShape, MouseEncoding, MouseTracking, Request, State};

/// The version number DA2 reports (`CSI > 1 ; Pv ; 0 c`):
/// `10000 + minor * 100 + patch` of this crate's version, so 0.1.0 is
/// 10100. vim reads a Pv of 2500 and up as "not an xterm" (it then skips
/// xterm-only workarounds and still uses SGR mouse reports).
pub const DA2_PV: u32 = 10_000
    + parse_decimal(env!("CARGO_PKG_VERSION_MINOR")) * 100
    + parse_decimal(env!("CARGO_PKG_VERSION_PATCH"));

// vim treats a DA2 version below 2500 as an xterm patch level.
const _: () = assert!(DA2_PV >= 2500, "DA2_PV must read as \"not an xterm\"");

/// The text XTVERSION reports after `DCS > |`.
const XTVERSION: &str = concat!("Tessera ", env!("CARGO_PKG_VERSION"));

/// Titles the title stack keeps (xterm's `MAX_SAVED_TITLES`).
pub const TITLE_STACK_DEPTH: usize = 10;

/// Longest DECRQSS request (`Pt`) the terminal reads, in bytes; a longer
/// one is answered as invalid.
pub(crate) const MAX_DECRQSS_LEN: usize = 64;

/// `s` as a decimal number, at compile time. Panics (fails the build) on
/// anything but ASCII digits.
const fn parse_decimal(s: &str) -> u32 {
    let bytes = s.as_bytes();
    assert!(!bytes.is_empty(), "empty version component");
    let mut value = 0;
    let mut i = 0;
    while i < bytes.len() {
        assert!(
            bytes[i].is_ascii_digit(),
            "version component is not a number"
        );
        value = value * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    value
}

/// Whether a report's parameters ask for the default report: none, or a
/// single zero (the parser reports both as `[0]`).
fn default_query(params: &[u16]) -> bool {
    matches!(params, [] | [0])
}

/// The first parameter, absent meaning zero.
fn first(params: &[u16]) -> u16 {
    params.first().copied().unwrap_or(0)
}

/// A DECRPM mode value (`Pm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeReport {
    /// 0: not recognised.
    Unknown = 0,
    /// 1: set.
    Set = 1,
    /// 2: reset.
    Reset = 2,
    /// 3: permanently set.
    AlwaysSet = 3,
    /// 4: permanently reset.
    AlwaysReset = 4,
}

impl ModeReport {
    fn of(on: bool) -> Self {
        if on { Self::Set } else { Self::Reset }
    }
}

/// The XTWINOPS title stack (`CSI 22 t` / `CSI 23 t`).
#[derive(Debug, Default)]
pub(crate) struct TitleStack {
    /// Oldest first, at most [`TITLE_STACK_DEPTH`].
    titles: Vec<String>,
}

impl TitleStack {
    fn push(&mut self, title: String) {
        if self.titles.len() == TITLE_STACK_DEPTH {
            self.titles.remove(0);
        }
        self.titles.push(title);
    }

    fn pop(&mut self) -> Option<String> {
        self.titles.pop()
    }
}

impl State {
    /// Queues a reply: `ESC [` followed by `body`.
    pub(crate) fn reply_csi(&mut self, body: fmt::Arguments<'_>) {
        self.reply(b"\x1b[", body, b"");
    }

    /// Queues a reply: `ESC P`, `body`, `ESC \`.
    pub(crate) fn reply_dcs(&mut self, body: fmt::Arguments<'_>) {
        self.reply(b"\x1bP", body, b"\x1b\\");
    }

    /// Queues a reply: `ESC ]`, `body`, `ESC \` (the OSC colour echo of
    /// #10's second part is its first user).
    #[expect(dead_code, reason = "the OSC colour queries (#10, 10-colors) use it")]
    pub(crate) fn reply_osc(&mut self, body: fmt::Arguments<'_>) {
        self.reply(b"\x1b]", body, b"\x1b\\");
    }

    fn reply(&mut self, intro: &[u8], body: fmt::Arguments<'_>, end: &[u8]) {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(intro);
        // Writing to a `Vec` cannot fail.
        let _ = bytes.write_fmt(body);
        bytes.extend_from_slice(end);
        self.pending.push(Request::Reply(bytes));
    }

    /// DA1 (`CSI c`, `CSI 0 c`) and DECID (`ESC Z`).
    pub(crate) fn primary_device_attributes(&mut self) {
        match self.conformance_level {
            1 => self.reply_csi(format_args!("?1;2c")),
            level => self.reply_csi(format_args!("?6{level};22c")),
        }
    }

    /// The reports `CSI c`, `CSI > c`, `CSI = c` and `CSI > q` with
    /// `marker` the private marker (`0` for none); see the module
    /// documentation.
    pub(crate) fn identity_report(&mut self, marker: u8, action: char, params: &[u16]) {
        if !default_query(params) {
            trace!(?params, action = ?action, "identity report with a parameter ignored");
            return;
        }
        match (marker, action) {
            (0, 'c') => self.primary_device_attributes(),
            (b'>', 'c') => self.reply_csi(format_args!(">1;{DA2_PV};0c")),
            (b'=', 'c') if self.conformance_level >= 4 => {
                self.reply_dcs(format_args!("!|00000000"));
            }
            (b'>', 'q') => self.reply_dcs(format_args!(">|{XTVERSION}")),
            _ => trace!(marker, action = ?action, "identity report not answered"),
        }
    }

    /// DSR (`CSI Ps n`) and, with `dec`, DEC DSR (`CSI ? Ps n`): only the
    /// first parameter counts, as in xterm.
    pub(crate) fn device_status(&mut self, params: &[u16], dec: bool) {
        let level = self.conformance_level;
        let marker = if dec { "?" } else { "" };
        match (first(params), dec) {
            (5, false) => self.reply_csi(format_args!("0n")),
            (6, _) => {
                let grid = &self.grid;
                // `origin_row` subtracts the top margin in origin mode; a
                // pending wrap keeps the cursor on the last column.
                let row = u32::from(grid.origin_row()) + 1;
                let col = u32::from(grid.cursor().col) + 1;
                // DECXCPR appends the page from level 3 (xterm, with its
                // default terminal id 420).
                let page = if dec && level >= 3 { ";1" } else { "" };
                self.reply_csi(format_args!("{marker}{row};{col}{page}R"));
            }
            (15, true) if level >= 2 => self.reply_csi(format_args!("?13n")),
            (25, true) if level >= 2 => self.reply_csi(format_args!("?20n")),
            (26, true) if level >= 2 => {
                let ready = if level >= 3 { ";0" } else { "" };
                let keyboard = if level >= 4 { ";0" } else { "" };
                self.reply_csi(format_args!("?27;1{ready}{keyboard}n"));
            }
            (55, true) if level >= 3 => self.reply_csi(format_args!("?53n")),
            (56, true) if level >= 3 => self.reply_csi(format_args!("?57;0n")),
            (75, true) if level >= 4 => self.reply_csi(format_args!("?70n")),
            (85, true) if level >= 4 => self.reply_csi(format_args!("?83n")),
            (ps, dec) => trace!(ps, dec, "DSR not answered"),
        }
    }

    /// DECRQM: `CSI Ps $ p` (ANSI) or, with `dec`, `CSI ? Ps $ p`. One
    /// reply per query, for the first parameter only.
    pub(crate) fn request_mode(&mut self, params: &[u16], dec: bool) {
        let mode = first(params);
        if mode == u16::MAX {
            // A saturated parameter names no mode the program asked for.
            trace!(dec, "DECRQM with a saturated parameter ignored");
            return;
        }
        let report = if dec {
            self.dec_mode_report(mode)
        } else {
            self.ansi_mode_report(mode)
        };
        let marker = if dec { "?" } else { "" };
        let value = report as u8;
        self.reply_csi(format_args!("{marker}{mode};{value}$y"));
    }

    /// The DECRPM value of ANSI mode `mode`.
    fn ansi_mode_report(&self, mode: u16) -> ModeReport {
        match mode {
            4 => ModeReport::of(self.modes.insert),
            20 => ModeReport::of(self.modes.linefeed_newline),
            // SRM: Tessera never echoes locally and ignores SM/RM 12.
            12 => ModeReport::AlwaysSet,
            // KAM and CRM are ignored; the rest DEC lists as permanently
            // reset (GATM, SRTM, VEM, HEM, PUM, FEAM, FETM, MATM, TTM, SATM,
            // TSM, EBM).
            1..=3 | 5 | 7 | 10 | 11 | 13..=19 => ModeReport::AlwaysReset,
            _ => ModeReport::Unknown,
        }
    }

    /// The DECRPM value of DEC private mode `mode`.
    fn dec_mode_report(&self, mode: u16) -> ModeReport {
        let modes = &self.modes;
        let of = ModeReport::of;
        match mode {
            1 => of(self.keyboard.application_cursor_keys),
            // No VT52 mode: ANSI mode is always on.
            2 => ModeReport::AlwaysSet,
            3 => of(modes.columns_132),
            // Smooth scroll and auto-repeat are not modelled.
            4 | 8 => ModeReport::AlwaysReset,
            5 => of(modes.reverse_video),
            6 => of(self.grid.origin_mode()),
            7 => of(modes.auto_wrap),
            9 => of(modes.mouse == MouseTracking::X10),
            12 => of(modes.cursor_blink),
            25 => of(modes.cursor_visible),
            40 => of(modes.allow_column_change),
            47 | 1047 | 1049 => of(self.grid.is_alt_screen()),
            66 => of(self.keyboard.application_keypad),
            1000 => of(modes.mouse == MouseTracking::Normal),
            1002 => of(modes.mouse == MouseTracking::Button),
            1003 => of(modes.mouse == MouseTracking::AnyMotion),
            1004 => of(modes.focus_events),
            1005 => of(modes.mouse_encoding == MouseEncoding::Utf8),
            1006 => of(modes.mouse_encoding == MouseEncoding::Sgr),
            1015 => of(modes.mouse_encoding == MouseEncoding::Urxvt),
            1016 => of(modes.mouse_encoding == MouseEncoding::SgrPixels),
            2004 => of(modes.bracketed_paste),
            2026 => of(modes.synchronized_output),
            _ => ModeReport::Unknown,
        }
    }

    /// The kitty keyboard query `CSI ? u`: the active screen's flags.
    /// `CSI ? flags u` with a non-zero parameter is the reply form and is
    /// never answered; `CSI ? 0 u` reaches here exactly like `CSI ? u` and
    /// is answered (ADR 0024).
    pub(crate) fn kitty_keyboard_query(&mut self, params: &[u16]) {
        if !default_query(params) {
            trace!(?params, "kitty keyboard reply form ignored");
            return;
        }
        let flags = self.kitty.get(self.grid.is_alt_screen()).current().bits();
        self.reply_csi(format_args!("?{flags}u"));
    }

    /// XTWINOPS (`CSI Ps ; ... t`): the size reports and the title stack;
    /// every window action and every other report is ignored.
    pub(crate) fn window_ops(&mut self, params: &[u16]) {
        let rows = self.grid.rows();
        let cols = self.grid.cols();
        match params {
            [14] | [14, 0 | 1, ..] => {
                if let Some((width, height)) = self.cell_pixels {
                    let px = |cells: u16, cell: u16| {
                        u16::try_from(u32::from(cells) * u32::from(cell)).unwrap_or(u16::MAX)
                    };
                    let (height, width) = (px(rows, height), px(cols, width));
                    self.reply_csi(format_args!("4;{height};{width}t"));
                }
            }
            [16, ..] => {
                if let Some((width, height)) = self.cell_pixels {
                    self.reply_csi(format_args!("6;{height};{width}t"));
                }
            }
            [18, ..] => self.reply_csi(format_args!("8;{rows};{cols}t")),
            // There is no display behind the grid: the screen is the grid.
            [19, ..] => self.reply_csi(format_args!("9;{rows};{cols}t")),
            [22] | [22, 0 | 2] => self.title_stack.push(self.title.clone()),
            [23] | [23, 0 | 2] => {
                if let Some(title) = self.title_stack.pop() {
                    self.title.clone_from(&title);
                    self.pending.push(Request::SetTitle(title));
                }
            }
            _ => trace!(?params, "XTWINOPS not answered"),
        }
    }

    /// DECRQSS: answers request `pt` (the DCS payload after `$ q`); see
    /// the module documentation.
    pub(crate) fn status_string(&mut self, pt: &[u8]) {
        match pt {
            b"m" => {
                let sgr = sgr_report(self.grid.pen());
                self.reply_dcs(format_args!("1$r{sgr}m"));
            }
            b"r" => {
                let region = self.grid.scroll_region();
                let (top, bottom) = (u32::from(region.top) + 1, u32::from(region.bottom) + 1);
                self.reply_dcs(format_args!("1$r{top};{bottom}r"));
            }
            b" q" => {
                let style = self.cursor_style();
                let steady = match style.shape {
                    CursorShape::Block => 2,
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                };
                let ps = steady - u8::from(style.blink);
                self.reply_dcs(format_args!("1$r{ps} q"));
            }
            b"\"p" => {
                // xterm: "none of DECRQSS is valid for vt100's", and it
                // sends nothing at all for this one.
                if self.conformance_level < 2 {
                    trace!("DECRQSS of DECSCL at level 1 not answered");
                    return;
                }
                let level = self.conformance_level;
                let c1 = u8::from(!self.c1_8bit_replies);
                self.reply_dcs(format_args!("1$r6{level};{c1}\"p"));
            }
            _ => self.reply_dcs(format_args!("0$r")),
        }
    }
}

/// The SGR parameters (without `CSI` and `m`) that set `pen`, in xterm's
/// order: `0`, bold, underline, blink, inverse, hidden, faint, italic,
/// strikeout, then the foreground and the background. Underline styles
/// other than single print as `4:n` (kitty; neovim's probe needs `4:3`);
/// the underline colour and overline are not reported.
fn sgr_report(pen: Style) -> String {
    let mut out = String::from("0");
    let mut add = |text: &str| {
        out.push(';');
        out.push_str(text);
    };
    if pen.has(Style::BOLD) {
        add("1");
    }
    match pen.underline_style() {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => add("4"),
        UnderlineStyle::Double => add("4:2"),
        UnderlineStyle::Curly => add("4:3"),
        UnderlineStyle::Dotted => add("4:4"),
        UnderlineStyle::Dashed => add("4:5"),
    }
    for (flag, text) in [
        (Style::BLINK, "5"),
        (Style::INVERSE, "7"),
        (Style::HIDDEN, "8"),
        (Style::DIM, "2"),
        (Style::ITALIC, "3"),
        (Style::STRIKETHROUGH, "9"),
    ] {
        if pen.has(flag) {
            add(text);
        }
    }
    if let Some(fg) = color_report(pen.fg, 30, 90, 38) {
        add(&fg);
    }
    if let Some(bg) = color_report(pen.bg, 40, 100, 48) {
        add(&bg);
    }
    out
}

/// One colour of [`sgr_report`]: `base + n` for indexes 0..8, `bright +
/// n - 8` for 8..16, `ext:5:n` above, `ext:2::r:g:b` for direct colour and
/// nothing for the default.
fn color_report(color: Color, base: u8, bright: u8, ext: u8) -> Option<String> {
    match color {
        Color::Default => None,
        Color::Indexed(n @ 0..8) => Some((base + n).to_string()),
        Color::Indexed(n @ 8..16) => Some((bright + n - 8).to_string()),
        Color::Indexed(n) => Some(format!("{ext}:5:{n}")),
        Color::Rgb(r, g, b) => Some(format!("{ext}:2::{r}:{g}:{b}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da2_pv_follows_the_crate_version() {
        let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
        let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap();
        assert_eq!(DA2_PV, 10_000 + minor * 100 + patch);
    }

    #[test]
    fn a_full_title_stack_drops_the_oldest() {
        let mut stack = TitleStack::default();
        for n in 0..=TITLE_STACK_DEPTH {
            stack.push(n.to_string());
        }
        let popped: Vec<String> = std::iter::from_fn(|| stack.pop()).collect();
        let want: Vec<String> = (1..=TITLE_STACK_DEPTH)
            .rev()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(popped, want);
    }
}
