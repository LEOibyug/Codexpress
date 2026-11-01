import SwiftUI

struct GeneralSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var port = 17863

    private var preferencesChanged: Bool { port != model.settings.port }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("设置") {
                    HStack(spacing: 10) {
                        if preferencesChanged { Text("未保存").font(.system(size: 12)).foregroundStyle(.secondary) }
                        Button("保存设置") { model.savePreferences(port: port, codexHome: model.settings.codexHome) }
                            .disabled(model.busy || model.metadataLoadFailed || !preferencesChanged)
                    }
                }
                gatewayCard
                CredentialStorageView()
                HistoryCacheView()
            }.settingsPage()
        }.onAppear { port = model.settings.port }
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
                        if !model.running { Button("重试") { model.retry() } }
                    }
                }
                Divider().overlay(AppPalette.border)
                SettingRow(title: "登录时启动") {
                    Toggle("登录时启动 Codexpress", isOn: Binding(get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                        .toggleStyle(.switch).labelsHidden()
                }
                LabeledContent("网关地址") {
                    Text(model.baseURL).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                }.foregroundStyle(.secondary)
            }
        }
    }

}
