import SwiftUI

struct GeneralSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var port = "17863"
    @State private var officialQuotaMinutes = "30"

    private var preferencesChanged: Bool { port != String(model.settings.port) }
    private var validPort: Int? { Int(port).flatMap { (1024...65535).contains($0) ? $0 : nil } }
    private var validQuotaInterval: Bool { Int(officialQuotaMinutes.trimmingCharacters(in: .whitespacesAndNewlines)).map { $0 > 0 } ?? false }
    private var canSavePreferences: Bool { !model.busy && !model.metadataLoadFailed }
    private var officialQuotaIntervalChanged: Bool { officialQuotaMinutes != String(model.settings.officialQuotaRefreshInterval) }

    var body: some View {
        GeneralSettingsPage(subtitle: "管理应用启动、本机网关和后台查询。") {
            gatewayCard
            quotaCard
            CredentialStorageView()
            HistoryCacheView()
        }.onAppear {
            port = String(model.settings.port)
            officialQuotaMinutes = String(model.settings.officialQuotaRefreshInterval)
        }
            .onChange(of: model.settings.port) { _, value in port = String(value) }
            .onChange(of: model.settings.officialQuotaRefreshInterval) { _, value in officialQuotaMinutes = String(value) }
    }

    private var gatewayCard: some View {
        SettingsCard("本机网关", symbol: "network", accessory: {
            StatusPill(text: model.running ? "运行中" : "未连接", color: model.running ? .green : .orange, active: model.gatewayHasActivity)
        }) {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: "监听端口", subtitle: "保存后重启 Codexpress 生效。") {
                    HStack(spacing: 8) {
                        TextField("端口", text: $port)
                            .font(.system(size: 13, design: .monospaced)).multilineTextAlignment(.trailing).settingsField().frame(width: 100)
                            .onSubmit(savePort)
                        Button("保存端口", action: savePort)
                            .disabled(!canSavePreferences || !preferencesChanged || validPort == nil)
                    }
                }
                if preferencesChanged && validPort == nil {
                    InfoNote(text: "请输入 1024–65535 之间的整数。", symbol: "exclamationmark.triangle", color: .orange)
                }
                if model.portNeedsRestart {
                    InfoNote(text: "新端口已保存，重启前继续使用当前地址。", symbol: "arrow.clockwise", color: .orange)
                }
                if model.running {
                    LabeledContent("当前地址") {
                        Text(model.baseURL).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                    }.foregroundStyle(.secondary)
                } else {
                    SettingRow(title: "网关尚未运行", subtitle: "启动后才能处理本机请求。") {
                        Button("重试启动") { model.retry() }.buttonStyle(.borderedProminent).disabled(model.busy)
                    }
                }
                if model.gatewayUpdateWaiting {
                    Divider()
                    SettingRow(title: "等待网关空闲", subtitle: "请求和后台操作结束后开始更新。") {
                        Button("取消等待") { model.cancelGatewayUpdate() }
                    }
                } else if let update = model.installedGatewayUpdate {
                    Divider()
                    if update.requiresRestart {
                        InfoNote(text: "已检测到 Codexpress \(update.version)，重启应用后生效。", symbol: "arrow.clockwise", color: .orange)
                    } else {
                        SettingRow(title: "网关 \(update.version) 可用", subtitle: model.gatewayVersion.map { "当前版本 \($0)。可在网关空闲后手动切换。" } ?? "可在网关空闲后手动切换。") {
                            if model.running {
                                Button("空闲后更新网关") { model.updateGateway() }
                                    .buttonStyle(.borderedProminent).disabled(model.busy)
                            }
                        }
                    }
                }
            }
        }
    }

    private var quotaCard: some View {
        OfficialQuotaSettingsCard(minutes: $officialQuotaMinutes, subtitle: "保存后立即生效，不影响前台或手动刷新。",
                                  error: officialQuotaIntervalChanged && !validQuotaInterval ? "请输入大于 0 的整数分钟。" : nil,
                                  canSave: canSavePreferences && officialQuotaIntervalChanged && validQuotaInterval, save: saveQuotaInterval)
    }

    private func savePort() {
        guard canSavePreferences, preferencesChanged, let value = validPort else { return }
        model.savePreferences(port: value, codexHome: model.settings.codexHome)
    }

    private func saveQuotaInterval() {
        guard canSavePreferences, officialQuotaIntervalChanged, validQuotaInterval else { return }
        model.saveOfficialQuotaRefresh(minutes: officialQuotaMinutes)
    }
}

struct GeneralSettingsPage<Content: View>: View {
    @EnvironmentObject private var model: AppModel
    let subtitle: String
    @ViewBuilder var content: Content

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("设置", subtitle: subtitle)
                SettingsCard("Codexpress", symbol: "app.badge") {
                    SettingRow(title: "登录时启动") {
                        Toggle("登录时启动 Codexpress", isOn: Binding(get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                            .toggleStyle(.switch).labelsHidden()
                    }
                }
                content
            }.settingsPage()
        }
    }
}

struct OfficialQuotaSettingsCard: View {
    @Binding var minutes: String
    let subtitle: String
    let error: String?
    var detail = "API 余额仍每 30 分钟查询。查询失败时会延迟重试。"
    var canSave = false
    var save: (() -> Void)?

    var body: some View {
        SettingsCard("官方额度", symbol: "clock") {
            SettingRow(title: "后台查询间隔", subtitle: subtitle) {
                HStack(spacing: 8) {
                    TextField("分钟数", text: $minutes)
                        .font(.system(size: 13, design: .monospaced)).multilineTextAlignment(.trailing).settingsField().frame(width: 80)
                        .accessibilityLabel("官方额度后台查询间隔（分钟）")
                        .onSubmit { save?() }
                    Text("分钟").foregroundStyle(.secondary)
                    if let save { Button("保存间隔", action: save).disabled(!canSave) }
                }
            }
            if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            else {
                Text(detail).font(AppTypography.caption).foregroundStyle(.secondary)
            }
        }
    }
}
