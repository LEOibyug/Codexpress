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
        XCTAssertNil(QuotaWindow(window("secondary", seconds: 1e30, used: nil), index: 0).seconds)
        let nearlySpent = QuotaOverview(.object(["windows": .array([window("primary", seconds: 18000, used: 99.5)])]))
        XCTAssertEqual(nearlySpent.mainWindows[0].remainingLabel, "<1%")
        XCTAssertFalse(nearlySpent.isExhausted)
    }

    func testWeeklyOnlyAccountsOmitShortQuotaButKeepUnknownExistingWindows() {
        for name in ["primary", "secondary"] {
            let display = QuotaOverview(.object(["windows": .array([
                window(name, seconds: 604800, used: 25),
                window("another-model:primary", seconds: 18000, used: 10)
            ])]))
            XCTAssertEqual(display.mainWindows.map(\.title), ["每周"])
            XCTAssertEqual(display.mainWindows.map(\.remainingLabel), ["75%"])
        }
        let existing = QuotaOverview(.object(["windows": .array([
            window("primary", seconds: 18000, used: nil),
            window("secondary", seconds: 604800, used: 25)
        ])]))
        XCTAssertEqual(existing.mainWindows.map(\.title), ["5 小时", "每周"])
        XCTAssertEqual(existing.mainWindows.map(\.remainingLabel), ["未知", "75%"])
    }

    func testWeeklyReferenceUsesTheResetCycleAndNeverInventsANewCycle() {
        let reset = 2_000_000_000.0
        let weekly = QuotaWindow(window("primary", seconds: 604800, used: 60), index: 0)
        for (elapsed, expected) in [(-1.0, 100.0), (0, 100), (151200, 75), (302400, 50), (453600, 25), (604800, 0), (1209600, 0)] {
            let date = Date(timeIntervalSince1970: reset - 604800 + elapsed)
            XCTAssertEqual(weekly.referenceCost(at: date), expected)
        }
        XCTAssertEqual(weekly.remaining, 40)
        let date = Date(timeIntervalSince1970: reset)
        let missing = QuotaWindow(.object(["window_seconds": .number(604800)]), index: 0)
        XCTAssertNil(missing.referenceCost(at: date))
        let supplied = QuotaWindow(.object(["window_seconds": .number(604800), "ref_cost": .number(45)]), index: 0)
        XCTAssertEqual(supplied.referenceCost(at: date), 45)
        for invalid in [Double.nan, -Double.infinity, -1, 101] {
            XCTAssertNil(QuotaWindow(.object(["window_seconds": .number(604800), "ref_cost": .number(invalid)]), index: 0).referenceCost(at: date))
        }
        XCTAssertNil(QuotaWindow(window("primary", seconds: 18000, used: 60), index: 0).referenceCost(at: date))
    }

    func testQuotaRefreshUsesConfiguredIntervalAndForegroundDoesNotRepeatedlyFetchCredits() {
        let start = Date(timeIntervalSince1970: 2000000000)
        var schedule = OfficialRefreshSchedule()
        XCTAssertTrue(schedule.isDue("source", now: start))
        XCTAssertTrue(schedule.needsCredits("source", now: start))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(1800), interval: 3600, since: start))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(3600), interval: 3600, since: start))
        XCTAssertEqual(schedule.backgroundDelay(for: ["source"], since: start, interval: 60, now: start), 60)
        schedule.record("source", includeCredits: true, now: start)
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(14), foreground: true))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(15), foreground: true))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(29)))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(30)))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(1799)))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(1800)))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(59), interval: 60))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(60), interval: 60))
        XCTAssertFalse(schedule.isDue("source", now: start.addingTimeInterval(3599), interval: 3600))
        XCTAssertTrue(schedule.isDue("source", now: start.addingTimeInterval(3600), interval: 3600))
        XCTAssertEqual(schedule.backgroundDelay(for: ["source"], since: start, interval: 60, now: start.addingTimeInterval(30)), 30)
        XCTAssertEqual(schedule.backgroundDelay(for: ["source"], since: start, interval: 60, now: start.addingTimeInterval(60)), 15)
        XCTAssertEqual(schedule.backgroundDelay(for: ["source"], since: start, interval: 3600, now: start.addingTimeInterval(3000)), 600)
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
