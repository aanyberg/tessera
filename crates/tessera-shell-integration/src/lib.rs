//! Shell integration.
//!
//! Tessera's scripts for zsh, bash and fish (in the repository's
//! `shell-integration/` directory) send `FinalTerm` OSC 133 prompt marks, OSC 7
//! working-directory reports and a one-time OSC 1337 `ShellIntegrationVersion`
//! handshake. This crate holds them ([`scripts`]), says how a shell Tessera
//! starts is made to load them ([`inject`]), and decodes what they send:
//! [`parse_osc133`] into [`tessera_grid::Mark`] values, [`parse_osc7`] into a
//! [`Cwd`] and [`parse_osc1337`] into a [`ShellIntegrationVersion`]. Prompt
//! navigation, command timing and the future agent usage tracker build on
//! the marks. The terminal applies OSC 133 itself (through
//! [`tessera_grid::Mark::from_osc133`], which [`parse_osc133`] forwards to)
//! and hands OSC 7 and the handshake up raw for `tessera-app-core` to decode
//! here. See `docs/adr/0021-shell-integration-injection.md` and
//! `docs/adr/0022-osc-title-clipboard-and-shell-integration-routing.md`.

#![forbid(unsafe_code)]

pub mod inject;
pub mod osc7;
pub mod scripts;

pub use inject::{Injection, Launch, Shell};
pub use osc7::{Cwd, parse_osc7};
use tessera_grid::Mark;

/// Parses an OSC 133 payload (`A`, `B`, `C`, `D;<code>`, the bytes after
/// `133;`) into a mark: [`Mark::from_osc133`], which the terminal applies
/// itself (#9; `tessera-term` does not depend on this crate).
///
/// Strict: the letter must end the payload or be followed by `;`, so
/// `Afoo` is not a mark. Options after the letter are ignored, except
/// that `A;k=s`, `A;k=c` and `A;k=r` (a secondary, continuation or right
/// prompt) give `None`, and `D`'s first option is its exit code (`None`
/// when it is missing or not a number).
#[must_use]
pub fn parse_osc133(payload: &[u8]) -> Option<Mark> {
    Mark::from_osc133(payload)
}

/// The handshake a script sends once at startup:
/// `OSC 1337 ; ShellIntegrationVersion=<version> ; shell=<shell> ST`.
///
/// The form is `iTerm2`'s, but the number is Tessera's own
/// ([`scripts::PROTOCOL_VERSION`]); `iTerm2` numbers its scripts separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIntegrationVersion {
    /// The protocol version the script speaks.
    pub version: u32,
    /// The shell's name (`zsh`, `bash`, `fish`, or another shell's).
    pub shell: String,
}

/// Longest shell name [`parse_osc1337`] accepts, in bytes.
pub const MAX_SHELL_NAME_LEN: usize = 32;

/// Parses an OSC 1337 payload (the bytes after `1337;`) that is a
/// [`ShellIntegrationVersion`] handshake.
///
/// Expects `ShellIntegrationVersion=<n>` (decimal, fitting a `u32`) first and
/// a `shell=<name>` parameter after it (letters, digits, `.`, `_`, `-`; at
/// most [`MAX_SHELL_NAME_LEN`] bytes); other parameters are ignored. Returns
/// `None` for any other OSC 1337 key (`SetUserVar`, `File`, ...) and for a
/// handshake without a valid shell name.
#[must_use]
pub fn parse_osc1337(payload: &[u8]) -> Option<ShellIntegrationVersion> {
    let mut params = payload.split(|&b| b == b';');
    let version = params.next()?.strip_prefix(b"ShellIntegrationVersion=")?;
    if version.is_empty() || !version.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let version = std::str::from_utf8(version).ok()?.parse().ok()?;
    let shell = params.find_map(|param| param.strip_prefix(b"shell="))?;
    let valid = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    if shell.is_empty() || shell.len() > MAX_SHELL_NAME_LEN || !shell.iter().all(valid) {
        return None;
    }
    Some(ShellIntegrationVersion {
        version,
        shell: String::from_utf8(shell.to_vec()).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_end_with_code() {
        assert_eq!(
            parse_osc133(b"D;130"),
            Some(Mark::CommandEnd {
                exit_code: Some(130)
            })
        );
        assert_eq!(parse_osc133(b"A"), Some(Mark::PromptStart));
        assert_eq!(parse_osc133(b"Z"), None);
    }

    #[test]
    fn a_command_end_with_a_bad_code_is_still_a_command_end() {
        for payload in [&b"D;\xff"[..], b"D;abc", b"D;", b"D;99999999999"] {
            assert_eq!(
                parse_osc133(payload),
                Some(Mark::CommandEnd { exit_code: None }),
                "{payload:?}"
            );
        }
    }

    #[test]
    fn command_end_ignores_parameters_after_the_code() {
        assert_eq!(
            parse_osc133(b"D;1;aid=42"),
            Some(Mark::CommandEnd { exit_code: Some(1) })
        );
    }
}
