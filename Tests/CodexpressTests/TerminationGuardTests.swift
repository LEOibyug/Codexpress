import XCTest
@testable import Codexpress

final class TerminationGuardTests: XCTestCase {
    @MainActor func testActiveQuitCanBeCancelledAndRetriedWithFreshStatus() async {
        let guarder = TerminationGuard()
        var events: [String] = []
        let cancelled = await guarder.request(check: {
            events.append("check"); return 2
        }, confirm: { count in
            XCTAssertEqual(count, 2); events.append("cancel"); return false
        }, shutdown: { events.append("shutdown") })
        XCTAssertEqual(cancelled, false)
        XCTAssertEqual(events, ["check", "cancel"])

        let approved = await guarder.request(check: {
            events.append("recheck"); return 1
        }, confirm: { count in
            XCTAssertEqual(count, 1); events.append("confirm"); return true
        }, shutdown: { events.append("shutdown") })
        XCTAssertEqual(approved, true)
        XCTAssertEqual(events, ["check", "cancel", "recheck", "confirm", "shutdown"])
    }

    @MainActor func testIdleQuitsDirectlyButUnavailableStatusRequiresConfirmation() async {
        let guarder = TerminationGuard()
        var stopped = 0
        let idle = await guarder.request(check: { 0 }, confirm: { _ in
            XCTFail("Idle quit should not prompt"); return false
        }, shutdown: { stopped += 1 })
        XCTAssertEqual(idle, true)
        XCTAssertEqual(stopped, 1)
        for accept in [false, true] {
            let result = await guarder.request(check: { throw AppError.message("test unavailable") }, confirm: { count in
                XCTAssertNil(count); return accept
            }, shutdown: { stopped += 1 })
            XCTAssertEqual(result, accept)
        }
        XCTAssertEqual(stopped, 2)
    }

    @MainActor func testRepeatedQuitDoesNotOpenAnotherPromptOrShutdownTwice() async {
        let guarder = TerminationGuard()
        var continuation: CheckedContinuation<Int, Never>?
        var confirmations = 0
        var stops = 0
        let first = Task {
            await guarder.request(check: {
                await withCheckedContinuation { continuation = $0 }
            }, confirm: { _ in confirmations += 1; return true }, shutdown: { stops += 1 })
        }
        while continuation == nil { await Task.yield() }
        let duplicate = await guarder.request(check: {
            XCTFail("Duplicate quit must not check again"); return 0
        }, confirm: { _ in XCTFail("Duplicate prompt"); return true }, shutdown: { stops += 1 })
        XCTAssertNil(duplicate)
        continuation?.resume(returning: 3)
        let approved = await first.value
        XCTAssertEqual(approved, true)
        XCTAssertEqual(confirmations, 1)
        XCTAssertEqual(stops, 1)
    }
}
