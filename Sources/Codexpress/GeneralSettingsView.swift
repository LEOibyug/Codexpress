import SwiftUI

struct GeneralSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var port = 17863
    @State private var officialQuotaMinutes = "30"

    private var preferencesChanged: Bool { port != model.settings.port }
    private var officialQuotaIntervalChanged: Bool { officialQuotaMinutes != String(model.settings.officialQuotaRefreshInterval) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("设置")
                gatewayCard
                CredentialStorageView()
                HistoryCacheView()
            }.settingsPage()
        }.onAppear {
            port = model.settings.port
            officialQuotaMinutes = String(model.settings.officialQuotaRefreshInterval)
        }
            .onChange(of: model.settings.port) { _, value in port = value }
            .onChange(of: model.settings.officialQuotaRefreshInterval) { _, value in officialQuotaMinutes = String(value) }
    }

    private var gatewayCard: some View {
        SettingsCard("应用与网关", symbol: "network", accessory: {
            StatusPill(text: model.running ? "运行中" : "未连接", color: model.running ? .green : .orange)
        }) {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: "监听端口") {
                    HStack(spacing: 8) {
                        TextField("端口", value: $port, format: .number.grouping(.never))
                            .font(.system(size: 13, design: .monospaced)).multilineTextAlignment(.trailing).settingsField().frame(width: 120)
                            .onSubmit { if preferencesChanged { model.savePreferences(port: port, codexHome: model.settings.codexHome) } }
                        if !model.running { Button("重试") { model.retry() } }
                        Button("应用端口") { model.savePreferences(port: port, codexHome: model.settings.codexHome) }
                            .disabled(model.busy || model.metadataLoadFailed || !preferencesChanged)
                    }
                }
                Divider().overlay(AppPalette.border)
                SettingRow(title: "官方额度后台查询间隔（分钟）") {
                    HStack(spacing: 8) {
                        TextField("30", text: $officialQuotaMinutes)
                            .font(.system(size: 13, design: .monospaced)).multilineTextAlignment(.trailing).settingsField().frame(width: 120)
                            .accessibilityLabel("官方额度后台查询间隔（分钟）")
                            .onSubmit { if officialQuotaIntervalChanged { model.saveOfficialQuotaRefresh(minutes: officialQuotaMinutes) } }
                        Button("保存间隔") { model.saveOfficialQuotaRefresh(minutes: officialQuotaMinutes) }
                            .disabled(model.busy || model.metadataLoadFailed || !officialQuotaIntervalChanged)
                    }
                }
                Text("正整数分钟，保存后立即生效；仅调整官方账户后台查询，API 余额仍每 30 分钟查询。前台和手动刷新不受此间隔限制，失败重试仍遵循退避。")
                    .font(.caption).foregroundStyle(.secondary)
                Divider().overlay(AppPalette.border)
                SettingRow(title: "登录时启动") {
                    Toggle("登录时启动 Codexpress", isOn: Binding(get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                        .toggleStyle(.switch).labelsHidden()
                }
                LabeledContent("网关地址") {
                    Text(model.baseURL).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                }.foregroundStyle(.secondary)
                if let version = model.gatewayVersion {
                    SettingRow(title: "网关版本") {
                        HStack(spacing: 10) {
                            Text(version).monospacedDigit().foregroundStyle(.secondary)
                            if model.gatewayUpdateWaiting {
                                Button("取消等待") { model.cancelGatewayUpdate() }
                            } else {
                                Button("空闲后更新网关") { model.updateGateway() }
                                    .disabled(model.busy || !model.running)
                            }
                        }
                    }
                    if model.gatewayUpdateWaiting {
                        InfoNote(text: "正在等待请求和后台操作结束。", symbol: "clock")
                    }
                }
                if model.portNeedsRestart { InfoNote(text: "端口已保存，重启 Codexpress 后生效。", symbol: "arrow.clockwise", color: .orange) }
            }
        }
    }

}
