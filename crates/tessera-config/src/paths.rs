//! Where configuration lives on disk.

use std::path::{Path, PathBuf};

/// File name of the main configuration document.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Application support directory name under `~/Library/Application Support`.
pub const APP_SUPPORT_DIR_NAME: &str = "Tessera";

/// Directory under the root that holds the shell integration scripts.
pub const SHELL_INTEGRATION_DIR_NAME: &str = "shell-integration";

/// The set of configuration paths rooted at one directory.
///
/// The root is injectable so tests and alternative frontends can point the
/// store at a temporary or custom directory. Production code uses
/// [`ConfigPaths::for_current_user`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    root: PathBuf,
}

impl ConfigPaths {
    /// Paths rooted at `root`. The directory need not exist yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Paths under the current user's application support directory:
    /// `~/Library/Application Support/Tessera`.
    ///
    /// Returns `None` when the home directory cannot be determined.
    pub fn for_current_user() -> Option<Self> {
        let home = std::env::home_dir()?;
        Some(Self::new(
            home.join("Library")
                .join("Application Support")
                .join(APP_SUPPORT_DIR_NAME),
        ))
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.root.join(CONFIG_FILE_NAME)
    }

    /// Directory of `profiles/*.toml`.
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// Directory of `themes/*.toml`.
    pub fn themes_dir(&self) -> PathBuf {
        self.root.join("themes")
    }

    /// Directory the shell integration scripts are written to, one
    /// subdirectory per scripts version (see `tessera-shell-integration`).
    pub fn shell_integration_dir(&self) -> PathBuf {
        self.root.join(SHELL_INTEGRATION_DIR_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_root() {
        let p = ConfigPaths::new("/tmp/t");
        assert_eq!(p.config_file(), Path::new("/tmp/t/config.toml"));
        assert_eq!(p.profiles_dir(), Path::new("/tmp/t/profiles"));
        assert_eq!(p.themes_dir(), Path::new("/tmp/t/themes"));
        assert_eq!(
            p.shell_integration_dir(),
            Path::new("/tmp/t/shell-integration")
        );
    }

    #[test]
    fn user_paths_end_in_app_support() {
        if let Some(p) = ConfigPaths::for_current_user() {
            assert!(p.root().ends_with("Library/Application Support/Tessera"));
        }
    }
}
