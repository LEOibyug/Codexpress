import SwiftUI

struct SourceEditor: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State var source: Source
    let isNew: Bool
    let remote: RemotePoolDraft?
    private let initialModels: [String]
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
    @FocusState private var nameFocused: Bool

    init(source: Source, isNew: Bool, remote: RemotePoolDraft? = nil) {
        _source = State(initialValue: source); self.isNew = isNew; self.remote = remote
        initialModels = source.models
        _modelsText = State(initialValue: source.models.joined(separator: ", "))
        _automaticModels = State(initialValue: source.discoversModels)
        _proxyPort = State(initialValue: source.proxy.localPort ?? "")
        _balanceWarningEnabled = State(initialValue: source.balanceWarningThreshold != nil)
        _balanceWarningAmount = State(initialValue: source.balanceWarningThreshold.map { String($0) } ?? "")
        _capacityLimitEnabled = State(initialValue: source.max_active_conversations != nil)
        _maxActiveConversations = State(initialValue: source.max_active_conversations.map(String.init) ?? "3")
        _activityWindowSeconds = State(initialValue: String(source.activeConversationWindowSeconds))
    }

    private var remoteDestination: [String] {
        guard remote != nil else { return [] }
        return [source.base_url, source.proxy.mode, source.proxy.mode == "explicit" ? source.proxy.url ?? "" : ""]
    }

    // Clear an entered key in the same edit that changes its destination, before Save can run.
    private func destinationField(_ field: Binding<String>) -> Binding<String> {
        guard remote != nil else { return field }
        return Binding(get: { field.wrappedValue }, set: { value in
            if value != field.wrappedValue && !apiKey.isEmpty {
                apiKey = ""; localError = "地址或代理已更改，请重新输入此目标的 API 密钥。"
            }
            field.wrappedValue = value
        })
    }

    private var discoveryInput: [String] {
        if remote != nil { return remoteDestination + [String(apiKey.isEmpty), String(automaticModels)] }
        return [source.base_url, source.proxy.mode, proxyPort, apiKey, String(automaticModels), String(model.running), model.settings.sources.first(where: { $0.id == source.id })?.account_id ?? ""]
    }
    private var canFetch: Bool {
        if let remote {
            guard remote.valid, !isNew, apiKey.isEmpty, remote.target.management.capabilities.contains("models.list"),
                  let configured = try? configuredSource() else { return false }
            return remote.isSavedDestination(configured)
        }
        guard model.running else { return false }
        if source.official {
            return !model.isMissingCredential(source) && model.settings.sources.contains { $0.id == source.id && $0.account_id != nil }
        }
        return !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || (!isNew && !model.isMissingCredential(source) && model.settings.sources.first(where: { $0.id == source.id })?.base_url == source.base_url)
    }

    private var keyPrompt: String {
        if remote != nil { return isNew ? "请输入 API 密钥" : "留空保留密钥；更换地址或代理后须重新输入" }
        return isNew || model.isMissingCredential(source) ? "请输入 API 密钥" : "留空保留现有密钥"
    }

    private var saveTitle: String {
        if saving { return "保存中…" }
        if remote == nil && isNew && source.official && model.loginFlow == nil { return "添加并登录…" }
        return isNew ? "添加来源" : "保存来源"
    }

    private var saveUnavailable: Bool {
        if let remote { return saving || !remote.valid }
        return saving || !model.running || model.busy || model.metadataLoadFailed
    }

    var body: some View {
        VStack(spacing: 0) {
            PageHeading(isNew ? "添加来源" : "编辑来源", subtitle: source.official ? "ChatGPT 官方账号" : source.kindLabel)
                .padding(.horizontal, 24).padding(.top, 20).padding(.bottom, 8)

            Form {
                Section("基本信息") {
                    TextField("名称", text: $source.name).focused($nameFocused)
                    if !source.official {
                        TextField("API 基址", text: destinationField($source.base_url)).textContentType(.URL)
                        SecureField("API 密钥", text: $apiKey, prompt: Text(keyPrompt))
                    }
                    Toggle("启用此来源", isOn: $source.enabled).toggleStyle(.switch)
                    Toggle("在 cmux 中展示", isOn: Binding(get: { source.showInCmux }, set: { source.show_in_cmux = $0 }))
                        .toggleStyle(.switch)
                    if remote == nil && isNew && source.official && model.loginFlow != nil {
                        Text("完成当前登录后，可在来源池登录此账号。").font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                }
                Section("网络") {
                    Picker("出站策略", selection: destinationField($source.proxy.mode)) {
                        if !source.official { Text("强制直连").tag("direct") }
                        Text("系统代理").tag("system")
                        Text("指定代理").tag("explicit")
                    }
                    if source.proxy.mode == "system" {
                        Text("不做代理干预，直接交由系统代理、环境变量或 TUN 路由处理。")
                            .font(.system(size: 12)).foregroundStyle(.secondary)
                    } else if source.proxy.mode == "explicit" {
                        if remote != nil {
                            TextField("代理 URL", text: destinationField(Binding(get: { source.proxy.url ?? "" }, set: { source.proxy.url = $0 })), prompt: Text("例如 http://127.0.0.1:7890"))
                            Text("localhost 指服务器，不是这台 Mac。不要在 URL 中填写凭据。")
                                .font(.system(size: 12)).foregroundStyle(.secondary)
                        } else {
                            TextField("本地代理端口", text: $proxyPort, prompt: Text("例如 7890"))
                            if source.proxy.localPort == nil, let existing = source.proxy.url {
                                Text("现有代理：\(existing)")
                                    .font(.system(size: 12)).foregroundStyle(.secondary)
                            }
                        }
                    }
                    if remote != nil && source.official {
                        Text("更换代理后，需要在服务器重新登录并重新确认自动重置授权。")
                            .font(.system(size: 12)).foregroundStyle(.secondary)
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
                        if let remote, !canFetch {
                            Text(remote.target.management.capabilities.contains("models.list") ? "仅能查询已提交的来源。若修改了地址、代理或密钥，请先提交更改。" : "此服务器不支持手动刷新模型。")
                                .font(.system(size: 12)).foregroundStyle(.secondary)
                        } else if source.official && !canFetch {
                            Text("登录后自动获取模型。").font(.system(size: 12)).foregroundStyle(.secondary)
                        }
                        if !source.models.isEmpty {
                            ScrollView {
                                VStack(alignment: .leading, spacing: 6) {
                                    ForEach(source.models, id: \.self) { Text($0).frame(maxWidth: .infinity, alignment: .leading) }
                                }.font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                            }.frame(height: min(CGFloat(source.models.count) * 24, 144))
                        }
                        if let modelError { InfoNote(text: modelError, symbol: "exclamationmark.triangle", color: .orange) }
                    } else {
                        TextField("支持模型", text: $modelsText, prompt: Text("模型 ID，以逗号分隔；留空不限"))
                    }
                    if let remote, remote.target.supportsSourceCompactionV2, let enabled = source.effectiveRemoteCompactionV2 {
                        Toggle("允许 OMP 使用流式上下文压缩（V2）", isOn: Binding(get: { enabled }, set: { source.remote_compaction_v2 = $0 }))
                            .toggleStyle(.switch)
                        if !source.official {
                            Text("仅在确认此 API 支持 Responses 流式上下文压缩时开启。")
                                .font(.system(size: 12)).foregroundStyle(.secondary)
                        }
                    }
                }
                conversationCapacitySection
                if source.supportsBalanceQuery {
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
            }.formStyle(.grouped).scrollContentBackground(.hidden).disabled(saving)

            Divider()
            VStack(alignment: .leading, spacing: 10) {
                if let localError { InfoNote(text: localError, symbol: "exclamationmark.triangle", color: .orange) }
                HStack(spacing: 12) {
                    Text(remote == nil ? "保存后，来源设置才会生效。" : "保存到草稿，提交后才在服务器生效。")
                        .font(AppTypography.caption).foregroundStyle(.secondary)
                    Spacer()
                    Button("取消", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction).disabled(saving)
                    Button(saveTitle) { save() }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(saveUnavailable)
                }
            }.padding(.horizontal, 24).padding(.vertical, 14)
        }.font(AppTypography.body).background(AppPalette.canvas).frame(width: 640, height: 600)
            .interactiveDismissDisabled(saving)
            .defaultFocus($nameFocused, true)
            .task(id: discoveryInput) {
                fetchID = UUID(); loadingModels = false; modelError = nil
                guard remote == nil, automaticModels, canFetch, !saving else { return }
                do { try await Task.sleep(for: .milliseconds(800)) } catch { return }
                guard !saving else { return }
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
        if draft.proxy.mode != "explicit" { draft.proxy.url = nil }
        else if remote == nil && (!proxyPort.isEmpty || draft.proxy.url == nil || draft.proxy.localPort != nil) {
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
            let models: [String]
            if let remote {
                guard canFetch else { return }
                let response = try await remote.target.call("models.list", ["source_id": .string(source.id)])
                guard canFetch else { return }
                models = try remoteConsoleDecode([String].self, response["models"])
            } else {
                let draft = try configuredSource()
                models = try await model.fetchModels(for: draft, apiKey: apiKey)
            }
            guard !Task.isCancelled, !saving, fetchID == requestID, discoveryInput == input else { return }
            source.models = models
        } catch {
            guard !Task.isCancelled, !saving, fetchID == requestID, discoveryInput == input else { return }
            modelError = error.localizedDescription
        }
    }

    private func save() {
        saving = true; localError = nil
        fetchID = UUID(); loadingModels = false
        Task {
            do {
                var draft = try configuredSource()
                draft = try draft.withConversationCapacity(enabled: capacityLimitEnabled, limit: maxActiveConversations, windowSeconds: activityWindowSeconds)
                if draft.supportsBalanceQuery, balanceWarningEnabled {
                    guard let threshold = Double(balanceWarningAmount.trimmingCharacters(in: .whitespacesAndNewlines)), threshold.isFinite, threshold >= 0 else {
                        throw AppError.message("预警余额须为大于或等于 0 的金额。")
                    }
                    draft.balanceWarningThreshold = threshold
                } else { draft.balanceWarningThreshold = nil }
                draft.automaticModelDiscovery = automaticModels
                if !automaticModels {
                    draft.models = Array(Set(modelsText.split(whereSeparator: { $0 == "," || $0 == "，" }).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty })).sorted()
                } else if remote == nil, source.models == initialModels, apiKey.isEmpty,
                          let current = model.settings.sources.first(where: { $0.id == source.id }),
                          current.base_url == draft.base_url, current.proxy == draft.proxy {
                    draft.models = current.models
                }
                if let remote {
                    try remote.stage(draft, key: apiKey)
                    apiKey = ""; saving = false; dismiss()
                    return
                }
                try await model.saveSource(draft, apiKey: apiKey)
                apiKey = ""; dismiss()
                if isNew && draft.defaultsDiscoveredModelsDisabled && !draft.models.isEmpty {
                    model.notice = "已添加 \(draft.kindLabel) 来源，新获取的模型默认停用，请在「模型」页勾选启用。"
                }
                if isNew && draft.official && model.loginFlow == nil { model.login(draft) }
            }
            catch { localError = error.localizedDescription }
            saving = false
        }
    }
}
