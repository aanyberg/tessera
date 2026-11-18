//! Copy with Formatting: a selection's text with its colours and
//! attributes as HTML and RTF (ADR 0042).
//!
//! The core writes both formats and escapes every character itself
//! (lead decision D10): the frontend puts the strings on the pasteboard and
//! never parses them. The output holds only the elements and control words
//! listed here, so nothing a program printed can become markup, a link or
//! a reference to anything outside the text:
//!
//! - HTML: `html`, `head`, `meta charset`, `body`, `pre` and `span`, whose
//!   only attribute is a `style` of `color`, `background-color`,
//!   `font-weight:bold`, `font-style:italic` and `text-decoration`. Every
//!   `&` `<` `>` `"` `'` `\` `{` `}` and every scalar above U+007E is a
//!   numeric or named character reference; C0 controls and DEL in the
//!   text are dropped (line breaks are written as a line feed). No `<a>`
//!   (OSC 8 links copy as their text), no image, style sheet, script or
//!   URL.
//! - RTF: `\rtf1\ansi\ansicpg1252\deff0`, a one-font table, a colour
//!   table, and runs of `\cfN`, `\cbN`, `\b`, `\i`, `\ul` and `\strike` in
//!   groups; `\` `{` `}` are escaped, every scalar above U+007E is `\uN?`
//!   (UTF-16, signed), C0 controls and DEL are dropped, and a line break
//!   is `\par`. No `\field`, `\pict`, `\object` or `\*` destination.
//!
//! Default foreground and background colours are written as no colour, so
//! the text takes the colours of the document it is pasted into; the font
//! is the constant `Menlo` (nothing from the configuration reaches the
//! output). Each format is at most [`MAX_RICH_COPY_LEN`] bytes, and the RTF
//! colour table at most [`MAX_RTF_COLORS`] entries: past either the copy is
//! refused ([`RichCopy::TooLarge`]) and only the plain text is copied.

use std::collections::HashMap;
use std::fmt::Write as _;

use tessera_grid::{Cell, Color, Style};
use tessera_term::{Rgb, ThemeColors};

/// The longest HTML or RTF a Copy with Formatting writes, in bytes
/// (16 MiB); a longer one is refused and the plain text alone is copied.
pub const MAX_RICH_COPY_LEN: usize = 16 << 20;

/// The most colours the RTF colour table holds; a selection with more
/// distinct colours is refused like one over [`MAX_RICH_COPY_LEN`].
pub const MAX_RTF_COLORS: usize = 4096;

/// The HTML before the body. It ends with a line feed: a parser drops the
/// one right after a `<pre>` open tag, so a selection starting with a blank
/// line keeps it (#351).
const HTML_HEAD: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body><pre style=\"font-family:Menlo,monospace\">\n";
/// The HTML after the body.
const HTML_TAIL: &str = "</pre></body></html>";
/// The RTF before the colour table.
const RTF_HEAD: &str = "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fmodern Menlo;}}";
/// The RTF between the colour table and the body.
const RTF_BODY: &str = "\\f0\\fs24 ";
/// The fewest bytes of HTML markup a run with a look of its own adds: an
/// empty `span` element plus the shortest declaration (`color:#rrggbb`).
/// [`Styled`] charges it per such run, so the segment list is bounded by
/// the cap too, not only the text (S5).
const HTML_SPAN_MIN: usize = "<span style=\"\">".len() + "</span>".len() + "color:#000000".len();

/// A selection's text for Copy with Formatting: the plain text, as Copy
/// gives it, and the same text as HTML and RTF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedText {
    /// The text as [`crate::App::selection_text`] gives it.
    pub plain: String,
    /// The formatted forms, or why there are none.
    pub rich: RichCopy,
}

/// The formatted forms of a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichCopy {
    /// Both forms.
    Text {
        /// An HTML document (UTF-8, ASCII only in fact).
        html: String,
        /// An RTF document (ASCII).
        rtf: String,
    },
    /// One form would be longer than [`MAX_RICH_COPY_LEN`] bytes, or the
    /// selection has more than [`MAX_RTF_COLORS`] colours: only the plain
    /// text is copied.
    TooLarge,
}

/// The look of a run of text, resolved to colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per attribute the formats write"
)]
pub(crate) struct RunStyle {
    /// The text colour; `None` for the default.
    pub(crate) fg: Option<Rgb>,
    /// The background; `None` for the default.
    pub(crate) bg: Option<Rgb>,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    /// Any underline style.
    pub(crate) underline: bool,
    pub(crate) strike: bool,
}

impl RunStyle {
    /// The look of a cell drawn with `style` under `colors`: indexed
    /// colours through the palette, the default ones as none, and inverse
    /// as the two resolved colours swapped (the defaults then written,
    /// since the swap cannot be left to the document). Faint, blink,
    /// conceal and overline give nothing.
    pub(crate) fn of(style: &Style, colors: &ThemeColors) -> Self {
        let resolve = |color: Color| match color {
            Color::Default => None,
            Color::Indexed(i) => Some(colors.palette[usize::from(i)]),
            Color::Rgb(r, g, b) => Some(Rgb::new(r, g, b)),
        };
        let (mut fg, mut bg) = (resolve(style.fg), resolve(style.bg));
        if style.flags & Style::INVERSE != 0 {
            (fg, bg) = (Some(bg.unwrap_or(colors.bg)), Some(fg.unwrap_or(colors.fg)));
        }
        Self {
            fg,
            bg,
            bold: style.flags & Style::BOLD != 0,
            italic: style.flags & Style::ITALIC != 0,
            underline: style.flags & Style::UNDERLINE_STYLE_MASK != 0,
            strike: style.flags & Style::STRIKETHROUGH != 0,
        }
    }
}

/// One piece of a selection's styled text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Text of one look; never empty, never holding a line feed.
    Run(RunStyle, String),
    /// A line break.
    Newline,
}

/// A selection's text in runs of one look, collected under the terminal
/// lock and turned into HTML and RTF after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Styled {
    segments: Vec<Segment>,
    /// A lower bound of the HTML's length: the bytes of text kept (the
    /// scalars it does not drop, each written as at least its UTF-8
    /// bytes), a byte per line break and [`HTML_SPAN_MIN`] per run with a
    /// look of its own. Past the cap the HTML, and so the copy, is refused
    /// anyway, so a collection that passes it is stopped: the segment list
    /// it builds under the terminal lock stays within the cap's reach.
    weight: usize,
    /// The collection went past the cap and stopped.
    over: bool,
}

impl Styled {
    /// Whether the collection went past [`MAX_RICH_COPY_LEN`] (or the cap
    /// given to [`Self::push_cell`]).
    pub(crate) fn is_over(&self) -> bool {
        self.over
    }

    /// Whether there is no text.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Appends the text of `cell` (its cluster `text`, else its character)
    /// in `style`, merged into the run before it when the look is the
    /// same; stops collecting past `cap`.
    pub(crate) fn push_cell(
        &mut self,
        cell: &Cell,
        text: Option<&str>,
        style: RunStyle,
        cap: usize,
    ) {
        if self.over {
            return;
        }
        let mut one = [0u8; 4];
        let text = text.unwrap_or_else(|| cell.ch.encode_utf8(&mut one));
        let merges = matches!(self.segments.last(), Some(Segment::Run(last, _)) if *last == style);
        let markup = if merges || style == RunStyle::default() {
            0
        } else {
            HTML_SPAN_MIN
        };
        self.weight += markup
            + text
                .chars()
                .filter(|&c| !is_dropped(c))
                .map(char::len_utf8)
                .sum::<usize>();
        if self.weight > cap {
            self.over = true;
            self.segments = Vec::new();
            return;
        }
        if merges && let Some(Segment::Run(_, run)) = self.segments.last_mut() {
            run.push_str(text);
            return;
        }
        self.segments.push(Segment::Run(style, text.to_owned()));
    }

    /// Appends a line break.
    pub(crate) fn push_newline(&mut self, cap: usize) {
        if self.over {
            return;
        }
        self.weight += 1;
        if self.weight > cap {
            self.over = true;
            self.segments = Vec::new();
            return;
        }
        self.segments.push(Segment::Newline);
    }

    /// Both formats, each at most `cap` bytes.
    pub(crate) fn render(&self, cap: usize) -> RichCopy {
        if self.over {
            return RichCopy::TooLarge;
        }
        match (html(self, cap), rtf(self, cap)) {
            (Some(html), Some(rtf)) => RichCopy::Text { html, rtf },
            _ => RichCopy::TooLarge,
        }
    }
}

/// A scalar of cell text neither format writes: a C0 control or DEL. A
/// line feed is dropped too (the grid never holds one in a cell): line
/// breaks are [`Segment::Newline`] alone.
fn is_dropped(c: char) -> bool {
    c.is_ascii_control()
}

/// A string that refuses to grow past a cap, reserved once: each format
/// is written in one walk into a string reserved to an upper bound of its
/// length (at most the cap), so a copy of many megabytes allocates its
/// output once instead of doubling into it (#351), and walks its text
/// once (#433 adv M3). The pages of the reservation the text does not
/// reach are never touched, so they cost no memory.
struct Capped {
    text: String,
    cap: usize,
}

/// The cap was reached.
struct Over;

impl Capped {
    /// A string of at most `cap` bytes, `bound` of them (at most `cap`)
    /// reserved.
    fn new(bound: usize, cap: usize) -> Self {
        Self {
            text: String::with_capacity(bound.min(cap)),
            cap,
        }
    }

    fn push_str(&mut self, s: &str) -> Result<(), Over> {
        if self.text.len() + s.len() > self.cap {
            return Err(Over);
        }
        self.text.push_str(s);
        Ok(())
    }

    fn push(&mut self, c: char) -> Result<(), Over> {
        self.push_str(c.encode_utf8(&mut [0u8; 4]))
    }

    /// The text, which never outgrew its reservation.
    fn finish(self, reserved: usize) -> String {
        debug_assert_eq!(self.text.capacity(), reserved, "the output grew");
        self.text
    }
}

/// A short piece of text built on the stack: one character reference or
/// RTF control word or colour entry, so escaping allocates nothing. The
/// longest is a run's RTF control words with every look set and both
/// colours at [`MAX_RTF_COLORS`]: `\cf4096 \cb4096 \b \i \ul \strike `, 34
/// bytes ([`RTF_WORD_MAX`]).
struct Small {
    bytes: [u8; 48],
    len: usize,
}

impl Small {
    const fn new() -> Self {
        Self {
            bytes: [0; 48],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // Only `write_str` fills it, with whole `str`s.
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

/// The longest RTF control words of a run, which [`Small`] holds.
const RTF_WORD_MAX: &str = "\\cf4096 \\cb4096 \\b \\i \\ul \\strike ";

/// The longest HTML style of a run, every look set.
const HTML_STYLE_MAX: &str = "color:#000000;background-color:#000000;font-weight:bold;font-style:italic;text-decoration:underline line-through";

/// The most HTML bytes one byte of a run's text becomes: `&quot;` for `"`
/// (a scalar above U+007E takes fewer per byte: `&#x7ff;` for 2 bytes,
/// `&#xffff;` for 3, `&#x10ffff;` for 4).
const HTML_PER_BYTE_MAX: usize = "&quot;".len();

/// Twice the most RTF bytes one byte of a run's text becomes: a 4-byte
/// scalar is two `\uN?` words (UTF-16 surrogates) of at most 9 bytes
/// (`\u-10240?`), under 4.5 a byte; `\\` is 2 for 1 byte, `\u2047?` 7 for
/// 2 and `\u-32768?` 9 for 3.
const RTF_PER_TWO_BYTES_MAX: usize = 9;

/// The longest RTF colour table entry.
const RTF_COLOR_MAX: &str = "\\red255\\green255\\blue255;";

impl std::fmt::Write for Small {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        let into = self.bytes.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        into.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// The HTML document of `styled`, or `None` past `cap` bytes: written in
/// one walk of the text into one reservation of at most `cap` bytes.
pub(crate) fn html(styled: &Styled, cap: usize) -> Option<String> {
    let mut out = Capped::new(html_bound(styled), cap);
    let reserved = out.text.capacity();
    html_into(styled, &mut out)?;
    Some(out.finish(reserved))
}

/// An upper bound of the length of the HTML of `styled`, from the length
/// of each run (not its text): the most markup a run takes, and the most
/// bytes each byte of text becomes.
fn html_bound(styled: &Styled) -> usize {
    let span = "<span style=\"".len() + HTML_STYLE_MAX.len() + "\">".len() + "</span>".len();
    styled
        .segments
        .iter()
        .fold(HTML_HEAD.len() + HTML_TAIL.len(), |bound, segment| {
            bound.saturating_add(match segment {
                Segment::Newline => 1,
                Segment::Run(_, text) => {
                    span.saturating_add(text.len().saturating_mul(HTML_PER_BYTE_MAX))
                }
            })
        })
}

/// The HTML document of `styled` into `out`.
fn html_into(styled: &Styled, out: &mut Capped) -> Option<()> {
    out.push_str(HTML_HEAD).ok()?;
    let mut style = String::new();
    for segment in &styled.segments {
        match segment {
            Segment::Newline => out.push('\n').ok()?,
            Segment::Run(look, text) => {
                html_style(look, &mut style);
                if !style.is_empty() {
                    out.push_str("<span style=\"").ok()?;
                    out.push_str(&style).ok()?;
                    out.push_str("\">").ok()?;
                }
                for c in text.chars() {
                    html_escape(c, out).ok()?;
                }
                if !style.is_empty() {
                    out.push_str("</span>").ok()?;
                }
            }
        }
    }
    out.push_str(HTML_TAIL).ok()
}

/// The declarations of `look`, in the fixed order, into `out` (emptied
/// first).
fn html_style(look: &RunStyle, out: &mut String) {
    out.clear();
    let mut decl = |text: std::fmt::Arguments<'_>| {
        if !out.is_empty() {
            out.push(';');
        }
        // Writing to a String cannot fail.
        let _ = out.write_fmt(text);
    };
    if let Some(Rgb { r, g, b }) = look.fg {
        decl(format_args!("color:#{r:02x}{g:02x}{b:02x}"));
    }
    if let Some(Rgb { r, g, b }) = look.bg {
        decl(format_args!("background-color:#{r:02x}{g:02x}{b:02x}"));
    }
    if look.bold {
        decl(format_args!("font-weight:bold"));
    }
    if look.italic {
        decl(format_args!("font-style:italic"));
    }
    match (look.underline, look.strike) {
        (true, true) => decl(format_args!("text-decoration:underline line-through")),
        (true, false) => decl(format_args!("text-decoration:underline")),
        (false, true) => decl(format_args!("text-decoration:line-through")),
        (false, false) => {}
    }
}

/// One scalar of text as HTML.
fn html_escape(c: char, out: &mut Capped) -> Result<(), Over> {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        '\\' => out.push_str("&#92;"),
        '{' => out.push_str("&#123;"),
        '}' => out.push_str("&#125;"),
        c if is_dropped(c) => Ok(()),
        c if u32::from(c) > 0x7e => {
            let mut reference = Small::new();
            let written = write!(reference, "&#x{:x};", u32::from(c));
            debug_assert!(written.is_ok(), "a reference fits");
            out.push_str(reference.as_str())
        }
        c => out.push(c),
    }
}

/// The RTF document of `styled`, or `None` past `cap` bytes or
/// [`MAX_RTF_COLORS`] colours. The colour table comes before the body that
/// uses it, so it is built first from the looks alone ([`rtf_colors`]);
/// then the text is walked once, into one reservation of at most `cap`
/// bytes.
pub(crate) fn rtf(styled: &Styled, cap: usize) -> Option<String> {
    let (table, colors) = rtf_colors(styled)?;
    let mut out = Capped::new(rtf_bound(styled, table.len()), cap);
    let reserved = out.text.capacity();
    out.push_str(RTF_HEAD).ok()?;
    out.push_str("{\\colortbl;").ok()?;
    let mut entry = Small::new();
    for &Rgb { r, g, b } in &table {
        entry.len = 0;
        let written = write!(entry, "\\red{r}\\green{g}\\blue{b};");
        debug_assert!(written.is_ok(), "a colour entry fits");
        out.push_str(entry.as_str()).ok()?;
    }
    out.push('}').ok()?;
    out.push_str(RTF_BODY).ok()?;
    rtf_body(styled, &mut out, &colors)?;
    out.push('}').ok()?;
    Some(out.finish(reserved))
}

/// The RTF colour table of `styled`: its colours in the order the body
/// first uses them (each run's text colour, then its background), and
/// each colour's number in the table (from 1). `None` past
/// [`MAX_RTF_COLORS`] colours.
fn rtf_colors(styled: &Styled) -> Option<(Vec<Rgb>, HashMap<Rgb, usize>)> {
    let mut table: Vec<Rgb> = Vec::new();
    let mut colors: HashMap<Rgb, usize> = HashMap::new();
    for segment in &styled.segments {
        let Segment::Run(look, _) = segment else {
            continue;
        };
        for rgb in [look.fg, look.bg].into_iter().flatten() {
            if colors.contains_key(&rgb) {
                continue;
            }
            if table.len() >= MAX_RTF_COLORS {
                return None;
            }
            table.push(rgb);
            colors.insert(rgb, table.len());
        }
    }
    Some((table, colors))
}

/// An upper bound of the length of the RTF of `styled` with `colors`
/// colours, from the length of each run (not its text).
fn rtf_bound(styled: &Styled, colors: usize) -> usize {
    let head = RTF_HEAD.len()
        + "{\\colortbl;".len()
        + colors.saturating_mul(RTF_COLOR_MAX.len())
        + "}".len()
        + RTF_BODY.len()
        + "}".len();
    let group = "{".len() + RTF_WORD_MAX.len() + "}".len();
    styled.segments.iter().fold(head, |bound, segment| {
        bound.saturating_add(match segment {
            Segment::Newline => "\\par ".len(),
            Segment::Run(_, text) => {
                group.saturating_add(text.len().saturating_mul(RTF_PER_TWO_BYTES_MAX).div_ceil(2))
            }
        })
    })
}

/// The RTF body of `styled` into `out`, its colours numbered by `colors`.
fn rtf_body(styled: &Styled, body: &mut Capped, colors: &HashMap<Rgb, usize>) -> Option<()> {
    let index = |rgb: Rgb| colors.get(&rgb).copied();
    let mut word = Small::new();
    for segment in &styled.segments {
        match segment {
            Segment::Newline => body.push_str("\\par ").ok()?,
            Segment::Run(look, text) => {
                word.len = 0;
                if let Some(fg) = look.fg {
                    let written = write!(word, "\\cf{} ", index(fg)?);
                    debug_assert!(written.is_ok(), "the control words fit");
                }
                if let Some(bg) = look.bg {
                    let written = write!(word, "\\cb{} ", index(bg)?);
                    debug_assert!(written.is_ok(), "the control words fit");
                }
                for (on, control) in [
                    (look.bold, "\\b "),
                    (look.italic, "\\i "),
                    (look.underline, "\\ul "),
                    (look.strike, "\\strike "),
                ] {
                    if on {
                        let written = word.write_str(control);
                        debug_assert!(written.is_ok(), "the control words fit");
                    }
                }
                let grouped = word.len > 0;
                if grouped {
                    body.push('{').ok()?;
                    body.push_str(word.as_str()).ok()?;
                }
                for c in text.chars() {
                    rtf_escape(c, body).ok()?;
                }
                if grouped {
                    body.push('}').ok()?;
                }
            }
        }
    }
    Some(())
}

/// One scalar of text as RTF.
fn rtf_escape(c: char, out: &mut Capped) -> Result<(), Over> {
    match c {
        '\\' => out.push_str("\\\\"),
        '{' => out.push_str("\\{"),
        '}' => out.push_str("\\}"),
        c if is_dropped(c) => Ok(()),
        c if u32::from(c) > 0x7e => {
            let mut units = [0u16; 2];
            let mut item = Small::new();
            for unit in c.encode_utf16(&mut units) {
                // RTF's \uN takes a signed 16-bit number.
                let signed = i16::from_ne_bytes(unit.to_ne_bytes());
                let written = write!(item, "\\u{signed}?");
                debug_assert!(written.is_ok(), "a \\u word fits");
            }
            out.push_str(item.as_str())
        }
        c => out.push(c),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RED: Rgb = Rgb::new(0xff, 0, 0);

    fn styled(parts: &[(&str, RunStyle)]) -> Styled {
        let mut styled = Styled::default();
        for (i, &(text, look)) in parts.iter().enumerate() {
            if i > 0 && text.is_empty() {
                styled.push_newline(MAX_RICH_COPY_LEN);
                continue;
            }
            let cell = Cell::default();
            styled.push_cell(&cell, Some(text), look, MAX_RICH_COPY_LEN);
        }
        styled
    }

    fn body_html(styled: &Styled) -> String {
        let html = html(styled, MAX_RICH_COPY_LEN).unwrap();
        html.strip_prefix(HTML_HEAD)
            .and_then(|rest| rest.strip_suffix(HTML_TAIL))
            .unwrap()
            .to_owned()
    }

    /// The RTF after the colour table, without the final brace.
    fn body_rtf(styled: &Styled) -> String {
        let rtf = rtf(styled, MAX_RICH_COPY_LEN).unwrap();
        let at = rtf.find(RTF_BODY).unwrap() + RTF_BODY.len();
        rtf[at..rtf.len() - 1].to_owned()
    }

    #[test]
    fn rt1_plain() {
        let s = styled(&[("hi", RunStyle::default())]);
        assert_eq!(
            html(&s, MAX_RICH_COPY_LEN).unwrap(),
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body><pre style=\"font-family:Menlo,monospace\">\nhi</pre></body></html>"
        );
        assert_eq!(
            rtf(&s, MAX_RICH_COPY_LEN).unwrap(),
            "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fmodern Menlo;}}{\\colortbl;}\\f0\\fs24 hi}"
        );
    }

    /// #351: a parser drops the line feed right after `<pre>`, so one is
    /// always written there and a selection starting with a blank line
    /// keeps it.
    #[test]
    fn a_leading_blank_line_survives_the_pre_tag() {
        let mut s = Styled::default();
        s.push_newline(MAX_RICH_COPY_LEN);
        s.push_cell(
            &Cell::default(),
            Some("a"),
            RunStyle::default(),
            MAX_RICH_COPY_LEN,
        );
        let html = html(&s, MAX_RICH_COPY_LEN).unwrap();
        assert!(html.contains("monospace\">\n\na</pre>"), "{html}");
        assert!(HTML_HEAD.ends_with("\">\n"));
    }

    /// #351, #433 adv M3: each format is written in one walk into one
    /// reservation, however long the copy: the string never grows past it
    /// (checked in [`Capped::finish`]), the reservation is the format's
    /// bound or the cap, and the text fits the cap exactly. 2 MiB of mixed
    /// text and looks.
    #[test]
    fn each_format_is_written_into_one_allocation() {
        let mut s = Styled::default();
        let cell = Cell::default();
        for i in 0..40_000u32 {
            let look = RunStyle {
                fg: Some(Rgb::new(byte(i), 0, 0)),
                bold: i % 3 == 0,
                ..RunStyle::default()
            };
            s.push_cell(&cell, Some("ab<\u{e9}\u{65e5}{"), look, MAX_RICH_COPY_LEN);
            s.push_cell(
                &cell,
                Some(" x\u{1f600}"),
                RunStyle::default(),
                MAX_RICH_COPY_LEN,
            );
            if i % 50 == 0 {
                s.push_newline(MAX_RICH_COPY_LEN);
            }
        }
        let RichCopy::Text { html, rtf } = s.render(MAX_RICH_COPY_LEN) else {
            panic!("refused");
        };
        assert!(html.len() > 1 << 20 && rtf.len() > 1 << 20);
        let colors = rtf_colors(&s).unwrap().0.len();
        assert!(html.capacity() >= html_bound(&s).min(MAX_RICH_COPY_LEN));
        assert!(rtf.capacity() >= rtf_bound(&s, colors).min(MAX_RICH_COPY_LEN));
        assert!(html.len() <= html_bound(&s) && rtf.len() <= rtf_bound(&s, colors));
        // At the exact cap too, and refused one byte under it.
        let (h, r) = (html.len(), rtf.len());
        assert_eq!(super::html(&s, h).as_deref(), Some(html.as_str()));
        assert_eq!(super::rtf(&s, r).as_deref(), Some(rtf.as_str()));
        assert!(super::html(&s, h - 1).is_none() && super::rtf(&s, r - 1).is_none());
    }

    /// The bounds the formats reserve hold for the text that writes the
    /// most bytes a byte in each (`"` and `'` in HTML, U+07FF, U+0800,
    /// U+8000, U+FFFF and the first astral scalar), with every look set on
    /// one-cell runs: the output fits its reservation.
    #[test]
    fn the_reserved_bounds_hold_for_the_widest_text() {
        let cell = Cell::default();
        for text in [
            "\"",
            "'",
            "\\",
            "{",
            "\u{7ff}",
            "\u{800}",
            "\u{8000}",
            "\u{ffff}",
            "\u{10000}",
            "\u{10ffff}",
        ] {
            let mut s = Styled::default();
            for i in 0..1_000u32 {
                let look = RunStyle {
                    fg: Some(Rgb::new(byte(i), byte(i >> 8), 1)),
                    bg: Some(Rgb::new(255, byte(i), 255)),
                    bold: true,
                    italic: true,
                    underline: true,
                    strike: true,
                };
                s.push_cell(
                    &cell,
                    Some(&text.repeat(1 + usize::try_from(i % 3).unwrap())),
                    look,
                    MAX_RICH_COPY_LEN,
                );
                if i % 7 == 0 {
                    s.push_newline(MAX_RICH_COPY_LEN);
                }
            }
            let html = html(&s, MAX_RICH_COPY_LEN).unwrap();
            let rtf = rtf(&s, MAX_RICH_COPY_LEN).unwrap();
            let colors = rtf_colors(&s).unwrap().0.len();
            assert!(html.len() <= html_bound(&s), "{text:?}: html");
            assert!(rtf.len() <= rtf_bound(&s, colors), "{text:?}: rtf");
        }
        // One long run of 4-byte scalars, the widest RTF a byte (two
        // surrogate words, 17 bytes for 4), so the run's control-word
        // allowance cannot hide a per-byte bound that is too small.
        let mut s = Styled::default();
        for _ in 0..1_000 {
            s.push_cell(
                &cell,
                Some("\u{10000}"),
                RunStyle::default(),
                MAX_RICH_COPY_LEN,
            );
        }
        let rtf_text = rtf(&s, MAX_RICH_COPY_LEN).unwrap();
        assert!(rtf_text.contains(&"\\u-10240?\\u-9216?".repeat(1_000)));
        assert!(rtf_text.len() <= rtf_bound(&s, rtf_colors(&s).unwrap().0.len()));
        let mut all = String::new();
        html_style(
            &RunStyle {
                fg: Some(RED),
                bg: Some(RED),
                bold: true,
                italic: true,
                underline: true,
                strike: true,
            },
            &mut all,
        );
        assert_eq!(all.len(), HTML_STYLE_MAX.len());
    }

    /// #430 m2: the longest RTF control words of a run (every look, both
    /// colours numbered 4096) are written whole: [`Small`] holds them.
    #[test]
    fn the_longest_control_words_are_written_whole() {
        let cell = Cell::default();
        let mut s = Styled::default();
        for i in 0..4095u32 {
            let look = RunStyle {
                fg: Some(Rgb::new(byte(i), byte(i >> 8), 0)),
                ..RunStyle::default()
            };
            s.push_cell(&cell, Some("x"), look, MAX_RICH_COPY_LEN);
        }
        let last = Rgb::new(1, 2, 3);
        let every = RunStyle {
            fg: Some(last),
            bg: Some(last),
            bold: true,
            italic: true,
            underline: true,
            strike: true,
        };
        s.push_cell(&cell, Some("y"), every, MAX_RICH_COPY_LEN);
        let rtf = rtf(&s, MAX_RICH_COPY_LEN).unwrap();
        let group = format!("{{{RTF_WORD_MAX}y}}}}");
        assert_eq!(RTF_WORD_MAX.len(), 34);
        assert!(rtf.ends_with(&group), "{}", &rtf[rtf.len() - 60..]);
        assert!(rtf.contains("\\red1\\green2\\blue3;}"));
    }

    #[test]
    fn rt2_red_bold_then_a_newline() {
        let bold_red = RunStyle {
            fg: Some(RED),
            bold: true,
            ..RunStyle::default()
        };
        let s = styled(&[
            ("a", bold_red),
            ("", RunStyle::default()),
            ("b", RunStyle::default()),
        ]);
        assert_eq!(
            body_html(&s),
            "<span style=\"color:#ff0000;font-weight:bold\">a</span>\nb"
        );
        assert_eq!(
            rtf(&s, MAX_RICH_COPY_LEN).unwrap(),
            "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fmodern Menlo;}}{\\colortbl;\\red255\\green0\\blue0;}\\f0\\fs24 {\\cf1 \\b a}\\par b}"
        );
    }

    #[test]
    fn rt3_hostile_text_is_escaped() {
        let s = styled(&[("<script>&\"'\\{}é😀", RunStyle::default())]);
        assert_eq!(
            body_html(&s),
            "&lt;script&gt;&amp;&quot;&#39;&#92;&#123;&#125;&#xe9;&#x1f600;"
        );
        assert_eq!(
            body_rtf(&s),
            "<script>&\"'\\\\\\{\\}\\u233?\\u-10179?\\u-8704?"
        );
    }

    #[test]
    fn rt4_colours_and_decorations() {
        let look = RunStyle {
            fg: Some(Rgb::new(0, 0xff, 0)),
            bg: Some(Rgb::new(0, 0, 0xff)),
            italic: true,
            underline: true,
            ..RunStyle::default()
        };
        let s = styled(&[("x", look)]);
        assert_eq!(
            body_html(&s),
            "<span style=\"color:#00ff00;background-color:#0000ff;font-style:italic;text-decoration:underline\">x</span>"
        );
        let rtf = rtf(&s, MAX_RICH_COPY_LEN).unwrap();
        assert!(
            rtf.contains("{\\colortbl;\\red0\\green255\\blue0;\\red0\\green0\\blue255;}"),
            "{rtf}"
        );
        assert_eq!(body_rtf(&s), "{\\cf1 \\cb2 \\i \\ul x}");
    }

    #[test]
    fn rt5_runs_merge_and_split() {
        let red = RunStyle {
            fg: Some(RED),
            ..RunStyle::default()
        };
        let mut s = Styled::default();
        let cell = Cell::default();
        s.push_cell(&cell, Some("a"), red, MAX_RICH_COPY_LEN);
        s.push_cell(&cell, Some("b"), red, MAX_RICH_COPY_LEN);
        s.push_cell(
            &cell,
            Some("c"),
            RunStyle { bold: true, ..red },
            MAX_RICH_COPY_LEN,
        );
        assert_eq!(
            body_html(&s),
            "<span style=\"color:#ff0000\">ab</span><span style=\"color:#ff0000;font-weight:bold\">c</span>"
        );
        assert_eq!(body_rtf(&s), "{\\cf1 ab}{\\cf1 \\b c}");
    }

    #[test]
    fn rt6_strike_and_underline() {
        let look = RunStyle {
            underline: true,
            strike: true,
            ..RunStyle::default()
        };
        let s = styled(&[("z", look)]);
        assert_eq!(
            body_html(&s),
            "<span style=\"text-decoration:underline line-through\">z</span>"
        );
        assert_eq!(body_rtf(&s), "{\\ul \\strike z}");
    }

    #[test]
    fn rt7_a_leading_space_after_a_control_word_is_kept() {
        let bold = RunStyle {
            bold: true,
            ..RunStyle::default()
        };
        let s = styled(&[("a", RunStyle::default()), (" b", bold)]);
        assert_eq!(body_rtf(&s), "a{\\b  b}");
    }

    #[test]
    fn rt8_controls_are_dropped() {
        let s = styled(&[("a\u{1b}b\u{7}c\u{85}d\u{7f}", RunStyle::default())]);
        assert_eq!(body_html(&s), "abc&#x85;d");
        assert_eq!(body_rtf(&s), "abc\\u133?d");
    }

    #[test]
    fn rt9_clusters_and_wide_characters() {
        let s = styled(&[("e\u{301}日", RunStyle::default())]);
        assert_eq!(body_html(&s), "e&#x301;&#x65e5;");
        // The fixture's `☈5?` is a mangled `☈5?` (U+65E5 = 26085).
        assert_eq!(body_rtf(&s), "e\\u769?\\u26085?");
        let s = styled(&[("\u{f8ff}", RunStyle::default())]);
        assert_eq!(body_rtf(&s), "\\u-1793?");
    }

    #[test]
    fn rt12_inverse_swaps_the_resolved_colours() {
        let mut colors = ThemeColors {
            fg: Rgb::new(0xc0, 0xc0, 0xc0),
            bg: Rgb::new(0x10, 0x10, 0x10),
            ..ThemeColors::default()
        };
        let style = Style {
            flags: Style::INVERSE,
            ..Style::DEFAULT
        };
        let s = styled(&[("i", RunStyle::of(&style, &colors))]);
        assert_eq!(
            body_html(&s),
            "<span style=\"color:#101010;background-color:#c0c0c0\">i</span>"
        );
        // An indexed foreground goes through the palette; faint, blink,
        // conceal and overline give nothing; every underline style counts.
        colors.palette[1] = Rgb::new(1, 2, 3);
        let style = Style {
            fg: Color::Indexed(1),
            flags: Style::DIM
                | Style::BLINK
                | Style::HIDDEN
                | Style::OVERLINE
                | Style::CURLY_UNDERLINE,
            ..Style::DEFAULT
        };
        assert_eq!(
            RunStyle::of(&style, &colors),
            RunStyle {
                fg: Some(Rgb::new(1, 2, 3)),
                underline: true,
                ..RunStyle::default()
            }
        );
    }

    /// The cap is exact: the output at the cap passes, one byte less
    /// refuses; the collection itself stops past it.
    #[test]
    fn each_format_is_capped_exactly() {
        let s = styled(&[(
            "ab<c",
            RunStyle {
                fg: Some(RED),
                ..RunStyle::default()
            },
        )]);
        let html_len = html(&s, MAX_RICH_COPY_LEN).unwrap().len();
        let rtf_len = rtf(&s, MAX_RICH_COPY_LEN).unwrap().len();
        assert!(html(&s, html_len).is_some());
        assert!(html(&s, html_len - 1).is_none());
        assert!(rtf(&s, rtf_len).is_some());
        assert!(rtf(&s, rtf_len - 1).is_none());
        assert!(matches!(
            s.render(html_len.max(rtf_len)),
            RichCopy::Text { .. }
        ));
        assert_eq!(s.render(html_len.min(rtf_len) - 1), RichCopy::TooLarge);

        let mut flood = Styled::default();
        let cell = Cell::default();
        for _ in 0..100 {
            flood.push_cell(&cell, Some("abcdefghij"), RunStyle::default(), 64);
        }
        assert!(flood.is_over());
        assert!(flood.segments.is_empty(), "nothing kept past the cap");
        assert_eq!(flood.render(MAX_RICH_COPY_LEN), RichCopy::TooLarge);
    }

    /// The low byte of `i`.
    fn byte(i: u32) -> u8 {
        i.to_le_bytes()[0]
    }

    /// S5: the hostile worst case (every cell its own look, every scalar a
    /// long reference) is refused at the cap instead of growing.
    #[test]
    fn a_hostile_flood_is_refused_at_the_cap() {
        let cap = 1 << 16;
        let mut s = Styled::default();
        let cell = Cell::default();
        // Collected without the cap, so the formats' own caps are tested.
        for i in 0..10_000u32 {
            let look = RunStyle {
                fg: Some(Rgb::new(byte(i), byte(i >> 8), 1)),
                bg: Some(Rgb::new(1, byte(i), byte(i >> 8))),
                bold: true,
                italic: true,
                underline: true,
                strike: true,
            };
            s.push_cell(&cell, Some("\u{10ffff}"), look, MAX_RICH_COPY_LEN);
        }
        assert!(!s.is_over());
        assert!(html(&s, cap).is_none());
        assert!(rtf(&s, cap).is_none());
        assert_eq!(s.render(cap), RichCopy::TooLarge);
    }

    /// S5: a flood of one-cell runs, each a look of its own, stops the
    /// collection once the markup they need passes the cap, long before
    /// their text does (a red and green flood of 1 MiB of text against a
    /// 1 MiB cap stops within cap / 36 cells), and the charge never passes
    /// the HTML's real length.
    #[test]
    fn a_flood_of_looks_stops_the_collection_at_its_markup() {
        let cap = 1 << 20;
        let cell = Cell::default();
        let looks = [RED, Rgb::new(0, 0xff, 0)].map(|fg| RunStyle {
            fg: Some(fg),
            ..RunStyle::default()
        });
        let mut s = Styled::default();
        let mut stopped = None;
        for i in 0..cap {
            s.push_cell(&cell, Some("x"), looks[i % 2], cap);
            if s.is_over() {
                stopped = Some(i);
                break;
            }
        }
        let stopped = stopped.expect("the collection stops");
        assert!(
            stopped <= cap / (HTML_SPAN_MIN + 1) + 1,
            "stopped after {stopped} cells"
        );
        assert!(s.segments.is_empty(), "nothing kept past the cap");
        assert_eq!(s.render(cap), RichCopy::TooLarge);
        // Alternating with the default look (which writes no markup) stops
        // within twice as many cells.
        let mut s = Styled::default();
        let mut cells = 0;
        while !s.is_over() && cells < cap {
            let look = if cells % 2 == 0 {
                looks[0]
            } else {
                RunStyle::default()
            };
            s.push_cell(&cell, Some("x"), look, cap);
            cells += 1;
        }
        assert!(
            s.is_over() && cells <= 2 * (cap / (HTML_SPAN_MIN + 2)) + 2,
            "{cells} cells"
        );
        // Every look, merged or not, around line breaks: the charge is
        // never more than the HTML.
        let mut s = Styled::default();
        for i in 0..200usize {
            let look = RunStyle {
                fg: (i % 3 == 0).then_some(RED),
                bg: (i % 5 == 0).then_some(RED),
                bold: i % 2 == 0,
                italic: i % 7 == 0,
                underline: i % 11 == 0,
                strike: i % 13 == 0,
            };
            if i % 17 == 0 {
                s.push_newline(MAX_RICH_COPY_LEN);
            }
            s.push_cell(
                &cell,
                Some(if i % 4 == 0 { "\u{10ffff}" } else { "a" }),
                look,
                MAX_RICH_COPY_LEN,
            );
        }
        let body = body_html(&s).len();
        assert!(s.weight <= body, "{} > {body}", s.weight);
    }

    #[test]
    fn more_than_4096_colours_are_refused() {
        let cell = Cell::default();
        let colours = |n: u32| {
            let mut s = Styled::default();
            for i in 0..n {
                let look = RunStyle {
                    fg: Some(Rgb::new(byte(i), byte(i >> 8), 0)),
                    ..RunStyle::default()
                };
                s.push_cell(&cell, Some("x"), look, MAX_RICH_COPY_LEN);
            }
            s
        };
        assert!(rtf(&colours(4096), MAX_RICH_COPY_LEN).is_some());
        assert!(rtf(&colours(4097), MAX_RICH_COPY_LEN).is_none());
        assert_eq!(colours(4097).render(MAX_RICH_COPY_LEN), RichCopy::TooLarge);
    }

    /// Every scalar 0x00..=0xFF and a sample of astral, bidi and ZWJ
    /// scalars: the HTML body holds no `<` `>` `&` (but in references)
    /// `"` `'` `\` `{` `}` and nothing above 0x7E; the RTF body no
    /// unescaped `\` `{` `}` and nothing above 0x7E; neither a control.
    #[test]
    fn every_byte_value_and_odd_scalars_are_escaped() {
        let mut text: String = (0u32..=0xff).filter_map(char::from_u32).collect();
        text.push_str("\u{200d}\u{202e}\u{2066}\u{feff}\u{1f600}\u{10ffff}\u{e000}");
        let s = styled(&[(&text, RunStyle::default())]);
        check_html_body(&body_html(&s));
        check_rtf_body(&body_rtf(&s));
    }

    fn check_html_body(body: &str) {
        assert!(body.is_ascii(), "{body}");
        let mut rest = body;
        while let Some(at) = rest.find('&') {
            let tail = &rest[at..];
            let end = tail.find(';').expect("a reference ends");
            let name = &tail[1..end];
            assert!(
                ["amp", "lt", "gt", "quot", "#39", "#92", "#123", "#125"].contains(&name)
                    || name
                        .strip_prefix("#x")
                        .is_some_and(|hex| u32::from_str_radix(hex, 16).is_ok()),
                "{name}"
            );
            rest = &tail[end + 1..];
        }
        for c in ['<', '>', '"', '\'', '\\', '{', '}'] {
            assert!(!body.contains(c), "{c} in {body}");
        }
        assert!(
            body.chars().all(|c| c == '\n' || !c.is_control()),
            "{body:?}"
        );
    }

    fn check_rtf_body(body: &str) {
        assert!(body.is_ascii(), "{body}");
        let bytes = body.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    let next = bytes[i + 1];
                    if matches!(next, b'\\' | b'{' | b'}') {
                        i += 2;
                        continue;
                    }
                    // \uN? or \par
                    let tail = &body[i..];
                    assert!(
                        tail.starts_with("\\u") || tail.starts_with("\\par "),
                        "{tail}"
                    );
                    i += 2;
                }
                b'{' | b'}' => panic!("unescaped brace at {i} in {body}"),
                c => {
                    assert!(!c.is_ascii_control(), "control {c} in {body:?}");
                    i += 1;
                }
            }
        }
    }

    proptest! {
        /// Any text in any look gives bodies within the grammar.
        #[test]
        fn any_text_escapes(text in "\\PC{0,40}", control in proptest::collection::vec(0u32..0x20, 0..4), bold in any::<bool>(), red in any::<bool>()) {
            let mut text = text;
            for c in control {
                text.push(char::from_u32(c).unwrap_or('x'));
            }
            let look = RunStyle { bold, fg: red.then_some(RED), ..RunStyle::default() };
            let s = styled(&[(&text, look)]);
            if s.is_empty() {
                return Ok(());
            }
            let html = body_html(&s);
            let inner = html
                .strip_prefix("<span style=\"color:#ff0000;font-weight:bold\">")
                .or_else(|| html.strip_prefix("<span style=\"color:#ff0000\">"))
                .or_else(|| html.strip_prefix("<span style=\"font-weight:bold\">"))
                .and_then(|rest| rest.strip_suffix("</span>"))
                .unwrap_or(&html);
            check_html_body(inner);
            let rtf = body_rtf(&s);
            let inner = rtf
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
                .map_or(rtf.as_str(), |rest| {
                    rest.trim_start_matches("\\cf1 ").trim_start_matches("\\b ")
                });
            check_rtf_body(inner);
        }
    }
}
