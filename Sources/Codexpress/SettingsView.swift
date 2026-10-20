import SwiftUI

private enum SettingsPage: String, CaseIterable, Identifiable {
    case sources = "来源", routing = "路由", usage = "用量", general = "设置"
    var id: String { rawValue }
    var symbol: String { switch self { case .sources: return "server.rack"; case .routing: return "arrow.triangle.branch"; case .usage: return "chart.bar.xaxis"; case .general: return "gearshape" } }
}
struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var page: SettingsPage? = .sources
    var body: some View {
        NavigationSplitView {
            List(SettingsPage.allCases, selection: $page) { item in Label(item.rawValue, systemImage: item.symbol).tag(item) }
                .navigationSplitViewColumnWidth(155)
                .safeAreaInset(edge: .bottom) {
                    HStack { Circle().fill(model.running ? .green : .orange).frame(width: 7, height: 7); Text(model.running ? "网关运行中" : "网关未连接").font(.caption); Spacer() }.padding(14)
                }
        } detail: {
            VStack(spacing: 0) {
                if model.metadataLoadFailed {
                    Label("来源文件无法读取，已进入只读保护。请从备份修复 sources.json 后重启；原文件未改动。", systemImage: "lock.shield").font(.callout).padding(12).frame(maxWidth: .infinity, alignment: .leading).background(.orange.opacity(0.12))
                }
                if model.pendingSync {
                    Label("配置已保存，但尚未成功应用到网关。当前路由以网关最后状态为准，请重试启动。", systemImage: "exclamationmark.arrow.triangle.2.circlepath").font(.callout).padding(12).frame(maxWidth: .infinity, alignment: .leading).background(.orange.opacity(0.12))
                }
                if let error = model.error {
                    HStack(alignment: .top) { Image(systemName: "exclamationmark.triangle.fill"); Text(error).textSelection(.enabled); Spacer(); Button { model.error = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain) }
                        .font(.callout).padding(12).background(.orange.opacity(0.12))
                }
                if let notice = model.notice {
                    HStack { Text(notice); Spacer(); Button { model.notice = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain) }.font(.callout).padding(10).background(.blue.opacity(0.08))
                }
                Group {
                    switch page ?? .sources { case .sources: SourcesView(); case .routing: RoutingView(); case .usage: UsageView(); case .general: GeneralSettingsView() }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }.frame(minWidth: 800, minHeight: 570)
    }
}
private struct EditingSource: Identifiable { let source: Source; let isNew: Bool; var id: String { source.id } }
struct SourcesView: View {
    @EnvironmentObject var model: AppModel
    @State private var editing: EditingSource?
    @State private var deleting: Source?
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                VStack(alignment: .leading, spacing: 4) { Text("来源").font(.largeTitle.bold()); Text("官方账号与 API 共享同一个优先级池。").foregroundStyle(.secondary) }
                Spacer()
                Menu {
                    Button("官方账号") { editing = EditingSource(source: .newOfficial(), isNew: true) }
                    Button("Responses API") { editing = EditingSource(source: Source(), isNew: true) }
                } label: { Label("添加来源", systemImage: "plus") }.fixedSize(horizontal: true, vertical: true).disabled(!model.running || model.busy || model.metadataLoadFailed)
            }
            if model.sources.isEmpty {
                ContentUnavailableView { Label("还没有来源", systemImage: "server.rack") } description: { Text("添加一个官方账号或 API 来源。真实凭据只保存在钥匙串中。") } actions: {
                    Button("添加 API 来源") { editing = EditingSource(source: Source(), isNew: true) }.disabled(!model.running)
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 12) {
                        ForEach(model.sources) { source in
                            VStack(alignment: .leading, spacing: 12) {
                                HStack {
                                    Image(systemName: source.official ? "person.crop.circle" : "key.horizontal").font(.title2).foregroundStyle(.secondary)
                                    VStack(alignment: .leading, spacing: 3) { Text(source.name).font(.headline); Text(source.official ? "官方 OAuth" : "Responses API").font(.caption).foregroundStyle(.secondary) }
                                    Spacer()
                                    Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用").font(.caption).foregroundStyle(.secondary)
                                    Toggle("启用", isOn: Binding(get: { source.enabled }, set: { _ in model.toggle(source) })).labelsHidden().disabled(model.busy)
                                    Menu {
                                        Button("编辑…") { editing = EditingSource(source: source, isNew: false) }
                                        Button("删除…", role: .destructive) { deleting = source }
                                    } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).frame(width: 24)
                                }
                                Text(source.base_url).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                                Label(source.policyLabel + (source.proxy.url.map { " · \($0)" } ?? ""), systemImage: source.proxy.mode == "direct" ? "network" : "shield.lefthalf.filled").font(.caption)
                                if let reason = model.state(source.id)["last_error"].string { Text(reason).font(.caption).foregroundStyle(.orange) }
                                if source.official {
                                    Divider()
                                    OfficialAccountView(source: source)
                                }
                            }.padding(16).background(.background, in: RoundedRectangle(cornerRadius: 10)).overlay(RoundedRectangle(cornerRadius: 10).stroke(.quaternary))
                        }
                    }
                }
            }
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .sheet(item: $editing) { item in SourceEditor(source: item.source, isNew: item.isNew).environmentObject(model) }
            .alert("删除来源？", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })) {
                Button("取消", role: .cancel) { deleting = nil }
                Button("删除", role: .destructive) { if let source = deleting { model.removeSource(source.id) }; deleting = nil }
            } message: { Text("将删除此来源的元数据和钥匙串凭据。已有用量记录会保留。") }
    }
}
struct RoutingView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("路由").font(.largeTitle.bold())
            Picker("模式", selection: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: model.settings.fixed_source_id ?? model.sources.first?.id) })) {
                Text("自动优先级池").tag("auto"); Text("固定来源").tag("fixed")
            }.pickerStyle(.segmented).disabled(model.busy || model.sources.isEmpty)
            if model.routingMode == "fixed" {
                Picker("固定来源", selection: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) })) { ForEach(model.sources.filter(\.enabled)) { Text($0.name).tag($0.id) } }.disabled(model.busy)
            }
            Text(model.routingMode == "auto" ? "从上到下选择可用且支持请求模型的来源。拖动调整优先级。" : "请求仅交给所选来源；发生错误时不会改用其他账号或供应商。").font(.callout).foregroundStyle(.secondary)
            if model.sources.isEmpty { ContentUnavailableView("暂无可排序的来源", systemImage: "arrow.triangle.branch", description: Text("请先在“来源”中添加账号或 API。")).frame(maxWidth: .infinity, maxHeight: .infinity) }
            else {
                List {
                    ForEach(Array(model.sources.enumerated()), id: \.element.id) { index, source in
                        HStack {
                            Text("\(index + 1)").monospacedDigit().foregroundStyle(.secondary).frame(width: 22)
                            VStack(alignment: .leading) { Text(source.name); Text(source.policyLabel).font(.caption).foregroundStyle(.secondary) }
                            Spacer(); Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用").font(.caption).foregroundStyle(.secondary)
                            Button { model.move(IndexSet(integer: index), to: index - 1) } label: { Image(systemName: "chevron.up") }.disabled(index == 0 || model.busy).help("提高优先级")
                            Button { model.move(IndexSet(integer: index), to: index + 2) } label: { Image(systemName: "chevron.down") }.disabled(index == model.sources.count - 1 || model.busy).help("降低优先级")
                        }.padding(.vertical, 7)
                    }.onMove { model.move($0, to: $1) }
                }.listStyle(.inset).disabled(model.busy)
            }
            Label("依赖 previous_response_id 的请求优先保留原来源；已开始的生成不会拼接其他源的响应。", systemImage: "info.circle").font(.caption).foregroundStyle(.secondary)
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}
struct GeneralSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var port = 17863
    @State private var home = ""
    @State private var confirming: String?
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Text("设置").font(.largeTitle.bold())
                GroupBox("本地网关") {
                    VStack(alignment: .leading, spacing: 12) {
                        HStack { Text("监听端口"); TextField("端口", value: $port, format: .number.grouping(.never)).frame(width: 100); Spacer(); Text(model.running ? "运行中" : "未连接").foregroundStyle(.secondary); if !model.running { Button("重试") { model.retry() } } }
                        Text(model.baseURL).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                        Toggle("登录时启动 Codexpress", isOn: Binding(get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                        Text("更改端口需重启应用，再重新预览并更新 Codex 连接配置。").font(.caption).foregroundStyle(.secondary)
                    }.padding(8)
                }
                ModelCompactionView()
                GroupBox("Codex 连接") {
                    VStack(alignment: .leading, spacing: 12) {
                        TextField("Codex 配置目录（绝对路径）", text: $home)
                        Button("保存设置") { model.savePreferences(port: port, codexHome: home) }.disabled(model.busy || model.metadataLoadFailed)
                        Text("应用不会自动接管连接。预览将读取所选目录的配置；确认后只更新受管字段，不修改原生登录、历史或工具设置。").font(.callout).foregroundStyle(.secondary)
                        Label("已有会话可能仍使用原连接，需要重新载入。根配置变化不代表所有历史会话已切换。", systemImage: "clock.arrow.circlepath").font(.callout).foregroundStyle(.orange)
                        if model.portNeedsRestart { Label("端口变更待重启；重新启动后才能预览和接管连接。", systemImage: "arrow.clockwise").font(.callout).foregroundStyle(.orange) }
                        Button("预览连接配置…") { model.configPreview() }.disabled(model.busy || !model.running || home != model.settings.codexHome || model.portNeedsRestart || model.metadataLoadFailed)
                    }.padding(8)
                }
                if model.preview != .null {
                    Text(model.preview["warning"].string ?? "请在接管后验证新会话和恢复会话的真实连接目标。").font(.callout).foregroundStyle(.orange)
                    GroupBox("当前配置（敏感字段已隐藏）") { Text(model.preview["before"].string ?? "").font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding(8) }
                    GroupBox("接管后的配置") { Text(model.preview["after"].string ?? "").font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding(8) }
                    if !model.preview["model_limits"].array.isEmpty {
                        GroupBox("将应用的模型压缩规则") {
                            VStack(alignment: .leading, spacing: 8) {
                                ForEach(Array(model.preview["model_limits"].array.enumerated()), id: \.offset) { _, rule in
                                    Text("\(rule["model"].string ?? "")：达到 \(countLabel(rule["auto_compact_token_limit"])) Token 时压缩；窗口 \(countLabel(rule["context_window"])) Token").font(.callout)
                                }
                            }.frame(maxWidth: .infinity, alignment: .leading).padding(8)
                        }
                    }
                    HStack { Button(model.preview["attached"].bool == true ? "应用连接与模型规则…" : "接管所选 Codex 连接…") { confirming = "config.attach" }.buttonStyle(.borderedProminent).disabled(model.portNeedsRestart || model.metadataLoadFailed); Button("恢复接管前连接…") { confirming = "config.restore" }.disabled(model.preview["attached"].bool != true) }.disabled(model.busy)
                }
            }.padding(24)
        }.onAppear { port = model.settings.port; home = model.settings.codexHome }
            .alert(confirming == "config.restore" ? "恢复原连接和模型设置？" : "应用 Codex 连接和模型规则？", isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } })) {
                Button("取消", role: .cancel) { confirming = nil }
                Button("确认") { if let op = confirming { model.configAction(op) }; confirming = nil }
            } message: { Text("操作目录：\(model.settings.codexHome)\n只更改本应用管理的字段。已有会话的连接需要另行验证。") }
    }
}
