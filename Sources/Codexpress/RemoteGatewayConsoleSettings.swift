import SwiftUI

struct RemoteConsoleRoutingEditor: View {
    @Environment(\.dismiss) private var dismiss
    let target: RemoteConsoleTarget
    let config: RemoteConsoleConfig
    @State private var values: [String: JSONValue]
    @State private var consent: Set<String>
    @State private var authorizing: RemotePoolSource?
    @State private var busy = false
    @State private var failed = false

    init(target: RemoteConsoleTarget, config: RemoteConsoleConfig) {
        self.target = target; self.config = config
        _values = State(initialValue: config.settings)
        _consent = State(initialValue: Set(config.settings["auto_reset_weekly_accounts"]?.array.compactMap(\.string) ?? []))
    }

    private func flag(_ key: String, default fallback: Bool = false) -> Binding<Bool> {
        Binding(get: { values[key]?.bool ?? fallback }, set: { values[key] = .bool($0) })
    }
    private func integer(_ key: String, _ fallback: Int) -> Binding<Int> {
        Binding(get: { values[key]?.number.flatMap { Int(exactly: $0) } ?? fallback }, set: { values[key] = .number(Double($0)) })
    }
    private func number(_ title: String, _ key: String, _ range: ClosedRange<Int>, _ fallback: Int) -> some View {
        LabeledContent(title) {
            let value = integer(key, fallback)
            TextField(title, value: value, format: .number.grouping(.never)).settingsField().frame(width: 76)
            Stepper(title, value: value, in: range).labelsHidden()
        }
    }
    private var valid: Bool {
        let limits = [("upstream_attempts", 1...10, 3), ("upstream_retry_delay_seconds", 0...300, 3), ("idle_return_minutes", 0...10080, 5), ("official_low_quota_percent", 1...99, 10), ("official_quota_refresh_minutes", 1...1440, 30)]
        return limits.allSatisfy { key, range, fallback in range.contains(integer(key, fallback).wrappedValue) } &&
        (values["mode"]?.string != "fixed" || target.pool.sources.contains { $0.enabled && $0.id == values["fixed_source_id"]?.string })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("远端分配设置").font(.title2)
            Text(target.endpoint).foregroundStyle(.secondary)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    SettingsCard("路由与重试", symbol: "arrow.triangle.branch") {
                        Picker("分配方式", selection: Binding(get: { values["mode"]?.string ?? "auto" }, set: { mode in
                            values["mode"] = .string(mode)
                            if mode == "auto" { values["fixed_source_id"] = .null }
                            else if values["fixed_source_id"]?.string == nil, let first = target.pool.sources.first(where: \.enabled) { values["fixed_source_id"] = .string(first.id) }
                        })) {
                            Text("自动切换").tag("auto"); Text("固定来源").tag("fixed").disabled(!target.pool.sources.contains(where: \.enabled))
                        }
                        if values["mode"]?.string == "fixed" {
                            Picker("固定来源", selection: Binding(get: { values["fixed_source_id"]?.string ?? "" }, set: { values["fixed_source_id"] = .string($0) })) {
                                ForEach(target.pool.sources.filter(\.enabled)) { Text($0.name).tag($0.id) }
                            }
                        } else { Toggle("按来源余量负载均衡", isOn: flag("load_balance")) }
                        Toggle("无法续接时重建上下文", isOn: flag("degraded_context_fallback", default: true))
                        number("空闲回切（分钟，0 表示关闭）", "idle_return_minutes", 0...10080, 5)
                        number("最多尝试次数", "upstream_attempts", 1...10, 3)
                        number("重试间隔（秒）", "upstream_retry_delay_seconds", 0...300, 3)
                        number("官方余量保护（%）", "official_low_quota_percent", 1...99, 10)
                        number("官方额度刷新间隔（分钟）", "official_quota_refresh_minutes", 1...1440, 30)
                    }
                    SettingsCard("自动使用周重置卡", symbol: "calendar.badge.clock") {
                        Text("默认关闭。仅在远端服务器检测到对应账号周额度耗尽时使用；无需本机在线。")
                            .font(.caption).foregroundStyle(.secondary)
                        if !target.pool.sources.contains(where: { $0.kind == "official" }) { Text("没有官方账号").foregroundStyle(.secondary) }
                        ForEach(target.pool.sources.filter { $0.kind == "official" }) { source in
                            let bound = config.sources.first { $0["id"].string == source.id }?["reset_consent_binding"].string?.isEmpty == false
                            Toggle(source.name, isOn: Binding(get: { consent.contains(source.id) }, set: { on in
                                if on { authorizing = source } else { consent.remove(source.id) }
                            })).disabled(!bound && !consent.contains(source.id))
                        }
                    }
                }
            }
            if failed { InfoNote(text: "未确认保存；请关闭并刷新远端配置后核对。", color: .orange) }
            HStack { Spacer(); Button("取消") { dismiss() }; Button("保存到远端") { save() }.buttonStyle(.borderedProminent).disabled(!valid) }
        }.padding(22).frame(width: 560, height: 630).disabled(busy)
        .alert("允许此账号自动消耗周重置卡？", isPresented: Binding(get: { authorizing != nil }, set: { if !$0 { authorizing = nil } }), presenting: authorizing) { source in
            Button("取消", role: .cancel) {}; Button("允许") { consent.insert(source.id) }
        } message: { source in Text("\(target.endpoint) 上的「\(source.name)」可在周额度耗尽时自动使用卡片。保存后生效。") }
    }

    private func save() {
        guard valid, !busy else { return }
        var next = config; next.settings = values; next.settings["auto_reset_weekly_accounts"] = .array(consent.sorted().map(JSONValue.string))
        busy = true
        Task { defer { busy = false }; do {
            _ = try await target.call("config.save", next.payload); dismiss(); await target.management.refresh()
        } catch { failed = true } }
    }
}

private struct RemoteHistoryEdit: Identifiable {
    let id = UUID()
    let target: RemoteConsoleTarget
    let original: HistoryCacheSettings
}

struct RemoteConsoleHistory: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    @State private var value: JSONValue = .null
    @State private var editing: RemoteHistoryEdit?
    @State private var clearing: RemoteConsoleTarget?
    @State private var failed = false
    @State private var busy = false
    private var preferences: HistoryCacheSettings? {
        guard let enabled = value["enabled"].bool, let bytes = value["max_bytes"].number.flatMap({ Int64(exactly: $0) }) else { return nil }
        return HistoryCacheSettings(enabled: enabled, maxBytes: bytes)
    }
    var body: some View {
        SettingsCard("服务器原文缓存", symbol: "externaldrive") {
            if failed { InfoNote(text: "无法确认缓存状态。请刷新后核对。", color: .orange) }
            if let preferences {
                LabeledContent("保存原文", value: preferences.enabled ? "已开启" : "已关闭")
                LabeledContent("容量", value: ByteCountFormatter.string(fromByteCount: preferences.maxBytes, countStyle: .decimal))
                if let used = value["disk_bytes"].number.flatMap({ Int64(exactly: $0) }) {
                    LabeledContent("已用空间", value: ByteCountFormatter.string(fromByteCount: used, countStyle: .decimal))
                }
                HStack {
                    Button("配置…") { editing = .init(target: target, original: preferences) }
                    Button("清空缓存…", role: .destructive) { clearing = target }
                }.disabled(busy)
            } else if !failed { ProgressView("读取缓存状态…") }
        }.task(id: management.fetchedAt) { await load() }
        .sheet(item: $editing) { session in RemoteHistoryForm(edit: session, saved: { value = $0; failed = false }) }
        .alert("清空远端原文缓存？", isPresented: Binding(get: { clearing != nil }, set: { if !$0 { clearing = nil } }), presenting: clearing) { captured in
            Button("取消", role: .cancel) {}; Button("清空", role: .destructive) {
                busy = true
                Task { defer { busy = false }; do { value = try await captured.call("history.clear", ["confirmed": .bool(true)]); failed = false } catch { failed = true } }
            }
        } message: { Text("将删除 \($0.endpoint) 上已有的原文缓存。依赖这些块的对话可能无法恢复；此操作不可撤销。") }
    }
    private func load() async {
        do { value = try await target.call("history.status"); failed = false } catch { if !Task.isCancelled { failed = true } }
    }
}

private struct RemoteHistoryForm: View {
    @Environment(\.dismiss) private var dismiss
    let edit: RemoteHistoryEdit
    let saved: (JSONValue) -> Void
    @State private var enabled: Bool
    @State private var capacity: String
    @State private var busy = false
    @State private var failed = false
    init(edit: RemoteHistoryEdit, saved: @escaping (JSONValue) -> Void) {
        self.edit = edit; self.saved = saved
        _enabled = State(initialValue: edit.original.enabled); _capacity = State(initialValue: String(Double(edit.original.maxBytes) / 1e9))
    }
    private var next: HistoryCacheSettings? { try? HistoryCacheSettings.parse(enabled: enabled, gigabytes: capacity) }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("服务器原文缓存").font(.title2)
            Text(edit.target.endpoint)
            Toggle("将对话原文保存在服务器上", isOn: $enabled)
            Text("默认关闭。开启后，远端网关将保存可用于重建上下文的私密内容。关闭不会清空已有缓存。") .foregroundStyle(.secondary)
            LabeledContent("容量（0.064–1000 GB）") { TextField("GB", text: $capacity).settingsField().frame(width: 120) }
            if failed { InfoNote(text: "未确认缓存配置，请关闭并刷新核对。", color: .orange) }
            HStack { Spacer(); Button("取消") { dismiss() }; Button("确认应用") {
                guard let next else { return }; busy = true
                Task { defer { busy = false }; do {
                    let updated = try await edit.target.call("history.configure", ["enabled": .bool(next.enabled), "max_bytes": .number(Double(next.maxBytes))]); saved(updated); dismiss()
                } catch { failed = true } }
            }.disabled(next == nil || next == edit.original) }
        }.padding(22).frame(width: 460).disabled(busy)
    }
}
