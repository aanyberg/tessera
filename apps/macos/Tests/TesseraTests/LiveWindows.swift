import AppKit
import XCTest
import os

/// Counts the `NSWindow` instances alive in the test host, closed or not
/// (`NSApp.windows` leaves out the closed ones a reference still holds), so
/// a suite that starts leaking windows fails by name: every live window
/// adds load to each later hosted test (#524).
enum LiveWindows {
    /// The most windows that may be alive when a test starts. A full run
    /// peaks at 36 and ends with 7.
    static let bound = 100

    private struct State {
        var live = Set<UInt>()
        var peak = 0
        var reported = false
    }

    private static let state = OSAllocatedUnfairLock(initialState: State())

    static var count: Int { state.withLock { $0.live.count } }
    static var peak: Int { state.withLock { $0.peak } }

    static func contains(_ address: UInt) -> Bool { state.withLock { $0.live.contains(address) } }

    /// What a test run over `bound` reports, or nil within it.
    static func message(live: Int, bound: Int) -> String? {
        live > bound ? "\(live) windows are alive, over the bound of \(bound): earlier tests leak them" : nil
    }

    /// The report for the current count, once per run.
    static func takeReport() -> String? {
        state.withLock { state in
            guard !state.reported, let text = message(live: state.live.count, bound: bound) else { return nil }
            state.reported = true
            return text
        }
    }

    private static func added(_ window: UnsafeRawPointer) {
        let address = UInt(bitPattern: window)
        state.withLock { state in
            state.live.insert(address)
            state.peak = max(state.peak, state.live.count)
        }
    }

    private static func removed(_ window: UnsafeRawPointer) {
        let address = UInt(bitPattern: window)
        _ = state.withLock { $0.live.remove(address) }
    }

    /// Hooks `NSWindow`'s initialisers and `dealloc`; once, before the first
    /// test. Raw pointers throughout: nothing here may retain the window.
    static func install() {
        typealias Init = @convention(c) (UnsafeRawPointer, Selector, NSRect, UInt, UInt, ObjCBool) -> UnsafeRawPointer?
        typealias InitScreen =
            @convention(c) (UnsafeRawPointer, Selector, NSRect, UInt, UInt, ObjCBool, UnsafeRawPointer?) ->
            UnsafeRawPointer?
        typealias Dealloc = @convention(c) (UnsafeRawPointer, Selector) -> Void

        func method(_ name: String) -> (Method, Selector)? {
            let selector = NSSelectorFromString(name)
            return class_getInstanceMethod(NSWindow.self, selector).map { ($0, selector) }
        }
        if let (method, selector) = method("initWithContentRect:styleMask:backing:defer:") {
            let original = unsafeBitCast(method_getImplementation(method), to: Init.self)
            let hook: @convention(block) (UnsafeRawPointer, NSRect, UInt, UInt, ObjCBool) -> UnsafeRawPointer? = {
                let window = original($0, selector, $1, $2, $3, $4)
                if let window { added(window) }
                return window
            }
            method_setImplementation(method, imp_implementationWithBlock(hook))
        }
        if let (method, selector) = method("initWithContentRect:styleMask:backing:defer:screen:") {
            let original = unsafeBitCast(method_getImplementation(method), to: InitScreen.self)
            let hook: @convention(block) (UnsafeRawPointer, NSRect, UInt, UInt, ObjCBool, UnsafeRawPointer?) ->
                UnsafeRawPointer? = {
                    let window = original($0, selector, $1, $2, $3, $4, $5)
                    if let window { added(window) }
                    return window
                }
            method_setImplementation(method, imp_implementationWithBlock(hook))
        }
        if let (method, selector) = method("dealloc") {
            let original = unsafeBitCast(method_getImplementation(method), to: Dealloc.self)
            let hook: @convention(block) (UnsafeRawPointer) -> Void = {
                removed($0)
                original($0, selector)
            }
            method_setImplementation(method, imp_implementationWithBlock(hook))
        }
    }
}

@MainActor
final class LiveWindowsTests: XCTestCase {
    func testAWindowCountsFromItsCreationUntilItIsFreedEvenAfterItCloses() {
        var address: UInt = 0
        autoreleasepool {
            let window = NSWindow(contentRect: .zero, styleMask: [], backing: .buffered, defer: true)
            window.isReleasedWhenClosed = false
            address = UInt(bitPattern: Unmanaged.passUnretained(window).toOpaque())
            XCTAssertTrue(LiveWindows.contains(address))
            window.close()
            XCTAssertTrue(LiveWindows.contains(address), "closed but still referenced: alive")
        }
        XCTAssertFalse(LiveWindows.contains(address))
    }

    func testTheReportNamesTheCountOnlyOverTheBound() throws {
        XCTAssertNil(LiveWindows.message(live: 10, bound: 10))
        let text = try XCTUnwrap(LiveWindows.message(live: 11, bound: 10))
        XCTAssertTrue(text.contains("11") && text.contains("10"), text)
    }
}
