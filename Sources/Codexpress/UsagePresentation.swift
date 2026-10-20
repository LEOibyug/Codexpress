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
    var tps: Double? { sum("total_generation_ms") > 0 ? sum("timed_output_tokens") * 1000 / sum("total_generation_ms") : nil }
    var cacheRate: Double? { sum("cached_reported_input_tokens") > 0 ? min(100, sum("cached_reported_tokens") * 100 / sum("cached_reported_input_tokens")) : nil }
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
