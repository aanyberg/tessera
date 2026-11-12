import AppKit
import os

/// The window's blur radius (#390, ADR 0045): a plain gaussian blur of what
/// is behind a whole window, at `[appearance] blur_radius`, through the
/// private WindowServer call `CGSSetWindowBackgroundBlurRadius`.
///
/// The functions are looked up once, lazily, with `dlopen` and `dlsym` (never
/// linked), so a macOS without them still launches; then every window keeps
/// the material blur of ADR 0043 and one line is logged. The call returns 0
/// for any window number, a bogus one included, so its result is not proof
/// that it took: the guards are the lookup, a window on screen (its number
/// is -1 before, and a call then has no effect) and a radius clamped to
/// 0...``maximum`` (a larger or negative one gives the same blur as 64).
///
/// ``backend`` and ``windowNumber`` are the seam the tests replace: they
/// record the calls and give a window that is never shown a number.
@MainActor
enum WindowBlurRadius {
    /// The largest radius the call distinguishes (ADR 0045).
    static let maximum: UInt32 = 64

    /// Sets the blur radius of a window on screen; returns the call's
    /// result, 0 for submitted.
    struct Backend {
        var setRadius: @MainActor (_ windowNumber: Int, _ radius: UInt32) -> Int32
    }

    /// The system's call, or `nil` if it is missing (or turned off with
    /// `TESSERA_WINDOW_BLUR_RADIUS=off`, to check the fallback by eye).
    static var backend: Backend? = systemBackend()

    /// `window`'s number while it is on screen, `nil` otherwise: only then
    /// does a call take effect.
    static var windowNumber: @MainActor (NSWindow) -> Int? = { window in
        window.isVisible && window.windowNumber > 0 ? window.windowNumber : nil
    }

    /// How many times the fallback was logged: once per process at most.
    private(set) static var fallbackLogCount = 0

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "window")

    /// Sets the blur radius of the window numbered `windowNumber` to
    /// `radius`, clamped to 0...``maximum``. Returns whether the call was
    /// made and reported no failure; `false` (the function is missing or
    /// returned an error) means the window must use the material blur.
    static func set(_ radius: UInt32, windowNumber: Int) -> Bool {
        guard let backend else {
            logFallback("CGSSetWindowBackgroundBlurRadius is not available; using the material blur")
            return false
        }
        let result = backend.setRadius(windowNumber, min(radius, maximum))
        guard result == 0 else {
            logFallback("CGSSetWindowBackgroundBlurRadius failed (\(result)); using the material blur")
            return false
        }
        return true
    }

    /// Logs `message` the first time only. It names the call and its
    /// result, never a window's or session's content (S8).
    private static func logFallback(_ message: String) {
        guard fallbackLogCount == 0 else { return }
        fallbackLogCount += 1
        log.error("\(message, privacy: .public)")
    }

    /// Lets a test see the next fallback logged.
    static func resetLoggedFallback() {
        fallbackLogCount = 0
    }

    /// `CGSConnectionID CGSMainConnectionID(void)`.
    private typealias MainConnectionID = @convention(c) () -> Int32
    /// `CGError CGSSetWindowBackgroundBlurRadius(CGSConnectionID, CGSWindowID, int)`.
    private typealias SetBlurRadius = @convention(c) (Int32, UInt32, Int32) -> Int32

    /// Looks the two functions up in ApplicationServices (which re-exports
    /// them from SkyLight); `nil` if either is missing.
    static func systemBackend() -> Backend? {
        if ProcessInfo.processInfo.environment["TESSERA_WINDOW_BLUR_RADIUS"] == "off" { return nil }
        let path = "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices"
        // Never closed: the functions stay in use for the process's life.
        guard let handle = dlopen(path, RTLD_LAZY),
            let connectionSymbol = dlsym(handle, "CGSMainConnectionID"),
            let setSymbol = dlsym(handle, "CGSSetWindowBackgroundBlurRadius")
        else { return nil }
        // The two casts of looked-up pointers to function types, each to
        // its signature as WindowServer has it (iTerm2, Ghostty, par-term);
        // every argument is a 32-bit integer in a register.
        let connection = unsafeBitCast(connectionSymbol, to: MainConnectionID.self)
        let setRadius = unsafeBitCast(setSymbol, to: SetBlurRadius.self)
        return Backend { windowNumber, radius in
            guard let window = UInt32(exactly: windowNumber) else { return -1 }
            return setRadius(connection(), window, Int32(min(radius, maximum)))
        }
    }
}
