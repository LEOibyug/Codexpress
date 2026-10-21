import SwiftUI

struct APIBalanceView: View {
    @EnvironmentObject var model: AppModel
    let sourceID: String
    var compact = false
    @State private var showUpdating = false

    var body: some View {
        let balance = model.apiBalances[sourceID] ?? .null
        let amount = balance["amount"].number
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text("余额").foregroundStyle(.secondary)
            if !compact { Spacer(minLength: 8) }
            if let amount, amount.isFinite, let currency = balance["currency"].string {
                let money = BalanceAmount(amount, currency: currency)
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    Text(money.unit).font(.system(size: compact ? 10 : 12, weight: .medium))
                    Text(money.number).font(.system(size: compact ? 14 : 18, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                }.foregroundStyle(AccountLevel.balance(amount, threshold: model.sources.first { $0.id == sourceID }?.balanceWarningThreshold).color)
                    .textSelection(.enabled).lineLimit(1).minimumScaleFactor(0.8)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(balanceMoney(amount, currency: currency))
                if balance["stale"].bool == true && !model.refreshingBalances.contains(sourceID) {
                    Image(systemName: "clock").foregroundStyle(.orange).help("更新失败，显示上次获取的余额")
                }
            } else {
                Text(model.refreshingBalances.contains(sourceID) ? "查询中…" : balance == .null ? "未查询" : "无法获取")
                    .foregroundStyle(.secondary)
            }
            if showUpdating { ProgressView().controlSize(.mini).help("正在更新余额") }
        }.font(.system(size: compact ? 10 : 12))
            .help(balance["updated_at"].number.map { "\(balance["scope"].string == "key" ? "当前密钥可用余额" : "账户总余额") · 更新于 \(timestampLabel(.number($0)))" } ?? "供应商需支持余额查询")
            .task(id: model.refreshingBalances.contains(sourceID)) {
                if model.refreshingBalances.contains(sourceID) {
                    do { try await Task.sleep(for: .milliseconds(150)) } catch { return }
                    showUpdating = true
                } else { showUpdating = false }
            }
    }
}

func balanceMoney(_ amount: Double, currency: String) -> String {
    let money = BalanceAmount(amount, currency: currency)
    return money.unit + " " + money.number
}
