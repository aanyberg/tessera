import Darwin
import Foundation
import TesseraKit
import XCTest

/// Counts bells and spends about a microsecond on each, like a cheap beep.
@MainActor
private final class SlowBellDelegate: TesseraAppDelegate {
    private(set) var bells = 0
    func sessionDidOutput(_ session: TesseraSession) {}
    func session(_ session: TesseraSession, didSetTitle title: String) {}
    func sessionDidRingBell(_ session: TesseraSession) {
        bells += 1
        let end = DispatchTime.now().uptimeNanoseconds + 1_000
        while DispatchTime.now().uptimeNanoseconds < end {}
    }
    func session(_ session: TesseraSession, didExitWithCode code: Int32, signal: Int32) {}
}

/// The process's physical footprint in bytes (what Activity Monitor shows).
private func physFootprint() -> UInt64 {
    var usage = rusage_info_v4()
    let status = withUnsafeMutablePointer(to: &usage) {
        $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
            proc_pid_rusage(getpid(), RUSAGE_INFO_V4, $0)
        }
    }
    return status == 0 ? usage.ri_phys_footprint : 0
}

/// Regression test for the adversarial finding on #32: the core calls back once
/// per BEL byte, and one main-actor task per bell grew the process by gigabytes
/// under `yes $'\a'` and made quitting take a minute.
@MainActor
final class BellFloodTests: XCTestCase {
    func testBellFloodKeepsMemoryAndLatencyBounded() async throws {
        let app = try TesseraApp()
        let delegate = SlowBellDelegate()
        app.delegate = delegate
        var shutDown = false
        defer { if !shutDown { app.shutdown() } }

        _ = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", #"exec yes "$(printf '\007')""#]))
        // Let the flood reach a steady state before taking the baseline.
        try await Task.sleep(for: .milliseconds(500))
        let baseline = physFootprint()
        var peak = baseline
        var worstLag: Duration = .zero
        let end = ContinuousClock.now + .seconds(3)
        while ContinuousClock.now < end {
            let start = ContinuousClock.now
            try await Task.sleep(for: .milliseconds(100))
            worstLag = max(worstLag, ContinuousClock.now - start - .milliseconds(100))
            peak = max(peak, physFootprint())
        }

        let start = ContinuousClock.now
        app.shutdown()
        shutDown = true
        let shutdownTime = ContinuousClock.now - start

        // `peak` starts at `baseline` and only grows, so this cannot underflow.
        let growthMB = Double(peak - baseline) / 1_048_576
        print("bell flood: \(delegate.bells) deliveries, growth \(Int(growthMB)) MB, worst lag \(worstLag), shutdown \(shutdownTime)")
        XCTAssertGreaterThan(delegate.bells, 0, "no bell was delivered")
        XCTAssertLessThan(growthMB, 100, "footprint grew \(growthMB) MB during the flood")
        XCTAssertLessThan(worstLag, .milliseconds(500), "main actor lagged \(worstLag)")
        XCTAssertLessThan(shutdownTime, .seconds(3), "shutdown took \(shutdownTime)")
    }
}
