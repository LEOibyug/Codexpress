import SwiftUI

private struct EditingSource: Identifiable {
    let source: Source
    let isNew: Bool
    var id: String { source.id }
}

struct SourcesView: View {
    @EnvironmentObject var model: AppModel
    @State private var editing: EditingSource?
    @State private var deleting: Source?
    @State private var search = ""
    @State private var expandedSources = Set<String>()
    @State private var showingRouting = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            PageHeading("来源池", subtitle: "管理官方账号和 API 来源。") {
                if !model.sources.isEmpty {
                    Menu {
                        Button("官方账号") { addOfficial() }
                        Button("OpenAI API Platform") { addOpenAI() }
                        Button("Google AI Studio") { addGoogle() }
                        Button("Responses API") { addAPI() }
                    } label: { Label("添加来源", systemImage: "plus") }
                        .menuStyle(.borderedButton).buttonStyle(.borderedProminent).disabled(!canEdit)
                }
            }
            HStack(spacing: 12) {
                if !model.sources.isEmpty {
                    TextField("搜索来源", text: $search).settingsField().frame(maxWidth: 240)
                        .accessibilityLabel("搜索来源")
                }
                Spacer(minLength: 0)
                Button { showingRouting = true } label: {
                    Label("分配策略", systemImage: "slider.horizontal.3")
                }.help(routingSummary)
            }
            if model.sources.isEmpty {
                EmptyState("暂无来源", subtitle: model.running ? "添加 ChatGPT 账号、OpenAI API Platform、Google AI Studio 或 Responses API 服务，开始分配请求。" : "网关启动后可添加 ChatGPT 账号、OpenAI API Platform、Google AI Studio 或 Responses API 服务。", symbol: "server.rack") {
                    HStack(spacing: 10) {
                        Button("官方账号") { addOfficial() }.buttonStyle(.borderedProminent)
                        Button("OpenAI API Platform") { addOpenAI() }.buttonStyle(.bordered)
                        Button("Google AI Studio") { addGoogle() }.buttonStyle(.bordered)
                        Button("Responses API") { addAPI() }.buttonStyle(.bordered)
                    }.disabled(!canEdit)
                }
            } else {
                HStack(spacing: 8) {
                    Label(routingSummary, systemImage: "arrow.triangle.branch").lineLimit(1)
                    Spacer()
                    Text("已启用 \(model.sources.filter(\.enabled).count) / \(model.sources.count)")
                        .monospacedDigit().fixedSize()
                }.font(AppTypography.caption).foregroundStyle(.secondary)
                sourceList
            }
        }.settingsPage()
            .task(id: "\(model.running):\(model.accountRefreshGeneration)") { await model.refreshAccountInfo() }
            .task(id: "\(model.running):\(model.settingsWindowActive)") {
                guard model.running, model.settingsWindowActive else { return }
                while !Task.isCancelled {
                    model.refreshVisibleModels()
                    do { try await Task.sleep(for: .seconds(300)) } catch { break }
                }
            }
            .onChange(of: model.loginSource) { _, sourceID in
                guard let sourceID else { return }
                expandedSources.insert(sourceID)
                let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
                if !query.isEmpty, model.sources.first(where: { $0.id == sourceID })?.name.localizedCaseInsensitiveContains(query) != true {
                    search = ""
                }
            }
            .sheet(item: $editing) { item in SourceEditor(source: item.source, isNew: item.isNew).environmentObject(model) }
            .sheet(isPresented: $showingRouting) {
                VStack(spacing: 0) {
                    RoutingView().environmentObject(model)
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
                Button("删除", role: .destructive) { if let source = deleting { model.removeSource(source.id) }; deleting = nil }
            } message: { Text("将移除此来源及其登录信息，用量记录保留。") }
    }

    private var canEdit: Bool { model.running && !model.busy && !model.metadataLoadFailed }
    private var routingSummary: String {
        if model.routingMode == "fixed" { return "固定来源 · \(model.fixedName)" }
        return model.settings.loadBalanceEnabled ? "自动切换 · 负载均衡" : "自动切换 · 按优先级分配"
    }
    private func uniqueSourceName(_ base: String) -> String {
        let names = Set(model.sources.map(\.name))
        if !names.contains(base) { return base }
        var suffix = 2
        while names.contains("\(base) \(suffix)") { suffix += 1 }
        return "\(base) \(suffix)"
    }
    private func addOfficial() {
        var source = Source.newOfficial()
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
        let visible = model.sources.enumerated().filter { query.isEmpty || $0.element.name.localizedCaseInsensitiveContains(query) }
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
        return HStack(alignment: .top, spacing: 12) {
            Text("\(index + 1)").font(AppTypography.caption.weight(.semibold)).monospacedDigit()
                .foregroundStyle(.secondary).frame(width: 26, height: 26)
                .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 5))
                .help("来源池优先级").accessibilityLabel("优先级 \(index + 1)")
            VStack(alignment: .leading, spacing: 10) {
                sourceHeader(source, index: index)
                HStack(alignment: .center, spacing: 12) {
                    if source.official {
                        quotaSummary(source)
                        OfficialResetControl(source: source)
                    } else if source.supportsBalanceQuery {
                        APIBalanceView(sourceID: source.id)
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
                if let reason = model.state(source.id)["last_error"].string {
                    InfoNote(text: reason, symbol: "exclamationmark.triangle", color: .orange)
                }
                if let error = model.modelErrors[source.id] {
                    InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange)
                }
                if expanded {
                    Divider()
                    if source.official { OfficialAccountView(source: source) }
                    sourceDetails(source)
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }.padding(16)
    }

    private func sourceHeader(_ source: Source, index: Int) -> some View {
        let needsLogin = source.official && source.account_id == nil
        let takingOver = model.status["manual_route"]["source_id"].string == source.id
        return HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(source.name).font(AppTypography.sectionTitle).lineLimit(1).help(source.name)
                Text(source.kindLabel).font(AppTypography.small).foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if takingOver {
                StatusPill(text: "正在抢占", color: .accentColor).fixedSize()
            }
            StatusPill(text: source.enabled ? (needsLogin ? "需要登录" : healthLabel(model.state(source.id)["health"].string)) : "已停用",
                       color: sourceColor(source), active: model.sourceHasActivity(source.id)).fixedSize()
            if needsLogin {
                Button("登录…") { model.login(source) }.buttonStyle(.borderedProminent)
                    .disabled(!canEdit || model.loginFlow != nil)
            }
            Button("编辑…") { editing = EditingSource(source: source, isNew: false) }
                .accessibilityLabel("编辑来源 \(source.name)")
            Menu {
                Button(source.enabled ? "停用来源" : "启用来源") { model.toggle(source) }.disabled(!canEdit)
                Divider()
                SourceRoutingActions(source: source)
                Divider()
                Button("提高优先级") { model.move(IndexSet(integer: index), to: index - 1) }
                    .disabled(index == 0 || !canEdit)
                Button("降低优先级") { model.move(IndexSet(integer: index), to: index + 2) }
                    .disabled(index == model.sources.count - 1 || !canEdit)
                Divider()
                Button("删除来源…", role: .destructive) { deleting = source }.disabled(!canEdit)
            } label: {
                Image(systemName: "ellipsis.circle").font(.system(size: 14))
            }.menuStyle(.borderlessButton).fixedSize()
                .accessibilityLabel("\(source.name) 的更多操作").help("更多操作")
        }.controlSize(.small)
    }

    private func quotaSummary(_ source: Source) -> some View {
        let quota = model.state(source.id)["quota"]
        let overview = QuotaOverview(quota)
        return HStack(alignment: .top, spacing: 14) {
            ForEach(overview.mainWindows) { window in
                VStack(alignment: .leading, spacing: 3) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(window.title).font(AppTypography.small).foregroundStyle(.secondary)
                        Text(window.remainingLabel).font(AccountTypography.value).monospacedDigit()
                            .foregroundStyle(AccountLevel.quota(window.remaining).color)
                            .accessibilityLabel("\(window.title)剩余 \(window.remainingLabel)")
                        if !overview.hasShortWindow { QuotaReferenceLabel(window: window) }
                    }
                    if overview.hasShortWindow { QuotaReferenceLabel(window: window) }
                    Text(window.resetLabel).font(AppTypography.small).foregroundStyle(.secondary)
                        .lineLimit(1).help(window.resetLabel)
                }
            }
            if model.refreshingQuotas.contains(source.id) {
                ProgressView().controlSize(.mini).help("正在更新额度")
            } else if quota["stale"].bool == true {
                Image(systemName: "clock").font(AppTypography.small).foregroundStyle(.orange)
                    .help("更新失败，显示上次获取的额度")
            }
        }
    }

    private func sourceDetails(_ source: Source) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            LabeledContent("服务地址") {
                Text(source.base_url).font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            LabeledContent("网络策略") {
                Text(source.policyLabel + (source.proxy.url.map { " · \($0)" } ?? ""))
                    .textSelection(.enabled)
            }
            if model.state(source.id)["admission"]["capacity_active_conversations"].number != nil || source.max_active_conversations != nil {
                LabeledContent("对话容量") {
                    SourceCapacityLabel(source: source, admission: model.state(source.id)["admission"], showsRoutingNote: true)
                }
            }
            HStack(spacing: 8) {
                Text(source.models.isEmpty ? (source.discoversModels ? "尚未获取模型" : "未限定模型") : "\(source.models.count) 个模型")
                Spacer()
                if source.discoversModels {
                    if model.refreshingModels.contains(source.id) { ProgressView().controlSize(.mini) }
                    Button("刷新模型") { Task { await model.refreshModels(source.id) } }
                        .buttonStyle(.borderless)
                        .disabled(model.busy || !model.running || model.refreshingModels.contains(source.id) || (source.official && source.account_id == nil))
                }
            }
            if !source.models.isEmpty {
                Text(source.models.joined(separator: "、")).font(AppTypography.small)
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
        }.font(AppTypography.caption).foregroundStyle(.secondary).padding(.top, 2)
    }

    private func sourceColor(_ source: Source) -> Color {
        guard source.enabled else { return .secondary }
        if source.official && source.account_id == nil { return .orange }
        switch model.state(source.id)["health"].string { case "ready": return .green; case nil: return .secondary; default: return .orange }
    }
}
