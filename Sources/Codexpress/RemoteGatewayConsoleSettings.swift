import SwiftUI

struct RemoteGeneralSettingsPage: View {
    @ObservedObject var draft: RemotePreferencesDraft

    var body: some View {
        if let target = draft.target, let config = draft.config {
            GeneralSettingsPage(subtitle: "登录时启动仅影响此 Mac；网关设置提交至 \(target.endpoint)。") {
                OfficialQuotaSettingsCard(minutes: draft.integerText("official_quota_refresh_minutes", default: 30, range: 1...1440),
                    subtitle: "提交后立即生效，不影响前台或手动刷新。",
                    error: draft.settings["official_quota_refresh_minutes"]?.string == nil ? nil : "请输入 1–1440 之间的整数分钟。",
                    detail: "服务器的查询间隔为 1–1440 分钟。")
                RemoteHistoryCacheCard(draft: draft, target: target)
                RemoteRoutingSettingsCards(draft: draft, target: target)
                RemoteWeeklyResetCard(draft: draft, target: target, config: config)
            }
        }
    }

}

struct RemoteRoutingSettingsCards: View {
    @ObservedObject var draft: RemotePreferencesDraft
    let target: RemoteConsoleTarget

    var body: some View {
        if let all = try? RemotePoolDraft(draft, target: target).list() {
            let sources = all.filter(\.enabled)
            RoutingSettingsCards(enabledSources: sources, sourceName: \.name,
                mode: Binding(get: { draft.settings["mode"]?.string ?? "auto" }, set: { mode in
                    draft.settings["mode"] = .string(mode)
                    if mode == "auto" { draft.settings["fixed_source_id"] = .null }
                    else if !sources.contains(where: { $0.id == draft.settings["fixed_source_id"]?.string }), let first = sources.first {
                        draft.settings["fixed_source_id"] = .string(first.id)
                    }
                }),
                fixedSource: Binding(get: { draft.settings["fixed_source_id"]?.string ?? "" }, set: { draft.settings["fixed_source_id"] = .string($0) }),
                loadBalance: flag("load_balance"), quotaThreshold: integer("official_low_quota_percent", default: 10),
                attempts: integer("upstream_attempts", default: 3), retryDelay: integer("upstream_retry_delay_seconds", default: 3),
                idleMinutes: integer("idle_return_minutes", default: 5), rebuild: flag("degraded_context_fallback", default: true),
                customIdleMinutes: draft.integerText("idle_return_minutes", default: 5, range: 0...10080))
        } else { InfoNote(text: "无法读取来源配置，请重新载入。", color: .orange) }
    }

    private func flag(_ key: String, default fallback: Bool = false) -> Binding<Bool> {
        Binding(get: { draft.settings[key]?.bool ?? fallback }, set: { draft.settings[key] = .bool($0) })
    }

    private func integer(_ key: String, default fallback: Int) -> Binding<Int> {
        Binding(get: { draft.settings[key]?.number.flatMap { Int(exactly: $0) } ?? fallback }, set: { draft.settings[key] = .number(Double($0)) })
    }
}

private extension RemotePreferencesDraft {
    func integerText(_ key: String, default fallback: Int, range: ClosedRange<Int>) -> Binding<String> {
        Binding(get: { self.settings[key]?.string ?? String(self.settings[key]?.number.flatMap { Int(exactly: $0) } ?? fallback) }, set: { text in
            if let value = Int(text.trimmingCharacters(in: .whitespacesAndNewlines)), range.contains(value) {
                self.settings[key] = .number(Double(value))
            } else { self.settings[key] = .string(text) }
        })
    }
}

private struct WeeklyResetAuthorization {
    let target: RemoteConsoleTarget
    let source: RemotePoolSource
    let binding: String
}

private struct RemoteWeeklyResetCard: View {
    @ObservedObject var draft: RemotePreferencesDraft
    let target: RemoteConsoleTarget
    let config: RemoteConsoleConfig
    @State private var authorizing: WeeklyResetAuthorization?

    private var enrolled: [JSONValue] { draft.settings["auto_reset_weekly_accounts"]?.array ?? [] }

    var body: some View {
        let accounts = target.pool.sources.filter { $0.kind == "official" }
        SettingsCard("自动使用周重置卡", subtitle: "默认关闭。授权后，服务器可在该账号周额度耗尽时自动使用卡片，无需本机在线。", symbol: "calendar.badge.clock") {
            if accounts.isEmpty { Text("没有官方账号").foregroundStyle(.secondary) }
            ForEach(accounts) { source in
                let binding = draft.sources.first { $0["id"].string == source.id }?["reset_consent_binding"].string
                let id = JSONValue.string(source.id)
                SettingRow(title: source.name) {
                    Toggle(source.name, isOn: Binding(get: { enrolled.contains(id) }, set: { enabled in
                        if enabled {
                            if let binding, !binding.isEmpty { authorizing = .init(target: target, source: source, binding: binding) }
                        } else { updateEnrollment(enrolled.filter { $0 != id }) }
                    })).toggleStyle(.switch).labelsHidden().disabled(binding?.isEmpty != false && !enrolled.contains(id))
                }
            }
        }
        .alert("允许此账号自动消耗周重置卡？", isPresented: Binding(get: { authorizing != nil }, set: { if !$0 { authorizing = nil } }), presenting: authorizing) { request in
            Button("取消", role: .cancel) {}
            Button("允许") {
                let id = JSONValue.string(request.source.id)
                guard request.target.current, draft.target?.generation == request.target.generation, !draft.submitting,
                      draft.sources.first(where: { $0["id"] == id })?["reset_consent_binding"].string == request.binding,
                      !enrolled.contains(id) else { return }
                updateEnrollment(enrolled + [id])
            }
        } message: { Text("\($0.target.endpoint) 上的「\($0.source.name)」可在周额度耗尽时自动使用卡片。提交修改后生效。") }
    }

    private func updateEnrollment(_ next: [JSONValue]) {
        let key = "auto_reset_weekly_accounts"
        let original = config.settings[key]?.array ?? []
        if next.count == original.count, original.allSatisfy(next.contains) {
            draft.settings[key] = config.settings[key]
        } else { draft.settings[key] = .array(next) }
    }
}

private struct RemoteHistoryCacheCard: View {
    @ObservedObject var draft: RemotePreferencesDraft
    let target: RemoteConsoleTarget
    @State private var clearing: RemoteConsoleTarget?
    @State private var busy = false
    @State private var failed = false

    private var validationError: String? {
        do { _ = try HistoryCacheSettings.parse(enabled: draft.historyEnabled, gigabytes: draft.historyCapacity); return nil }
        catch { return error.localizedDescription }
    }

    var body: some View {
        if draft.historyStatus["enabled"].bool != nil, draft.historyStatus["max_bytes"].number != nil {
            HistoryCacheSettingsCard(title: "服务器原文缓存", scope: "默认关闭。开启后仅在此服务器保存用于重建上下文的对话原文；关闭不会清空已有缓存。",
                enabled: $draft.historyEnabled, capacity: $draft.historyCapacity, status: draft.historyStatus,
                validationError: failed ? "未确认缓存是否已清空，请刷新后核对。" : validationError,
                canClear: target.current, clear: { clearing = target })
                .disabled(busy || draft.submitting)
                .alert("清空远端原文缓存？", isPresented: Binding(get: { clearing != nil }, set: { if !$0 { clearing = nil } }), presenting: clearing) { captured in
                    Button("取消", role: .cancel) {}
                    Button("清空", role: .destructive) { clear(captured) }
                } message: { Text("将立即删除 \($0.endpoint) 上已有的原文缓存。依赖这些块的对话可能无法恢复；此操作不可撤销。") }
        } else {
            SettingsCard("服务器原文缓存", symbol: "externaldrive") {
                InfoNote(text: "无法确认服务器的原文缓存设置，请刷新后核对。", color: .orange)
            }
        }
    }

    private func clear(_ captured: RemoteConsoleTarget) {
        guard captured.current, !busy, !draft.submitting else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                _ = try await captured.call("history.clear", ["confirmed": .bool(true)])
                failed = false
                if captured.current { await draft.reloadHistory() }
            } catch { failed = true }
        }
    }
}
