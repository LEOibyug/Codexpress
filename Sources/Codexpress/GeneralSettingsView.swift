import SwiftUI

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
