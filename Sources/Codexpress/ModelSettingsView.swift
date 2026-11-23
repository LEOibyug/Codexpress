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
    @State private var showingMetadataHelp = false
    @State private var updatingMetadata = false
    @State private var metadataNotice: String?
    @State private var metadataError: String?
    @State private var modelUsage: [String: JSONValue]?
    @State private var usageRefreshing = false
    @State private var usageError: String?

    private static let tokenFormat = FloatingPointFormatStyle<Double>.number
        .notation(.compactName).precision(.significantDigits(1...3)).locale(Locale(identifier: "en_US_POSIX"))

    private var rankedModels: [String] {
        let imported = model.modelInformation.compactMap { id, information in
            information["local_metadata"].bool == true ? id : nil
        }
        return Set(model.configuredModelIDs).union(imported).sorted { a, b in
            let aTokens = modelUsage?[a]?["total_tokens"].number ?? 0
            let bTokens = modelUsage?[b]?["total_tokens"].number ?? 0
            return aTokens == bTokens ? a < b : aTokens > bTokens
        }
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
        .task(id: model.running) { await refreshInformation() }
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
        .sheet(isPresented: $showingMetadataHelp) { metadataHelp }
    }

    private var modelList: some View {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        let ranked = rankedModels
        let visible = query.isEmpty ? ranked : ranked.filter { $0.localizedCaseInsensitiveContains(query) }
        return VStack(alignment: .leading, spacing: 14) {
            PageHeading("模型", subtitle: "比较用量表现并管理各模型的接入规则。") {
                Button { editing = EditingModel(id: "", isNew: true) } label: { Label("添加模型", systemImage: "plus") }
                    .buttonStyle(.borderedProminent).disabled(model.busy || model.metadataLoadFailed)
            }
            HStack(spacing: 12) {
                TextField("搜索模型 ID", text: $search).settingsField().frame(maxWidth: 270)
                    .accessibilityLabel("搜索模型")
                Spacer(minLength: 0)
                Button { Task { await refreshInformation() } } label: {
                    Label(model.modelInformationRefreshing || usageRefreshing ? "读取中…" : "刷新信息", systemImage: "arrow.clockwise")
                }.disabled(!model.running || model.modelInformationRefreshing || usageRefreshing || updatingMetadata)
                Menu(updatingMetadata ? "更新中…" : "模型能力") {
                    Group {
                        Button("导入 JSON…") { choosingMetadata = true }
                        Button("恢复默认…") { confirmingMetadataReset = true }
                    }.disabled(!model.running || model.modelInformationRefreshing || updatingMetadata)
                    Divider()
                    Button("JSON 格式与优先级…") { showingMetadataHelp = true }
                }.fixedSize(horizontal: true, vertical: false)
            }
            if let error = model.modelInformationError { InfoNote(text: "模型信息未更新：\(error)", color: .orange) }
            if let usageError { InfoNote(text: "模型用量未更新：\(usageError)", color: .orange) }
            if let metadataError { InfoNote(text: "模型能力未更新：\(metadataError)", color: .orange) }
            if let metadataNotice { InfoNote(text: metadataNotice) }
            if ranked.isEmpty {
                EmptyState("暂无模型", subtitle: "在来源池获取模型列表，或添加一个模型 ID。", symbol: "square.stack")
            } else if visible.isEmpty {
                EmptyState("未找到模型", subtitle: "试试其他模型 ID。", symbol: "magnifyingglass") {
                    Button("清除搜索") { search = "" }
                }
            } else {
                Text("按全局最近 1,000 条已结束请求的 Token 用量排序；模型不足 10 条时不显示速度和缓存命中率。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
                    .help("生成 TPS 按首个输出到生成结束的耗时计算。缺少生成计时的旧记录沿用请求总耗时。")
            }
            if !visible.isEmpty {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(visible, id: \.self) { id in
                            if id != visible.first { Divider() }
                            modelRow(id)
                        }
                    }.cardSurface()
                }
            }
        }.settingsPage()
    }

    private var metadataHelp: some View {
        VStack(alignment: .leading, spacing: 16) {
            PageHeading("模型能力 JSON")
            Text("导入将整份替换上次导入的数据。未列出的模型使用已缓存的官方能力；未缓存时使用内置能力。")
            Text(#"{"models":[{"slug":"gpt-6.2-sol","max_context_window":600000,"auto_compact_token_limit":400000}]}"#)
                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
            Text("省略压缩阈值或写 null 表示使用客户端默认。单独设置的模型规则优先，并受窗口上限和 90% 压缩边界约束。")
            Text("最多导入 200 个模型、256 KiB。导入后无需重启，不修改价格、来源或 Codex 配置。")
            HStack {
                Spacer()
                Button("完成") { showingMetadataHelp = false }.keyboardShortcut(.defaultAction)
            }
        }.padding(24).frame(width: 660)
    }

    private func modelMetric(_ value: String, symbol: String, help: String) -> some View {
        Label {
            Text(value).fontWeight(.medium).monospacedDigit()
        } icon: {
            Image(systemName: symbol).foregroundStyle(Color.accentColor)
        }.fixedSize().help(help).accessibilityHidden(true)
    }

    private func modelRow(_ id: String) -> some View {
        let policy = model.settings.modelPolicies?.first { $0.model == id }
        let rule = model.settings.modelCompaction?.first { $0.model == id }
        let clients = (policy?.clients ?? ModelClient.allCases).filter { $0.isApplicable(to: id) }
        let information = model.modelInformation[id] ?? .null
        let defaultWindow = (information["default_max_context_window"].number ?? information["default_context_window"].number).map(Int.init)
        let windowValue = rule?.context_window.map { min($0, defaultWindow ?? $0) } ?? defaultWindow
        let thresholdValue = (rule?.auto_compact_token_limit ?? information["default_auto_compact_token_limit"].number.map(Int.init)).map { min($0, windowValue.map { $0 * 9 / 10 } ?? $0) }
        let window = windowValue.map { $0.formatted() + " Token" } ?? "模型默认"
        let threshold = thresholdValue.map { $0.formatted() + " Token" } ?? "客户端默认"
        let usage = modelUsage?[id]
        let emptyCount = modelUsage != nil && usage == nil ? "0" : "—"
        let total = usage?["total_tokens"].number?.formatted(Self.tokenFormat) ?? emptyCount
        let showPerformance = (usage?["attempts"].number ?? 0) >= 10
        let rate = showPerformance ? usageRate(usage?["tps"].number) : ""
        let cache = showPerformance ? (usage?["cache_rate"].number.map { $0.formatted(.number.precision(.fractionLength(0...1))) + "%" } ?? "—") : ""
        let ratio = usageInputOutputRatio(usage?["input_tokens"].number, usage?["output_tokens"].number)
        let disabled = clients.isEmpty || policy?.sourceIDs?.isEmpty == true
        let imported = information["local_metadata"].bool == true
        return Button { editing = EditingModel(id: id, isNew: false) } label: {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 5) {
                    Text(id).font(.system(size: 13, weight: .semibold, design: .monospaced))
                        .foregroundStyle(.primary).lineLimit(1)
                    Text(sourceSummary(policy)).lineLimit(1)
                    HStack(spacing: 6) {
                        if disabled { StatusPill(text: "未启用", compact: true) }
                        if imported { StatusPill(text: "已导入", compact: true) }
                        if !disabled { Text(clients.map(\.label).joined(separator: "、")).lineLimit(1) }
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
                    .help("\(id)\n最大输入：\(window) · 压缩点：\(threshold)")
                HStack(spacing: 18) {
                    modelMetric(total, symbol: "number", help: "近期 Token")
                    modelMetric(ratio, symbol: "arrow.left.arrow.right", help: "输入输出比；按全局最近 1,000 条已结束请求中该模型的输入、输出总量计算，输入包含缓存读取。")
                    if showPerformance {
                        modelMetric(rate, symbol: "speedometer", help: "生成 TPS")
                        modelMetric(cache, symbol: "bolt.circle", help: "缓存命中率")
                    }
                }
                Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).frame(width: 8)
            }.font(AppTypography.caption).foregroundStyle(.secondary)
                .padding(.horizontal, 14).padding(.vertical, 12).contentShape(Rectangle())
        }.buttonStyle(.plain).accessibilityLabel("编辑模型 \(id)")
            .accessibilityValue("\(disabled ? "未启用" : clients.map(\.label).joined(separator: "、"))，\(sourceSummary(policy))，近期 Token \(total)，输入输出比 \(ratio)" + (showPerformance ? "，生成 TPS \(rate)，缓存命中 \(cache)" : ""))
    }

    private func sourceSummary(_ policy: ModelPolicy?) -> String {
        guard let ids = policy?.sourceIDs else { return "跟随来源池" }
        guard !ids.isEmpty else { return "未启用来源" }
        return ids.map { id in model.settings.sources.first { $0.id == id }?.name ?? "已移除来源" }.joined(separator: " → ")
    }

    private func refreshInformation() async {
        guard model.running else { return }
        await model.refreshModelInformation()
        guard !Task.isCancelled else { return }
        usageRefreshing = true
        defer { usageRefreshing = false }
        do {
            let result = try await model.remoteCall("usage.models")
            guard !Task.isCancelled else { return }
            modelUsage = result["models"].array.reduce(into: [:]) { usage, row in
                if let id = row["model"].string { usage[id] = row }
            }
            usageError = nil
        } catch {
            if !Task.isCancelled { usageError = error.localizedDescription }
        }
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
    @FocusState private var modelIDFocused: Bool
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
        VStack(alignment: .leading, spacing: 14) {
            PageHeading(isNew && savedModelID == nil ? "添加模型" : "模型设置", subtitle: isNew && savedModelID == nil ? nil : modelID) {
                Button(action: onBack) { Label("返回模型列表", systemImage: "chevron.left") }
                    .keyboardShortcut(.cancelAction).disabled(saving)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if isNew && savedModelID == nil {
                        SettingsCard("模型 ID", symbol: "number") {
                            TextField("例如 gpt-6.2-sol", text: $modelID).settingsField()
                                .accessibilityLabel("模型 ID").focused($modelIDFocused)
                        }
                    }
                    sourceSection
                    clientSection
                    compactionSection
                    informationSection
                    InfoNote(text: syncNote)
                }.disabled(saving || model.metadataLoadFailed)
            }
            Divider()
            if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            HStack(spacing: 12) {
                Button("恢复默认值") {
                    customPolicy = isNew; customSources = false; sourceIDs = []; sourceSelectionInitialized = false; clients = ModelClient.allCases
                    customCompaction = false; threshold = ""; window = ""
                }.help("将草稿恢复为默认值，保存后才会生效。")
                Spacer()
                Text("保存后应用规则。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
                Button(saving ? "保存中…" : isNew && savedModelID == nil ? "添加模型" : "保存模型设置") { save() }
                    .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }.disabled(saving || model.metadataLoadFailed)
        }.settingsPage().defaultFocus($modelIDFocused, isNew)
    }

    private var syncNote: String {
        let local = model.connectionMode == "official" ? "" : "本地 Codex 在「接入」中手动同步；"
        return "OMP 自动同步；\(local)远端 Codex 在「远端 SSH」中预览并应用；聊天客户端请刷新模型列表。"
    }

    private var informationSection: some View {
        SettingsCard("模型能力", symbol: "info.circle") {
            if let name = information["name"].string { LabeledContent("名称", value: name) }
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
        SettingsCard("来源及优先级", subtitle: "设置新对话使用的来源；已有对话优先沿用原来源。图片生成的来源单独设置。", symbol: "arrow.triangle.branch") {
            SettingRow(title: "自定义来源及顺序") {
                Toggle("自定义来源及顺序", isOn: Binding(get: { customSources }, set: { enabled in
                    customPolicy = true; customSources = enabled
                    if enabled && !sourceSelectionInitialized {
                        sourceIDs = model.sources.filter(supports).map(\.id)
                        sourceSelectionInitialized = true
                    }
                })).labelsHidden().toggleStyle(.switch).controlSize(.small)
            }
            if customSources {
                InfoNote(text: sourceIDs.isEmpty ? "未选择来源：此模型不可用，不会回退到来源池。" : "由上到下优先使用；未选中的来源不会用于此模型。")
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
                InfoNote(text: "跟随来源池的可用来源和顺序。")
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
            Spacer(minLength: 8)
            Button { sourceIDs.swapAt(index, index - 1); customPolicy = true } label: { Image(systemName: "arrow.up") }
                .disabled(index == 0).accessibilityLabel("上移来源 \(source?.name ?? id)")
            Button { sourceIDs.swapAt(index, index + 1); customPolicy = true } label: { Image(systemName: "arrow.down") }
                .disabled(index == sourceIDs.count - 1).accessibilityLabel("下移来源 \(source?.name ?? id)")
        }.controlSize(.small)
    }

    private var clientSection: some View {
        let id = modelID.trimmingCharacters(in: .whitespacesAndNewlines)
        return SettingsCard("可用接入端", symbol: "link") {
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
                InfoNote(text: "未选择接入端：所有客户端均禁用此模型。")
            }
        }
    }

    private var compactionSection: some View {
        SettingsCard("上下文", symbol: "rectangle.compress.vertical") {
            SettingRow(title: "自定义输入与压缩", subtitle: "默认使用模型和客户端提供的上限。") {
                Toggle("自定义输入与压缩", isOn: $customCompaction)
                    .labelsHidden().toggleStyle(.switch).controlSize(.small)
            }
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
