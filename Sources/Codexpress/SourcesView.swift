import SwiftUI

private struct EditingSource: Identifiable {
    let source: Source
    let isNew: Bool
    var id: String { source.id }
}

struct SourcesView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        SourcesPage(sources: model.sources)
            .task(id: "\(model.running):\(model.accountRefreshGeneration)") { await model.refreshAccountInfo() }
            .task(id: "\(model.running):\(model.settingsWindowActive)") {
                guard model.running, model.settingsWindowActive else { return }
                while !Task.isCancelled {
                    model.refreshVisibleModels()
                    do { try await Task.sleep(for: .seconds(300)) } catch { break }
                }
            }
    }
}

struct SourcesPage: View {
    @EnvironmentObject private var model: AppModel
    let sources: [Source]
    var remote: RemoteSourcesContext? = nil
    @State private var editing: EditingSource?
    @State private var deleting: Source?
    @State private var search = ""
    @State private var expandedSources = Set<String>()
    @State private var showingRouting = false
    @State private var importing = false
    @State private var remoteAccount: RemoteConsoleAccountSession?
    @State private var remoteLogin: RemoteConsoleAccountSession?
    @State private var editError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            PageHeading("来源池", subtitle: "管理官方账号和 API 来源。") {
                if !sources.isEmpty {
                    Menu {
                        Button("官方账号") { addOfficial() }
                        Button("OpenAI API Platform") { addOpenAI() }
                        Button("Google AI Studio") { addGoogle() }
                        Button("Responses API") { addAPI() }
                        if remote != nil { Divider(); importButton }
                    } label: { Label("添加来源", systemImage: "plus") }
                        .menuStyle(.borderedButton).buttonStyle(.borderedProminent).disabled(!canEdit)
                }
            }
            HStack(spacing: 12) {
                if !sources.isEmpty {
                    TextField("搜索来源", text: $search).settingsField().frame(maxWidth: 240)
                        .accessibilityLabel("搜索来源")
                }
                Spacer(minLength: 0)
                Button { showingRouting = true } label: {
                    Label("分配策略", systemImage: "slider.horizontal.3")
                }.help(routingSummary)
            }
            if let editError { InfoNote(text: editError, color: .orange) }
            if let message = remote?.notice { InfoNote(text: message, color: .orange) }
            if sources.isEmpty {
                EmptyState("暂无来源", subtitle: remote != nil || model.running ? "添加 ChatGPT 账号、OpenAI API Platform、Google AI Studio 或 Responses API 服务，开始分配请求。" : "网关启动后可添加 ChatGPT 账号、OpenAI API Platform、Google AI Studio 或 Responses API 服务。", symbol: "server.rack") {
                    HStack(spacing: 10) {
                        Button("官方账号") { addOfficial() }.buttonStyle(.borderedProminent)
                        Button("OpenAI API Platform") { addOpenAI() }.buttonStyle(.bordered)
                        Button("Google AI Studio") { addGoogle() }.buttonStyle(.bordered)
                        Button("Responses API") { addAPI() }.buttonStyle(.bordered)
                        if remote != nil { importButton.buttonStyle(.bordered) }
                    }.disabled(!canEdit)
                }
            } else {
                HStack(spacing: 8) {
                    Label(routingSummary, systemImage: "arrow.triangle.branch").lineLimit(1)
                    Spacer()
                    Text("已启用 \(sources.filter(\.enabled).count) / \(sources.count)")
                        .monospacedDigit().fixedSize()
                }.font(AppTypography.caption).foregroundStyle(.secondary)
                sourceList
            }
        }.settingsPage()
            .onChange(of: remote == nil ? model.loginSource : nil) { _, sourceID in
                guard let sourceID else { return }
                expandedSources.insert(sourceID)
                let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
                if !query.isEmpty, sources.first(where: { $0.id == sourceID })?.name.localizedCaseInsensitiveContains(query) != true {
                    search = ""
                }
            }
            .sheet(item: $editing) { item in
                SourceEditor(source: item.source, isNew: item.isNew, remote: remote?.pool).environmentObject(model)
                    .onAppear { remote?.pool.draft.sourceEditorOpen = true }
                    .onDisappear { remote?.pool.draft.sourceEditorOpen = false }
            }
            .sheet(isPresented: $importing) {
                if let remote {
                    RemoteSourceImport(sources: model.sources, endpoint: remote.pool.target.endpoint, current: remote.pool.valid, hasCredentials: model.canManageCredentials) { source, includeKey in
                        try remote.pool.importSource(source, includeKey: includeKey, credential: model.credentialSnapshot.load(source.id))
                    }
                }
            }
            .sheet(item: $remoteLogin) { session in
                if let remote, let source = sources.first(where: { $0.id == session.source.id }), remote.canUse(source), session.target.current {
                    RemoteSourceAccountActions(source: session.source, snapshot: session.target.pool, generation: session.target.generation, canQueryUpstream: !remote.missingCredential(source))
                } else { unavailableAccount { remoteLogin = nil } }
            }
            .sheet(item: $remoteAccount) { session in
                if let remote, let source = sources.first(where: { $0.id == session.source.id }), remote.canUse(source), !remote.missingCredential(source), session.target.current {
                    RemoteConsoleAccount(session: session)
                } else { unavailableAccount { remoteAccount = nil } }
            }
            .sheet(isPresented: $showingRouting) {
                VStack(spacing: 0) {
                    if let remote {
                        ScrollView {
                            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                                PageHeading("分配策略", subtitle: "设置来源选择和失败重试。提交修改后生效。")
                                RemoteRoutingSettingsCards(draft: remote.pool.draft, target: remote.pool.target)
                            }.settingsPage().disabled(!remote.pool.valid)
                        }
                    } else { RoutingView().environmentObject(model) }
                    Divider()
                    HStack {
                        Spacer()
                        Button("完成") { showingRouting = false }.keyboardShortcut(.defaultAction)
                    }.padding(16)
                }.frame(width: 760, height: 620)
                    .onExitCommand { showingRouting = false }
            }
            .alert("删除来源？", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })) {
                Button("取消", role: .cancel) { deleting = nil }
                Button("删除", role: .destructive) {
                    if let source = deleting {
                        if let remote { modify { try remote.pool.remove(source) } }
                        else { model.removeSource(source.id) }
                    }
                    deleting = nil
                }
            } message: { Text(remote == nil ? "将移除此来源及其登录信息，用量记录保留。" : "提交修改后，服务器将移除此来源及其登录信息，用量记录保留。") }
    }

    private var importButton: some View {
        Button("从本机导入…") { importing = true }.disabled(model.sources.isEmpty || model.metadataLoadFailed)
    }

    private func unavailableAccount(close: @escaping () -> Void) -> some View {
        VStack(spacing: 14) { Text("来源配置已变更。请关闭后重新选择。"); Button("关闭", action: close) }.padding(24)
    }

    private var canEdit: Bool {
        if let remote { return remote.pool.valid }
        return model.running && !model.busy && !model.metadataLoadFailed
    }

    private func modify(_ action: () throws -> Void) {
        do { try action(); editError = nil } catch { editError = error.localizedDescription }
    }
    private var routingSummary: String {
        if let remote {
            let settings = remote.pool.draft.settings
            if settings["mode"]?.string == "fixed" {
                let fixed = sources.first { $0.id == settings["fixed_source_id"]?.string }
                return "固定来源 · \(fixed?.name ?? "未选择")"
            }
            return settings["load_balance"]?.bool == true ? "自动切换 · 负载均衡" : "自动切换 · 按优先级分配"
        }
        if model.routingMode == "fixed" { return "固定来源 · \(model.fixedName)" }
        return model.settings.loadBalanceEnabled ? "自动切换 · 负载均衡" : "自动切换 · 按优先级分配"
    }
    private func uniqueSourceName(_ base: String) -> String {
        let names = Set(sources.map(\.name))
        if !names.contains(base) { return base }
        var suffix = 2
        while names.contains("\(base) \(suffix)") { suffix += 1 }
        return "\(base) \(suffix)"
    }
    private func addOfficial() {
        var source = Source.newOfficial()
        if remote != nil { source.proxy = ProxyPolicy(mode: "system") }
        source.name = uniqueSourceName(source.name)
        editing = EditingSource(source: source, isNew: true)
    }
    private func addAPI() { editing = EditingSource(source: Source(), isNew: true) }
    private func addOpenAI() {
        var source = Source.newOpenAI()
        source.name = uniqueSourceName(source.name)
        editing = EditingSource(source: source, isNew: true)
    }
    private func addGoogle() {
        var source = Source.newGoogle()
        source.name = uniqueSourceName(source.name)
        editing = EditingSource(source: source, isNew: true)
    }

    private var sourceList: some View {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        let visible = sources.enumerated().filter { query.isEmpty || $0.element.name.localizedCaseInsensitiveContains(query) }
        return Group {
            if visible.isEmpty {
                EmptyState("未找到来源", subtitle: "试试其他名称。", symbol: "magnifyingglass") {
                    Button("清除搜索") { search = "" }
                }
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(visible, id: \.element.id) { index, source in
                            if index != visible.first?.offset { Divider().padding(.leading, 54) }
                            sourceRow(source, index: index)
                        }
                    }.cardSurface()
                }
            }
        }
    }

    private func sourceRow(_ source: Source, index: Int) -> some View {
        let expanded = expandedSources.contains(source.id)
        let live = remote.map { $0.live(source) } ?? model.state(source.id)
        return HStack(alignment: .top, spacing: 12) {
            Text("\(index + 1)").font(AppTypography.caption.weight(.semibold)).monospacedDigit()
                .foregroundStyle(.secondary).frame(width: 26, height: 26)
                .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 5))
                .help("来源池优先级").accessibilityLabel("优先级 \(index + 1)")
            VStack(alignment: .leading, spacing: 10) {
                sourceHeader(source, index: index, live: live)
                HStack(alignment: .center, spacing: 12) {
                    if source.official {
                        quotaSummary(source, quota: live["quota"])
                        if let remote { RemoteSourceCards(source: source, live: live, remote: remote) { remoteAccount = remote.session(source) } }
                        else { OfficialResetControl(source: source) }
                    } else if source.supportsBalanceQuery {
                        if let remote { RemoteSourceBalance(source: source, live: live, remote: remote) { remoteAccount = remote.session(source) } }
                        else { APIBalanceView(sourceID: source.id) }
                    } else {
                        Text(source.models.isEmpty ? (source.discoversModels ? "尚未获取模型" : "未限定模型") : "\(source.models.count) 个模型")
                            .font(AppTypography.caption).foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 4)
                    Button {
                        if expanded { expandedSources.remove(source.id) }
                        else { expandedSources.insert(source.id) }
                    } label: {
                        Label(expanded ? "收起" : "详情", systemImage: expanded ? "chevron.up" : "chevron.down")
                    }.buttonStyle(.borderless).font(AppTypography.caption).fixedSize()
                        .accessibilityLabel("\(expanded ? "收起" : "展开") \(source.name) 的详情")
                }
                if let reason = live["last_error"].string {
                    InfoNote(text: reason, symbol: "exclamationmark.triangle", color: .orange)
                }
                if remote == nil, let error = model.modelErrors[source.id] {
                    InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange)
                }
                if expanded {
                    Divider()
                    if source.official {
                        if let remote {
                            HStack {
                                if let plan = live["quota"]["plan_type"].string { Text(plan).foregroundStyle(.secondary) }
                                Spacer()
                                if remote.canLogin || remote.supports("official.refresh") {
                                    Button("登录与额度…") { remoteLogin = remote.session(source) }.disabled(!remote.canUse(source))
                                }
                            }.controlSize(.small)
                        } else { OfficialAccountView(source: source) }
                    }
                    sourceDetails(source, live: live)
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }.padding(16)
    }

    private func sourceHeader(_ source: Source, index: Int, live: JSONValue) -> some View {
        let missingCredential = remote.map { $0.missingCredential(source) } ?? model.isMissingCredential(source)
        let needsLogin = source.official && (missingCredential || (remote == nil && source.account_id == nil))
        let takingOver = remote.map { $0.takingOver(source) } ?? (model.status["manual_route"]["source_id"].string == source.id)
        let pending = remote.map { $0.pool.valid && !$0.savedIDs.contains(source.id) } ?? false
        let health = pending ? "待提交" : needsLogin ? "需要登录" : missingCredential ? "缺少密钥" : healthLabel(live["health"].string)
        let active = remote == nil ? model.sourceHasActivity(source.id) : (live["active_requests"].number ?? 0) > 0
        return HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(source.name).font(AppTypography.sectionTitle).lineLimit(1).help(source.name)
                Text(source.kindLabel).font(AppTypography.small).foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if takingOver {
                StatusPill(text: "正在抢占", color: .accentColor).fixedSize()
            }
            StatusPill(text: source.enabled ? health : "已停用",
                       color: sourceColor(source, live: live, needsCredential: needsLogin || missingCredential), active: active).fixedSize()
            if needsLogin {
                if let remote {
                    if remote.canLogin {
                        Button("登录…") { remoteLogin = remote.session(source) }.buttonStyle(.borderedProminent).disabled(!remote.canUse(source))
                    }
                } else {
                    Button("登录…") { model.login(source) }.buttonStyle(.borderedProminent)
                        .disabled(!canEdit || model.loginFlow != nil)
                }
            }
            Button(!source.official && missingCredential ? "填写 API Key…" : "编辑…") { editing = EditingSource(source: source, isNew: false) }
                .accessibilityLabel("编辑来源 \(source.name)")
                .disabled(remote != nil && !canEdit)
            Menu {
                Button(source.enabled ? "停用来源" : "启用来源") {
                    if let remote { modify { var changed = source; changed.enabled.toggle(); try remote.pool.stage(changed) } }
                    else { model.toggle(source) }
                }.disabled(!canEdit)
                Button(source.showInCmux ? "在 cmux 中隐藏" : "在 cmux 中展示") {
                    if let remote { modify { var changed = source; changed.show_in_cmux = !source.showInCmux; try remote.pool.stage(changed) } }
                    else { model.toggleShowInCmux(source) }
                }.disabled(!canEdit)
                Divider()
                if let remote { RemoteSourceRoutingActions(source: source, remote: remote) }
                else { SourceRoutingActions(source: source) }
                Divider()
                Button("提高优先级") { move(source, index: index, by: -1) }.disabled(index == 0 || !canEdit)
                Button("降低优先级") { move(source, index: index, by: 1) }.disabled(index == sources.count - 1 || !canEdit)
                Divider()
                Button("删除来源…", role: .destructive) { deleting = source }.disabled(!canEdit)
            } label: {
                Image(systemName: "ellipsis.circle").font(.system(size: 14))
            }.menuStyle(.borderlessButton).fixedSize()
                .accessibilityLabel("\(source.name) 的更多操作").help("更多操作")
        }.controlSize(.small)
    }

    private func move(_ source: Source, index: Int, by delta: Int) {
        if let remote { modify { try remote.pool.move(source, by: delta) } }
        else { model.move(IndexSet(integer: index), to: index + (delta > 0 ? 2 : -1)) }
    }

    private func quotaSummary(_ source: Source, quota: JSONValue) -> some View {
        let windows: [QuotaWindow]
        let referenceBelow: Bool
        if remote != nil {
            windows = quota["windows"].array.enumerated().map { QuotaWindow($0.element, index: $0.offset) }
            referenceBelow = windows.count > 1
        } else {
            let overview = QuotaOverview(quota)
            windows = overview.mainWindows; referenceBelow = overview.hasShortWindow
        }
        return HStack(alignment: .top, spacing: 14) {
            if windows.isEmpty { Text("暂无额度数据").font(AppTypography.caption).foregroundStyle(.secondary) }
            ForEach(windows) { window in
                VStack(alignment: .leading, spacing: 3) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(window.title).font(AppTypography.small).foregroundStyle(.secondary)
                        Text(window.remainingLabel).font(AccountTypography.value).monospacedDigit()
                            .foregroundStyle(AccountLevel.quota(window.remaining).color)
                            .accessibilityLabel("\(window.title)剩余 \(window.remainingLabel)")
                        if !referenceBelow { QuotaReferenceLabel(window: window) }
                    }
                    if referenceBelow { QuotaReferenceLabel(window: window) }
                    Text(window.resetLabel).font(AppTypography.small).foregroundStyle(.secondary)
                        .lineLimit(1).help(window.resetLabel)
                }
            }
            if remote == nil, model.refreshingQuotas.contains(source.id) {
                ProgressView().controlSize(.mini).help("正在更新额度")
            } else if quota["stale"].bool == true {
                Image(systemName: "clock").font(AppTypography.small).foregroundStyle(.orange)
                    .help("更新失败，显示上次获取的额度")
            }
        }
    }

    private func sourceDetails(_ source: Source, live: JSONValue) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            LabeledContent("服务地址") {
                Text(source.base_url).font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            LabeledContent("网络策略") {
                Text(source.policyLabel + (source.proxy.mode == "explicit" ? (source.proxy.url.map { " · \($0)" } ?? "") : ""))
                    .textSelection(.enabled)
            }
            LabeledContent("cmux 展示") {
                Text(source.showInCmux ? "展示" : "隐藏")
            }
            if remote != nil {
                RemoteSourceMeasurements(source: source, live: live)
            } else if live["admission"]["capacity_active_conversations"].number != nil || source.max_active_conversations != nil {
                LabeledContent("对话容量") {
                    SourceCapacityLabel(source: source, admission: live["admission"], showsRoutingNote: true)
                }
            }
            HStack(spacing: 8) {
                Text(source.models.isEmpty ? (source.discoversModels ? "尚未获取模型" : "未限定模型") : "\(source.models.count) 个模型")
                Spacer()
                if let remote {
                    if remote.supports("models.list") {
                        if source.discoversModels, remote.pool.valid {
                            RemoteSourceModelRefresh(source: source, remote: remote)
                        } else {
                            Button("查询模型…") { remoteLogin = remote.session(source) }.buttonStyle(.borderless)
                                .disabled(!remote.canUse(source) || remote.missingCredential(source))
                        }
                    }
                } else if source.discoversModels {
                    if model.refreshingModels.contains(source.id) { ProgressView().controlSize(.mini) }
                    Button("刷新模型") { Task { await model.refreshModels(source.id) } }
                        .buttonStyle(.borderless)
                        .disabled(model.busy || !model.running || model.isMissingCredential(source) || model.refreshingModels.contains(source.id) || (source.official && source.account_id == nil))
                }
            }
            if !source.models.isEmpty {
                Text(source.models.joined(separator: "、")).font(AppTypography.small)
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
        }.font(AppTypography.caption).foregroundStyle(.secondary).padding(.top, 2)
    }

    private func sourceColor(_ source: Source, live: JSONValue, needsCredential: Bool) -> Color {
        guard source.enabled else { return .secondary }
        if needsCredential { return .orange }
        switch live["health"].string { case "ready": return .green; case nil: return .secondary; default: return .orange }
    }
}
