import XCTest
@testable import Codexpress

final class OfficialQuotaTests: XCTestCase {
    private func window(_ name: String, seconds: Double?, used: Double?) -> JSONValue {
        .object(["name": .string(name), "window_seconds": seconds.map(JSONValue.number) ?? .null,
                 "used_percent": used.map(JSONValue.number) ?? .null, "reset_at": .number(2000000000)])
    }

    func testWeeklyAndShortQuotaUseDurationsAndNeverBorrowAnotherModelsLimit() {
        let quota: JSONValue = .object(["windows": .array([
            window("primary", seconds: 604800, used: 75),
            window("secondary", seconds: 18000, used: 12),
            window("another-model:secondary", seconds: 604800, used: 99)
        ])])
        let display = QuotaOverview(quota)
        XCTAssertEqual(display.mainWindows.map(\.title), ["5 小时", "每周"])
        XCTAssertEqual(display.mainWindows.map(\.remainingLabel), ["88%", "25%"])
        XCTAssertEqual(display.windows.count, 3)
        let missing = QuotaOverview(.object(["windows": .array([
            window("primary", seconds: 18000, used: 22),
            window("secondary", seconds: nil, used: 40),
            window("another-model:secondary", seconds: 604800, used: 99)
        ])]))
        XCTAssertEqual(missing.mainWindows[0].remainingLabel, "78%")
        XCTAssertEqual(missing.mainWindows[1].remainingLabel, "未知")
        XCTAssertNil(missing.mainWindows[1].resetAt)
        XCTAssertTrue(missing.windows.contains { $0.title == "次要额度" && $0.remainingLabel == "60%" })
        XCTAssertEqual(QuotaOverview(.null).mainWindows.map(\.remainingLabel), ["未知", "未知"])
        XCTAssertNil(QuotaWindow(window("secondary", seconds: 1e30, used: nil), index: 0).seconds)
    }

    func testQuotaRefreshesEveryThirtySecondsAndForegroundDoesNotRepeatedlyFetchCredits() {
        let start = Date(timeIntervalSince1970: 2000000000)
        var schedule = OfficialRefreshSchedule()
        XCTAssertTrue(schedule.isDue("source", now: start))
        XCTAssertTrue(schedule.needsCredits("source", now: start))
        schedule.record("source", includeCredits: true, now: start)
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(14), foreground: true))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(15), foreground: true))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(29)))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(30)))
        schedule.record("source", includeCredits: false, now: start.addingTimeInterval(30))
        XCTAssertFalse(schedule.needsCredits("source", now: start.addingTimeInterval(299)))
        XCTAssertTrue(schedule.needsCredits("source", now: start.addingTimeInterval(300)))
        schedule.record("source", includeCredits: true, now: start.addingTimeInterval(300))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(310), foreground: true))
        schedule.reset("source")
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(310)))
        XCTAssertTrue(schedule.needsCredits("source", now: start.addingTimeInterval(310)))
    }
}
