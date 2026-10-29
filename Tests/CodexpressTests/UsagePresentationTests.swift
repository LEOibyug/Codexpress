import XCTest
@testable import Codexpress

final class UsagePresentationTests: XCTestCase {
    func testUnconfirmedProviderCostStaysProvisionalUntilReconciled() {
        let reference: JSONValue = .object(["billing_basis":.string("subscription_reference"),"status":.string("reported"),"total_usd":.number(0.1)])
        XCTAssertEqual(usageCostStatus(reference), "参考价估算")
        XCTAssertEqual(usageCostMoney(reference), "≈$0.1000")
        let actual: JSONValue = .object(["billing_basis":.string("upstream_charge"),"status":.string("upstream_reported"),"total_usd":.number(0.13)])
        XCTAssertEqual(usageCostStatus(actual), "上游实际扣费")
        XCTAssertEqual(usageCostMoney(actual), "$0.1300")
    }
    func testProviderChargesRemainExactAndMixedTotalsKeepTheirEstimateMarker() {
        let charge: JSONValue = .object(["billing_basis": .string("upstream_charge"), "status": .string("upstream_reported"), "total_usd": .number(0)])
        XCTAssertEqual(usageCostMoney(charge), "$0.00")
        XCTAssertEqual(usageCostStatus(charge), "上游实际扣费")
        let actual: JSONValue = .object(["priced_requests": .number(2), "upstream_priced_requests": .number(2), "total_usd": .number(0.3)])
        XCTAssertFalse(UsageTotals(rows: [actual]).costEstimated)
        XCTAssertNil(UsageTotals(rows: [actual]).savings)
        let reference: JSONValue = .object(["priced_requests": .number(1), "total_usd": .number(0.2)])
        let mixed = UsageTotals(rows: [actual, reference])
        XCTAssertTrue(mixed.costEstimated)
        XCTAssertEqual(mixed.cost ?? 0, 0.5, accuracy: 1e-9)
    }
    func testMoneyAndModelComparisonsPreserveUnknownPricesAndSumMatchingModels() {
        XCTAssertEqual(usageMoney(nil), "—")
        XCTAssertEqual(usageMoney(0), "≈$0.00")
        XCTAssertEqual(usageMoney(0.000123), "≈$0.000123")
        let rows: [JSONValue] = [
            .object(["model":.string("m"),"source_id":.string("a"),"requests":.number(2),"priced_requests":.number(2),"total_usd":.number(0.5)]),
            .object(["model":.string("m"),"source_id":.string("b"),"requests":.number(1),"priced_requests":.number(1),"total_usd":.number(1)]),
            .object(["model":.string("unknown"),"source_id":.string("b"),"requests":.number(1),"unpriced_requests":.number(1)])
        ]
        let models = usageComparisons(rows, key: "model", sort: "cost")
        XCTAssertEqual(models.count, 2)
        XCTAssertEqual(models.first?.id, "m")
        XCTAssertEqual(models.first?.totals.cost, 1.5)
        XCTAssertEqual(models.first?.totals.averageCost, 0.5)
        XCTAssertNil(models.last?.totals.cost)
    }
    func testUsageCountsAbbreviateLargeValuesAndPromoteRoundedUnitBoundaries() {
        let cases: [(Double, String)] = [
            (0, "0"), (999, "999"), (1000, "1K"), (1234, "1.23K"),
            (734600, "734.6K"), (999994, "999.99K"), (999999, "1M"),
            (1234567, "1.23M"), (999999999, "1B"), (7890000000, "7.89B"),
            (1000000000000, "1T")
        ]
        for (number, expected) in cases { XCTAssertEqual(usageCount(number), expected) }
        XCTAssertEqual(usageCount(1234567, estimated: true), "≈1.23M")
        XCTAssertEqual(usageRate(60), "60.0")
        XCTAssertEqual(usageRate(1234, estimated: true), "≈1.23K")
        XCTAssertEqual(usageRate(nil), "—")
    }

    func testSourceTotalsWeightThroughputAndExcludeInflightRequestsFromSuccess() {
        let rows: [JSONValue] = [
            .object(["requests":.number(4),"active_requests":.number(2),"successes":.number(1),"input_tokens":.number(300),"output_tokens":.number(10),"total_latency_ms":.number(2000),"throughput_output_tokens":.number(10),"throughput_latency_ms":.number(1000),"total_first_token_ms":.number(300),"first_token_samples":.number(1)]),
            .object(["requests":.number(1),"successes":.number(1),"input_tokens":.number(200),"output_tokens":.number(100),"total_latency_ms":.number(6000),"throughput_output_tokens":.number(100),"throughput_latency_ms":.number(5000),"total_first_token_ms":.number(500),"first_token_samples":.number(1),"estimated_requests":.number(1)])
        ]
        let totals = UsageTotals(rows: rows)
        XCTAssertEqual(totals.total,610)
        XCTAssertEqual(totals.finished,3)
        XCTAssertEqual(totals.successRate ?? 0,200.0/3,accuracy:0.001)
        XCTAssertEqual(totals.tps ?? 0,110000.0/6000,accuracy:0.001)
        XCTAssertTrue(totals.estimated)
        XCTAssertNil(UsageTotals(rows: []).tps)
        XCTAssertNil(UsageTotals(rows: []).successRate)
    }
    func testLocalNaturalDaysUseCalendarBoundariesAcrossDST() throws {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        let formatter = ISO8601DateFormatter()
        let spring = formatter.date(from: "2026-03-09T18:30:00Z")!
        let query = UsageQuery.last(days: 3, now: spring, calendar: calendar)
        XCTAssertEqual(query.dayStarts.count, 4)
        XCTAssertEqual(query.dayStarts[2] - query.dayStarts[1], 23 * 3600)
        XCTAssertEqual(query.from, query.dayStarts.first)
        XCTAssertEqual(query.to, query.dayStarts.last)
        let fall = formatter.date(from: "2026-11-02T18:30:00Z")!
        let autumn = UsageQuery.last(days: 3, now: fall, calendar: calendar)
        XCTAssertEqual(autumn.dayStarts[2] - autumn.dayStarts[1], 25 * 3600)
        XCTAssertEqual(UsageQuery.last(days: 30, now: spring, calendar: calendar).dayStarts.count, 31)
        let start = calendar.startOfDay(for: spring)
        let tooFar = calendar.date(byAdding: .day, value: 366, to: start)!
        XCTAssertThrowsError(try UsageQuery(start: start, end: tooFar, calendar: calendar))
        XCTAssertThrowsError(try UsageQuery(start: spring, end: calendar.date(byAdding: .day, value: -1, to: start)!, calendar: calendar))
    }

    func testDailySeriesRetainsZeroDaysAndDoesNotAddCachedInputTwice() {
        let daily = UsageDaily(.object(["start_at": .number(100), "input_tokens": .number(200), "cached_input_tokens": .number(150), "output_tokens": .number(20), "requests": .number(1)]))
        XCTAssertEqual(daily.total, 220)
        XCTAssertNil(daily.cost)
        let empty = UsageDaily(.object(["start_at": .number(200), "total_tokens": .number(0), "requests": .number(0)]))
        XCTAssertEqual(empty.total, 0)
        XCTAssertEqual(empty.requests, 0)
        XCTAssertEqual(usageActivityLevel(0, maximum: 100), 0)
        XCTAssertLessThan(usageActivityLevel(20, maximum: 100), usageActivityLevel(100, maximum: 100))
    }

    func testDistributionKeepsUnpricedSourcesOutOfMonetaryShares() {
        let rows: [JSONValue] = [
            .object(["source_id": .string("known"), "source_name": .string("已计价"), "requests": .number(1), "priced_requests": .number(1), "total_usd": .number(2), "input_tokens": .number(100), "cached_input_tokens": .number(80), "output_tokens": .number(10)]),
            .object(["source_id": .string("unknown"), "source_name": .string("未知"), "requests": .number(1), "unpriced_requests": .number(1), "input_tokens": .number(1000)])
        ]
        let priced = usageComparisons(rows, key: "source_id", sort: "cost")
        XCTAssertEqual(priced.first?.totals.total, 110)
        XCTAssertNil(priced.last?.totals.cost)
        XCTAssertEqual(UsageTotals(rows: rows).cost, 2)
        XCTAssertEqual(UsageTotals(rows: rows).total, 1110)
    }

    func testFirstPageRefreshPreservesOnlyOverlappingCompletedHistory() {
        func records(_ range: ClosedRange<Int>) -> [UsageItem] {
            range.reversed().map { number in UsageItem(.object(["attempt_id": .string("attempt-\(number)"), "sequence": .number(Double(number)), "active": .bool(false)])) }
        }
        let previous = records(1...100)
        let overlapping = usageMergeFirstPage(previous: previous, incoming: records(71...120), previousCursor: 1, incomingCursor: 71, preserving: true, capacity: 150)
        XCTAssertEqual(overlapping.records.map(\.id), records(1...120).map(\.id))
        XCTAssertEqual(overlapping.nextCursor, 1)
        let active = UsageItem(.object(["attempt_id": .string("active"), "active": .bool(true)]))
        let gap = usageMergeFirstPage(previous: [active] + previous, incoming: [active] + records(151...200), previousCursor: 1, incomingCursor: 151, preserving: true)
        XCTAssertEqual(gap.records.map(\.id), ([active] + records(151...200)).map(\.id))
        XCTAssertEqual(gap.nextCursor, 151)
        let changedFilter = usageMergeFirstPage(previous: previous, incoming: records(71...120), previousCursor: 1, incomingCursor: 71, preserving: false)
        XCTAssertEqual(changedFilter.records.count, 50)
        XCTAssertEqual(changedFilter.nextCursor, 71)
    }

    func testFirstPageRefreshIsBoundedByExplicitlyLoadedPages() {
        func records(_ range: ClosedRange<Int>) -> [UsageItem] {
            range.reversed().map { number in UsageItem(.object(["attempt_id": .string("attempt-\(number)"), "sequence": .number(Double(number))])) }
        }
        let result = usageMergeFirstPage(previous: records(1...100), incoming: records(52...101), previousCursor: 1, incomingCursor: 52, preserving: true, capacity: 100)
        XCTAssertEqual(result.records.count, 100)
        XCTAssertEqual(result.records.last?.value["sequence"].number, 2)
        XCTAssertEqual(result.nextCursor, 2)
        let firstOnly = usageMergeFirstPage(previous: records(1...50), incoming: records(2...51), previousCursor: 1, incomingCursor: 2, preserving: false)
        XCTAssertEqual(firstOnly.records.count, 50)
        XCTAssertEqual(firstOnly.nextCursor, 2)
    }

    func testSourceRingShowsTopThreeAndAggregatesAllRemainingPositiveValues() {
        let groups: [JSONValue] = (1...6).map { index in
            let price: JSONValue = index == 6 ? .null : .number(Double(index))
            let fields: [String: JSONValue] = ["source_id": .string("source-\(index)"), "source_name": .string("来源 \(index)"), "input_tokens": .number(Double(index * 100)), "output_tokens": .number(10), "priced_requests": .number(index == 6 ? 0 : 1), "total_usd": price]
            return .object(fields)
        }
        let cost = usageDistributionSlices(groups, groupBy: "source_id")
        XCTAssertEqual(cost.map(\.id), ["source-5", "source-4", "source-3", "usage-other-sources"])
        XCTAssertEqual(cost.last?.value, 3)
        XCTAssertEqual(cost.last?.cost, 3)
        XCTAssertEqual(cost.last?.tokenTotal, 320)
        XCTAssertEqual(cost.reduce(0) { $0 + $1.value }, 15)
        XCTAssertEqual(cost.reduce(0) { $0 + ($1.cost ?? 0) }, 15)
        XCTAssertEqual(cost.reduce(0) { $0 + $1.tokenTotal }, 1550)
        XCTAssertTrue(usageDistributionSlices([.object(["source_id": .string("zero"), "priced_requests": .number(1), "total_usd": .number(0)])], groupBy: "source_id").isEmpty)
    }

    func testActivityHeightUsesTheSameCellSizeAsTheRenderedGrid() {
        for count in [7, 30, 90, 180, 366] {
            let layout = UsageActivityLayout(width: 508, days: count, leading: 6)
            XCTAssertEqual(layout.height, layout.cell * 7 + 18)
            XCTAssertLessThanOrEqual(Double(layout.weeks) * (layout.cell + 3) + 21, 511)
        }
    }

    func testRingHitTestingStartsAtTwelveClockwiseAndRejectsHoleAndOutside() {
        let slices = [
            UsageSourceSlice(id: "first", name: "第一", value: 1, tokenTotal: 100, cost: 1, estimated: false),
            UsageSourceSlice(id: "second", name: "第二", value: 1, tokenTotal: 200, cost: 1, estimated: false),
            UsageSourceSlice(id: "third", name: "第三", value: 1, tokenTotal: 300, cost: 1, estimated: false),
            UsageSourceSlice(id: "usage-other-sources", name: "其他", value: 1, tokenTotal: 400, cost: 1, estimated: true)
        ]
        XCTAssertEqual(usageRingHit(x: 0, y: -9, outerRadius: 10, innerRatio: 0.78, slices: slices), "first")
        XCTAssertEqual(usageRingHit(x: 7, y: -7, outerRadius: 10, innerRatio: 0.78, slices: slices), "first")
        XCTAssertEqual(usageRingHit(x: 7, y: 7, outerRadius: 10, innerRatio: 0.78, slices: slices), "second")
        XCTAssertEqual(usageRingHit(x: -7, y: 7, outerRadius: 10, innerRatio: 0.78, slices: slices), "third")
        XCTAssertEqual(usageRingHit(x: -7, y: -7, outerRadius: 10, innerRatio: 0.78, slices: slices), "usage-other-sources")
        XCTAssertNil(usageRingHit(x: 0, y: 0, outerRadius: 10, innerRatio: 0.78, slices: slices))
        XCTAssertNil(usageRingHit(x: 0, y: 11, outerRadius: 10, innerRatio: 0.78, slices: slices))
        XCTAssertNil(usageRingHit(x: 0, y: -9, outerRadius: 10, innerRatio: 0.78, slices: []))
        let estimated: [JSONValue] = [.object(["source_id": .string("estimated"), "input_tokens": .number(10), "estimated_requests": .number(1), "priced_requests": .number(1), "total_usd": .number(1)])]
        XCTAssertTrue(usageDistributionSlices(estimated, groupBy: "source_id").first?.estimated == true)
    }

    func testModelDistributionCombinesMatchingModelsAndConservesAllTokens() {
        let groups: [JSONValue] = [
            .object(["source_id": .string("a"), "source_name": .string("供应商 A"), "model": .string("shared-model"), "input_tokens": .number(100), "output_tokens": .number(20), "priced_requests": .number(1), "total_usd": .number(3)]),
            .object(["source_id": .string("b"), "source_name": .string("供应商 B"), "model": .string("shared-model"), "input_tokens": .number(200), "output_tokens": .number(30), "priced_requests": .number(1), "total_usd": .number(2), "estimated_requests": .number(1)]),
            .object(["source_id": .string("c"), "source_name": .string("供应商 C"), "model": .string("another-model"), "input_tokens": .number(300), "priced_requests": .number(1), "total_usd": .number(1)]),
            .object(["source_id": .string("d"), "source_name": .string("供应商 D"), "model": .string("unpriced"), "input_tokens": .number(999), "unpriced_requests": .number(1)])
        ]
        let models = usageDistributionSlices(groups, groupBy: "model")
        XCTAssertEqual(models.map(\.name), ["shared-model", "another-model", "其他"])
        XCTAssertEqual(models.first?.cost, 5)
        XCTAssertEqual(models.first?.tokenTotal, 350)
        XCTAssertTrue(models.first?.estimated == true)
        XCTAssertEqual(models.reduce(0) { $0 + ($1.cost ?? 0) }, 6)
        XCTAssertEqual(models.reduce(0) { $0 + $1.tokenTotal }, 1649)
        let sources = usageDistributionSlices(groups, groupBy: "source_id")
        XCTAssertEqual(sources.first?.name, "供应商 A")
        XCTAssertEqual(sources.reduce(0) { $0 + ($1.cost ?? 0) }, 6)
        XCTAssertEqual(sources.reduce(0) { $0 + $1.tokenTotal }, 650)
    }

    func testModelTokenOrderingAlwaysCombinesUnknownModelsIntoOther() {
        func row(_ model: String, _ tokens: Double, priced: Bool = true) -> JSONValue {
            .object(["model": .string(model), "input_tokens": .number(tokens), "priced_requests": .number(priced ? 1 : 0), "unpriced_requests": .number(priced ? 0 : 1), "total_usd": priced ? .number(1) : .null])
        }
        let knownAndUnknown = usageDistributionSlices([row("known", 10), row("unlisted-model", 200, priced: false)], groupBy: "model")
        XCTAssertEqual(knownAndUnknown.map(\.name), ["known", "其他"])
        XCTAssertEqual(knownAndUnknown.last?.tokenTotal, 200)
        XCTAssertNil(knownAndUnknown.last?.cost)
        let unknownOnly = usageDistributionSlices([row("", 10), row("unknown", 20), row("unknown_model", 30), row("未知模型", 40)], groupBy: "model")
        XCTAssertEqual(unknownOnly.count, 1)
        XCTAssertEqual(unknownOnly.first?.name, "其他")
        XCTAssertEqual(unknownOnly.first?.tokenTotal, 100)
        let many = usageDistributionSlices([row("model-a", 10), row("model-b", 40), row("model-c", 30), row("model-d", 20), row("unlisted-model", 500, priced: false)], groupBy: "model")
        XCTAssertEqual(many.map(\.name), ["model-b", "model-c", "model-d", "其他"])
        XCTAssertEqual(many.last?.tokenTotal, 510)
        XCTAssertEqual(many.reduce(0) { $0 + $1.tokenTotal }, 600)
        XCTAssertEqual(many.reduce(0) { $0 + ($1.cost ?? 0) }, 4)
    }

}
