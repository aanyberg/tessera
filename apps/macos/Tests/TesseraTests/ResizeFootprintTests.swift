import AppKit
import XCTest

@testable import Tessera

/// The footprint a live resize leaves behind (#326).
///
/// Every resize gives the pane's `CAMetalLayer` a new drawable size, and
/// every presented frame at that size hands an IOSurface of it to the
/// window server. macOS keeps those surfaces after the layer has dropped
/// them and charges them to this process as "Owned physical footprint
/// (unmapped)", releasing them in batches and once the window is idle. That
/// is not ours (no change on our side moved it; see "Live resize footprint"
/// in docs/debugging.md for the measurements), so this bounds what remains:
/// the in-flight peak stays below what keeping every resized drawable would
/// cost, and the footprint comes back within 150 MB of its start once the
/// resizing stops.
@MainActor
final class ResizeFootprintTests: XCTestCase {
    private var appDelegate: AppDelegate!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
        appDelegate = nil
    }

    /// 200 resize steps 20 ms apart with a rendering pane (#326).
    ///
    /// On CI macOS (scale 1, sizes around 1000 × 700) each presented
    /// resize step added about 2.6 MB, the size of one drawable, and the
    /// peak reached 0.2-0.4 of every step's drawable added up (104-212 MB
    /// over 200 steps, with and without Thread Sanitizer), dropping back
    /// in batches; 0.5-3 s after the last step it was back within 10 MB of
    /// its start. The peak bound (0.9 of that sum) leaves room for a late
    /// batch; a texture of ours kept per resize reached 1.17-1.2 of it and
    /// never came back, failing both bounds.
    func testTwoHundredResizeStepsLeaveTheFootprintWhereItStarted() async throws {
        let controller = try appDelegate.newWindow()
        let window = try XCTUnwrap(controller.window)
        window.setContentSize(NSSize(width: 1200, height: 800))
        controller.paneContainer.layoutSubtreeIfNeeded()
        let pane = controller.focusedPane
        guard pane.view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the view (no GPU can present, or no font)")
        }
        try pane.session.write(Array("ls -la /; seq 1 300\n".utf8))
        let warmUpEnd = ContinuousClock.now + .seconds(1)
        while ContinuousClock.now < warmUpEnd {
            try await Task.sleep(for: .milliseconds(20))
        }
        // The view draws only while its window is visible, and a window that
        // is not (a locked screen, a sleeping display, covered, on another
        // Space) vends no drawables: there is nothing to measure.
        try Self.skipUnlessVisible(pane.view)

        let sizes = [
            NSSize(width: 900, height: 600), NSSize(width: 1200, height: 800),
            NSSize(width: 1000, height: 700), NSSize(width: 1100, height: 650),
        ]
        let start = try Self.physFootprint()
        var peak = start
        let presentedBefore = pane.view.presentedFrameCount
        let resizesBefore = pane.view.coreResizeCount
        // What keeping one BGRA drawable per step would cost.
        var keptEveryDrawable: UInt64 = 0
        for step in 0..<200 {
            window.setContentSize(sizes[step % sizes.count])
            controller.paneContainer.layoutSubtreeIfNeeded()
            let pixels = pane.view.convertToBacking(pane.view.bounds).size
            keptEveryDrawable += UInt64(pixels.width) * UInt64(pixels.height) * 4
            try await Task.sleep(for: .milliseconds(20))
            peak = max(peak, try Self.physFootprint())
        }
        try Self.skipUnlessVisible(pane.view)
        let presented = pane.view.presentedFrameCount - presentedBefore
        let resizes = pane.view.coreResizeCount - resizesBefore

        let mb = { (bytes: UInt64) in Double(bytes) / 1_048_576 }
        let growth = mb(peak - min(peak, start))
        let detail = "start \(mb(start)) MB, peak \(mb(peak)) MB, \(presented) frames presented, \(resizes) resizes"
        XCTAssertGreaterThanOrEqual(resizes, 100, "the resizes reached the core: \(detail)")
        XCTAssertGreaterThanOrEqual(presented, 100, "the pane rendered while resizing: \(detail)")
        XCTAssertLessThan(
            growth, 0.9 * mb(keptEveryDrawable),
            "the peak stays below 0.9 of keeping every drawable (\(mb(keptEveryDrawable)) MB): \(detail)")

        // Idle: macOS releases the old drawables within a few seconds (the
        // loop ends as soon as the bound holds).
        let budget: UInt64 = 150 * 1_048_576
        let presentedAfterSteps = pane.view.presentedFrameCount
        var settled = try Self.physFootprint()
        let settleEnd = ContinuousClock.now + .seconds(10)
        while settled > start + budget, ContinuousClock.now < settleEnd {
            try await Task.sleep(for: .milliseconds(50))
            settled = try Self.physFootprint()
        }
        let settleFrames = pane.view.presentedFrameCount - presentedAfterSteps
        print(
            "resize footprint: \(detail), growth \(growth) MB of \(mb(keptEveryDrawable)) MB,",
            "settled \(mb(settled)) MB after \(settleFrames) more frames")
        XCTAssertLessThanOrEqual(
            settled, start + budget,
            "10 s after the last step the footprint is \(mb(settled)) MB (\(settleFrames) frames meanwhile): \(detail)")
    }

    /// Skips unless `view` would draw: it renders only while its window is
    /// visible.
    private static func skipUnlessVisible(_ view: TerminalSurfaceView) throws {
        if view.isOccluded {
            throw XCTSkip("the window is not visible (locked, asleep, covered or on another Space): nothing is drawn")
        }
    }

    /// The process's physical footprint in bytes.
    private static func physFootprint() throws -> UInt64 {
        var usage = rusage_info_v4()
        let status = withUnsafeMutablePointer(to: &usage) {
            $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
                proc_pid_rusage(getpid(), RUSAGE_INFO_V4, $0)
            }
        }
        guard status == 0 else {
            throw FootprintUnavailable(errno: errno)
        }
        return usage.ri_phys_footprint
    }

    private struct FootprintUnavailable: Error, CustomStringConvertible {
        let errno: Int32
        var description: String { "proc_pid_rusage failed (errno \(errno))" }
    }
}
