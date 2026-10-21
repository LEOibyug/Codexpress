import Foundation

struct QuotaWindow: Identifiable {
    let id: String
    let title: String
    let remaining: Double?
    let resetAt: Double?
    let seconds: Double?
    let name: String

    var remainingLabel: String { remaining.map { "\(Int($0))%" } ?? "未知" }
    var resetLabel: String { resetAt.map { "\(timestampLabel(.number($0))) 重置" } ?? "重置时间未知" }

    init(_ raw: JSONValue, index: Int) {
        name = raw["name"].string ?? ""
        id = "\(name):\(index)"
        seconds = raw["window_seconds"].number.flatMap { $0.isFinite && $0 > 0 && $0 <= 31536000 ? $0 : nil }
        remaining = raw["used_percent"].number.map { max(0, min(100, 100 - $0)) }
        resetAt = raw["reset_at"].number.flatMap { $0.isFinite && $0 > 0 && $0 <= 253402300799 ? $0 : nil }
        let parts = name.split(separator: ":", omittingEmptySubsequences: false)
        let period: String
        switch seconds {
        case 604800: period = "每周"
        case 18000: period = "5 小时"
        case let value? where value.truncatingRemainder(dividingBy: 86400) == 0: period = "\(Int(value / 86400)) 天"
        case let value? where value.truncatingRemainder(dividingBy: 3600) == 0: period = "\(Int(value / 3600)) 小时"
        case let value?: period = "\(Int(value / 60)) 分钟"
        default: period = parts.last == "secondary" ? "次要额度" : "短周期"
        }
        title = parts.count > 1 ? "\(parts.dropLast().joined(separator: ":")) · \(period)" : period
    }

    init(placeholder name: String, title: String) {
        self.name = name; self.title = title; id = "missing:\(name)"
        remaining = nil; resetAt = nil; seconds = nil
    }
}

struct QuotaOverview {
    let mainWindows: [QuotaWindow]
    let windows: [QuotaWindow]

    init(_ quota: JSONValue) {
        let parsed = quota["windows"].array.enumerated().map { QuotaWindow($0.element, index: $0.offset) }
        let base = parsed.filter { !$0.name.contains(":") }
        let weekly = base.first { $0.seconds == 604800 }
        let short = base.first { $0.seconds.map { $0 < 604800 } == true }
            ?? base.first { $0.name == "primary" && $0.seconds == nil }
        mainWindows = [short ?? QuotaWindow(placeholder: "short", title: "短周期"),
                       weekly ?? QuotaWindow(placeholder: "weekly", title: "每周")]
        let selected = Set(mainWindows.map(\.id))
        windows = mainWindows + parsed.filter { !selected.contains($0.id) }
    }
}

struct OfficialRefreshSchedule {
    private var quotaAttempts: [String: Date] = [:]
    private var creditAttempts: [String: Date] = [:]

    func isDue(_ id: String, now: Date = Date(), foreground: Bool = false) -> Bool {
        now.timeIntervalSince(quotaAttempts[id] ?? .distantPast) >= (foreground ? 15 : 1800)
    }
    func needsCredits(_ id: String, now: Date = Date()) -> Bool {
        now.timeIntervalSince(creditAttempts[id] ?? .distantPast) >= 300
    }
    mutating func record(_ id: String, includeCredits: Bool, now: Date = Date()) {
        quotaAttempts[id] = now
        if includeCredits { creditAttempts[id] = now }
    }
    mutating func reset(_ id: String) { quotaAttempts[id] = nil; creditAttempts[id] = nil }
}
