//! Status codes returned by every fallible export.

use tessera_app_core::{Error as CoreError, PtyError};

/// Result of a `tessera_*` call. `TESSERA_ERROR_OK` (zero) is success;
/// every other value names the reason for failure.
///
/// A call that fails leaves its out-parameters untouched unless its
/// documentation says otherwise. Details of a failure are logged by the
/// core; the code is all the caller receives.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum Error {
    /// The call succeeded.
    Ok = 0,
    /// A pointer argument that must not be `NULL` was `NULL`.
    NullPointer = 1,
    /// An argument was out of range or malformed (for example a size with
    /// zero rows or columns, an empty program name or a buffer too small).
    InvalidArgument = 2,
    /// The session id does not name a live session of this application.
    SessionNotFound = 3,
    /// The session could not be started: no PTY was available, the program
    /// does not exist or is not executable, its arguments and environment
    /// are too large to start it with, or a thread could not be created.
    SpawnFailed = 4,
    /// An operating-system I/O call failed, for example writing to a
    /// session whose child has exited.
    Io = 5,
    /// The core panicked. The panic was caught and logged; the application
    /// handle stays valid and usable.
    Panic = 6,
    /// The renderer failed: it could not be created (no GPU can present to
    /// the layer, or no usable font), the layer's swapchain was lost, or
    /// drawing a frame failed.
    Gpu = 7,
    /// The session has no surface attached (`tessera_surface_attach`).
    NoSurface = 8,
}

impl From<&CoreError> for Error {
    fn from(error: &CoreError) -> Self {
        match error {
            CoreError::SessionNotFound(_) => Self::SessionNotFound,
            CoreError::Pty(pty) => match pty {
                PtyError::Open(_) | PtyError::Spawn { .. } | PtyError::ArgumentsTooLong { .. } => {
                    Self::SpawnFailed
                }
                PtyError::InvalidSize(_) => Self::InvalidArgument,
                PtyError::Resize(_) | PtyError::Io(_) => Self::Io,
            },
            CoreError::ReaderThread { .. } => Self::SpawnFailed,
            CoreError::Write { .. } => Self::Io,
            CoreError::Gpu(_) => Self::Gpu,
            CoreError::NoView(_) => Self::NoSurface,
            CoreError::ViewAttached(_) => Self::InvalidArgument,
        }
    }
}

/// Logs `error` and converts it to its status code.
pub(crate) fn report(function: &'static str, error: &CoreError) -> Error {
    let code = Error::from(error);
    if matches!(code, Error::SessionNotFound | Error::NoSurface) {
        tracing::debug!(function, %error, "ffi call failed");
    } else {
        tracing::warn!(function, %error, "ffi call failed");
    }
    code
}

/// Maps a core result to a status code, logging failures.
pub(crate) fn status(function: &'static str, result: Result<(), CoreError>) -> Error {
    match result {
        Ok(()) => Error::Ok,
        Err(error) => report(function, &error),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use tessera_app_core::view::RenderError;
    use tessera_app_core::{PtySize, SessionId};

    use super::*;

    #[test]
    fn codes_are_stable() {
        let codes = [
            Error::Ok,
            Error::NullPointer,
            Error::InvalidArgument,
            Error::SessionNotFound,
            Error::SpawnFailed,
            Error::Io,
            Error::Panic,
            Error::Gpu,
            Error::NoSurface,
        ];
        let values: Vec<i32> = codes.iter().map(|&code| code as i32).collect();
        assert_eq!(values, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn core_errors_map_to_codes() {
        let id = SessionId::from_raw(1);
        let cases = [
            (CoreError::SessionNotFound(id), Error::SessionNotFound),
            (
                CoreError::Pty(PtyError::Open("x".into())),
                Error::SpawnFailed,
            ),
            (
                CoreError::Pty(PtyError::Spawn {
                    program: "x".into(),
                    reason: "y".into(),
                }),
                Error::SpawnFailed,
            ),
            (
                CoreError::Pty(PtyError::ArgumentsTooLong { bytes: 2, max: 1 }),
                Error::SpawnFailed,
            ),
            (
                CoreError::Pty(PtyError::InvalidSize(PtySize::new(0, 1))),
                Error::InvalidArgument,
            ),
            (CoreError::Pty(PtyError::Resize("x".into())), Error::Io),
            (
                CoreError::Pty(PtyError::Io(io::Error::other("x"))),
                Error::Io,
            ),
            (
                CoreError::ReaderThread {
                    id,
                    source: io::Error::other("x"),
                },
                Error::SpawnFailed,
            ),
            (
                CoreError::Write {
                    id,
                    source: io::Error::other("x"),
                },
                Error::Io,
            ),
            (CoreError::Gpu(RenderError::SurfaceLost), Error::Gpu),
            (CoreError::Gpu(RenderError::NoFont("x".into())), Error::Gpu),
            (CoreError::NoView(id), Error::NoSurface),
            (CoreError::ViewAttached(id), Error::InvalidArgument),
        ];
        for (error, code) in cases {
            assert_eq!(Error::from(&error), code, "{error:?}");
        }
    }
}
