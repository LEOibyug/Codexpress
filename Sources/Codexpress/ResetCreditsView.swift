import SwiftUI

struct OfficialAccountView: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    @State private var creditsExpanded = false
    var body: some View {
        let quota = model.state(source.id)["quota"]
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text(quota["plan_type"].string.map { "套餐 · \($0)" } ?? "套餐未知").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("登录…") { model.login(source) }.disabled(model.busy || model.loginFlow != nil)
                Button { model.refresh(source.id) } label: { Label("刷新额度", systemImage: "arrow.clockwise") }.disabled(model.busy)
            }
            if let account = model.state(source.id)["account_id"].string { Text("账号／工作区 · \(account)").font(.caption).foregroundStyle(.secondary).textSelection(.enabled) }
            if quota["windows"].array.isEmpty { Text("额度尚未知；查询失败不表示额度耗尽。").font(.caption).foregroundStyle(.secondary) }
            ForEach(Array(quota["windows"].array.enumerated()), id: \.offset) { _, window in
                VStack(alignment: .leading, spacing: 5) {
                    HStack {
                        Text(window["name"].string ?? "用量窗口")
                        Spacer()
                        Text(window["used_percent"].number.map { "剩余 \(Int(max(0, 100 - $0)))%" } ?? "剩余额度未知")
                    }.font(.caption)
                    if let used = window["used_percent"].number { ProgressView(value: max(0, min(100, 100 - used)), total: 100).tint(used >= 90 ? .orange : .accentColor) }
                    Text("重置时间 · \(timestampLabel(window["reset_at"]))").font(.caption2).foregroundStyle(.secondary)
                }
            }
            if quota != .null { Text("更新 · \(timestampLabel(quota["fetched_at"]))" + (quota["stale"].bool == true ? " · 陈旧缓存" : "")).font(.caption2).foregroundStyle(quota["stale"].bool == true ? .orange : .secondary) }
            if let error = quota["error"].string { Text(error).font(.caption).foregroundStyle(.orange) }
            DisclosureGroup(isExpanded: $creditsExpanded) { ResetCreditsView(source: source).padding(.top, 8) } label: { Text("重置卡 · 可用 \(countLabel(model.state(source.id)["credits"]["available_count"]))").font(.callout) }
            if model.loginSource == source.id {
                VStack(alignment: .leading, spacing: 8) {
                    Text(model.loginMessage ?? "等待浏览器授权…").font(.callout)
                    Text("授权网页使用系统浏览器的网络设置。若未自动完成，可粘贴浏览器地址栏中的完整 localhost 回调地址。").font(.caption).foregroundStyle(.secondary)
                    SecureField("完整回调地址（包含一次性授权码）", text: $model.callbackURL)
                    HStack {
                        Button("完成登录") { model.completeLogin() }.disabled(model.callbackURL.isEmpty || model.busy || model.loginFlow == nil)
                        Button("取消等待") { model.cancelLogin() }.disabled(model.busy)
                    }
                }.padding(10).background(.blue.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
            } else if model.loginMessage == "官方账号已连接。" { Text("官方账号已连接。").font(.caption).foregroundStyle(.green) }
        }
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
