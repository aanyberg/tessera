import AppKit
import XCTest

@testable import Tessera

/// The test bundle's principal class (`NSPrincipalClass`, project.yml):
/// before the first test it pins the app's reads of the machine's
/// Accessibility display settings (Reduce Motion, Reduce Transparency,
/// Increase Contrast) to off, so no test depends on the machine it runs on
/// (a CI runner may report Reduce Motion on, and a scroll bar would then
/// never fade). A test that needs one on sets the app delegate's read
/// itself and calls ``pin(_:)`` in its tearDown.
@objc(TesseraTestsHostDisplaySettings)
final class HostDisplaySettings: NSObject, XCTestObservation {
    override init() {
        super.init()
        XCTestObservationCenter.shared.addTestObserver(self)
    }

    func testBundleWillStart(_ testBundle: Bundle) {
        MainActor.assumeIsolated {
            if let appDelegate = NSApp.delegate as? AppDelegate {
                Self.pin(appDelegate)
            }
        }
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
}
