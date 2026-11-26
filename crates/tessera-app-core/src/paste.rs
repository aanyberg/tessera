//! The paste guard (#471, ADR 0022 D-PASTE): whether a paste asks before it
//! is sent, and the preview its confirmation shows.
//!
//! The decision is a pure function of `[terminal] paste_guard`, whether the
//! program turned on bracketed paste (DECSET 2004) and the text. The preview
//! is bounded whatever the paste's size, so a 16 MiB paste costs one scan
//! and a sheet of at most [`PREVIEW_MAX_BYTES`].

use tessera_config::PasteGuard;

/// Whether a paste goes to the program now or asks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PasteDecision {
    /// Send it.
    Paste = 0,
    /// Show the confirmation; send it only if the user agrees.
    Ask = 1,
}

/// A paste's decision and the mode it was made in
/// ([`crate::App::paste_check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteCheck {
    /// Whether the paste asks first.
    pub decision: PasteDecision,
    /// Whether the program had bracketed paste (DECSET 2004) on: without
    /// it each line of the paste runs as typed.
    pub bracketed: bool,
}

/// The most lines a preview shows; the rest are counted
/// ([`PastePreview::more_lines`]).
pub const PREVIEW_MAX_LINES: usize = 20;
/// The most characters a preview line shows (an escape counts one per
/// character it is written with); a longer line is cut and ends with `…`.
pub const PREVIEW_MAX_LINE_CHARS: usize = 200;
/// The most UTF-8 bytes a preview's text takes: every shown character is at
/// most 4 bytes, each line ends with at most `…` (3 bytes) and lines are
/// joined by `\n`.
pub const PREVIEW_MAX_BYTES: usize =
    PREVIEW_MAX_LINES * (PREVIEW_MAX_LINE_CHARS * 4 + 3) + (PREVIEW_MAX_LINES - 1);

/// Whether `ch` is a control a paste removes and the guard asks about: C0
/// other than tab, line feed and carriage return, DEL, or C1
/// (`docs/security.md` S4).
fn is_guarded_control(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')
}

/// What a paste of `text` does under `guard`, with bracketed paste on
/// (`bracketed`) or off.
///
/// * `Off` never asks, and nothing asks for an empty paste (it sends
///   nothing).
/// * `Always` asks for every other paste.
/// * `Unbracketed` asks when the text holds a line break (`\n`, a lone
///   `\r` or `\r\n`) and bracketed paste is off, since each line then runs
///   as typed, or when it holds a guarded control (C0 other than tab, line
///   feed and carriage return, DEL or C1; `ESC [ 201 ~` among them), in
///   either mode.
#[must_use]
pub fn paste_decision(guard: PasteGuard, bracketed: bool, text: &str) -> PasteDecision {
    if text.is_empty() {
        return PasteDecision::Paste;
    }
    let asks = match guard {
        PasteGuard::Off => false,
        PasteGuard::Always => true,
        PasteGuard::Unbracketed => text
            .chars()
            .any(|ch| is_guarded_control(ch) || (!bracketed && matches!(ch, '\n' | '\r'))),
    };
    if asks {
        PasteDecision::Ask
    } else {
        PasteDecision::Paste
    }
}

/// What the confirmation shows of a paste: its first lines, escaped, and
/// how many lines it has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PastePreview {
    /// The first [`PREVIEW_MAX_LINES`] lines joined by `\n`, each cut at
    /// [`PREVIEW_MAX_LINE_CHARS`], with every guarded control and every
    /// character that hides or reorders text escaped (see [`paste_preview`]).
    /// At most [`PREVIEW_MAX_BYTES`].
    pub text: String,
    /// How many lines the paste has: a trailing line break ends the last
    /// line rather than starting another; an empty paste has none.
    pub lines: usize,
    /// The lines not shown (`lines` less those in `text`).
    pub more_lines: usize,
    /// Whether the paste holds a guarded control, which pasting removes.
    pub has_controls: bool,
    /// Whether the paste holds a line break (`\n` or `\r`): without
    /// bracketed paste, each one is a Return.
    pub has_breaks: bool,
}

/// How a character is shown in the preview: `None` as itself.
///
/// * C0 controls other than tab in caret notation (`ESC` is `^[`, NUL `^@`),
///   DEL as `^?`, as `cat -v` writes them;
/// * C1 controls, the Unicode line and paragraph separators, the
///   interlinear annotation controls and every default-ignorable code point
///   ([`crate::url::is_default_ignorable`]: bidi controls, zero-width
///   characters, BOM, soft hyphen, tags, fillers) as `<U+XXXX>`, so the
///   sheet neither hides nor reorders text (`docs/security.md` S3). VS15
///   and VS16 stay, so emoji show as they will paste.
fn escape(ch: char) -> Option<String> {
    let code = u32::from(ch);
    match ch {
        '\t' | '\u{fe0e}' | '\u{fe0f}' => None,
        '\u{7f}' => Some("^?".to_owned()),
        _ if code < 0x20 => Some(format!("^{}", char::from_u32(code + 0x40).unwrap_or('?'))),
        '\u{80}'..='\u{9f}' | '\u{2028}' | '\u{2029}' | '\u{fff9}'..='\u{fffb}' => {
            Some(format!("<U+{code:04X}>"))
        }
        _ if crate::url::is_default_ignorable(ch) => Some(format!("<U+{code:04X}>")),
        _ => None,
    }
}

/// The preview of `text` for the paste confirmation, in one pass over it:
/// lines split at `\n`, `\r\n` and a lone `\r` (all three are a Return
/// without bracketed paste), only the first [`PREVIEW_MAX_LINES`] copied,
/// each cut at [`PREVIEW_MAX_LINE_CHARS`] characters as shown (escapes
/// included), and the rest only counted.
#[must_use]
pub fn paste_preview(text: &str) -> PastePreview {
    let mut preview = PastePreview::default();
    // Characters shown on the current line, or `None` once it was cut.
    let mut shown = Some(0usize);
    // Whether the current line has started (a character after a break).
    let mut open = false;
    // Line breaks written to `text` so far: lines are joined by `\n`.
    let mut joins = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '\n' | '\r') {
            if ch == '\r' {
                chars.next_if_eq(&'\n');
            }
            // A break ends a line, empty or not.
            preview.has_breaks = true;
            preview.lines += 1;
            open = false;
            shown = Some(0);
            continue;
        }
        preview.has_controls |= is_guarded_control(ch);
        open = true;
        if preview.lines >= PREVIEW_MAX_LINES {
            continue;
        }
        // The current line is line `lines` (from 0): join it, and any empty
        // lines before it, to those shown.
        while joins < preview.lines {
            preview.text.push('\n');
            joins += 1;
        }
        let Some(count) = shown else { continue };
        let escaped = escape(ch);
        let width = escaped
            .as_ref()
            .map_or(1, |escaped| escaped.chars().count());
        if count + width > PREVIEW_MAX_LINE_CHARS {
            preview.text.push('…');
            shown = None;
            continue;
        }
        match escaped {
            Some(escaped) => preview.text.push_str(&escaped),
            None => preview.text.push(ch),
        }
        shown = Some(count + width);
    }
    if open {
        preview.lines += 1;
    }
    // Empty lines at the end of those shown are shown too.
    while joins + 1 < preview.lines.min(PREVIEW_MAX_LINES) {
        preview.text.push('\n');
        joins += 1;
    }
    preview.more_lines = preview.lines.saturating_sub(PREVIEW_MAX_LINES);
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use PasteDecision::{Ask, Paste};

    /// Each case: the text, then the decision under `"unbracketed"` with
    /// bracketed paste off and on, `"always"` (either mode) and `"off"`.
    const CASES: &[(
        &str,
        PasteDecision,
        PasteDecision,
        PasteDecision,
        PasteDecision,
    )] = &[
        ("", Paste, Paste, Paste, Paste),
        ("ls -l", Paste, Paste, Ask, Paste),
        ("a\tb", Paste, Paste, Ask, Paste),
        // A line break: one trailing, between lines, a lone CR, CRLF.
        ("ls\n", Ask, Paste, Ask, Paste),
        ("a\nb", Ask, Paste, Ask, Paste),
        ("\r", Ask, Paste, Ask, Paste),
        ("a\rb", Ask, Paste, Ask, Paste),
        ("a\r\nb", Ask, Paste, Ask, Paste),
        // Controls ask in either mode.
        ("\x1b", Ask, Ask, Ask, Paste),
        ("a\x1b[201~b", Ask, Ask, Ask, Paste),
        ("a\x1b[201~\nrm -rf ~\n", Ask, Ask, Ask, Paste),
        ("\0", Ask, Ask, Ask, Paste),
        ("\x03", Ask, Ask, Ask, Paste),
        ("\x1f", Ask, Ask, Ask, Paste),
        ("\x7f", Ask, Ask, Ask, Paste),
        ("\u{80}", Ask, Ask, Ask, Paste),
        ("\u{9b}201~", Ask, Ask, Ask, Paste),
        ("\u{9f}", Ask, Ask, Ask, Paste),
        // Printable Unicode next to the ranges, and a bidirectional
        // override (shown escaped, but not removed, so not asked about).
        ("\u{a0}\u{7e}", Paste, Paste, Ask, Paste),
        ("abc\u{202e}def", Paste, Paste, Ask, Paste),
        // Unicode line separators are not line breaks for the shell.
        ("a\u{2028}b\u{85}", Ask, Ask, Ask, Paste),
        ("a\u{2028}b", Paste, Paste, Ask, Paste),
    ];

    #[test]
    fn the_guard_decides_every_case() {
        for &(text, off, on, always, never) in CASES {
            assert_eq!(
                paste_decision(PasteGuard::Unbracketed, false, text),
                off,
                "{text:?} unbracketed"
            );
            assert_eq!(
                paste_decision(PasteGuard::Unbracketed, true, text),
                on,
                "{text:?} bracketed"
            );
            for bracketed in [false, true] {
                assert_eq!(
                    paste_decision(PasteGuard::Always, bracketed, text),
                    always,
                    "{text:?} always"
                );
                assert_eq!(
                    paste_decision(PasteGuard::Off, bracketed, text),
                    never,
                    "{text:?} off"
                );
            }
        }
    }

    #[test]
    fn guarded_controls_are_exactly_c0_less_tab_lf_cr_plus_del_and_c1() {
        for code in 0u32..0x110 {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            let expected = matches!(code, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f..=0x9f);
            assert_eq!(is_guarded_control(ch), expected, "U+{code:04X}");
            let text = format!("a{ch}b");
            assert_eq!(paste_preview(&text).has_controls, expected, "U+{code:04X}");
        }
    }

    #[test]
    fn line_counts_follow_the_breaks_a_paste_sends() {
        for (text, lines) in [
            ("", 0),
            ("a", 1),
            ("a\n", 1),
            ("\n", 1),
            ("\r", 1),
            ("\r\n", 1),
            ("\n\r", 2),
            ("a\nb", 2),
            ("a\rb", 2),
            ("a\r\nb", 2),
            ("a\r\n\r\nb\n", 3),
            ("a\n\n", 2),
        ] {
            let preview = paste_preview(text);
            assert_eq!(preview.lines, lines, "{text:?}");
            assert_eq!(preview.more_lines, 0, "{text:?}");
            assert_eq!(preview.has_breaks, text.contains(['\n', '\r']), "{text:?}");
        }
    }

    #[test]
    fn past_the_shown_lines_an_open_last_line_still_counts() {
        // 25 lines, the last without a line break.
        let text = (0..25)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = paste_preview(&text);
        assert_eq!(preview.lines, 25);
        assert_eq!(preview.more_lines, 25 - PREVIEW_MAX_LINES);
        assert_eq!(preview.text.split('\n').count(), PREVIEW_MAX_LINES);
    }

    #[test]
    fn each_escaped_class_is_escaped_at_both_ends_and_its_neighbours_are_not() {
        // The first and last of every range, between visible text.
        let mut ranges = vec![(0x80, 0x9F), (0x2028, 0x2029), (0xFFF9, 0xFFFB)];
        ranges.extend(crate::url::DEFAULT_IGNORABLE);
        let mut probes: Vec<char> = ranges
            .iter()
            .flat_map(|&(first, last)| [first, last])
            .filter_map(char::from_u32)
            .collect();
        // VS15 and VS16 (the end of U+FE00-U+FE0F) stay; a tag and CGJ.
        probes.retain(|c| !matches!(c, '\u{fe0e}' | '\u{fe0f}'));
        probes.extend(['\u{fe0d}', '\u{e0041}', '\u{34f}', '\u{200e}', '\u{202d}']);
        for c in probes {
            let code = u32::from(c);
            let preview = paste_preview(&format!("x{c}y"));
            assert_eq!(preview.text, format!("x<U+{code:04X}>y"), "{code:04X}");
        }
        // Neighbours of the ranges, and the emoji presentation selectors,
        // are shown as they are.
        for c in [
            'a',
            '\u{e9}',
            '\u{ac}',
            '\u{ae}',
            '\u{a0}',
            '\u{34e}',
            '\u{350}',
            '\u{2027}',
            '\u{202f}',
            '\u{205f}',
            '\u{2070}',
            '\u{fe10}',
            '\u{fefe}',
            '\u{ffee}',
            '\u{fffc}',
            '\u{1f600}',
            '\u{fe0e}',
            '\u{fe0f}',
            '\u{e1000}',
        ] {
            let text = format!("x{c}y");
            assert_eq!(paste_preview(&text).text, text, "{:04X}", u32::from(c));
        }
    }

    #[test]
    fn the_preview_escapes_controls_and_hidden_characters() {
        let preview =
            paste_preview("echo hi\x1b[201~\r\nrm\0 -rf\x7f\u{9b}x\u{202e}y\u{200b}\u{2028}\tz");
        assert_eq!(
            preview.text,
            "echo hi^[[201~\nrm^@ -rf^?<U+009B>x<U+202E>y<U+200B><U+2028>\tz"
        );
        assert_eq!(preview.lines, 2);
        assert!(preview.has_controls);
        // Plain text, with its empty lines, is shown as it is.
        let plain = paste_preview("a\n\nb\r\rc\n\n");
        assert_eq!(plain.text, "a\n\nb\n\nc\n");
        assert_eq!(plain.lines, 6);
        assert!(!plain.has_controls);
        assert_eq!(paste_preview(""), PastePreview::default());
    }

    #[test]
    fn a_large_paste_shows_its_first_lines_cut_and_counts_the_rest() {
        // 1 MiB: 4096 lines of 255 characters and a line break.
        let line = "x".repeat(255);
        let text = format!("{line}\n").repeat(4096);
        assert_eq!(text.len(), 1 << 20);
        let preview = paste_preview(&text);
        assert_eq!(preview.lines, 4096);
        assert_eq!(preview.more_lines, 4096 - PREVIEW_MAX_LINES);
        let shown: Vec<&str> = preview.text.split('\n').collect();
        assert_eq!(shown.len(), PREVIEW_MAX_LINES);
        let cut = format!("{}…", "x".repeat(PREVIEW_MAX_LINE_CHARS));
        assert!(shown.iter().all(|shown| *shown == cut), "{:?}", shown[0]);
        // One line of 1 MiB of escapes is cut at the same width.
        let escapes = paste_preview(&"\u{1b}".repeat(1 << 20));
        assert_eq!(escapes.lines, 1);
        assert_eq!(
            escapes.text,
            format!("{}…", "^[".repeat(PREVIEW_MAX_LINE_CHARS / 2))
        );
        // A line of exactly the width is not cut.
        let exact = paste_preview(&"y".repeat(PREVIEW_MAX_LINE_CHARS));
        assert_eq!(exact.text.chars().count(), PREVIEW_MAX_LINE_CHARS);
    }

    #[test]
    fn the_preview_never_passes_its_byte_bound() {
        // The widest characters (4 bytes each) on every shown line.
        let line = "\u{1f600}".repeat(PREVIEW_MAX_LINE_CHARS + 10);
        let text = format!("{line}\n").repeat(PREVIEW_MAX_LINES + 5);
        let preview = paste_preview(&text);
        assert_eq!(preview.text.len(), PREVIEW_MAX_BYTES);
        assert_eq!(preview.more_lines, 5);
        // Escapes are ASCII, so a line of them is shorter.
        let escapes =
            format!("{}\n", "\u{9b}".repeat(PREVIEW_MAX_LINE_CHARS)).repeat(PREVIEW_MAX_LINES);
        assert!(paste_preview(&escapes).text.len() <= PREVIEW_MAX_BYTES);
    }
}
