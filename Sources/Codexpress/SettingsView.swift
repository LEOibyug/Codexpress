import SwiftUI

private enum SettingsPage: String, CaseIterable, Identifiable {
    case sources = "来源", routing = "路由", usage = "用量", remote = "远端", chatbot = "聊天客户端", general = "设置"
    var id: String { rawValue }
    var symbol: String {
        switch self {
        case .sources: return "server.rack"
        case .routing: return "arrow.triangle.branch"
        case .usage: return "chart.bar.xaxis"
        case .remote: return "desktopcomputer.and.arrow.down"
        case .chatbot: return "bubble.left.and.bubble.right"
        case .general: return "slider.horizontal.3"
        }
    }
}

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var page: SettingsPage? = .sources

    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    Image(systemName: "arrow.triangle.branch")
                        .font(.system(size: 17, weight: .semibold)).foregroundStyle(.white)
                        .frame(width: 34, height: 34)
                        .background(Color.accentColor.gradient, in: RoundedRectangle(cornerRadius: 10))
                    Text("Codexpress").font(.system(size: 14, weight: .semibold))
                    Spacer(minLength: 0)
                }.padding(.horizontal, 18).padding(.top, 18).padding(.bottom, 23)

                List(SettingsPage.allCases, selection: $page) { item in
                    Label {
                        Text(item.rawValue).font(.system(size: 13, weight: page == item ? .medium : .regular))
                    } icon: {
                        Image(systemName: item.symbol).font(.system(size: 14, weight: .medium)).frame(width: 20)
                    }.listItemTint(.monochrome).padding(.vertical, 5).tag(item)
                }.listStyle(.sidebar).scrollContentBackground(.hidden)

                VStack(alignment: .leading, spacing: 9) {
                    Divider().padding(.bottom, 5)
                    HStack(spacing: 7) {
                        Circle().fill(model.running ? .green : .orange).frame(width: 6, height: 6)
                        Text(model.running ? "网关运行中" : "网关未连接").font(.system(size: 11, weight: .medium))
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(18)
            }.navigationSplitViewColumnWidth(min: 178, ideal: 190, max: 210)
        } detail: {
            VStack(spacing: 0) {
                notices
                Group {
                    switch page ?? .sources {
                    case .sources: SourcesView()
                    case .routing: RoutingView()
                    case .usage: UsageView()
                    case .remote: SSHRemoteView()
                    case .chatbot: ChatbotSettingsView()
                    case .general: GeneralSettingsView()
                    }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }.background(AppPalette.canvas)
        }.font(.system(size: 13)).frame(minWidth: 860, minHeight: 620)
            .overlay(alignment: .bottomLeading) { AppFeedbackOverlay() }
    }

    @ViewBuilder private var notices: some View {
        if model.metadataLoadFailed || model.pendingSync {
            VStack(spacing: 8) {
                if model.metadataLoadFailed {
                    banner("来源文件无法读取，已进入只读保护。请从备份修复 sources.json 后重启。", symbol: "lock.shield", color: .orange)
                }
                if model.pendingSync {
                    banner("配置尚未应用到网关，当前路由仍使用上次设置。请重试启动。", symbol: "arrow.triangle.2.circlepath", color: .orange)
                }
            }.padding(.horizontal, 28).padding(.top, 18).frame(maxWidth: 820)
        }
    }

    private func banner(_ text: String, symbol: String, color: Color, dismiss: (() -> Void)? = nil) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(color)
            Text(text).font(.system(size: 12)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            if let dismiss {
                Button(action: dismiss) { Image(systemName: "xmark").font(.system(size: 10, weight: .medium)) }
                    .buttonStyle(.plain).foregroundStyle(.secondary).accessibilityLabel("关闭提示")
            }
        }.padding(12).background(color.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
    }
}

private struct EditingSource: Identifiable {
    let source: Source
    let isNew: Bool
    var id: String { source.id }
}

struct SourcesView: View {
    @EnvironmentObject var model: AppModel
    @State private var editing: EditingSource?
    @State private var deleting: Source?

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            PageHeading("来源") {
                DirectOfficialLoginButton().controlSize(.small)
                Menu {
                    Button("官方账号") { addOfficial() }
                    Button("Responses API") { addAPI() }
                } label: { Label("添加来源", systemImage: "plus") }
                    .menuStyle(.borderlessButton).fixedSize().padding(.horizontal, 12).padding(.vertical, 8)
                    .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 7)).foregroundStyle(.white)
                    .disabled(!canEdit)
            }
            if model.connectionMode == "official" {
                HStack {
                    Label("官方连接 · 系统代理", systemImage: "person.crop.circle").foregroundStyle(.secondary)
                    Spacer()
                    Button("使用来源池") { model.switchCodexConnection(.gateway) }
                        .disabled(!canEdit)
                }.font(.system(size: 12))
            }
            if model.sources.isEmpty {
                EmptyState("暂无来源", subtitle: "添加 ChatGPT 账号或 API 服务。", symbol: "server.rack") {
                    HStack(spacing: 10) {
                        Button("添加官方账号") { addOfficial() }.buttonStyle(.borderedProminent)
                        Button("添加 API") { addAPI() }.buttonStyle(.bordered)
                    }.controlSize(.large).disabled(!canEdit)
                }
            } else {
                ScrollView {
                    LazyVStack(spacing: 14) {
                        ForEach(model.sources) { source in sourceCard(source) }
                    }.padding(.bottom, 2)
                }
            }
        }.settingsPage()
            .task(id: "\(model.running):\(model.accountRefreshGeneration)") { await model.refreshAccountInfo() }
            .sheet(item: $editing) { item in SourceEditor(source: item.source, isNew: item.isNew).environmentObject(model) }
            .alert("删除来源？", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })) {
                Button("取消", role: .cancel) { deleting = nil }
                Button("删除", role: .destructive) { if let source = deleting { model.removeSource(source.id) }; deleting = nil }
            } message: { Text("将移除此来源及其登录信息，用量记录保留。") }
    }

    private var canEdit: Bool { model.running && !model.busy && !model.metadataLoadFailed }
    private func addOfficial() { editing = EditingSource(source: .newOfficial(), isNew: true) }
    private func addAPI() { editing = EditingSource(source: Source(), isNew: true) }

    private func sourceCard(_ source: Source) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 11) {
                Image(systemName: source.official ? "person.crop.circle" : "terminal")
                    .font(.system(size: 19, weight: .regular)).foregroundStyle(Color.accentColor)
                    .frame(width: 40, height: 40).background(Color.accentColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 11))
                VStack(alignment: .leading, spacing: 4) {
                    Text(source.name).font(.system(size: 14, weight: .semibold))
                    Text(source.official ? "官方账号" : "Responses API").font(.system(size: 11)).foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
                StatusPill(text: source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用", color: sourceColor(source))
                Toggle("启用 \(source.name)", isOn: Binding(get: { source.enabled }, set: { _ in model.toggle(source) }))
                    .toggleStyle(.switch).labelsHidden().controlSize(.small).disabled(model.busy)
                Menu {
                    Button("编辑…") { editing = EditingSource(source: source, isNew: false) }
                    Button("删除…", role: .destructive) { deleting = source }
                } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).frame(width: 20)
            }
            VStack(alignment: .leading, spacing: 10) {
                Text(source.base_url).font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary)
                    .textSelection(.enabled).lineLimit(2)
                Label(source.policyLabel + (source.proxy.url.map { " · \($0)" } ?? ""), systemImage: source.proxy.mode == "direct" ? "network" : "shield.lefthalf.filled")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    Label(source.models.isEmpty ? (source.discoversModels ? "尚未获取模型" : "未限定模型") : "\(source.models.count) 个模型", systemImage: "square.stack.3d.up")
                    Spacer()
                    if source.discoversModels {
                        if model.refreshingModels.contains(source.id) { ProgressView().controlSize(.mini) }
                        Button("刷新模型") { Task { await model.refreshModels(source.id) } }
                            .buttonStyle(.plain).foregroundStyle(Color.accentColor)
                            .disabled(model.busy || !model.running || model.refreshingModels.contains(source.id) || (source.official && source.account_id == nil))
                    }
                }.font(.system(size: 11)).foregroundStyle(.secondary)
            }.padding(12).frame(maxWidth: .infinity, alignment: .leading)
                .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 8))
            if let error = model.modelErrors[source.id] { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            if let reason = model.state(source.id)["last_error"].string { InfoNote(text: reason, symbol: "exclamationmark.triangle", color: .orange) }
            if source.official { Divider(); OfficialAccountView(source: source) }
            else { APIBalanceView(sourceID: source.id) }
        }.padding(18).cardSurface()
    }

    private func sourceColor(_ source: Source) -> Color {
        guard source.enabled else { return .secondary }
        switch model.state(source.id)["health"].string { case "ready": return .green; case nil: return .secondary; default: return .orange }
    }
}

struct RoutingView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                PageHeading("路由")
                SettingsCard("路由模式", symbol: "arrow.triangle.branch") {
                    Picker("分配方式", selection: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: model.settings.fixed_source_id ?? model.sources.first?.id) })) {
                        Text("自动切换").tag("auto")
                        Text("固定来源").tag("fixed")
                    }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 330).disabled(model.busy || model.sources.isEmpty)
                    if model.routingMode == "fixed" {
                        Picker("固定来源", selection: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) })) {
                            ForEach(model.sources.filter(\.enabled)) { Text($0.name).tag($0.id) }
                        }.disabled(model.busy)
                        InfoNote(text: "来源不可用时暂停请求。")
                    } else {
                        InfoNote(text: "新对话按优先级分配，连续对话优先沿用原来源。")
                    }
                }
                SettingsCard("官方余量保护", symbol: "shield.lefthalf.filled") {
                    HStack {
                        Text("保留余量阈值").font(.system(size: 12))
                        Spacer()
                        Stepper(value: Binding(get: { model.settings.lowQuotaPercent }, set: { model.changeLowQuotaThreshold($0) }), in: 1...99) {
                            Text("\(model.settings.lowQuotaPercent)%").monospacedDigit().frame(width: 42, alignment: .trailing)
                        }.fixedSize().disabled(model.busy)
                    }
                    Text("任一短周期或周额度不高于此值时，为已有活跃对话保留余量；额度归零后跳过此来源。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                SettingsCard("对话接续", symbol: "arrow.triangle.2.circlepath") {
                    Picker("闲置回切时间", selection: Binding(get: { model.settings.idleReturnDelay }, set: { model.changeIdleReturnDelay($0) })) {
                        Text("15 分钟").tag(15)
                        Text("30 分钟").tag(30)
                        Text("1 小时").tag(60)
                        Text("2 小时").tag(120)
                        Text("6 小时").tag(360)
                        Text("24 小时").tag(1440)
                        Text("不自动回切").tag(0)
                    }.disabled(model.busy || model.routingMode == "fixed")
                    Text("从上次请求结束计时。闲置到期后按当前优先级回到可用来源，官方与 API 使用同一排序。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                    Divider().padding(.vertical, 3)
                    Toggle("无法续接时移除加密块并重建上下文", isOn: Binding(get: { model.settings.degradedContextFallback ?? true }, set: { model.changeContextFallback($0) }))
                        .disabled(model.busy)
                    Text("跨来源接续加密上下文时使用，可能缺失隐藏推理或压缩摘要。触发时发送系统通知，本地对话记录保留。关闭后，含此类状态的对话保留当前来源。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                if model.sources.isEmpty {
                    EmptyState("暂无来源", subtitle: "添加来源后可设置优先级。", symbol: "arrow.triangle.branch")
                } else {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Text("来源优先级").font(.system(size: 12, weight: .semibold))
                            Spacer()
                            Text("拖动或使用箭头排序").font(.system(size: 11)).foregroundStyle(.tertiary)
                        }.padding(.horizontal, 4)
                        List {
                            ForEach(Array(model.sources.enumerated()), id: \.element.id) { index, source in
                                HStack(spacing: 12) {
                                    Text("\(index + 1)").font(.system(size: 11, weight: .medium, design: .monospaced))
                                        .foregroundStyle(.secondary).frame(width: 26, height: 26).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 7))
                                    VStack(alignment: .leading, spacing: 4) {
                                        Text(source.name).font(.system(size: 13, weight: .medium))
                                        Text(source.policyLabel).font(.system(size: 11)).foregroundStyle(.secondary)
                                        if let active = model.state(source.id)["admission"]["active_conversations"].number {
                                            Text("\(Int(active)) 个活跃对话" + (source.official ? " · \(model.state(source.id)["admission"]["routing_note"].string ?? "")" : ""))
                                                .font(.system(size: 10)).foregroundStyle(.secondary)
                                        }
                                    }
                                    Spacer()
                                    StatusPill(text: source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用")
                                    HStack(spacing: 8) {
                                        Button { model.move(IndexSet(integer: index), to: index - 1) } label: { Image(systemName: "chevron.up") }.disabled(index == 0 || model.busy).help("提高优先级")
                                        Button { model.move(IndexSet(integer: index), to: index + 2) } label: { Image(systemName: "chevron.down") }.disabled(index == model.sources.count - 1 || model.busy).help("降低优先级")
                                    }.buttonStyle(.borderless).foregroundStyle(.secondary)
                                }.padding(.vertical, 9).listRowBackground(Color.clear)
                            }.onMove { model.move($0, to: $1) }
                        }.listStyle(.plain).scrollContentBackground(.hidden)
                            .frame(height: CGFloat(min(max(model.sources.count * 80 + 14, 120), 334)))
                            .padding(7).cardSurface().disabled(model.busy)
                    }
                }
            }
        }.settingsPage()
    }
}

struct GeneralSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var port = 17863
    @State private var home = ""
    @State private var confirming: String?
    @State private var connectionDetails = false
    @State private var selectedBackup = ""

    private var preferencesChanged: Bool { port != model.settings.port || home != model.settings.codexHome }
    private var canPreview: Bool { !model.busy && model.running && !preferencesChanged && !model.portNeedsRestart && !model.metadataLoadFailed }

    var body: some View {
        ScrollViewReader { scroll in
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    PageHeading("设置") {
                        HStack(spacing: 10) {
                            if preferencesChanged { Text("未保存").font(.system(size: 11)).foregroundStyle(.secondary) }
                            Button("保存设置") { model.savePreferences(port: port, codexHome: home) }
                                .disabled(model.busy || model.metadataLoadFailed || !preferencesChanged)
                        }
                    }
                    gatewayCard
                    CredentialStorageView()
                    HistoryCacheView()
                    ModelCompactionView()
                    connectionCard
                    if model.preview != .null { previewCard.id("connection-preview") }
                    backupCard
                }.settingsPage()
            }.onChange(of: model.preview != .null) { _, visible in
                if visible { withAnimation(.easeInOut(duration: 0.25)) { scroll.scrollTo("connection-preview", anchor: .center) } }
            }.onAppear { port = model.settings.port; home = model.settings.codexHome }
                .task { await model.refreshConfigurationBackups() }
                .alert(confirming == "backup.restore" ? "还原所选备份？" : confirming == "config.restore" ? "恢复原连接？" : "应用连接设置？", isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } })) {
                    Button("取消", role: .cancel) { confirming = nil }
                    Button("确认") { if confirming == "backup.restore" { model.restoreConfigurationBackup(backupID) } else if let op = confirming { model.configAction(op) }; confirming = nil }
                } message: { Text(confirming == "backup.restore" ? "将还原此备份的 config.toml、配置档案和 auth.json 文件认证，并重启 Codex。当前状态会先备份；钥匙串登录需在 Codex 中确认。" : "配置目录：\(model.settings.codexHome)\n旧配置与文件认证将先备份，再更新连接并重启 Codex。") }
        }
    }

    private var gatewayCard: some View {
        SettingsCard("Codex／SSH 网关", symbol: "network", accessory: {
            StatusPill(text: model.running ? "运行中" : "未连接", color: model.running ? .green : .orange)
        }) {
            VStack(spacing: 13) {
                SettingRow(title: "监听端口", subtitle: "更改后需重新启动 Codexpress") {
                    HStack(spacing: 8) {
                        TextField("端口", value: $port, format: .number.grouping(.never))
                            .font(.system(size: 12, design: .monospaced)).multilineTextAlignment(.trailing).settingsField().frame(width: 90)
                        if !model.running { Button("重试") { model.retry() } }
                    }
                }
                Divider().overlay(AppPalette.border)
                SettingRow(title: "登录时启动") {
                    Toggle("登录时启动 Codexpress", isOn: Binding(get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                        .toggleStyle(.switch).labelsHidden().controlSize(.small)
                }
                HStack(spacing: 6) {
                    Image(systemName: "link").font(.system(size: 10))
                    Text(model.baseURL).font(.system(size: 11, design: .monospaced)).textSelection(.enabled)
                    Spacer()
                }.foregroundStyle(.secondary).padding(.top, 2)
                Text("SSH 远端共用此端口，使用独立机器 Key；仅用于可信局域网或 VPN。").font(.system(size: 11)).foregroundStyle(.secondary)
            }
        }
    }

    private var connectionCard: some View {
        SettingsCard("Codex 连接", symbol: "link") {
            VStack(alignment: .leading, spacing: 13) {
                HStack(spacing: 12) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(model.connectionMode == "official" ? "官方连接 · 系统代理" : model.connectionMode == "gateway" ? "来源池" : "连接方式")
                            .font(.system(size: 12, weight: .medium))
                        Text("切换时会重启 Codex").font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if model.connectionMode == "official" {
                        Button("使用来源池") { model.switchCodexConnection(.gateway) }
                            .disabled(model.busy || !model.running || preferencesChanged)
                    }
                    DirectOfficialLoginButton().disabled(preferencesChanged)
                }
                Divider().overlay(AppPalette.border)
                Text("配置目录").font(.system(size: 11, weight: .medium)).foregroundStyle(.secondary)
                TextField("Codex 配置目录（绝对路径）", text: $home)
                    .font(.system(size: 12, design: .monospaced)).settingsField()
                HStack(alignment: .center, spacing: 18) {
                    if preferencesChanged { InfoNote(text: "请先保存设置。") }
                    Spacer(minLength: 0)
                    Button { model.configPreview() } label: { Label("预览连接配置", systemImage: "doc.text.magnifyingglass") }
                        .buttonStyle(.borderedProminent).disabled(!canPreview)
                }
                if model.portNeedsRestart { InfoNote(text: "请重启 Codexpress 以启用新端口。", symbol: "arrow.clockwise", color: .orange) }
                DisclosureGroup("生效方式", isExpanded: $connectionDetails) {
                    Text("应用后自动重启 Codex。已有对话须确认原来源的同一账号和密钥，才能接入网关；未确认的供应商保持原连接。")
                        .font(.system(size: 11)).foregroundStyle(.secondary).padding(.top, 7)
                }.font(.system(size: 11)).foregroundStyle(.secondary)
            }
        }
    }

    private var backupID: String { model.configurationBackups.contains(where: { $0["id"].string == selectedBackup }) ? selectedBackup : model.configurationBackups.first?["id"].string ?? "" }
    private func backupLabel(_ value: JSONValue) -> String {
        let reasons = ["before_apply":"应用配置前", "before_official":"官方登录前", "before_restore":"还原备份前", "before_connection_restore":"恢复连接前"]
        return timestampLabel(value["created_at"]) + " · " + (reasons[value["reason"].string ?? ""] ?? "配置备份")
    }
    private var backupCard: some View {
        SettingsCard("配置备份", symbol: "clock.arrow.circlepath") {
            VStack(alignment: .leading, spacing: 12) {
                if model.configurationBackups.isEmpty {
                    Text("暂无备份，应用配置时自动创建。 ").font(.system(size: 12)).foregroundStyle(.secondary)
                } else {
                    Picker("备份时间", selection: Binding(get: { backupID }, set: { selectedBackup = $0 })) {
                        ForEach(Array(model.configurationBackups.enumerated()), id: \.offset) { _, item in Text(backupLabel(item)).tag(item["id"].string ?? "") }
                    }
                    Text("还原配置文件与 auth.json 等文件认证；钥匙串登录由 Codex 管理。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                HStack {
                    Button("刷新") { Task { await model.refreshConfigurationBackups() } }.disabled(model.busy || !model.running)
                    Spacer()
                    Button("还原备份…") { confirming = "backup.restore" }
                        .disabled(backupID.isEmpty || model.busy || !model.running || preferencesChanged)
                }
            }
        }
    }

    private var previewCard: some View {
        SettingsCard("连接预览", symbol: "doc.text") {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .top, spacing: 12) {
                    configurationBlock("当前配置", value: model.preview["before"].string ?? "")
                    configurationBlock("应用后", value: model.preview["after"].string ?? "")
                }
                ForEach(Array(model.preview["model_limits"].array.enumerated()), id: \.offset) { _, rule in
                    HStack {
                        Text(rule["model"].string ?? "").font(.system(size: 11, weight: .medium, design: .monospaced))
                        Spacer()
                        Text("压缩 \(countLabel(rule["auto_compact_token_limit"])) / 窗口 \(countLabel(rule["context_window"])) Token")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                }
                if !model.preview["compatibility_provider_ids"].array.isEmpty {
                    Text("接入已有供应商连接：" + model.preview["compatibility_provider_ids"].array.compactMap(\.string).joined(separator: "、"))
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                InfoNote(text: "应用前自动备份配置与文件认证，并重启 Codex。")
                if !model.preview["legacy_providers"].array.isEmpty || model.preview["origin_error"].string != nil { LegacyProviderOriginsView() }
                HStack(spacing: 10) {
                    Button("恢复原连接…") { confirming = "config.restore" }.disabled(model.preview["attached"].bool != true)
                    Spacer()
                    Button("应用连接设置…") { confirming = "config.attach" }
                        .buttonStyle(.borderedProminent).disabled(model.portNeedsRestart || model.metadataLoadFailed || preferencesChanged)
                }.disabled(model.busy)
            }
        }
    }

    private func configurationBlock(_ title: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 11, weight: .medium)).foregroundStyle(.secondary)
            ScrollView([.vertical, .horizontal]) {
                Text(value.isEmpty ? "（空配置）" : value).font(.system(size: 10, design: .monospaced))
                    .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .topLeading).padding(12)
            }.frame(height: 175).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 8))
        }.frame(maxWidth: .infinity)
    }
}
