import Foundation

struct UsageItem: Identifiable {
    let value: JSONValue
    let id: String
    init(_ value: JSONValue, index: Int = 0, key: String = "attempt_id") {
        self.value = value; id = value[key].string ?? "legacy-\(index)"
    }
}

struct UsageTotals {
    let rows: [JSONValue]
    func sum(_ key: String) -> Double { rows.reduce(0) { $0 + ($1[key].number ?? 0) } }
    var input: Double { sum("input_tokens") }
    var output: Double { sum("output_tokens") }
    var total: Double { input + output }
    var estimated: Bool { sum("estimated_requests") > 0 }
    var finished: Double { max(0, sum("requests") - sum("active_requests")) }
    var successRate: Double? { finished > 0 ? sum("successes") * 100 / finished : nil }
    var tps: Double? { sum("throughput_latency_ms") > 0 ? sum("throughput_output_tokens") * 1000 / sum("throughput_latency_ms") : nil }
    var cacheRate: Double? { sum("cached_reported_input_tokens") > 0 ? min(100, sum("cached_reported_tokens") * 100 / sum("cached_reported_input_tokens")) : nil }
    var cost: Double? { sum("priced_requests") > 0 ? sum("total_usd") : nil }
    var costEstimated: Bool { sum("priced_requests") > sum("upstream_priced_requests") }
    var averageCost: Double? { sum("priced_requests") > 0 ? sum("total_usd") / sum("priced_requests") : nil }
    var savings: Double? { costEstimated ? sum("cache_savings_usd") : nil }
}

struct UsageComparison: Identifiable {
    let id: String
    let name: String
    let totals: UsageTotals
}
func usageComparisons(_ rows: [JSONValue], key: String, sort: String) -> [UsageComparison] {
    Dictionary(grouping: rows, by: { $0[key].string ?? "" }).map { id, rows in
        UsageComparison(id: id, name: key == "model" ? (id.isEmpty ? "未知模型" : id) : (rows.first?["source_name"].string ?? "已移除来源"), totals: UsageTotals(rows: rows))
    }.sorted { a,b in
        let x = sort == "tokens" ? a.totals.total : sort == "requests" ? a.totals.sum("requests") : a.totals.cost ?? -1
        let y = sort == "tokens" ? b.totals.total : sort == "requests" ? b.totals.sum("requests") : b.totals.cost ?? -1
        return x == y ? a.id < b.id : x > y
    }
}
func usageMoney(_ amount: Double?, estimated: Bool = true) -> String {
    guard let amount, amount.isFinite else { return "—" }
    let digits = amount != 0 && abs(amount) < 0.01 ? 6 : amount != 0 && abs(amount) < 1 ? 4 : 2
    return (estimated ? "≈" : "") + "$" + amount.formatted(.number.locale(Locale(identifier: "en_US_POSIX")).precision(.fractionLength(digits)))
}
func usageCostStatus(_ cost: JSONValue) -> String {
    switch cost["status"].string {
    case "upstream_reported": return "上游实际扣费"
    case "unknown_model": return "未收录价格"
    case "unknown_tier": return "未收录此计费模式"
    case "unobserved": return "无可计费计数"
    case "partial": return "部分计价"
    case "estimated": return cost["historical"].bool == true ? "历史参考价补算" : "含估算项"
    case "reported": return "上游计数计价"
    default: return "待计价"
    }
}
func usageBillingBasis(_ cost: JSONValue) -> String {
    switch cost["billing_basis"].string {
    case "upstream_charge": return cost["method"].string == "cch_usage" ? "供应商账单匹配" : "供应商响应"
    case "official_subscription_reference": return "官方订阅参考折算"
    case "subscription_reference": return "订阅来源参考折算"
    default: return cost["source_type_known"].bool == false ? "API 参考折算（历史来源类型未知）" : "API 参考折算"
    }
}
func usageCostMoney(_ cost: JSONValue) -> String {
    usageMoney(cost["total_usd"].number, estimated: cost["billing_basis"].string != "upstream_charge")
}

func usageCount(_ number: Double, estimated: Bool = false) -> String {
    (estimated ? "≈" : "") + compactUsageNumber(number)
}
func usageRate(_ number: Double?, estimated: Bool = false) -> String {
    number.map { (estimated ? "≈" : "") + compactUsageNumber($0, minimumFractionDigits: 1) } ?? "—"
}
private func compactUsageNumber(_ number: Double, minimumFractionDigits: Int = 0) -> String {
    guard number.isFinite else { return "—" }
    let units = ["", "K", "M", "B", "T", "Q"]
    var scaled = number
    var unit = 0
    while abs(scaled) >= 1000 && unit < units.count - 1 {
        scaled /= 1000
        unit += 1
    }
    // Rounding at a unit boundary should display 1M rather than 1000K.
    if unit > 0 && (abs(scaled) * 100).rounded() >= 100_000 && unit < units.count - 1 {
        scaled /= 1000
        unit += 1
    }
    let precision: FloatingPointFormatStyle<Double>.Configuration.Precision = unit == 0
        ? .fractionLength(minimumFractionDigits)
        : .fractionLength(0...2)
    return scaled.formatted(.number.locale(Locale(identifier: "en_US_POSIX")).grouping(.never).precision(precision)) + units[unit]
}
func usageSeconds(_ milliseconds: Double?) -> String { milliseconds.map { String(format: "%.2f 秒", $0 / 1000) } ?? "—" }

/// An inclusive selection of local calendar dates, sent as half-open day boundaries.
struct UsageRefreshKey<Selection: Equatable>: Equatable {
    let selection: Selection
    let active: Bool
}

struct UsageQuery: Equatable, Sendable {
    let from: Double
    let to: Double
    let dayStarts: [Double]
    var sourceID = ""
    var modelID = ""
    var ingress = ""

    init(start: Date, end: Date, calendar: Calendar = .current, sourceID: String = "", modelID: String = "", ingress: String = "") throws {
        let first = calendar.startOfDay(for: start), last = calendar.startOfDay(for: end)
        let count = (calendar.dateComponents([.day], from: first, to: last).day ?? -1) + 1
        guard (1...366).contains(count) else { throw AppError.message("请选择不超过 366 天的日期范围，结束日期须晚于或等于开始日期。") }
        let dates = (0...count).compactMap { calendar.date(byAdding: .day, value: $0, to: first)?.timeIntervalSince1970 }
        guard dates.count == count + 1, let from = dates.first, let to = dates.last, from >= 0 else { throw AppError.message("无法读取所选日期，请重新选择。") }
        self.from = from; self.to = to; dayStarts = dates
        self.sourceID = sourceID; self.modelID = modelID; self.ingress = ingress
    }
    static func last(days: Int, now: Date = Date(), calendar: Calendar = .current, sourceID: String = "", modelID: String = "") -> UsageQuery {
        let count = min(366, max(1, days))
        let today = calendar.startOfDay(for: now)
        let start = calendar.date(byAdding: .day, value: 1 - count, to: today) ?? today
        // Calendar arithmetic on supported macOS calendars always yields a valid interval.
        return try! UsageQuery(start: start, end: today, calendar: calendar, sourceID: sourceID, modelID: modelID)
    }
    var payload: [String: JSONValue] {
        var values: [String: JSONValue] = ["from": .number(from), "to": .number(to), "day_starts": .array(dayStarts.map(JSONValue.number))]
        if !sourceID.isEmpty { values["source_id"] = .string(sourceID) }
        if !modelID.isEmpty { values["model"] = .string(modelID) }
        if !ingress.isEmpty { values["ingress"] = .string(ingress) }
        return values
    }
    var label: String {
        let first = Date(timeIntervalSince1970: from).formatted(.dateTime.month().day())
        let last = Date(timeIntervalSince1970: dayStarts.dropLast().last ?? from).formatted(.dateTime.month().day())
        return "\(first) – \(last)"
    }
}

struct UsageDaily: Identifiable {
    let date: Date
    let input: Double
    let output: Double
    let cached: Double
    let total: Double
    let cost: Double?
    let requests: Double
    let estimated: Bool
    let costEstimated: Bool
    var id: Date { date }
    init(_ value: JSONValue) {
        date = Date(timeIntervalSince1970: value["start_at"].number ?? 0)
        input = value["input_tokens"].number ?? 0; output = value["output_tokens"].number ?? 0
        cached = value["cached_input_tokens"].number ?? 0
        total = value["total_tokens"].number ?? (input + output)
        cost = value["total_usd"].number
        requests = value["requests"].number ?? 0
        estimated = (value["estimated_requests"].number ?? 0) > 0
        costEstimated = (value["priced_requests"].number ?? 0) > (value["upstream_priced_requests"].number ?? 0)
    }
}

func usageActivityLevel(_ value: Double, maximum: Double) -> Int {
    guard value > 0, maximum > 0 else { return 0 }
    return min(4, max(1, Int(ceil(value / maximum * 4))))
}

func usageRequestStatus(_ item: JSONValue) -> String {
    switch item["status"].string {
    case "connecting": return "连接中"
    case "streaming": return "接收中"
    case "success": return "完成"
    case "failed": return "失败"
    case "interrupted": return "已中断"
    default: return item["active"].bool == true ? "进行中" : "未知"
    }
}

func usageIngressName(_ value: String?) -> String {
    switch value {
    case "native": return "Codex"
    case "ssh": return "SSH"
    case "chatbot": return "聊天客户端"
    case "image": return "图片"
    default: return "未知入口"
    }
}

func usagePrettyJSON(_ value: JSONValue) -> String {
    guard value != .null else { return "—" }
    let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
    guard let data = try? encoder.encode(value), let text = String(data: data, encoding: .utf8) else { return "—" }
    return text
}

struct UsageFirstPage {
    let records: [UsageItem]
    let nextCursor: Double?
    let preservedHistory: Bool
}

/// Keep older pages only when a completed row proves the refreshed page overlaps them.
func usageMergeFirstPage(previous: [UsageItem], incoming: [UsageItem], previousCursor: Double?, incomingCursor: Double?, preserving: Bool, capacity: Int = 50) -> UsageFirstPage {
    let currentIDs = Set(incoming.filter { $0.value["active"].bool != true }.map(\.id))
    let overlaps = previous.contains { $0.value["active"].bool != true && currentIDs.contains($0.id) }
    guard preserving, overlaps, let minimum = incoming.filter({ $0.value["active"].bool != true }).compactMap({ $0.value["sequence"].number }).min() else {
        return UsageFirstPage(records: incoming, nextCursor: incomingCursor, preservedHistory: false)
    }
    let ids = Set(incoming.map(\.id))
    let tail = previous.filter { $0.value["active"].bool != true && ($0.value["sequence"].number ?? .infinity) < minimum && !ids.contains($0.id) }
    let merged = incoming + tail
    let kept = Array(merged.prefix(max(incoming.count, capacity)))
    let cursor = kept.count < merged.count ? kept.last(where: { $0.value["active"].bool != true })?.value["sequence"].number : tail.isEmpty ? incomingCursor : previousCursor
    return UsageFirstPage(records: kept, nextCursor: cursor, preservedHistory: !tail.isEmpty)
}

struct UsageSourceSlice: Identifiable {
    let id: String
    let name: String
    let value: Double
    let tokenTotal: Double
    let cost: Double?
    let estimated: Bool
    var costEstimated = true
}

func usageDistributionSlices(_ groups: [JSONValue], groupBy: String) -> [UsageSourceSlice] {
    let models = groupBy == "model"
    let rows = usageComparisons(groups, key: groupBy, sort: models ? "tokens" : "cost")
    var ranked: [UsageSourceSlice] = [], forcedOther: [UsageSourceSlice] = []
    for row in rows {
        let value = models ? row.totals.total : row.totals.cost
        guard let value, value.isFinite, value > 0 else { continue }
        let slice = UsageSourceSlice(id: row.id, name: row.name, value: value, tokenTotal: row.totals.total, cost: row.totals.cost, estimated: row.totals.estimated, costEstimated: row.totals.costEstimated)
        let unknownName = ["", "unknown", "unknown_model", "未知模型"].contains(row.id.trimmingCharacters(in: .whitespacesAndNewlines).lowercased())
        // Aggregates currently merge unknown model and tier prices into unpriced_requests.
        let unlisted = row.totals.sum("unpriced_requests") > 0 && row.totals.sum("priced_requests") == 0
        if models && (unknownName || unlisted) { forcedOther.append(slice) }
        else { ranked.append(slice) }
    }
    var slices = Array(ranked.prefix(3))
    let other = Array(ranked.dropFirst(3)) + forcedOther
    if !other.isEmpty {
        let knownCosts = other.compactMap(\.cost)
        slices.append(UsageSourceSlice(id: "usage-other-sources", name: "其他", value: other.reduce(0) { $0 + $1.value }, tokenTotal: other.reduce(0) { $0 + $1.tokenTotal }, cost: knownCosts.isEmpty ? nil : knownCosts.reduce(0, +), estimated: other.contains(where: \.estimated), costEstimated: other.contains(where: \.costEstimated)))
    }
    return slices
}

struct UsageActivityLayout {
    let weeks: Int
    let cell: Double
    let height: Double
    init(width: Double, days: Int, leading: Int) {
        weeks = max(1, Int(ceil(Double(days + leading) / 7)))
        cell = min(24, max(4, (width - 21) / Double(weeks) - 3))
        height = cell * 7 + 18
    }
}

/// Coordinates are relative to the ring center; twelve o'clock starts a clockwise turn.
func usageRingHit(x: Double, y: Double, outerRadius: Double, innerRatio: Double, slices: [UsageSourceSlice]) -> String? {
    guard x.isFinite, y.isFinite, outerRadius.isFinite, outerRadius > 0 else { return nil }
    let radius = hypot(x, y)
    guard radius <= outerRadius, radius >= outerRadius * innerRatio else { return nil }
    let positive = slices.filter { $0.value.isFinite && $0.value > 0 }
    let total = positive.reduce(0) { $0 + $1.value }
    guard total.isFinite, total > 0 else { return nil }
    var angle = atan2(y, x) + .pi / 2
    if angle < 0 { angle += 2 * .pi }
    let target = angle / (2 * .pi) * total
    var upper = 0.0
    for slice in positive {
        upper += slice.value
        if target < upper { return slice.id }
    }
    return positive.last?.id
}
