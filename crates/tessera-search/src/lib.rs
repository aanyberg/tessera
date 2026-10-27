//! Search over terminal content.
//!
//! Runs regex and literal queries across the visible grid and scrollback,
//! incrementally as new output arrives, and yields match ranges the renderer
//! highlights and the UI steps through.

#![forbid(unsafe_code)]

/// A compiled search query.
#[derive(Debug, Clone)]
pub struct Query {
    regex: regex::Regex,
}

impl Query {
    /// Compiles a regex query. Pass an escaped string for literal search.
    pub fn new(pattern: &str, case_sensitive: bool) -> Result<Self, regex::Error> {
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()?;
        Ok(Self { regex })
    }

    /// Byte ranges of matches within a single line of text.
    pub fn find_in_line<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.regex.find_iter(text).map(|m| (m.start(), m.end()))
    }
}
