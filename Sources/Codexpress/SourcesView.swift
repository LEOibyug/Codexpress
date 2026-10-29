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
            .task(id: "\(model.running):\(model.settingsWindowActive)") {
                guard model.running, model.settingsWindowActive else { return }
                while !Task.isCancelled {
                    model.refreshVisibleModels()
                    do { try await Task.sleep(for: .seconds(300)) } catch { break }
                }
            }
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
                StatusPill(text: source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用", color: sourceColor(source), active: model.sourceHasActivity(source.id))
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
                HStack(spacing: 8) {
                    Label(source.policyLabel + (source.proxy.url.map { " · \($0)" } ?? ""), systemImage: source.proxy.mode == "direct" ? "network" : "shield.lefthalf.filled")
                        .lineLimit(1)
                    Spacer(minLength: 0)
                    SourceCapacityLabel(source: source, admission: model.state(source.id)["admission"]).fixedSize()
                }.font(.system(size: 11)).foregroundStyle(.secondary)
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
            HStack { Spacer(); SourceRoutingActions(source: source) }
            if source.official { Divider(); OfficialAccountView(source: source) }
            else { APIBalanceView(sourceID: source.id) }
        }.padding(18).cardSurface()
    }

    private func sourceColor(_ source: Source) -> Color {
        guard source.enabled else { return .secondary }
        switch model.state(source.id)["health"].string { case "ready": return .green; case nil: return .secondary; default: return .orange }
    }
}
