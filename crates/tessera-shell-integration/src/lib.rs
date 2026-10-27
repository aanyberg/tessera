//! Shell integration protocol.
//!
//! Decodes the OSC sequences emitted by the scripts in `shell-integration/`
//! (`FinalTerm` OSC 133 prompt marks, OSC 7 working directory, selected `iTerm2`
//! OSC 1337 extensions) into [`tessera_grid::Mark`] values and session
//! metadata events. Prompt navigation, command timing and the future agent
//! usage tracker all build on these marks.

#![forbid(unsafe_code)]

use tessera_grid::Mark;

/// Parses an OSC 133 payload (`A`, `B`, `C`, `D;<code>`) into a mark.
pub fn parse_osc133(payload: &[u8]) -> Option<Mark> {
    match payload {
        [b'A', ..] => Some(Mark::PromptStart),
        [b'B', ..] => Some(Mark::CommandStart),
        [b'C', ..] => Some(Mark::OutputStart),
        [b'D'] => Some(Mark::CommandEnd { exit_code: None }),
        [b'D', b';', code @ ..] => {
            let exit_code = std::str::from_utf8(code).ok()?.parse().ok();
            Some(Mark::CommandEnd { exit_code })
        }
        _ => None,
    }
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
}
