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
    @State private var expandedSources = Set<String>()
    @State private var showingRouting = false

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            PageHeading("来源池") {
                HStack(spacing: 10) {
                    Button { showingRouting = true } label: {
                        Label("分配策略", systemImage: "slider.horizontal.3")
                    }
                    Menu {
                        Button("官方账号") { addOfficial() }
                        Button("Responses API") { addAPI() }
                    } label: { Label("添加来源", systemImage: "plus") }
                        .menuStyle(.borderedButton).buttonStyle(.borderedProminent)
                        .fixedSize().disabled(!canEdit)
                }.controlSize(.regular)
            }
            if model.sources.isEmpty {
                EmptyState("暂无来源", subtitle: "添加 ChatGPT 账号或 API 服务。", symbol: "server.rack") {
                    HStack(spacing: 10) {
                        Button("添加官方账号") { addOfficial() }.buttonStyle(.borderedProminent)
                        Button("添加 API") { addAPI() }.buttonStyle(.bordered)
                    }.controlSize(.large).disabled(!canEdit)
                }
            } else {
                HStack(spacing: 8) {
                    Label(routingSummary, systemImage: "arrow.triangle.branch")
                    Spacer()
                    Text("\(model.sources.filter(\.enabled).count) 个已启用 · 共 \(model.sources.count) 个来源")
                }.font(AppTypography.caption).foregroundStyle(.secondary)
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(Array(model.sources.enumerated()), id: \.element.id) { index, source in
                            if index > 0 { Divider().padding(.leading, 76) }
                            sourceRow(source, index: index)
                        }
                    }.cardSurface().padding(.bottom, 2)
                }
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
                if let sourceID { expandedSources.insert(sourceID) }
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
                }.frame(width: 800, height: 700)
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
    private func addOfficial() {
        var source = Source.newOfficial()
        let names = Set(model.sources.map(\.name))
        var suffix = 2
        while names.contains(source.name) {
            source.name = "官方账号 \(suffix)"
            suffix += 1
        }
        editing = EditingSource(source: source, isNew: true)
    }
    private func addAPI() { editing = EditingSource(source: Source(), isNew: true) }

    private func sourceRow(_ source: Source, index: Int) -> some View {
        let expanded = expandedSources.contains(source.id)
        return HStack(alignment: .top, spacing: 14) {
            priorityControls(source, index: index)
            VStack(alignment: .leading, spacing: 12) {
                sourceHeader(source)
                HStack(alignment: .center, spacing: 16) {
                    if source.official {
                        quotaSummary(source)
                        OfficialResetControl(source: source)
                    } else {
                        APIBalanceView(sourceID: source.id)
                    }
                    Spacer(minLength: 8)
                    Button {
                        if expanded { expandedSources.remove(source.id) }
                        else { expandedSources.insert(source.id) }
                    } label: {
                        Label(expanded ? "收起" : "详情", systemImage: expanded ? "chevron.up" : "chevron.down")
                    }.buttonStyle(.borderless).font(AppTypography.caption)
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

    private func priorityControls(_ source: Source, index: Int) -> some View {
        VStack(spacing: 9) {
            Text("\(index + 1)").font(.system(size: 15, weight: .medium, design: .rounded))
                .monospacedDigit().foregroundStyle(.secondary)
                .accessibilityLabel("优先级 \(index + 1)")
            HStack(spacing: 6) {
                Button { model.move(IndexSet(integer: index), to: index - 1) } label: {
                    Image(systemName: "chevron.up")
                }.disabled(index == 0 || model.busy)
                    .help("提高优先级").accessibilityLabel("提高 \(source.name) 的优先级")
                Button { model.move(IndexSet(integer: index), to: index + 2) } label: {
                    Image(systemName: "chevron.down")
                }.disabled(index == model.sources.count - 1 || model.busy)
                    .help("降低优先级").accessibilityLabel("降低 \(source.name) 的优先级")
            }.buttonStyle(.borderless).font(.system(size: 10, weight: .semibold))
        }.frame(width: 44).padding(.top, 3)
    }

    private func sourceHeader(_ source: Source) -> some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(source.name).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                Text(source.official ? "官方账号" : "Responses API").font(AppTypography.small).foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if source.official, source.account_id == nil {
                Button("登录…") { model.login(source) }.buttonStyle(.borderedProminent).controlSize(.small)
                    .disabled(model.busy || model.loginFlow != nil)
            }
            SourceRoutingActions(source: source, compact: true).fixedSize()
            StatusPill(text: source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用",
                       color: sourceColor(source), active: model.sourceHasActivity(source.id))
            Toggle("启用 \(source.name)", isOn: Binding(get: { source.enabled }, set: { _ in model.toggle(source) }))
                .toggleStyle(.switch).labelsHidden().controlSize(.small).disabled(model.busy)
            Menu {
                Button("编辑…") { editing = EditingSource(source: source, isNew: false) }
                Button("删除…", role: .destructive) { deleting = source }
            } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).frame(width: 20)
        }
    }

    private func quotaSummary(_ source: Source) -> some View {
        let quota = model.state(source.id)["quota"]
        return HStack(alignment: .top, spacing: 24) {
            ForEach(QuotaOverview(quota).mainWindows) { window in
                VStack(alignment: .leading, spacing: 3) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(window.title).font(AppTypography.small).foregroundStyle(.secondary)
                        Text(window.remainingLabel).font(AccountTypography.value).monospacedDigit()
                            .foregroundStyle(AccountLevel.quota(window.remaining).color)
                            .accessibilityLabel("\(window.title)剩余 \(window.remainingLabel)")
                    }
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
        switch model.state(source.id)["health"].string { case "ready": return .green; case nil: return .secondary; default: return .orange }
    }
}
