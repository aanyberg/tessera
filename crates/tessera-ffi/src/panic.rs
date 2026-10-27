//! Stops panics at the C boundary.
//!
//! Unwinding into C is undefined behaviour, so every export runs its body
//! through [`ffi_guard!`], which catches a panic, logs it and turns it into
//! [`Error::Panic`]. The core recovers poisoned locks, so the application
//! stays usable after a caught panic.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use crate::error::Error;

/// Runs an export's body, converting a panic into [`Error::Panic`].
///
/// `ffi_guard!("name", { ... })` evaluates a block producing an [`Error`];
/// `ffi_guard!(void "name", { ... })` guards a body without a result, for
/// exports that return nothing.
macro_rules! ffi_guard {
    ($function:literal, $body:block) => {
        $crate::panic::guard($function, || -> $crate::error::Error { $body })
    };
    (void $function:literal, $body:block) => {
        $crate::panic::guard_void($function, || $body)
    };
}

pub(crate) use ffi_guard;

/// Runs `body`, mapping a panic to [`Error::Panic`].
pub(crate) fn guard(function: &'static str, body: impl FnOnce() -> Error) -> Error {
    // The closure may capture raw pointers and references into the core;
    // the core restores its invariants after a panic (poisoned locks are
    // recovered), so observing it afterwards is sound.
    panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|payload| {
        log_panic(function, payload.as_ref());
        Error::Panic
    })
}

/// Runs `body`, swallowing (and logging) a panic.
pub(crate) fn guard_void(function: &'static str, body: impl FnOnce()) {
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(body)) {
        log_panic(function, payload.as_ref());
    }
}

fn log_panic(function: &'static str, payload: &(dyn Any + Send)) {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    tracing::error!(function, message, "panic caught at the ffi boundary");
}

/// Panics inside the core while it holds a lock, through the same guard
/// every export uses. Returns [`Error::Panic`]; `app` stays usable.
///
/// Test-only (feature `test-hooks`), not exported to C and absent from the
/// header.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle from `tessera_app_new`.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub unsafe extern "C" fn tessera_test_panic(app: *mut crate::app::App) -> Error {
    ffi_guard!("tessera_test_panic", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { crate::app::App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        app.core.panic_holding_session_lock();
        Error::Ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_passes_results_through() {
        assert_eq!(guard("t", || Error::Io), Error::Io);
    }

    #[test]
    fn guard_maps_panics() {
        assert_eq!(guard("t", || panic!("boom")), Error::Panic);
        assert_eq!(guard("t", || std::panic::panic_any(7_u32)), Error::Panic);
        guard_void("t", || panic!("{}", String::from("owned")));
    }
}
