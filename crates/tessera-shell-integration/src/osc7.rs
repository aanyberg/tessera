//! OSC 7: the shell reports its working directory.
//!
//! The payload (the bytes after `ESC ] 7 ;`) is a URL:
//! `file://<host><percent-encoded path>`, as Terminal.app, `iTerm2`, `WezTerm`
//! and Tessera's own scripts send it, or kitty's `kitty-shell-cwd://<host><raw
//! path>`. [`parse_osc7`] decodes either into a [`Cwd`]. It judges the path
//! (it must be absolute, decode to UTF-8 without control characters and have
//! no `..` segment) but not the host: deciding whether a host is this machine
//! is the terminal's job (#9), so the host comes back as sent.

/// Longest OSC 7 payload [`parse_osc7`] accepts, in bytes.
///
/// macOS paths are at most `PATH_MAX` (1024) bytes, three times that when
/// every byte is percent-encoded, plus the scheme and a host name; a longer
/// payload is not a working directory report.
pub const MAX_OSC7_LEN: usize = 4096;

/// A working directory reported with OSC 7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cwd {
    /// The host part of the URL, without any `user@`. Empty for
    /// `file:///path`, which means this machine (RFC 8089). Not checked
    /// against the local host name.
    pub host: String,
    /// The decoded absolute path.
    pub path: String,
}

/// Decodes an OSC 7 payload (the bytes after `7;`).
///
/// Accepted forms (scheme names in any case):
///
/// - `file://host/path` and `file:///path` (empty host). The path is
///   percent-decoded (`%20`, multi-byte UTF-8 such as `%C3%A9`); a `?query`
///   or `#fragment` after it is ignored, and so is a `user@` before the host
///   (`iTerm2` sends `file://user@host/path?machineID=...`).
/// - `kitty-shell-cwd://host/path`: kitty's form, whose path is raw (never
///   percent-encoded, so `%`, `?` and `#` are part of it).
///
/// Returns `None` for anything else, and for:
///
/// - a payload longer than [`MAX_OSC7_LEN`];
/// - a path that is empty or not absolute;
/// - a malformed escape (`%` not followed by two hex digits) or an encoded
///   `/` (`%2F`), which would change the path's segments;
/// - a path or host with a control character (NUL, newline, ...) or that is
///   not UTF-8 once decoded;
/// - a path with a `..` segment once decoded.
#[must_use]
pub fn parse_osc7(payload: &[u8]) -> Option<Cwd> {
    if payload.len() > MAX_OSC7_LEN {
        return None;
    }
    let (rest, encoded) = if let Some(rest) = strip_prefix_ignore_case(payload, b"file://") {
        (rest, true)
    } else {
        (
            strip_prefix_ignore_case(payload, b"kitty-shell-cwd://")?,
            false,
        )
    };
    // The authority runs to the first `/`; no `/` means no path.
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (authority, path) = rest.split_at(slash);
    let host = match authority.iter().rposition(|&b| b == b'@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    let host = text(host.to_vec())?;
    let path = if encoded {
        let end = path
            .iter()
            .position(|&b| b == b'?' || b == b'#')
            .unwrap_or(path.len());
        percent_decode(&path[..end])?
    } else {
        path.to_vec()
    };
    let path = text(path)?;
    if path.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(Cwd { host, path })
}

fn strip_prefix_ignore_case<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let head = bytes.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &bytes[prefix.len()..])
}

/// `bytes` as a string, if it is UTF-8 without control characters.
fn text(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

/// Decodes `%XX` escapes; `None` for a malformed escape or an encoded `/`.
fn percent_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let [high, low, ..] = *tail else {
                return None;
            };
            let decoded = (hex(high)? << 4) | hex(low)?;
            if decoded == b'/' {
                return None;
            }
            out.push(decoded);
            rest = &tail[2..];
        } else {
            out.push(byte);
            rest = tail;
        }
    }
    Some(out)
}

fn hex(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what parse_osc7 returns"
    )]
    fn cwd(host: &str, path: &str) -> Option<Cwd> {
        Some(Cwd {
            host: host.into(),
            path: path.into(),
        })
    }

    #[test]
    fn decodes_the_terminal_app_form() {
        assert_eq!(parse_osc7(b"file:///tmp"), cwd("", "/tmp"));
        assert_eq!(parse_osc7(b"file://Mac.home/tmp"), cwd("Mac.home", "/tmp"));
        assert_eq!(parse_osc7(b"file://h/tmp/a%20b"), cwd("h", "/tmp/a b"));
    }

    #[test]
    fn hex_digits_in_either_case() {
        assert_eq!(parse_osc7(b"file://h/caf%c3%a9"), cwd("h", "/café"));
    }

    #[test]
    fn a_truncated_escape_at_the_end_is_rejected() {
        assert_eq!(parse_osc7(b"file://h/tmp%4"), None);
        assert_eq!(parse_osc7(b"file://h/tmp%"), None);
    }
}
