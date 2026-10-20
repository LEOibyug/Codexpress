import SwiftUI

struct OfficialAccountView: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    @State private var creditsExpanded = false

    var body: some View {
        let live = model.state(source.id)
        let quota = live["quota"]
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Text(quota["plan_type"].string ?? "套餐未知").font(.system(size: 10, weight: .medium))
                    .foregroundStyle(.secondary).padding(.horizontal, 8).padding(.vertical, 5)
                    .background(AppPalette.well, in: Capsule())
                if let account = live["account_id"].string {
                    Text(account).font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1).textSelection(.enabled).help("账号／工作区：\(account)")
                }
                Spacer(minLength: 0)
                Button(live["account_id"].string == nil ? "登录…" : "重新登录…") { model.login(source) }.disabled(model.busy || model.loginFlow != nil)
                Button { model.refresh(source.id) } label: { Label("刷新额度", systemImage: "arrow.clockwise") }.disabled(model.busy)
            }.controlSize(.small)
            if quota["windows"].array.isEmpty {
                InfoNote(text: "额度尚未知，查询失败不表示额度耗尽。")
            } else {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 220), spacing: 12)], spacing: 12) {
                    ForEach(Array(quota["windows"].array.enumerated()), id: \.offset) { _, window in
                        quotaWindow(window)
                    }
                }
            }
            if quota != .null {
                HStack(spacing: 5) {
                    Image(systemName: "clock").font(.system(size: 9))
                    Text("更新于 \(timestampLabel(quota["fetched_at"]))" + (quota["stale"].bool == true ? " · 陈旧缓存" : ""))
                }.font(.system(size: 10)).foregroundStyle(quota["stale"].bool == true ? Color.orange : Color.secondary)
            }
            if let error = quota["error"].string { InfoNote(text: error, color: .orange) }
            DisclosureGroup(isExpanded: $creditsExpanded) {
                ResetCreditsView(source: source).padding(.top, 9)
            } label: {
                HStack(spacing: 7) {
                    Text("重置卡")
                    Text("可用 \(countLabel(live["credits"]["available_count"]))").foregroundStyle(.secondary)
                }.font(.system(size: 11))
            }
            if model.loginSource == source.id {
                VStack(alignment: .leading, spacing: 10) {
                    Text(model.loginMessage ?? "等待浏览器授权…").font(.system(size: 12, weight: .medium))
                    InfoNote(text: "授权网页遵循浏览器的网络设置。若未自动完成，可粘贴完整 localhost 回调地址。")
                    SecureField("完整回调地址（包含一次性授权码）", text: $model.callbackURL).settingsField()
                    HStack {
                        Button("完成登录") { model.completeLogin() }.buttonStyle(.borderedProminent).disabled(model.callbackURL.isEmpty || model.busy || model.loginFlow == nil)
                        Button("取消等待") { model.cancelLogin() }.disabled(model.busy)
                    }.controlSize(.small)
                }.padding(14).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
            } else if model.loginMessage == "官方账号已连接。" {
                Label("官方账号已连接", systemImage: "checkmark.circle").font(.system(size: 11)).foregroundStyle(.green)
            }
        }
    }

    private func quotaWindow(_ window: JSONValue) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text(windowName(window["name"].string)).font(.system(size: 11)).foregroundStyle(.secondary)
                Spacer()
                Text(window["used_percent"].number.map { "\(Int(max(0, 100 - $0)))%" } ?? "未知")
                    .font(.system(size: 20, weight: .medium, design: .rounded)).monospacedDigit()
                Text("剩余").font(.system(size: 10)).foregroundStyle(.secondary)
            }
            if let used = window["used_percent"].number {
                ProgressView(value: max(0, min(100, 100 - used)), total: 100).tint(used >= 90 ? .orange : .accentColor).controlSize(.small)
            }
            Text("重置于 \(timestampLabel(window["reset_at"]))").font(.system(size: 10)).foregroundStyle(.secondary)
        }.padding(12).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
    }

    private func windowName(_ name: String?) -> String {
        switch name { case "primary": return "主要窗口"; case "secondary": return "次要窗口"; default: return name ?? "用量窗口" }
    }
}
struct ResetCreditsView: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    @State private var confirmation: CreditConfirmation?
    @State private var refreshing = false
    @State private var localError: String?
    var body: some View {
        let credits = model.state(source.id)["credits"]
        VStack(alignment: .leading, spacing: 10) {
            if credits["stale"].bool == true { Label("卡片信息已陈旧，请刷新后使用。", systemImage: "clock").font(.caption).foregroundStyle(.orange) }
            if let message = credits["error"].string { Text(message).font(.caption).foregroundStyle(.orange) }
            if credits["credits"].array.isEmpty { Text(credits["available_count"].number == 0 ? "暂无可用重置卡。" : "卡片信息未知，尚无可操作卡片。").font(.caption).foregroundStyle(.secondary) }
            ForEach(Array(credits["credits"].array.enumerated()), id: \.offset) { _, credit in
                let key = source.id + ":" + (credit["id"].string ?? "")
                HStack(alignment: .top) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(credit["title"].string ?? "重置卡").font(.callout)
                        Text("状态 · \(creditStatus(credit["status"].string))  到期 · \(timestampLabel(credit["expires_at"]))").font(.caption).foregroundStyle(.secondary)
                        if let result = model.resetResults[key] { Text(result).font(.caption).foregroundStyle(.orange) }
                    }
                    Spacer()
                    Button(model.consuming.contains(key) ? "提交中…" : "使用…") { prepare(credit) }
                        .disabled(refreshing || model.busy || model.consuming.contains(key) || model.consuming.contains("all") || credit["expiry_known"].bool != true || model.resetResults[key] != nil || credit["status"].string != "available" || credit["id"].string?.isEmpty != false || credits["stale"].bool == true)
                }
            }
            if let localError { Text(localError).font(.caption).foregroundStyle(.orange) }
            Text("仅手动使用；不自动消耗卡片。本地累计用量不会清零。").font(.caption2).foregroundStyle(.secondary)
        }.sheet(item: $confirmation) { item in
            VStack(alignment: .leading, spacing: 16) {
                Text("确认使用重置卡").font(.title2.bold())
                LabeledContent("来源", value: item.source.name)
                LabeledContent("账号／工作区", value: item.accountID)
                LabeledContent("卡片", value: item.credit["title"].string ?? "重置卡")
                LabeledContent("到期时间", value: timestampLabel(item.credit["expires_at"]))
                Divider()
                ForEach(Array(item.quota["windows"].array.enumerated()), id: \.offset) { _, window in Text("\(window["name"].string ?? "用量窗口")：已用 \(window["used_percent"].number.map { "\(Int($0))%" } ?? "未知")").font(.callout) }
                Text("当前用量较低时可能浪费卡片。具体重置哪些窗口以官方返回和后续查询为准。提交结果不明时必须先核实，不会自动再次消耗。").font(.callout).foregroundStyle(.secondary)
                HStack { Spacer(); Button("取消") { confirmation = nil }.keyboardShortcut(.cancelAction); Button("确认使用") { model.consume(item); confirmation = nil }.buttonStyle(.borderedProminent) }
            }.padding(24).frame(width: 470)
        }
    }
    private func prepare(_ credit: JSONValue) {
        refreshing = true; localError = nil
        Task {
            do { confirmation = try await model.prepareCredit(source: source, credit: credit) }
            catch { localError = error.localizedDescription }
            refreshing = false
        }
    }
    private func creditStatus(_ value: String?) -> String { switch value { case "available": return "可用"; case "consumed", "redeemed": return "已使用"; case "expired": return "已过期"; default: return "未知" } }
}
