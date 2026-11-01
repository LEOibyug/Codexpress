import SwiftUI

struct SourceEditor: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State var source: Source
    let isNew: Bool
    @State private var apiKey = ""
    @State private var modelsText = ""
    @State private var automaticModels = true
    @State private var proxyPort = ""
    @State private var loadingModels = false
    @State private var modelError: String?
    @State private var fetchID = UUID()
    @State private var localError: String?
    @State private var saving = false
    @State private var balanceWarningEnabled = false
    @State private var balanceWarningAmount = ""
    @State private var capacityLimitEnabled = false
    @State private var maxActiveConversations = "3"
    @State private var activityWindowSeconds = "15"

    init(source: Source, isNew: Bool) {
        _source = State(initialValue: source); self.isNew = isNew
        _modelsText = State(initialValue: source.models.joined(separator: ", "))
        _automaticModels = State(initialValue: source.discoversModels)
        _proxyPort = State(initialValue: source.proxy.localPort ?? "")
        _balanceWarningEnabled = State(initialValue: source.balanceWarningThreshold != nil)
        _balanceWarningAmount = State(initialValue: source.balanceWarningThreshold.map { String($0) } ?? "")
        _capacityLimitEnabled = State(initialValue: source.max_active_conversations != nil)
        _maxActiveConversations = State(initialValue: source.max_active_conversations.map(String.init) ?? "3")
        _activityWindowSeconds = State(initialValue: String(source.activeConversationWindowSeconds))
    }

    private var discoveryInput: [String] {
        [source.base_url, source.proxy.mode, proxyPort, apiKey, String(automaticModels), String(model.running), model.settings.sources.first(where: { $0.id == source.id })?.account_id ?? ""]
    }
    private var canFetch: Bool {
        guard model.running else { return false }
        if source.official {
            return model.settings.sources.contains { $0.id == source.id && $0.account_id != nil }
        }
        return !apiKey.isEmpty || (!isNew && model.settings.sources.first(where: { $0.id == source.id })?.base_url == source.base_url)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 14) {
                Image(systemName: source.official ? "person.crop.circle" : "terminal")
                    .font(.system(size: 28, weight: .regular)).foregroundStyle(.secondary).frame(width: 36)
                VStack(alignment: .leading, spacing: 5) {
                    Text(isNew ? "添加来源" : "编辑来源").font(.system(size: 22, weight: .semibold))
                    Text(source.official ? "ChatGPT 账号" : "Responses API")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                }
                Spacer()
            }.padding(.horizontal, 28).padding(.top, 28).padding(.bottom, 10)

            Form {
                Section("基本信息") {
                    TextField("名称", text: $source.name)
                    TextField("API 基址", text: $source.base_url).textContentType(.URL).disabled(source.official)
                    if !source.official {
                        SecureField(isNew ? "API 密钥" : "新 API 密钥", text: $apiKey, prompt: Text(isNew ? "API 密钥" : "留空保留现有密钥"))
                    }
                    Toggle("启用此来源", isOn: $source.enabled).toggleStyle(.switch)
                }
                conversationCapacitySection
                if !source.official {
                    Section("余额预警") {
                        Toggle("余额不足时通知", isOn: $balanceWarningEnabled)
                            .toggleStyle(.switch)
                        if balanceWarningEnabled {
                            TextField("预警余额", text: $balanceWarningAmount, prompt: Text("例如 10"))
                        }
                    }
                    Section("费用估算") {
                        Toggle("订阅来源", isOn: Binding(get: { source.is_subscription ?? false }, set: { source.is_subscription = $0 }))
                            .toggleStyle(.switch)
                    }
                }
                Section("网络") {
                    Picker("出站策略", selection: $source.proxy.mode) {
                        if !source.official { Text("强制直连").tag("direct") }
                        Text("指定代理").tag("explicit")
                    }
                    if source.proxy.mode == "explicit" {
                        TextField("本地代理端口", text: $proxyPort, prompt: Text("例如 7890"))
                        if source.proxy.localPort == nil, let existing = source.proxy.url {
                            Text("现有代理：\(existing)")
                                .font(.system(size: 12)).foregroundStyle(.secondary)
                        }
                    }
                }
                Section("模型") {
                    Picker("模型列表", selection: $automaticModels) {
                        Text("自动获取").tag(true)
                        Text("手动填写").tag(false)
                    }
                    if automaticModels {
                        HStack(spacing: 8) {
                            if loadingModels { ProgressView().controlSize(.small) }
                            Text(loadingModels ? "正在获取…" : source.models.isEmpty ? "尚未获取" : "\(source.models.count) 个模型")
                                .foregroundStyle(.secondary)
                            Spacer()
                            Button("刷新") { Task { await loadModels() } }
                                .disabled(loadingModels || saving || !canFetch)
                        }
                        if !source.models.isEmpty {
                            ScrollView {
                                VStack(alignment: .leading, spacing: 6) {
                                    ForEach(source.models, id: \.self) { Text($0).frame(maxWidth: .infinity, alignment: .leading) }
                                }.font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                            }.frame(height: min(CGFloat(source.models.count) * 24, 144))
                        }
                        if let modelError { Text(modelError).font(.system(size: 12)).foregroundStyle(.orange) }
                    } else {
                        TextField("支持模型", text: $modelsText, prompt: Text("模型 ID，以逗号分隔；留空不限"))
                    }
                }
            }.formStyle(.grouped).scrollContentBackground(.hidden)

            Divider()
            VStack(spacing: 14) {
                if let localError { InfoNote(text: localError, symbol: "exclamationmark.triangle", color: .orange).frame(maxWidth: .infinity, alignment: .leading) }
                HStack {
                    Spacer()
                    Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(saving)
                    Button(saving ? "保存中…" : "保存来源") { save() }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(saving || !model.running || model.busy)
                }
            }.padding(.horizontal, 28).padding(.bottom, 24).padding(.top, 18)
        }.font(.system(size: 13)).background(AppPalette.canvas).frame(width: 660, height: 760)
            .task(id: discoveryInput) {
                fetchID = UUID(); loadingModels = false; modelError = nil
                guard automaticModels, canFetch else { return }
                do { try await Task.sleep(for: .milliseconds(800)) } catch { return }
                await loadModels()
            }
            .onDisappear { fetchID = UUID() }
    }

    private var conversationCapacitySection: some View {
        Section("活跃对话") {
            Toggle("限制活跃对话数", isOn: $capacityLimitEnabled)
                .toggleStyle(.switch)
            if capacityLimitEnabled {
                TextField("对话上限", text: $maxActiveConversations, prompt: Text("1–1024"))
                TextField("空闲释放时间（秒）", text: $activityWindowSeconds, prompt: Text("1–300，默认 15"))
            }
        }
    }

    private func configuredSource() throws -> Source {
        var draft = source
        if draft.proxy.mode == "direct" { draft.proxy.url = nil }
        else if !proxyPort.isEmpty || draft.proxy.url == nil || draft.proxy.localPort != nil {
            draft.proxy = try draft.proxy.withLocalPort(proxyPort)
        }
        return draft
    }

    private func loadModels() async {
        let requestID = UUID(); fetchID = requestID
        let input = discoveryInput
        loadingModels = true; modelError = nil
        defer { if fetchID == requestID { loadingModels = false } }
        do {
            let draft = try configuredSource()
            let models = try await model.fetchModels(for: draft, apiKey: apiKey)
            guard !Task.isCancelled, fetchID == requestID, discoveryInput == input else { return }
            source.models = models; modelsText = models.joined(separator: ", ")
        } catch {
            guard !Task.isCancelled, fetchID == requestID, discoveryInput == input else { return }
            modelError = error.localizedDescription
        }
    }

    private func save() {
        saving = true; localError = nil
        Task {
            do {
                var draft = try configuredSource()
                draft = try draft.withConversationCapacity(enabled: capacityLimitEnabled, limit: maxActiveConversations, windowSeconds: activityWindowSeconds)
                if !draft.official, balanceWarningEnabled {
                    guard let threshold = Double(balanceWarningAmount.trimmingCharacters(in: .whitespacesAndNewlines)), threshold.isFinite, threshold >= 0 else {
                        throw AppError.message("预警余额须为大于或等于 0 的金额。")
                    }
                    draft.balanceWarningThreshold = threshold
                } else { draft.balanceWarningThreshold = nil }
                draft.automaticModelDiscovery = automaticModels
                if !automaticModels {
                    draft.models = Array(Set(modelsText.split(whereSeparator: { $0 == "," || $0 == "，" }).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty })).sorted()
                }
                try await model.saveSource(draft, apiKey: apiKey); apiKey = ""; dismiss()
            }
            catch { localError = error.localizedDescription }
            saving = false
        }
    }
}
