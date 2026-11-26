// The Rust core's test hooks (tessera-ffi's `test-hooks` feature, #594):
// exported only by a core built with that feature, which `just app-test` and
// `just app-test-only` build (`just xcframework` and releases do not), and
// never declared by TesseraCore.h. The app never calls them; the test bundle
// binds them in the host executable (`BUNDLE_LOADER`), so they must survive
// its link. They do, dead stripping included (#650: a Debug host linked with
// `DEAD_CODE_STRIPPING` on still exports both), and a test fails if they
// ever do not.
//
// Weak and looked up when the bundle loads (`-U` in the TesseraTests target's
// `OTHER_LDFLAGS`): a host without them (a core built without the feature,
// or a stripped one) still loads every other test, and
// `TesseraTestsCoreTestHooksLinked()` is false, and a test that needs them
// fails naming the cause (TerminalSurfaceViewTests' `requireCoreTestHooks()`).
//
// The C types are TesseraCore.h's: `struct TesseraApp` is opaque, and the
// result is a `TesseraError` (a C enum, an `int` on Apple platforms), carried
// here as `int32_t` so this header needs no TesseraCore import.

#include <stdbool.h>
#include <stdint.h>

struct TesseraApp;

/// Holds (`hold` true) or releases every font fallback index that finishes
/// building, process-wide, and on holding forgets `app`'s font database, so
/// the next surface attach loads one whose index lands (and calls
/// `on_font_index_ready`) only once released. Release before the test ends.
int32_t tessera_test_hold_font_index(struct TesseraApp *_Nullable app, bool hold) __attribute__((weak_import));

/// Writes to `*out` whether session `session_id`'s surface shows a row shaped
/// with a provisional face; `TESSERA_ERROR_NO_SURFACE` (8) without a surface.
int32_t tessera_test_surface_shows_provisional_faces(
    struct TesseraApp *_Nullable app, uint64_t session_id, bool *_Nullable out) __attribute__((weak_import));

/// Makes every program lookup's stat of `path` take `delay_ms` longer,
/// process-wide; 0 takes the delay away (#722).
int32_t tessera_test_delay_program_lookup(const char *_Nullable path, uint64_t delay_ms) __attribute__((weak_import));

/// Whether the host exports the hooks above.
static inline bool TesseraTestsCoreTestHooksLinked(void) {
    return &tessera_test_hold_font_index != NULL && &tessera_test_surface_shows_provisional_faces != NULL
        && &tessera_test_delay_program_lookup != NULL;
}
