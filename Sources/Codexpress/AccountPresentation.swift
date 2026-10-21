import SwiftUI

enum AccountTypography {
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

struct ActivityDot: View {
    var color: Color = .secondary
    var active = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Image(systemName: "circle.fill").font(.system(size: 5))
            .foregroundStyle(active ? .green : color)
            .symbolEffect(.pulse, options: .repeating, isActive: active && !reduceMotion)
            .frame(width: 5, height: 5)
            .accessibilityLabel(active ? "正在处理请求" : "")
    }
}
