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
    var averageLatency: Double? { finished > 0 ? sum("total_latency_ms") / finished : nil }
    var firstToken: Double? { sum("first_token_samples") > 0 ? sum("total_first_token_ms") / sum("first_token_samples") : nil }
    var tps: Double? { sum("throughput_latency_ms") > 0 ? sum("throughput_output_tokens") * 1000 / sum("throughput_latency_ms") : nil }
    var cacheRate: Double? { sum("cached_reported_input_tokens") > 0 ? min(100, sum("cached_reported_tokens") * 100 / sum("cached_reported_input_tokens")) : nil }
    var cost: Double? { sum("priced_requests") > 0 ? sum("total_usd") : nil }
    var averageCost: Double? { sum("priced_requests") > 0 ? sum("total_usd") / sum("priced_requests") : nil }
    var savings: Double? { sum("priced_requests") > 0 ? sum("cache_savings_usd") : nil }
    var costCoverage: String {
        let priced = Int(sum("priced_requests")), total = Int(sum("requests"))
        return "已计价 \(priced) / \(total) 次"
    }
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
    case "official_subscription_reference": return "官方订阅参考折算"
    case "subscription_reference": return "订阅来源参考折算"
    default: return cost["source_type_known"].bool == false ? "API 参考折算（历史来源类型未知）" : "API 参考折算"
    }
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
