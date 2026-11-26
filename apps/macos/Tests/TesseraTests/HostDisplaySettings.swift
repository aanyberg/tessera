import AppKit
import XCTest

@testable import Tessera

/// The test bundle's principal class (`NSPrincipalClass`, project.yml):
/// before the first test it pins the app's reads of the machine's
/// Accessibility display settings (Reduce Motion, Reduce Transparency,
/// Increase Contrast) to off, so no test depends on the machine it runs on
/// (a CI runner may report Reduce Motion on, and a scroll bar would then
/// never fade). A test that needs one on sets the app delegate's read
/// itself and calls ``pin(_:)`` in its tearDown. It also holds the run to
/// ``LiveWindows/bound`` and turns window animations off
/// (``turnOffWindowAnimations()``), and has every core of the host share one
/// scan of the installed fonts and one font fallback index
/// (``shareTheFontIndex()``).
@objc(TesseraTestsHostDisplaySettings)
final class HostDisplaySettings: NSObject, XCTestObservation {
    override init() {
        super.init()
        TerminationSignalChild.mark("the test bundle loaded")
        XCTestObservationCenter.shared.addTestObserver(self)
    }

    func testBundleWillStart(_ testBundle: Bundle) {
        MainActor.assumeIsolated {
            Self.turnOffWindowAnimations()
            Self.shareTheFontIndex()
            // A child of TerminationSignalTests or FullScreenToggleTests
            // runs no test.
            FullScreenToggleChild.runIfRequested()
            TerminationSignalChild.runIfRequested()
            // Files of test suites that an earlier run's killed host left.
            TestDefaultsSuite.sweep()
            LiveWindows.install()
            if let appDelegate = NSApp.delegate as? AppDelegate {
                Self.pin(appDelegate)
                // Most tests need the terminal they open there when the
                // call returns; SpawnOffMainTests turn this back on (#722).
                appDelegate.spawnsInBackground = false
            }
        }
    }

    func testCaseWillStart(_ testCase: XCTestCase) {
        if let report = LiveWindows.takeReport() {
            testCase.record(XCTIssue(type: .assertionFailure, compactDescription: report))
        }
    }

    func testBundleDidFinish(_ testBundle: Bundle) {
        let line = "live windows at the end of the run: \(LiveWindows.count), at most \(LiveWindows.peak)\n"
        FileHandle.standardError.write(Data(line.utf8))
    }

    /// The test host opens and closes windows without AppKit's open and
    /// close animations (`NSAutomaticWindowAnimationsEnabled` off, in the
    /// volatile argument domain, which writes nothing to disk) (#637). In
    /// some console states (seen locally, cause unknown) those animations
    /// never get a frame: a closing window's animation never ends and holds
    /// one dispatch worker for the rest of the run. A full run closes hundreds
    /// of windows, so the pool reaches its limit of 64 workers, and
    /// `NSApp.terminate` then waits forever for the window-state work it
    /// queues before it asks the delegate (the quit tests hung under load).
    static func turnOffWindowAnimations() {
        let defaults = UserDefaults.standard
        var arguments = defaults.volatileDomain(forName: UserDefaults.argumentDomain)
        arguments[windowAnimationsKey] = false
        defaults.setVolatileDomain(arguments, forName: UserDefaults.argumentDomain)
    }

    static let windowAnimationsKey = "NSAutomaticWindowAnimationsEnabled"

    /// Every core the host starts from now on shares one scan of the
    /// installed fonts and one font fallback index
    /// (`tessera_test_share_font_index`, #660), where each core would build
    /// its own on its first surface: tests replace the core hundreds of
    /// times in a run, and Thread Sanitizer's `munmap` interceptor once
    /// faulted under that churn (docs/testing.md). Nothing is opened before
    /// the first test, so no core has loaded fonts yet. A host without the
    /// core's test hooks skips it; the tests that need them fail on their
    /// own.
    static func shareTheFontIndex() {
        guard TesseraTestsCoreTestHooksLinked() else { return }
        _ = tessera_test_share_font_index(true)
    }

    /// All three read as off, applied at once (the scroll bars' default
    /// and the core's options follow).
    @MainActor
    static func pin(_ appDelegate: AppDelegate) {
        appDelegate.reducesTransparency = { false }
        appDelegate.increasesContrast = { false }
        appDelegate.reducesMotion = { false }
        appDelegate.applyDisplayOptions()
    }
}

/// The pin is in place whatever the machine's settings.
@MainActor
final class HostDisplaySettingsTests: XCTestCase {
    func testTheMachinesDisplaySettingsReadAsOff() throws {
        let appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertFalse(appDelegate.reducesMotion())
        XCTAssertFalse(appDelegate.reducesTransparency())
        XCTAssertFalse(appDelegate.increasesContrast())
        XCTAssertFalse(TerminalScroller.systemReducesMotion, "a new scroll bar fades")
        XCTAssertFalse(TerminalScroller().reducesMotion)
    }

    /// Window animations are off in the test host (#637): in some console
    /// states (seen locally, cause unknown) each window close otherwise
    /// holds a dispatch worker in `-[NSAnimation _runBlocking]` for good,
    /// and the quit tests hang once 64 are held.
    func testWindowAnimationsAreOff() {
        XCTAssertEqual(
            UserDefaults.standard.object(forKey: HostDisplaySettings.windowAnimationsKey) as? Bool, false)
    }
}
