import SwiftUI
import UniformTypeIdentifiers

private struct EditingModel {
    let id: String
    let isNew: Bool
}

struct ModelSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var search = ""
    @State private var editing: EditingModel?
    @State private var choosingMetadata = false
    @State private var confirmingMetadataReset = false
    @State private var updatingMetadata = false
    @State private var metadataNotice: String?
    @State private var metadataError: String?
    private var visibleModels: [String] {
        let imported = model.modelInformation.compactMap { id, information in
            information["local_metadata"].bool == true ? id : nil
        }
        let ids = imported.isEmpty ? model.configuredModelIDs : Set(model.configuredModelIDs).union(imported).sorted()
        return search.isEmpty ? ids : ids.filter { $0.localizedCaseInsensitiveContains(search) }
    }

    var body: some View {
        Group {
            if let item = editing {
                ModelEditor(settings: model.settings, modelID: item.id, isNew: item.isNew, onBack: { editing = nil })
                    .id(item.id)
            } else {
                modelList
            }
        }
        .task { await model.refreshModelInformation() }
        .fileImporter(isPresented: $choosingMetadata, allowedContentTypes: [.json]) { result in
            switch result {
            case .success(let url): updateMetadata(from: url)
            case .failure(let error): metadataNotice = nil; metadataError = error.localizedDescription
            }
        }
        .confirmationDialog("恢复默认模型能力？", isPresented: $confirmingMetadataReset, titleVisibility: .visible) {
            Button("恢复默认", role: .destructive) { updateMetadata(from: nil) }
        } message: {
            Text("清除全部本地导入的默认值，恢复使用已缓存的官方或内置能力。不会删除各模型单独设置的规则。")
        }
    }

    private var modelList: some View {
        VStack(alignment: .leading, spacing: 18) {
            PageHeading("模型") {
                Button(model.modelInformationRefreshing ? "读取中…" : "刷新信息") { Task { await model.refreshModelInformation() } }
                    .disabled(!model.running || model.modelInformationRefreshing || updatingMetadata)
                Menu(updatingMetadata ? "更新中…" : "模型能力") {
                    Button("导入 JSON…") { choosingMetadata = true }
                    Button("恢复默认…") { confirmingMetadataReset = true }
                }.disabled(!model.running || model.modelInformationRefreshing || updatingMetadata)
                Button { editing = EditingModel(id: "", isNew: true) } label: { Label("添加模型", systemImage: "plus") }
                    .buttonStyle(.borderedProminent).disabled(model.busy || model.metadataLoadFailed)
            }
            Text("管理各模型的来源顺序、可用接入端和上下文设置。")
                .foregroundStyle(.secondary)
            DisclosureGroup("模型能力 JSON 格式与优先级") {
                VStack(alignment: .leading, spacing: 6) {
                    Text("导入会整份替换上次导入的数据。未列出的模型沿用已缓存的官方能力，未缓存则沿用内置能力。省略压缩阈值或写 null 表示使用客户端默认；各模型单独设置的规则仍优先，并受窗口上限和 90% 压缩边界约束。")
                    Text(#"{"models":[{"slug":"gpt-6.2-sol","max_context_window":600000,"auto_compact_token_limit":400000}]}"#)
                        .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    Text("最多 200 个模型、256 KiB。导入后网关模型目录和 OMP 预算立即更新，无需重启；不修改价格、来源或本地/远端 Codex 配置。")
                }.font(AppTypography.caption).foregroundStyle(.secondary)
            }
            TextField("搜索模型", text: $search).settingsField().accessibilityLabel("搜索模型")
            if let error = model.modelInformationError { InfoNote(text: "模型信息未更新：\(error)", color: .orange) }
            if let metadataError { InfoNote(text: "模型能力未更新：\(metadataError)", color: .orange) }
            if let metadataNotice { InfoNote(text: metadataNotice) }
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(visibleModels, id: \.self) { id in
                        modelRow(id)
                        Divider()
                    }
                    if visibleModels.isEmpty { Text("暂无匹配模型，可手动添加模型 ID。").foregroundStyle(.secondary).padding(24) }
                }.cardSurface()
            }
            Text("各模型单独保存的规则：OMP 自动同步；本地 Codex 请在「接入」中手动同步，远端 Codex 在「远端 SSH」中预览并应用。聊天客户端请刷新模型列表。")
                .font(AppTypography.caption).foregroundStyle(.secondary)
            Button("前往接入") { NotificationCenter.default.post(name: .codexpressNavigate, object: SettingsPage.connections) }
        }.settingsPage()
    }

    private func modelRow(_ id: String) -> some View {
        let policy = model.settings.modelPolicies?.first { $0.model == id }
        let rule = model.settings.modelCompaction?.first { $0.model == id }
        let clients = (policy?.clients ?? ModelClient.allCases).lazy.filter { $0.isApplicable(to: id) }
        let information = model.modelInformation[id] ?? .null
        let defaultWindow = (information["default_max_context_window"].number ?? information["default_context_window"].number).map(Int.init)
        let windowValue = rule?.context_window.map { min($0, defaultWindow ?? $0) } ?? defaultWindow
        let thresholdValue = (rule?.auto_compact_token_limit ?? information["default_auto_compact_token_limit"].number.map(Int.init)).map { min($0, windowValue.map { $0 * 9 / 10 } ?? $0) }
        let window = windowValue.map { $0.formatted() + " Token" } ?? "模型默认"
        let threshold = thresholdValue.map { $0.formatted() + " Token" } ?? "客户端默认"
        return Button { editing = EditingModel(id: id, isNew: false) } label: {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text(id).font(.system(size: 14, weight: .medium, design: .monospaced)).foregroundStyle(.primary).lineLimit(1)
                    Spacer()
                    if information["local_metadata"].bool == true {
                        Text("已导入能力").font(AppTypography.caption).foregroundStyle(.secondary)
                    }
                    if clients.isEmpty || policy?.sourceIDs?.isEmpty == true {
                        Text("未启用").font(AppTypography.caption).foregroundStyle(.secondary)
                    }
                    Image(systemName: "chevron.right").foregroundStyle(.secondary)
                }
                Text("来源：\(sourceSummary(policy))").lineLimit(1)
                Text("接入端：\(clients.isEmpty ? "未启用" : clients.map(\.label).joined(separator: "、"))")
                HStack(spacing: 24) {
                    Text("最大输入：\(window)")
                    Text("压缩点：\(threshold)")
                }.monospacedDigit()
            }.font(AppTypography.caption).foregroundStyle(.secondary)
                .padding(16).frame(maxWidth: .infinity, alignment: .leading).contentShape(Rectangle())
        }.buttonStyle(.plain).accessibilityLabel("编辑模型 \(id)")
    }

    private func sourceSummary(_ policy: ModelPolicy?) -> String {
        guard let ids = policy?.sourceIDs else { return "跟随来源池" }
        guard !ids.isEmpty else { return "未启用来源" }
        return ids.map { id in model.settings.sources.first { $0.id == id }?.name ?? "已移除来源" }.joined(separator: " → ")
    }

    private func updateMetadata(from file: URL?) {
        guard model.running, !updatingMetadata else { return }
        updatingMetadata = true; metadataNotice = nil; metadataError = nil
        Task {
            defer { updatingMetadata = false }
            do {
                let payload: JSONValue
                if let file {
                    let text = try await Task.detached(priority: .utility) {
                        let accessing = file.startAccessingSecurityScopedResource()
                        defer { if accessing { file.stopAccessingSecurityScopedResource() } }
                        guard try file.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else {
                            throw AppError.message("请选择普通 JSON 文件。")
                        }
                        let handle = try FileHandle(forReadingFrom: file)
                        defer { try? handle.close() }
                        let maxBytes = 256 * 1024
                        let data = try handle.read(upToCount: maxBytes + 1) ?? Data()
                        guard data.count <= maxBytes else { throw AppError.message("模型能力 JSON 不能超过 256 KiB。") }
                        guard let json = String(data: data, encoding: .utf8) else { throw AppError.message("模型能力 JSON 必须使用 UTF-8。") }
                        return json
                    }.value
                    payload = .object(["json": .string(text)])
                } else {
                    payload = .object(["reset": .bool(true)])
                }
                let result = try await model.remoteCall("models.metadata.replace", payload)
                await model.refreshModelInformation()
                metadataNotice = file == nil ? "已恢复默认模型能力。" : "已更新 \(Int(result["count"].number ?? 0)) 个模型的能力，无需重启。"
            } catch { metadataError = error.localizedDescription }
        }
    }
}

private struct ModelEditor: View {
    @EnvironmentObject var model: AppModel
    let isNew: Bool
    let onBack: () -> Void
    @State private var modelID: String
    @State private var customPolicy: Bool
    @State private var customSources: Bool
    @State private var sourceIDs: [String]
    @State private var sourceSelectionInitialized: Bool
    @State private var clients: [ModelClient]
    @State private var customCompaction: Bool
    @State private var threshold: String
    @State private var window: String
    @State private var saving = false
    @State private var error: String?
    @State private var savedModelID: String?
    private var information: JSONValue { model.modelInformation[modelID.trimmingCharacters(in: .whitespacesAndNewlines)] ?? .null }
    private var defaultWindow: Int? { (information["default_max_context_window"].number ?? information["default_context_window"].number).map(Int.init) }

    init(settings: AppSettings, modelID: String, isNew: Bool, onBack: @escaping () -> Void) {
        self.isNew = isNew; self.onBack = onBack
        let policy = settings.modelPolicies?.first { $0.model == modelID }
        let rule = settings.modelCompaction?.first { $0.model == modelID }
        _modelID = State(initialValue: modelID)
        _customPolicy = State(initialValue: policy != nil || isNew)
        _customSources = State(initialValue: policy?.sourceIDs != nil)
        _sourceIDs = State(initialValue: policy?.sourceIDs ?? [])
        _sourceSelectionInitialized = State(initialValue: policy?.sourceIDs != nil)
        _clients = State(initialValue: policy?.clients ?? ModelClient.allCases)
        _customCompaction = State(initialValue: rule != nil)
        _threshold = State(initialValue: rule?.auto_compact_token_limit.map(String.init) ?? "")
        _window = State(initialValue: rule?.context_window.map(String.init) ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Button(action: onBack) { Label("模型列表", systemImage: "chevron.left") }
                .buttonStyle(.plain).foregroundStyle(.secondary).accessibilityLabel("返回模型列表")
            PageHeading(isNew && savedModelID == nil ? "添加模型" : "模型设置")
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("模型 ID").font(AppTypography.caption).foregroundStyle(.secondary)
                        TextField("模型 ID", text: $modelID).settingsField().accessibilityLabel("模型 ID").disabled(!isNew || savedModelID != nil)
                    }
                    informationSection
                    sourceSection
                    clientSection
                    compactionSection
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    Text("来源顺序用于新对话；已有会话优先沿用已选来源。图片生成沿用其独立来源选择。")
                        .font(AppTypography.caption).foregroundStyle(.secondary)
                }.padding(20).frame(maxWidth: .infinity, alignment: .leading).cardSurface()
            }
            HStack {
                Button("恢复模型默认") {
                    customPolicy = isNew; customSources = false; sourceIDs = []; sourceSelectionInitialized = false; clients = ModelClient.allCases
                    customCompaction = false; threshold = ""; window = ""
                }
                Spacer()
                Button(saving ? "保存中…" : "保存模型设置") { save() }
                    .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }
        }.settingsPage().disabled(saving || model.metadataLoadFailed)
    }

    private var informationSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("基本信息").font(.headline)
            LabeledContent("名称", value: information["name"].string ?? modelID)
            LabeledContent("默认最大输入", value: defaultWindow.map { $0.formatted() + " Token" } ?? "未提供")
            LabeledContent("默认压缩阈值", value: information["default_auto_compact_token_limit"].number.map { Int($0).formatted() + " Token" } ?? "客户端默认")
            if let output = information["max_output_tokens"].number {
                LabeledContent("最大输出", value: Int(output).formatted() + " Token")
            }
            let inputs = information["input"].array.compactMap(\.string)
            if !inputs.isEmpty {
                LabeledContent("输入类型", value: inputs.map { ["text": "文本", "image": "图像", "audio": "音频"][$0] ?? $0 }.joined(separator: "、"))
            }
            let efforts = information["supported_reasoning_levels"].array.compactMap { $0["effort"].string ?? $0.string }
            if !efforts.isEmpty { LabeledContent("推理档位", value: efforts.joined(separator: " / ")) }
            let cost = information["cost"]
            if cost["input"].number != nil {
                Text("API 标准基础参考价 · USD/百万 Token：输入 \(price(cost["input"]))；缓存读 \(price(cost["cacheRead"]))；缓存写 \(price(cost["cacheWrite"]))；输出 \(price(cost["output"]))。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
                Text("参考费用随上下文长度和速度档位变化，不代表订阅实际扣费。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
        }.monospacedDigit()
    }

    private func price(_ value: JSONValue) -> String {
        value.number.map { "$" + $0.formatted(.number.precision(.fractionLength(0...4))) } ?? "未提供"
    }

    private var sourceSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("来源及优先级").font(.headline)
            Toggle("自定义来源及顺序", isOn: Binding(get: { customSources }, set: { enabled in
                customPolicy = true; customSources = enabled
                if enabled && !sourceSelectionInitialized {
                    sourceIDs = model.sources.filter(supports).map(\.id)
                    sourceSelectionInitialized = true
                }
            }))
            if customSources {
                Text(sourceIDs.isEmpty ? "未选择来源：此模型不可用，不会回退到来源池。" : "由上到下优先使用；未勾选来源不会用于此模型。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
                ForEach(Array(sourceIDs.enumerated()), id: \.element) { index, id in selectedSource(id, index: index) }
                ForEach(model.sources.filter { !sourceIDs.contains($0.id) }) { source in
                    Toggle(isOn: Binding(get: { false }, set: { selected in
                        if selected { sourceIDs.append(source.id); customPolicy = true }
                    })) {
                        VStack(alignment: .leading) {
                            Text(source.name)
                            if !supports(source) { Text("来源未声明支持此模型").font(AppTypography.caption).foregroundStyle(.secondary) }
                        }
                    }.disabled(!supports(source))
                }
            } else {
                Text("继承来源池中的可用来源和顺序，仅使用来源声明支持的模型。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func selectedSource(_ id: String, index: Int) -> some View {
        let source = model.settings.sources.first { $0.id == id }
        return HStack {
            Toggle(isOn: Binding(get: { true }, set: { selected in
                if !selected { sourceIDs.removeAll { $0 == id }; customPolicy = true }
            })) {
                VStack(alignment: .leading) {
                    Text(source?.name ?? "已移除来源：\(id)")
                    if source.map({ !supports($0) }) ?? true {
                        Text(source == nil ? "取消失效来源后，可选择其他来源。" : "请先在来源池中更新此来源的模型列表。")
                            .font(AppTypography.caption).foregroundStyle(.orange)
                    } else if source?.enabled == false {
                        Text("来源池中已停用").font(AppTypography.caption).foregroundStyle(.secondary)
                    }
                }
            }
            Button { sourceIDs.swapAt(index, index - 1); customPolicy = true } label: { Image(systemName: "arrow.up") }
                .disabled(index == 0).accessibilityLabel("上移来源 \(source?.name ?? id)")
            Button { sourceIDs.swapAt(index, index + 1); customPolicy = true } label: { Image(systemName: "arrow.down") }
                .disabled(index == sourceIDs.count - 1).accessibilityLabel("下移来源 \(source?.name ?? id)")
        }
    }

    private var clientSection: some View {
        let id = modelID.trimmingCharacters(in: .whitespacesAndNewlines)
        return VStack(alignment: .leading, spacing: 10) {
            Text("可用接入端").font(.headline)
            ForEach(ModelClient.allCases, id: \.self) { client in
                if client.isApplicable(to: id) {
                    Toggle(client.label, isOn: Binding(get: { clients.contains(client) }, set: { selected in
                        customPolicy = true
                        clients.removeAll { $0 == client }
                        if selected { clients.append(client) }
                    }))
                }
            }
            if !clients.contains(where: { $0.isApplicable(to: id) }) {
                Text("未选择接入端：所有客户端均禁用此模型。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var compactionSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Toggle("自定义输入与压缩", isOn: $customCompaction).font(.headline)
            if customCompaction {
                HStack(alignment: .top, spacing: 16) {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("压缩阈值 · Token").font(AppTypography.caption).foregroundStyle(.secondary)
                        TextField(information["default_auto_compact_token_limit"].number.map { "默认 " + Int($0).formatted() } ?? "客户端默认", text: $threshold).settingsField().accessibilityLabel("压缩阈值 Token，可选")
                    }.frame(maxWidth: .infinity)
                    VStack(alignment: .leading, spacing: 6) {
                        Text("最大输入 · Token").font(AppTypography.caption).foregroundStyle(.secondary)
                        TextField(defaultWindow.map { "默认 " + $0.formatted() } ?? "模型默认", text: $window).settingsField().accessibilityLabel("最大输入 Token，可选")
                    }.frame(maxWidth: .infinity)
                }
                Text("留空分别使用默认值；最大输入不可超过模型能力的默认最大输入，压缩阈值不可超过实际最大输入的 90%。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func supports(_ source: Source) -> Bool {
        let id = modelID.trimmingCharacters(in: .whitespacesAndNewlines)
        if id == "gpt-image-2" { return source.official }
        return source.models.isEmpty || source.models.contains(id)
    }
    private func save() {
        guard !saving else { return }
        do {
            let id = modelID.trimmingCharacters(in: .whitespacesAndNewlines)
            try ModelPolicy.validateModelID(id)
            if isNew, savedModelID != id, model.configuredModelIDs.contains(id) { throw AppError.message("此模型已存在，请从列表中编辑。") }
            let policy = customPolicy ? ModelPolicy(model: id, sourceIDs: customSources ? sourceIDs : nil, clients: clients.filter { $0.isApplicable(to: id) }) : nil
            let parsedRule = customCompaction ? try ModelCompactionRule.parse(model: id, threshold: threshold, window: window, defaultWindow: defaultWindow) : nil
            let rule = parsedRule.flatMap { $0.auto_compact_token_limit != nil || $0.context_window != nil ? $0 : nil }
            saving = true; error = nil
            Task {
                do {
                    try await model.saveModelSettings(model: id, policy: policy, compaction: rule)
                    modelID = id; savedModelID = id
                }
                catch {
                    if model.settings.modelPolicies?.first(where: { $0.model == id }) == policy,
                        model.settings.modelCompaction?.first(where: { $0.model == id }) == rule,
                        model.configuredModelIDs.contains(id) { savedModelID = id }
                    self.error = error.localizedDescription
                }
                saving = false
            }
        } catch { self.error = error.localizedDescription }
    }
}
