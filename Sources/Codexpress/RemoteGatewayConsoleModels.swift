import SwiftUI

/// The server replaces settings atomically. Keep untouched settings and source metadata exactly as received.
struct RemoteConsoleConfig {
    let sources: [JSONValue]
    var settings: [String: JSONValue]

    init(_ value: JSONValue) throws {
        guard case let .array(sources) = value["sources"], case let .object(settings) = value["settings"] else { throw RemoteConsoleError.invalid }
        self.sources = sources; self.settings = settings
    }
    var payload: [String: JSONValue] { ["sources": .array(sources), "settings": .object(settings)] }
    var modelIDs: [String] {
        var ids = Set(sources.flatMap { $0["models"].array.compactMap(\.string) })
        for key in ["model_compaction", "model_policies"] { ids.formUnion(settings[key]?.array.compactMap { $0["model"].string } ?? []) }
        return ids.sorted()
    }
    mutating func updateModel(_ model: String, policy: ModelPolicy?, compaction: ModelCompactionRule?) throws {
        try ModelPolicy.validateModelID(model)
        for (key, replacement) in [("model_policies", try policy.map(JSONValue.from)), ("model_compaction", try compaction.map(JSONValue.from))] {
            var rows = (settings[key]?.array ?? []).filter { $0["model"].string != model }
            if let replacement { rows.append(replacement) }
            settings[key] = .array(rows)
        }
    }
}

private struct RemoteConsoleEdit: Identifiable {
    let id = UUID()
    let target: RemoteConsoleTarget
    let config: RemoteConsoleConfig
    let model: String?
    let info: JSONValue
}

struct RemoteConsolePreferences: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    let page: SettingsPage
    @State private var config: RemoteConsoleConfig?
    @State private var configRevision: String?
    @State private var info: [String: JSONValue] = [:]
    @State private var metadataRevision: String?
    @State private var modelUsage: [String: Double] = [:]
    @State private var search = ""
    @State private var editing: RemoteConsoleEdit?
    @State private var failed = false

    var body: some View {
        VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
            if failed { InfoNote(text: "无法读取当前配置。请刷新后重试。", color: .orange) }
            if let config, configRevision == target.pool.revision {
                if page == .models {
                    if let metadataRevision, management.capabilities.contains("models.metadata.replace") {
                        RemoteConsoleMetadata(target: target, revision: metadataRevision)
                    }
                    SettingsCard("模型策略", symbol: "cube", accessory: {
                        Button("添加模型…") { edit(config, model: "") }
                    }) {
                        TextField("筛选模型", text: $search).settingsField()
                        let ids = Set(config.modelIDs).union(info.keys).sorted().filter { search.isEmpty || $0.localizedCaseInsensitiveContains(search) }
                        if ids.isEmpty { Text("没有匹配的模型").foregroundStyle(.secondary) }
                        ForEach(ids, id: \.self) { id in
                            HStack {
                                Text(id).textSelection(.enabled); Spacer()
                                if let tokens = modelUsage[id] { Text("\(usageCount(tokens)) Token").font(.caption).foregroundStyle(.secondary) }
                                if let policy = config.settings["model_policies"]?.array.first(where: { $0["model"].string == id }) {
                                    Text(policy["clients"].array.isEmpty || policy["source_ids"] == .array([]) ? "已禁用" : "自定义").foregroundStyle(.secondary)
                                }
                                Button("编辑…") { edit(config, model: id) }
                            }
                        }
                    }
                } else {
                    SettingsCard("分配策略", symbol: "arrow.triangle.branch", accessory: {
                        Button("编辑…") { edit(config, model: nil) }
                    }) {
                        LabeledContent("模式", value: target.pool.mode == "fixed" ? "固定来源" : "自动切换")
                        LabeledContent("负载均衡", value: config.settings["load_balance"]?.bool == true ? "已开启" : "已关闭")
                        LabeledContent("已授权自动周重置的来源", value: "\(config.settings["auto_reset_weekly_accounts"]?.array.count ?? 0)")
                    }
                }
            } else if !failed { ProgressView("读取远端配置…") }
            if page == .general { RemoteConsoleHistory(target: target) }
        }
        .task(id: management.fetchedAt) {
            do {
                let data = try await target.call("config.get")
                let loaded = try RemoteConsoleConfig(data)
                config = loaded; configRevision = target.pool.revision; failed = false
                if page == .models {
                    let values = try await target.call("models.info")
                    info = values["models"].array.reduce(into: [:]) { if let id = $1["id"].string { $0[id] = $1 } }
                    metadataRevision = values["metadata_revision"].string
                    let usage = try await target.call("usage.models")
                    modelUsage = usage["models"].array.reduce(into: [:]) { result, row in
                        if let id = row["model"].string, let count = row["total_tokens"].number { result[id] = count }
                    }
                }
            } catch { if !Task.isCancelled { failed = true } }
        }
        .sheet(item: $editing) { session in
            if let model = session.model { RemoteConsoleModelEditor(target: session.target, config: session.config, model: model, info: session.info) }
            else { RemoteConsoleRoutingEditor(target: session.target, config: session.config) }
        }
    }

    private func edit(_ config: RemoteConsoleConfig, model: String?) {
        editing = RemoteConsoleEdit(target: target, config: config, model: model, info: model.flatMap { info[$0] } ?? .null)
    }
}

private struct RemoteConsoleModelEditor: View {
    @Environment(\.dismiss) private var dismiss
    let target: RemoteConsoleTarget
    let config: RemoteConsoleConfig
    let info: JSONValue
    let creating: Bool
    @State private var id: String
    @State private var ordered: [String]
    @State private var customSources: Bool
    @State private var clients: Set<ModelClient>
    @State private var threshold: String
    @State private var window: String
    @State private var busy = false
    @State private var message: String?

    init(target: RemoteConsoleTarget, config: RemoteConsoleConfig, model: String, info: JSONValue) {
        self.target = target; self.config = config; self.info = info; creating = model.isEmpty
        let policy = config.settings["model_policies"]?.array.first { $0["model"].string == model }
        let rule = config.settings["model_compaction"]?.array.first { $0["model"].string == model }
        _id = State(initialValue: model)
        _customSources = State(initialValue: policy?["source_ids"] != nil && policy?["source_ids"] != .null)
        _ordered = State(initialValue: policy?["source_ids"].array.compactMap(\.string) ?? [])
        let allowed = policy?["clients"] ?? .null
        _clients = State(initialValue: Set(allowed == .null ? ModelClient.allCases : allowed.array.compactMap { $0.string.flatMap(ModelClient.init(rawValue:)) }))
        _threshold = State(initialValue: rule?["auto_compact_token_limit"].number.map { String(format: "%.0f", $0) } ?? "")
        _window = State(initialValue: rule?["context_window"].number.map { String(format: "%.0f", $0) } ?? "")
    }

    private var supporting: [RemotePoolSource] {
        target.pool.sources.filter { source in
            let models = config.sources.first { $0["id"].string == source.id }?["models"].array.compactMap(\.string) ?? []
            return id == "gpt-image-2" ? source.kind == "official" : models.isEmpty || models.contains(id)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(creating ? "添加模型" : id).font(.title2)
            if creating { TextField("模型 ID", text: $id).settingsField() }
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    SettingsCard("允许接入的客户端", symbol: "display") {
                        ForEach(ModelPolicy.applicableClients(for: id), id: \.rawValue) { client in
                            Toggle(client.label, isOn: Binding(get: { clients.contains(client) }, set: { if $0 { clients.insert(client) } else { clients.remove(client) } }))
                        }
                    }
                    SettingsCard("来源顺序", symbol: "list.number") {
                        Toggle("指定允许的来源和优先级", isOn: $customSources)
                        if customSources {
                            if ordered.isEmpty { Text("未选择来源；此模型不会被路由。").foregroundStyle(.secondary) }
                            ForEach(Array(ordered.enumerated()), id: \.element) { index, source in
                                HStack {
                                    Text("\(index + 1). \(target.pool.sources.first { $0.id == source }?.name ?? source)"); Spacer()
                                    Button("上移") { ordered.swapAt(index, index - 1) }.disabled(index == 0)
                                    Button("下移") { ordered.swapAt(index, index + 1) }.disabled(index + 1 == ordered.count)
                                    Button("移除") { ordered.remove(at: index) }
                                }
                            }
                            Menu("添加来源") { ForEach(supporting.filter { !ordered.contains($0.id) }) { s in Button(s.name) { ordered.append(s.id) } } }
                        } else { Text("跟随来源池中支持该模型的来源顺序。").foregroundStyle(.secondary) }
                    }
                    SettingsCard("上下文与压缩", symbol: "rectangle.compress.vertical") {
                        LabeledContent("最大输入 Token") { TextField("服务器默认", text: $window).settingsField().frame(width: 160) }
                        LabeledContent("自动压缩阈值 Token") { TextField("客户端默认", text: $threshold).settingsField().frame(width: 160) }
                        if let value = info["context_window"].number { LabeledContent("当前上下文窗口", value: usageCount(value)) }
                        if let value = info["auto_compact_token_limit"].number { LabeledContent("当前压缩阈值", value: usageCount(value)) }
                        if let value = info["max_output_tokens"].number { LabeledContent("最大输出", value: usageCount(value)) }
                    }
                }
            }
            if let message { InfoNote(text: message, color: .orange) }
            HStack {
                Button("恢复默认") { customSources = false; clients = Set(ModelClient.allCases); threshold = ""; window = "" }
                Spacer(); Button("取消") { dismiss() }; Button("保存到远端") { save() }.buttonStyle(.borderedProminent)
            }
        }.padding(22).frame(width: 580, height: 600).disabled(busy)
    }

    private func save() {
        do {
            let model = id.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !creating || !config.modelIDs.contains(model) else { message = "此模型已存在。"; return }
            let rule = try ModelCompactionRule.parse(model: model, threshold: threshold, window: window)
            let policy = ModelPolicy(model: model, sourceIDs: customSources ? ordered : nil, clients: ModelClient.allCases.filter { clients.contains($0) })
            var next = config
            try next.updateModel(model, policy: creating || customSources || clients != Set(ModelClient.allCases) ? policy : nil, compaction: rule.auto_compact_token_limit != nil || rule.context_window != nil ? rule : nil)
            busy = true
            Task { defer { busy = false }; do {
                _ = try await target.call("config.save", next.payload); dismiss(); await target.management.refresh()
            } catch { message = "未能确认保存。请关闭编辑器，刷新远端配置后核对。" } }
        } catch { message = (error as? AppError)?.errorDescription ?? "模型设置无效。" }
    }
}
