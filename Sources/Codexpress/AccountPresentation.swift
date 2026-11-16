import SwiftUI

enum AccountTypography {
    static let value = Font.system(size: 22, weight: .semibold, design: .rounded)
    static let compactValue = Font.system(size: 16, weight: .semibold, design: .rounded)
}

enum AccountLevel {
    case unknown, healthy, caution, low

    var color: Color {
        switch self {
        case .unknown: return .secondary
        case .healthy: return AppPalette.accountHealthy
        case .caution: return AppPalette.accountCaution
        case .low: return AppPalette.accountLow
        }
    }

    static func balance(_ amount: Double?, threshold: Double?) -> Self {
        guard let amount, amount.isFinite, let threshold, threshold.isFinite, threshold >= 0 else { return .unknown }
        if amount <= threshold { return .low }
        if threshold == 0 || amount / threshold > 5 { return .healthy }
        return .caution
    }

    static func quota(_ remaining: Double?) -> Self {
        guard let remaining, remaining.isFinite else { return .unknown }
        return remaining > 30 ? .healthy : remaining > 10 ? .caution : .low
    }

    static func quotaReference(_ reference: Double, remaining: Double?) -> Self {
        guard reference.isFinite, let remaining, remaining.isFinite else { return .unknown }
        let difference = reference - remaining
        return difference >= 10 ? .low : difference >= 5 ? .caution : .unknown
    }
}

struct BalanceAmount {
    let unit: String
    let number: String

    init(_ amount: Double, currency: String) {
        let formatter = NumberFormatter()
        formatter.locale = Locale(identifier: "zh_CN")
        formatter.numberStyle = .currency
        formatter.currencyCode = currency
        unit = formatter.currencySymbol ?? currency
        number = amount.formatted(.number.locale(Locale(identifier: "zh_CN"))
            .precision(.fractionLength(amount != 0 && abs(amount) < 0.01 ? 4 : 2)))
    }
}

struct QuotaReferenceLabel: View {
    let window: QuotaWindow
    var compact = false

    var body: some View {
        if window.seconds == 604800 && (window.resetAt != nil || window.refCost != nil) {
            TimelineView(.periodic(from: .now, by: 60)) { context in
                if let reference = window.referenceCost(at: context.date) {
                    Text("ref \(reference.formatted(.number.locale(Locale(identifier: "en_US_POSIX")).precision(.fractionLength(1))))%")
                        .font(.system(size: compact ? 10 : 11)).monospacedDigit().fixedSize()
                        .foregroundStyle(AccountLevel.quotaReference(reference, remaining: window.remaining).color)
                        .help("本周期剩余时间百分比")
                }
            }
        }
    }
}

private struct WindowActivityKey: EnvironmentKey {
    static let defaultValue = true
}
extension EnvironmentValues {
    var windowIsActive: Bool {
        get { self[WindowActivityKey.self] }
        set { self[WindowActivityKey.self] = newValue }
    }
}

struct ActivityDot: View {
    var color: Color = .secondary
    var active = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.windowIsActive) private var windowIsActive

    var body: some View {
        Image(systemName: "circle.fill").font(.system(size: 5))
            .foregroundStyle(active ? .green : color)
            .symbolEffect(.pulse, options: .repeating, isActive: active && windowIsActive && !reduceMotion)
            .frame(width: 5, height: 5)
            .accessibilityLabel(active ? "正在处理请求" : "")
    }
}
