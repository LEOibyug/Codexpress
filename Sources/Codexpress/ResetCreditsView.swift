import SwiftUI

struct ResetCreditsSheet: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let request: CreditResetRequest
    @State private var confirmation: CreditConfirmation?
    @State private var refreshing = false
    @State private var localError: String?
    @State private var quotaRecovered = false

    // A sheet never follows a source into a different account after it has opened.
    private var source: Source? {
        model.settings.sources.first {
            $0.id == request.sourceID && $0.official && $0.account_id == request.accountID
                && $0.account_id != nil && model.state($0.id)["account_id"].string == request.accountID
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 7) {
                    Text(confirmation == nil ? "重置卡" : "确认使用重置卡").font(.system(size: 22, weight: .semibold))
                    if let source {
                        Text(source.name).font(.system(size: 14, weight: .medium))
                        Text(request.accountID ?? "").font(AppTypography.small).foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                }
                Spacer()
                if refreshing { ProgressView().controlSize(.small) }
                else if confirmation == nil, let source {
                    Button { Task { await refresh(source: source, quick: false) } } label: {
                        Image(systemName: "arrow.clockwise")
                    }.help("刷新额度与卡片").disabled(model.refreshingQuotas.contains(source.id))
                }
            }
            Divider()
            if let source {
                if let confirmation { confirmationContent(confirmation) }
                else { inventoryContent(source) }
            } else {
                InfoNote(text: "来源或账号已变化，请关闭后重新选择。", color: .orange)
            }
            if quotaRecovered { Label("额度已恢复", systemImage: "checkmark.circle").foregroundStyle(.green) }
            if let localError { InfoNote(text: localError, color: .orange) }
            Divider()
            HStack {
                if confirmation != nil {
                    Button("返回卡片") { confirmation = nil }.disabled(refreshing)
                }
                Spacer()
                Button("关闭") { dismiss() }.keyboardShortcut(.cancelAction)
                if let confirmation {
                    Button("确认使用") {
                        model.consume(confirmation)
                        self.confirmation = nil
                    }.buttonStyle(.borderedProminent)
                        .disabled(source == nil || !model.canSubmitCredit(for: confirmation.source, creditID: confirmation.credit["id"].string ?? ""))
                }
            }
        }.font(AppTypography.body).padding(24).frame(width: 560)
            .overlay(alignment: .bottomLeading) { AppFeedbackOverlay(compact: true) }
            .task {
                guard let source else { return }
                await refresh(source: source, quick: request.quickReset)
            }
    }

    private func inventoryContent(_ source: Source) -> some View {
        let inventory = CreditInventory(model.state(source.id)["credits"])
        return VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("可用卡片").foregroundStyle(.secondary)
                Text(inventory.countLabel).font(.system(size: 24, weight: .semibold)).monospacedDigit()
                Spacer()
                if let fetched = inventory.snapshot["fetched_at"].number {
                    Text("更新于 \(timestampLabel(.number(fetched)))").font(AppTypography.small).foregroundStyle(.secondary)
                }
            }
            if inventory.snapshot["stale"].bool == true {
                InfoNote(text: "卡片信息待更新。", symbol: "clock", color: .orange)
            }
            if let message = inventory.snapshot["error"].string { InfoNote(text: message, color: .orange) }
            if inventory.cards.isEmpty {
                Text(inventory.availableCount == 0 ? "暂无可用重置卡" : "暂无卡片数据")
                    .foregroundStyle(.secondary).padding(.vertical, 16)
            } else {
                ScrollView {
                    VStack(spacing: 0) {
                        ForEach(Array(inventory.cards.enumerated()), id: \.offset) { index, credit in
                            if index > 0 { Divider().padding(.vertical, 12) }
                            cardRow(credit, source: source, inventory: inventory)
                        }
                    }.padding(14).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
                }.frame(height: min(CGFloat(inventory.cards.count) * 96, 310))
            }
        }
    }

    private func cardRow(_ credit: JSONValue, source: Source, inventory: CreditInventory) -> some View {
        let creditID = credit["id"].string ?? ""
        return HStack(spacing: 16) {
            Image(systemName: "ticket").font(.system(size: 20)).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 5) {
                Text(credit["title"].string ?? "重置卡").font(.system(size: 13, weight: .semibold))
                Text("\(creditStatus(credit["status"].string)) · 到期 \(timestampLabel(credit["expires_at"]))")
                    .font(AppTypography.small).foregroundStyle(.secondary)
                if let result = model.resetResult(for: source, creditID: creditID) {
                    Text(result).font(AppTypography.small).foregroundStyle(.orange)
                }
            }
            Spacer(minLength: 0)
            Button(model.isConsumingCredit(for: source, creditID: creditID) ? "提交中…" : "使用…") {
                Task { await prepare(credit, source: source) }
            }.buttonStyle(.bordered).controlSize(.small)
                .disabled(refreshing || model.busy || model.refreshingQuotas.contains(source.id) || !inventory.isFresh
                          || !CreditInventory.isUsable(credit) || !model.canSubmitCredit(for: source, creditID: creditID))
        }
    }

    private func confirmationContent(_ item: CreditConfirmation) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            LabeledContent("卡片", value: item.credit["title"].string ?? "重置卡")
            LabeledContent("到期时间", value: timestampLabel(item.credit["expires_at"]))
            ForEach(QuotaOverview(item.quota).mainWindows) { window in
                LabeledContent(window.title, value: "剩余 \(window.remainingLabel)")
            }
            Text("将消耗 1 张重置卡。").foregroundStyle(.secondary).padding(.top, 4)
        }
    }

    private func refresh(source: Source, quick: Bool) async {
        refreshing = true; localError = nil; quotaRecovered = false
        defer { refreshing = false }
        do {
            try await model.queryOfficial(source.id, includeCredits: true)
            guard !Task.isCancelled, let current = self.source else { return }
            if quick {
                let live = model.state(current.id)
                let quota = QuotaOverview(live["quota"])
                if !quota.isExhausted && live["health"].string != "exhausted" {
                    quotaRecovered = live["quota"]["stale"].bool == false && quota.mainWindows.contains { ($0.remaining ?? 0) > 0 }
                    return
                }
                if let card = CreditInventory(live["credits"]).usableCards.first(where: {
                    model.canSubmitCredit(for: current, creditID: $0["id"].string ?? "")
                }) {
                    confirmation = try CreditConfirmation.make(source: current, live: live, creditID: card["id"].string)
                }
            }
        } catch { if !Task.isCancelled { localError = error.localizedDescription } }
    }

    private func prepare(_ credit: JSONValue, source: Source) async {
        refreshing = true; localError = nil
        defer { refreshing = false }
        do {
            let prepared = try await model.prepareCredit(source: source, credit: credit)
            guard !Task.isCancelled, self.source != nil else { return }
            confirmation = prepared
        } catch { if !Task.isCancelled { localError = error.localizedDescription } }
    }

    private func creditStatus(_ value: String?) -> String {
        switch value {
        case "available": return "可用"
        case "consumed", "redeemed": return "已使用"
        case "expired": return "已过期"
        default: return "未知"
        }
    }
}
