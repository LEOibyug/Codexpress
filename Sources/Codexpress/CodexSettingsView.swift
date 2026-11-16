import SwiftUI

struct CodexSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var home = ""
    @State private var loadedHome = ""
    @State private var confirming: String?
    @State private var selectedBackup = ""

    private var preferencesChanged: Bool { (home.trimmingCharacters(in: .whitespacesAndNewlines) as NSString).expandingTildeInPath != model.settings.codexHome }
    private var canConnect: Bool { !model.busy && model.running && !preferencesChanged && !model.portNeedsRestart && !model.metadataLoadFailed }

    var body: some View {
        ScrollViewReader { scroll in
            ScrollView {
                VStack(alignment: .leading, spacing: 24) {
                    PageHeading("Codex")
                    connectionCard
                    modelSettingsCard
                    if model.preview != .null { previewCard.id("connection-preview") }
                    backupCard
                }.settingsPage()
            }.onChange(of: model.preview != .null) { _, visible in
                if visible { withAnimation(.easeInOut(duration: 0.25)) { scroll.scrollTo("connection-preview", anchor: .center) } }
            }.onAppear {
                if loadedHome.isEmpty { home = model.settings.codexHome; loadedHome = home }
            }
                .onChange(of: model.settings.codexHome) { _, value in
                    if home == loadedHome { home = value }
                    loadedHome = value
                }
                .task(id: "\(model.running):\(model.settings.codexHome)") { selectedBackup = ""; await model.refreshConfigurationBackups() }
                .alert(confirming == "backup.restore" ? "还原所选备份？" : "恢复原连接？", isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } })) {
                    Button("取消", role: .cancel) { confirming = nil }
                    Button("确认") {
                        if confirming == "backup.restore" { model.restoreConfigurationBackup(backupID) }
                        else if let op = confirming { model.configAction(op) }
                        confirming = nil
                    }
                } message: {
                    Text(confirming == "backup.restore" ? "将还原此备份的 config.toml、配置档案和 auth.json 文件认证，并重启 Codex。当前状态会先备份；钥匙串登录需在 Codex 中确认。" : "配置目录：\(model.settings.codexHome)\n旧配置与文件认证将先备份，再更新连接并重启 Codex。")
                }
        }
    }

    private var connectionActions: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                Button { model.configPreview() } label: {
                    Label("查看高级预览", systemImage: "doc.text.magnifyingglass")
                }.disabled(!canConnect)
                if model.connectionMode == "gateway" {
                    Button("恢复原连接…") { confirming = "config.restore" }
                        .disabled(model.busy || !model.running || preferencesChanged || model.metadataLoadFailed)
                }
                Spacer(minLength: 0)
                Button(model.codexConnectionPhase ?? (model.connectionMode == "gateway" ? "更新连接设置" : "接入来源池")) {
                    model.switchCodexConnection(.gateway)
                }.buttonStyle(.borderedProminent).disabled(!canConnect)
            }
            InfoNote(text: preferencesChanged ? "请先提交配置目录。" : "接入时自动备份配置并重启 Codex，请先结束 Codex 中的操作。")
            if model.portNeedsRestart {
                InfoNote(text: "请重启 Codexpress 以启用新端口。", symbol: "arrow.clockwise", color: .orange)
            }
        }
    }

    private var modelSettingsCard: some View {
        SettingsCard("模型设置同步", symbol: "arrow.triangle.2.circlepath", accessory: {
            if model.localModelSettingsNeedsSync {
                Button("同步模型设置") { model.retryLocalModelSettingsSync() }
                    .disabled(model.busy || !model.running || preferencesChanged || model.metadataLoadFailed)
            }
        }) {
            InfoNote(text: model.localModelSettingsStatus, symbol: model.localModelSettingsNeedsSync ? "exclamationmark.triangle" : "info.circle", color: model.localModelSettingsNeedsSync ? .orange : .secondary)
        }
    }

    private var connectionCard: some View {
        SettingsCard("连接方式与配置", symbol: "link") {
            VStack(alignment: .leading, spacing: 16) {
                HStack(spacing: 12) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(model.connectionMode == "official" ? "官方连接 · 系统代理" : model.connectionMode == "gateway" ? "来源池" : "连接方式")
                            .font(.system(size: 13, weight: .medium))
                    }
                    Spacer()
                    DirectOfficialLoginButton().disabled(preferencesChanged)
                }
                Divider().overlay(AppPalette.border)
                VStack(alignment: .leading, spacing: 8) {
                    Text("配置目录").font(.system(size: 13, weight: .medium))
                    HStack(spacing: 10) {
                        TextField("Codex 配置目录（绝对路径）", text: $home)
                            .font(.system(size: 13, design: .monospaced)).settingsField()
                            .onSubmit { saveDirectory() }
                        if preferencesChanged {
                            Button("保存目录") { saveDirectory() }
                                .disabled(model.busy || model.metadataLoadFailed)
                        }
                    }
                }
                Divider().overlay(AppPalette.border)
                connectionActions
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
            VStack(alignment: .leading, spacing: 16) {
                if model.configurationBackups.isEmpty {
                    Text("暂无备份").font(.system(size: 12)).foregroundStyle(.secondary)
                } else {
                    Picker("备份时间", selection: Binding(get: { backupID }, set: { selectedBackup = $0 })) {
                        ForEach(Array(model.configurationBackups.enumerated()), id: \.offset) { _, item in Text(backupLabel(item)).tag(item["id"].string ?? "") }
                    }
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
                HStack(alignment: .top, spacing: 20) {
                    configurationBlock("当前配置", value: model.preview["before"].string ?? "")
                    configurationBlock("应用后", value: model.preview["after"].string ?? "")
                }
                ForEach(Array(model.preview["model_limits"].array.enumerated()), id: \.offset) { _, rule in
                    HStack {
                        Text(rule["model"].string ?? "").font(.system(size: 13, weight: .medium, design: .monospaced))
                        Spacer()
                        Text("压缩 \(countLabel(rule["auto_compact_token_limit"])) / 窗口 \(countLabel(rule["context_window"])) Token")
                            .font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                }
                if !model.preview["compatibility_provider_ids"].array.isEmpty {
                    Text("接入已有供应商连接：" + model.preview["compatibility_provider_ids"].array.compactMap(\.string).joined(separator: "、"))
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                }
                if !model.preview["legacy_providers"].array.isEmpty || model.preview["origin_error"].string != nil { LegacyProviderOriginsView() }
                HStack {
                    Spacer()
                    Button("收起预览") { model.preview = .null }
                }
            }
        }
    }

    private func configurationBlock(_ title: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.system(size: 13, weight: .medium)).foregroundStyle(.secondary)
            ScrollView([.vertical, .horizontal]) {
                Text(value.isEmpty ? "（空配置）" : value).font(.system(size: 12, design: .monospaced))
                    .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .topLeading).padding(12)
            }.frame(height: 220).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 6))
        }.frame(maxWidth: .infinity)
    }

    private func saveDirectory() {
        guard preferencesChanged, !model.busy, !model.metadataLoadFailed else { return }
        home = home.trimmingCharacters(in: .whitespacesAndNewlines)
        model.savePreferences(port: model.settings.port, codexHome: home)
    }
}
