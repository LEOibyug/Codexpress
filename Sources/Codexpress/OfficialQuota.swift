import Foundation

struct QuotaWindow: Identifiable {
    let id: String
    let title: String
    let remaining: Double?
    let resetAt: Double?
    let seconds: Double?
    let name: String
    let refCost: Double?

    var remainingLabel: String { remaining.map { $0 > 0 && $0 < 1 ? "<1%" : "\(Int($0))%" } ?? "未知" }
    var resetLabel: String { resetAt.map { "\(timestampLabel(.number($0))) 重置" } ?? "重置时间未知" }

    func referenceCost(at date: Date) -> Double? {
        guard seconds == 604800 else { return nil }
        guard let resetAt else { return refCost }
        return max(0, min(100, (resetAt - date.timeIntervalSince1970) / 604800 * 100))
    }

    init(_ raw: JSONValue, index: Int) {
        name = raw["name"].string ?? ""
        id = "\(name):\(index)"
        seconds = raw["window_seconds"].number.flatMap { $0.isFinite && $0 > 0 && $0 <= 31536000 ? $0 : nil }
        remaining = raw["used_percent"].number.flatMap { $0.isFinite ? max(0, min(100, 100 - $0)) : nil }
        resetAt = raw["reset_at"].number.flatMap { $0.isFinite && $0 > 0 && $0 <= 253402300799 ? $0 : nil }
        refCost = seconds == 604800 ? raw["ref_cost"].number.flatMap { $0.isFinite && (0...100).contains($0) ? $0 : nil } : nil
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
        remaining = nil; resetAt = nil; seconds = nil; refCost = nil
    }
}

struct QuotaOverview {
    let mainWindows: [QuotaWindow]
    let windows: [QuotaWindow]
    let hasShortWindow: Bool

    init(_ quota: JSONValue) {
        let parsed = quota["windows"].array.enumerated().map { QuotaWindow($0.element, index: $0.offset) }
        let base = parsed.filter { !$0.name.contains(":") }
        let weekly = base.first { $0.seconds == 604800 }
        let short = base.first { $0.seconds.map { $0 < 604800 } == true }
            ?? base.first { $0.name == "primary" && $0.seconds == nil }
        hasShortWindow = short != nil
        mainWindows = [short, weekly ?? QuotaWindow(placeholder: "weekly", title: "每周")].compactMap { $0 }
        let selected = Set(mainWindows.map(\.id))
        windows = mainWindows + parsed.filter { !selected.contains($0.id) }
    }
}

struct OfficialRefreshSchedule {
    private var quotaAttempts: [String: Date] = [:]
    private var creditAttempts: [String: Date] = [:]
    private var quotaRetries: [String: Date] = [:]
    private var lastConfirmedQuota: [String: Double] = [:]

    func isRetrying(_ id: String) -> Bool { quotaRetries[id] != nil }

    private func deadline(_ id: String, foreground: Bool, interval: TimeInterval, since start: Date) -> Date {
        quotaRetries[id] ?? (quotaAttempts[id] ?? (foreground ? .distantPast : start)).addingTimeInterval(foreground ? 15 : interval)
    }

    func isDue(_ id: String, now: Date = Date(), foreground: Bool = false, interval: TimeInterval = 1800, since start: Date = .distantPast) -> Bool {
        now >= deadline(id, foreground: foreground, interval: interval, since: start)
    }
    func backgroundDelay(for ids: some Sequence<String>, since start: Date, interval: TimeInterval, now: Date = Date()) -> TimeInterval {
        var delay: TimeInterval?
        for id in ids {
            let remaining = deadline(id, foreground: false, interval: interval, since: start).timeIntervalSince(now)
            // Busy or ineligible accounts do not spin when past their deadline.
            delay = min(delay ?? 1800, remaining > 0 ? remaining : (isRetrying(id) ? 5 : 15))
        }
        return delay ?? min(interval, 1800)
    }
    func needsCredits(_ id: String, now: Date = Date()) -> Bool {
        now.timeIntervalSince(creditAttempts[id] ?? .distantPast) >= 300
    }
    mutating func record(_ id: String, includeCredits: Bool, now: Date = Date()) {
        quotaAttempts[id] = now
        if includeCredits { creditAttempts[id] = now }
    }
    @discardableResult mutating func observeQuota(_ id: String, snapshot: JSONValue, queried: Bool = false, now: Date = Date()) -> Bool {
        if snapshot["stale"].bool == false, snapshot["complete"].bool == true,
           snapshot["error"] == .null, let fetchedAt = snapshot["fetched_at"].number {
            guard queried || lastConfirmedQuota[id] != fetchedAt else { return false }
            lastConfirmedQuota[id] = fetchedAt
            quotaAttempts[id] = now
            quotaRetries[id] = nil
            return true
        }
        let failed = (snapshot["stale"].bool == true && (snapshot["error"].string != nil || (snapshot["failure_count"].number ?? 0) > 0))
            || (snapshot["stale"].bool == false && snapshot["complete"].bool == false)
        if failed { lastConfirmedQuota[id] = nil }
        guard queried || (failed && quotaRetries[id] == nil) else { return false }
        quotaQueryFailed(id, now: now)
        return true
    }
    mutating func quotaQueryFailed(_ id: String, now: Date = Date()) {
        quotaRetries[id] = now.addingTimeInterval(5)
    }
    mutating func reset(_ id: String) {
        quotaAttempts[id] = nil; creditAttempts[id] = nil
        quotaRetries[id] = nil; lastConfirmedQuota[id] = nil
    }
}
