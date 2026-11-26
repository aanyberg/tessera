//! What a child inherits of this process's environment (#737,
//! docs/security.md, ADR 0005).
//!
//! The app's environment is whatever launched it: launchd, `open`, a
//! shell, Xcode or a test runner. Most of it is the user's and reaches the
//! child unchanged, so a variable set with `launchctl setenv` or in the
//! shell that ran `open` still reaches the shell Tessera starts. Three
//! kinds do not: Tessera's own variables, which configure this process only
//! (`TESSERA_CONFIG_DIR`, test hooks); dyld's `DYLD_*` controls and their
//! XPC hand-over `__XPC_DYLD_*`, which would load code into every
//! unrestricted program the shell runs; and what an Xcode scheme launch or
//! `XCTest` adds for the app (`XCTest*`, `XCInjectBundle*`, `__XCODE_*`).
//! [`CHILD_ENV_FILTER`] lists them; [`inherits`] applies it.
//!
//! The filter applies to inherited variables only. What Tessera sets for
//! the child on purpose (the identification variables of
//! [`SpawnOptions::injected_env`](crate::SpawnOptions::injected_env), the
//! shell integration's, and any other entry of
//! [`SpawnOptions::env`](crate::SpawnOptions::env)) is applied after it
//! and always reaches the child.

use std::ffi::OsStr;

/// What [`CHILD_ENV_FILTER`] does with an inherited variable a rule
/// matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvAction {
    /// The child inherits it.
    Keep,
    /// The child does not inherit it.
    Strip,
}

/// The variable names an [`EnvRule`] matches. Names compare byte for
/// byte, case included, as `execve(2)` and dyld compare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvName {
    /// Exactly this name.
    Exact(&'static str),
    /// Every name that starts with this.
    Prefix(&'static str),
}

impl EnvName {
    /// Whether `key` is one of the names this matches.
    #[must_use]
    pub fn matches(self, key: &OsStr) -> bool {
        let key = key.as_encoded_bytes();
        match self {
            Self::Exact(name) => key == name.as_bytes(),
            Self::Prefix(prefix) => key.starts_with(prefix.as_bytes()),
        }
    }
}

/// One entry of [`CHILD_ENV_FILTER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnvRule {
    /// The names it matches.
    pub name: EnvName,
    /// What happens to an inherited variable it matches.
    pub action: EnvAction,
    /// Why.
    pub reason: &'static str,
}

/// The rules for what a child inherits of this process's environment,
/// checked in order: the first rule that matches a variable's name decides
/// ([`inherits`]), and a variable no rule matches is inherited.
///
/// Every spawn applies it: [`NativePty::spawn`](crate::NativePty::spawn),
/// the built-in start [`NativePty::spawn_builtin`](crate::NativePty::spawn_builtin)
/// (also the fallback after a profile's start fails, #728), and through
/// them every session the app starts.
pub const CHILD_ENV_FILTER: &[EnvRule] = &[
    EnvRule {
        name: EnvName::Exact("TESSERA_SHELL_INTEGRATION"),
        action: EnvAction::Keep,
        reason: "the shell integration's switch (ADR 0021): a user's own \
                 `TESSERA_SHELL_INTEGRATION=0` turns the injection off, and \
                 `enabled` turns on scripts sourced by hand",
    },
    EnvRule {
        name: EnvName::Prefix("TESSERA_"),
        action: EnvAction::Strip,
        reason: "Tessera's own settings and test hooks (`TESSERA_CONFIG_DIR`, \
                 `TESSERA_WINDOW_BLUR_RADIUS`, `TESSERA_SKIP_GPU_TESTS`, ...) \
                 configure this process only, and a Tessera session's \
                 variables inherited from the shell that started the app \
                 would describe that session; what the child should have \
                 Tessera sets for it anew (`TESSERA_SESSION_ID`, the shell \
                 integration's hand-over variables)",
    },
    EnvRule {
        name: EnvName::Prefix("DYLD_"),
        action: EnvAction::Strip,
        reason: "dyld's loader controls (`DYLD_INSERT_LIBRARIES`, \
                 `DYLD_LIBRARY_PATH`, ...), set for the app by Xcode, a test \
                 runner or whatever launched it, would load code into every \
                 program the shell runs that dyld does not restrict \
                 (Homebrew shells and tools)",
    },
    EnvRule {
        name: EnvName::Prefix("__XPC_DYLD_"),
        action: EnvAction::Strip,
        reason: "the same loader controls handed on through launchd and XPC \
                 (`__XPC_DYLD_LIBRARY_PATH`, set by an Xcode scheme launch), \
                 which become `DYLD_*` in the processes they reach",
    },
    EnvRule {
        name: EnvName::Prefix("XCTest"),
        action: EnvAction::Strip,
        reason: "XCTest's hand-over to the app it hosts tests in \
                 (`XCTestConfigurationFilePath`, which the app reads only to \
                 know that it does, `AppDelegate.isTestHost`; \
                 `XCTestBundleInjectPath`, `XCTestSessionIdentifier`): a test \
                 run's, not the shell's",
    },
    EnvRule {
        name: EnvName::Prefix("XCInjectBundle"),
        action: EnvAction::Strip,
        reason: "Xcode's test-bundle injection (`XCInjectBundleInto`), with \
                 `DYLD_INSERT_LIBRARIES` the way it loads tests into the app; \
                 a child that inherits it is told to load them too",
    },
    EnvRule {
        name: EnvName::Prefix("__XCODE_"),
        action: EnvAction::Strip,
        reason: "set by an Xcode scheme launch for the app it runs \
                 (`__XCODE_BUILT_PRODUCTS_DIR_PATHS`), describing that build, \
                 not the user's shell",
    },
];

/// Whether a child inherits this process's variable `key`: the action of
/// the first rule of [`CHILD_ENV_FILTER`] that matches it, or
/// [`EnvAction::Keep`] when none does.
#[must_use]
pub fn inherits(key: impl AsRef<OsStr>) -> bool {
    let key = key.as_ref();
    CHILD_ENV_FILTER
        .iter()
        .find(|rule| rule.name.matches(key))
        .is_none_or(|rule| rule.action == EnvAction::Keep)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    #[test]
    fn internal_and_dyld_variables_are_not_inherited() {
        for key in [
            "TESSERA_CONFIG_DIR",
            "TESSERA_SESSION_ID",
            "TESSERA_WINDOW_BLUR_RADIUS",
            "TESSERA_SKIP_GPU_TESTS",
            "TESSERA_ORIG_ZDOTDIR",
            "TESSERA_BASH_RCFILE",
            // The kept switch's exact name only.
            "TESSERA_SHELL_INTEGRATIONS",
            "TESSERA_",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "DYLD_FRAMEWORK_PATH",
            "DYLD_FALLBACK_LIBRARY_PATH",
            "DYLD_PRINT_LIBRARIES",
            "DYLD_",
            "__XPC_DYLD_LIBRARY_PATH",
            "__XPC_DYLD_FRAMEWORK_PATH",
            "__XPC_DYLD_",
            "XCTestConfigurationFilePath",
            "XCTestBundleInjectPath",
            "XCTestSessionIdentifier",
            "XCTest",
            "XCInjectBundleInto",
            "XCInjectBundle",
            "__XCODE_BUILT_PRODUCTS_DIR_PATHS",
            "__XCODE_",
        ] {
            assert!(!inherits(key), "{key} is inherited");
        }
        // A name that is not UTF-8 is matched on its bytes.
        assert!(!inherits(OsStr::from_bytes(b"DYLD_\xff")));
        assert!(!inherits(OsStr::from_bytes(b"TESSERA_\xff")));
    }

    #[test]
    fn everything_else_is_inherited() {
        for key in [
            "PATH",
            "HOME",
            "LANG",
            "SHELL",
            "TERM",
            "TERM_PROGRAM",
            "COLORTERM",
            "TESSERA_SHELL_INTEGRATION",
            // Names compare case and all, and only from their start.
            "tessera_config_dir",
            "dyld_insert_libraries",
            "TESSERA",
            "DYLD",
            "MY_TESSERA_CONFIG_DIR",
            "X_DYLD_INSERT_LIBRARIES",
            "XCTEST_CONFIGURATION",
            "xctestconfigurationfilepath",
            "MY_XCTestConfigurationFilePath",
            "XCInject",
            "__XCODE",
            "__XPC_DYLD",
            "__XPC_SERVICE_NAME",
            "",
        ] {
            assert!(inherits(key), "{key} is not inherited");
        }
        assert!(inherits(OsStr::from_bytes(b"\xffDYLD_")));
    }

    #[test]
    fn every_rule_has_a_reason_and_decides_some_name() {
        for (index, rule) in CHILD_ENV_FILTER.iter().enumerate() {
            assert!(!rule.reason.trim().is_empty(), "rule {index}");
            let (EnvName::Exact(name) | EnvName::Prefix(name)) = rule.name;
            assert!(!name.is_empty(), "rule {index}");
            // No earlier rule shadows it.
            assert_eq!(
                CHILD_ENV_FILTER
                    .iter()
                    .position(|earlier| earlier.name.matches(OsStr::new(name))),
                Some(index),
                "rule {index} ({name}) is shadowed"
            );
        }
    }
}
