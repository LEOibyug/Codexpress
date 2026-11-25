import SwiftUI

struct GatewayTargetSheet: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    @Environment(\.dismiss) private var dismiss
    @State private var remote = false
    @State private var url = ""
    @State private var token = ""

    private var useSaved: Bool { !management.endpoint.isEmpty && url == management.endpoint && token.isEmpty }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("管理网关").font(AppTypography.sectionTitle)
            Picker("管理目标", selection: $remote) {
                Text("本地网关").tag(false)
                Text("远端网关").tag(true)
            }.pickerStyle(.segmented).labelsHidden()
            if remote {
                VStack(alignment: .leading, spacing: 6) {
                    Text("网关根地址")
                    TextField("https://gateway.example.com", text: $url).settingsField().accessibilityLabel("网关根地址")
                }
                VStack(alignment: .leading, spacing: 6) {
                    Text("管理令牌")
                    SecureField(useSaved ? "使用已保存的令牌" : "输入管理令牌（不是设备 API Key）", text: $token)
                        .settingsField().accessibilityLabel("管理令牌")
                    Text("验证成功后保存到本机私有文件，不使用钥匙串。").font(AppTypography.caption).foregroundStyle(.secondary)
                }
                if let error = management.message { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            }
            Text("切换不会迁移来源或更改 Codex 接入。本机 OMP 默认跟随所选网关；首次选择远端会签发专用设备 Key。")
                .font(AppTypography.caption).foregroundStyle(.secondary)
            HStack {
                if management.busy { ProgressView().controlSize(.small) }
                Spacer()
                Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(management.targetSwitchBlocked)
                Button(remote ? "连接远端" : "使用本地网关") {
                    if !remote { if management.selectLocal() { dismiss() }; return }
                    Task {
                        let ok: Bool
                        if useSaved { ok = await management.selectRemote() }
                        else { ok = await management.connect(url: url, token: token) }
                        if ok { token = ""; dismiss() }
                    }
                }.keyboardShortcut(.defaultAction).disabled(management.targetSwitchBlocked || (remote && (url.isEmpty || (!useSaved && token.isEmpty))))
            }
            if !remote, let error = management.message { Text(error).font(AppTypography.caption).foregroundStyle(.secondary) }
        }.padding(24).frame(width: 460)
            .onAppear { remote = management.remoteSelected; url = management.endpoint }
    }
}

struct RemoteGatewayPoolView: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    private struct EditSession: Identifiable { let id = UUID(); let pool: RemotePoolSnapshot; let generation: UUID; let fullEditor: Bool }
    @State private var editing: EditSession?
    private struct AccountSession: Identifiable { let id = UUID(); let source: RemotePoolSource; let pool: RemotePoolSnapshot; let generation: UUID }
    @State private var account: AccountSession?
    @State private var showingOMP = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                PageHeading("来源池", subtitle: [management.endpoint, management.version.map { "网关 \($0)" }].compactMap { $0 }.joined(separator: " · ")) {
                    Button("本机 OMP…") { showingOMP = true }
                    Button("刷新") { Task { await management.refresh() } }.disabled(management.busy)
                }
                RemoteGatewayFreshness()
                if let error = management.message { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                if let error = management.ompMessage {
                    HStack {
                        InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange)
                        Button("重试 OMP 接入") { Task { await management.retryOMP() } }.disabled(management.busy)
                    }
                }
                if let pool = management.snapshot {
                    RemoteRuntimeSummary(data: management.runtimeData)
                    SettingsCard("远端来源", symbol: "server.rack", accessory: {
                        Button("编辑来源池") {
                            editing = EditSession(pool: pool, generation: management.targetGeneration, fullEditor: management.capabilities.contains("config.edit"))
                        }.disabled(management.busy)
                    }) {
                        if pool.sources.isEmpty { Text("暂无来源").foregroundStyle(.secondary) }
                        else {
                            LabeledContent("分配策略", value: pool.mode == "auto" ? "自动切换" : "固定来源")
                            ForEach(pool.sources) { source in
                                Divider()
                                HStack {
                                    Text(source.name)
                                    if pool.mode == "fixed", source.id == pool.fixedSourceID { Text("固定").font(AppTypography.caption).foregroundStyle(.tint) }
                                    Spacer()
                                    if management.capabilities.contains("config.edit"), management.capabilities.contains("models.list") || (source.kind == "official" && management.capabilities.contains("official.refresh")) {
                                        Button(source.kind == "official" ? "登录与额度…" : "模型发现…") { account = AccountSession(source: source, pool: pool, generation: management.targetGeneration) }
                                    }
                                    Text(source.enabled ? "已启用" : "已停用").foregroundStyle(.secondary)
                                }
                            }
                        }
                    }
                } else if management.busy { ProgressView("正在读取远端来源池…") }
                else { Text("远端来源池不可用。请刷新或切换管理网关。").foregroundStyle(.secondary) }
            }.settingsPage()
        }
        .sheet(item: $editing) { session in
            if session.fullEditor { RemoteSourceSettings(snapshot: session.pool, generation: session.generation) }
            else { RemotePoolEditor(snapshot: session.pool) }
        }
        .sheet(item: $account) { session in
            RemoteSourceAccountActions(source: session.source, snapshot: session.pool, generation: session.generation)
        }
        .onChange(of: management.targetGeneration) { _, _ in editing = nil; account = nil }
        .sheet(isPresented: $showingOMP) {
            VStack(alignment: .trailing, spacing: 0) {
                OMPSettingsView()
                Button("完成") { showingOMP = false }.keyboardShortcut(.cancelAction).padding()
            }.frame(width: 660, height: 530)
        }
        .background(WindowOpenRefresh(refresh: { Task { await management.refresh() } }))
    }
}

struct RemoteGatewayFreshness: View {
    @EnvironmentObject private var management: RemoteGatewayManagement

    var body: some View {
        if let date = management.fetchedAt {
            let stale = management.snapshot == nil || Date().timeIntervalSince(date) > 60
            Text("\(stale ? "数据已过期，上次成功刷新" : "上次刷新")：\(date.formatted(date: .abbreviated, time: .standard))")
                .font(AppTypography.caption).foregroundStyle(stale ? Color.orange : Color.secondary)
        }
    }
}

private struct RemotePoolEditor: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    @Environment(\.dismiss) private var dismiss
    @State private var draft: RemotePoolSnapshot
    @State private var adding = false
    @State private var name = ""
    @State private var baseURL = ""
    @State private var key = ""
    @State private var models = ""
    private let original: RemotePoolSnapshot

    init(snapshot: RemotePoolSnapshot) { original = snapshot; _draft = State(initialValue: snapshot) }

    private var modelIDs: [String] { models.split { $0 == "," || $0 == "，" || $0.isWhitespace }.map(String.init) }
    private var canSave: Bool {
        !management.busy && management.snapshot == original && (draft != original || adding)
            && draft.sources.allSatisfy { !$0.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
            && (draft.mode == "auto" || draft.sources.contains { $0.enabled && $0.id == draft.fixedSourceID })
            && (!adding || (!name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !baseURL.isEmpty && !key.isEmpty && !modelIDs.isEmpty))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("编辑远端来源池").font(AppTypography.sectionTitle)
            Text(management.endpoint).font(AppTypography.caption).foregroundStyle(.secondary)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    Picker("分配策略", selection: $draft.mode) { Text("自动切换").tag("auto"); Text("固定来源").tag("fixed") }
                    if draft.mode == "fixed" {
                        Picker("固定来源", selection: $draft.fixedSourceID) {
                            Text("请选择").tag(nil as String?)
                            ForEach(draft.sources.filter(\.enabled)) { Text($0.name).tag(Optional($0.id)) }
                        }
                    }
                    ForEach($draft.sources) { $source in
                        HStack {
                            Toggle("启用 \(source.name)", isOn: $source.enabled).labelsHidden()
                            TextField("来源名称", text: $source.name).settingsField()
                            Button { move(source.id, by: -1) } label: { Image(systemName: "arrow.up") }
                                .disabled(source.id == draft.sources.first?.id).accessibilityLabel("上移 \(source.name)")
                            Button { move(source.id, by: 1) } label: { Image(systemName: "arrow.down") }
                                .disabled(source.id == draft.sources.last?.id).accessibilityLabel("下移 \(source.name)")
                        }
                    }
                    Divider()
                    Toggle("添加 Responses API 来源", isOn: $adding)
                    if adding {
                        Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 9) {
                            GridRow { Text("名称"); TextField("来源名称", text: $name) }
                            GridRow { Text("上游地址"); TextField("https://api.example.com/v1", text: $baseURL) }
                            GridRow { Text("API Key"); SecureField("仅发送给当前远端网关", text: $key) }
                            GridRow { Text("模型"); TextField("多个模型用逗号分隔", text: $models) }
                        }.textFieldStyle(.roundedBorder)
                    }
                }.padding(2)
            }
            if management.snapshot != original {
                Text("当前快照不可用或已变更。请确认刷新结果后重新打开编辑器。")
                    .font(AppTypography.caption).foregroundStyle(.orange)
            }
            if let error = management.message { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            HStack {
                Spacer()
                Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(management.busy)
                Button("保存到远端") {
                    let additions = adding ? [RemotePoolAddition(id: UUID().uuidString, name: name, baseURL: baseURL, apiKey: key, models: modelIDs)] : []
                    var updated = draft
                    if updated.mode == "auto" { updated.fixedSourceID = nil }
                    Task { if await management.save(updated, additions: additions) { key = ""; dismiss() } }
                }.keyboardShortcut(.defaultAction).disabled(!canSave)
            }
        }.padding(24).frame(width: 560, height: 480)
    }

    private func move(_ id: String, by offset: Int) {
        guard let index = draft.sources.firstIndex(where: { $0.id == id }), draft.sources.indices.contains(index + offset) else { return }
        draft.sources.swapAt(index, index + offset)
        for i in draft.sources.indices { draft.sources[i].priority = i }
    }
}
