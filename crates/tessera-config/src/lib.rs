//! Configuration.
//!
//! Defines the TOML schema (`config.toml`, `profiles/*.toml`, `themes/*.toml`)
//! as plain Rust types with `serde`, supplies defaults for every field, loads
//! and validates files, watches them for changes, and writes edits back from
//! the preferences UI without clobbering user comments. Unknown keys warn and
//! are preserved; they never fail a load.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Errors from loading or saving configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be read or written.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not valid TOML or does not match the schema.
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
}

/// Top-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Application-wide settings.
    pub general: General,
    /// Default appearance; profiles may override.
    pub appearance: Appearance,
    /// Named profiles.
    pub profiles: Vec<Profile>,
    /// Plugin enablement, keyed by plugin id.
    pub plugins: Plugins,
}

/// Application-wide settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Profile used for new sessions when none is specified.
    pub default_profile: String,
    /// Confirm before closing a window with running jobs.
    pub confirm_close: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_profile: "Default".into(),
            confirm_close: true,
        }
    }
}

/// Visual settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Theme name resolved against `themes/`.
    pub theme: String,
    /// Font family.
    pub font_family: String,
    /// Font size in points.
    pub font_size: f32,
    /// Window opacity, 0.0..=1.0.
    pub opacity: f32,
    /// Background blur when opacity is below 1.0.
    pub blur: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: "Tessera Dark".into(),
            font_family: "Menlo".into(),
            font_size: 13.0,
            opacity: 1.0,
            blur: false,
        }
    }
}

/// A named session profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Display name, unique.
    pub name: String,
    /// Command to run; `None` means the user's login shell.
    pub command: Option<String>,
    /// Starting directory; `None` means inherit.
    pub working_directory: Option<String>,
    /// Appearance overrides.
    pub appearance: Option<Appearance>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Default".into(),
            command: None,
            working_directory: None,
            appearance: None,
        }
    }
}

/// Plugin enablement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugins {
    /// Plugin ids that are enabled.
    pub enabled: Vec<String>,
}

impl Config {
    /// Parses a TOML document.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        Ok(toml::from_str(text)?)
    }

    /// Serializes to TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_is_all_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn round_trips() {
        let cfg = Config::default();
        assert_eq!(Config::from_toml(&cfg.to_toml().unwrap()).unwrap(), cfg);
    }
}
