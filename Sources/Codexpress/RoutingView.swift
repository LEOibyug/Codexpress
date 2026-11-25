import SwiftUI

struct RoutingView: View {
    @EnvironmentObject var model: AppModel

    private var enabledSources: [Source] { model.sources.filter(\.enabled) }
    private var preferredFixedSourceID: String? {
        enabledSources.first { $0.id == model.settings.fixed_source_id }?.id ?? enabledSources.first?.id
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                PageHeading("分配策略", subtitle: "设置来源选择和失败重试。更改后自动应用，无需另行保存。")
                RoutingSettingsCards(enabledSources: enabledSources, sourceName: \.name,
                    mode: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: preferredFixedSourceID) }),
                    fixedSource: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) }),
                    loadBalance: Binding(get: { model.settings.loadBalanceEnabled }, set: { model.changeLoadBalance($0) }),
                    quotaThreshold: Binding(get: { model.settings.lowQuotaPercent }, set: { model.changeLowQuotaThreshold($0) }),
                    attempts: Binding(get: { model.settings.upstreamAttemptLimit }, set: { model.changeUpstreamRetry(attempts: $0, delaySeconds: model.settings.upstreamRetryDelay) }),
                    retryDelay: Binding(get: { model.settings.upstreamRetryDelay }, set: { model.changeUpstreamRetry(attempts: model.settings.upstreamAttemptLimit, delaySeconds: $0) }),
                    idleMinutes: Binding(get: { model.settings.idleReturnDelay }, set: { model.changeIdleReturnDelay($0) }),
                    rebuild: Binding(get: { model.settings.degradedContextFallback ?? true }, set: { model.changeContextFallback($0) }))
                    .disabled(model.busy)
            }.settingsPage().disabled(!model.running || model.metadataLoadFailed)
        }
    }

}

struct RoutingSettingsCards<Option: Identifiable>: View where Option.ID == String {
    let enabledSources: [Option]
    let sourceName: KeyPath<Option, String>
    @Binding var mode: String
    @Binding var fixedSource: String
    @Binding var loadBalance: Bool
    @Binding var quotaThreshold: Int
    @Binding var attempts: Int
    @Binding var retryDelay: Int
    @Binding var idleMinutes: Int
    @Binding var rebuild: Bool
    var customIdleMinutes: Binding<String>?
    @State private var enteringCustomDelay = false

    private var invalidCustomDelay: Bool {
        guard let text = customIdleMinutes?.wrappedValue else { return false }
        return Int(text).map { !(0...10080).contains($0) } ?? true
    }

    var body: some View {
        SettingsCard("分配方式", symbol: "arrow.triangle.branch") {
            SettingRow(title: "路由模式", subtitle: enabledSources.isEmpty ? "启用至少一个来源后可固定分配。" : nil) {
                Picker("路由模式", selection: $mode) {
                    Text("自动切换").tag("auto")
                    Text("固定来源").tag("fixed").disabled(enabledSources.isEmpty)
                }.pickerStyle(.segmented).labelsHidden().frame(width: 230)
            }
            Divider()
            if mode == "fixed" {
                SettingRow(title: "固定来源") {
                    Picker("固定来源", selection: $fixedSource) {
                        ForEach(enabledSources) { Text($0[keyPath: sourceName]).tag($0.id) }
                    }.labelsHidden().frame(width: 230)
                }
            } else {
                SettingRow(title: "负载均衡", subtitle: "按来源余量分配请求。") {
                    Toggle("负载均衡", isOn: $loadBalance).labelsHidden().toggleStyle(.switch).controlSize(.small)
                }
                if loadBalance {
                    Divider()
                    SettingRow(title: "官方余量保护") {
                        Stepper(value: $quotaThreshold, in: 1...99) {
                            Text("\(quotaThreshold)%").monospacedDigit().frame(width: 46, alignment: .trailing)
                        }.fixedSize()
                    }
                }
            }
        }
        SettingsCard("上游重试", symbol: "arrow.clockwise") {
            SettingRow(title: "最多尝试次数（含首次）") {
                Stepper(value: $attempts, in: 1...10) {
                    Text("\(attempts) 次").monospacedDigit().frame(width: 62, alignment: .trailing)
                }.fixedSize()
            }
            Divider()
            SettingRow(title: "重试间隔") {
                Stepper(value: $retryDelay, in: 0...300) {
                    Text("\(retryDelay) 秒").monospacedDigit().frame(width: 62, alignment: .trailing)
                }.fixedSize()
            }
        }
        SettingsCard("对话接续", symbol: "arrow.triangle.2.circlepath") {
            SettingRow(title: "空闲后回切", subtitle: mode == "fixed" ? "固定来源时使用所选来源。" : loadBalance ? "负载均衡按各来源余量分配。" : "对话空闲达到所选时长后，可回到优先来源。") {
                HStack {
                    if enteringCustomDelay || invalidCustomDelay, let customIdleMinutes {
                        TextField("分钟", text: customIdleMinutes).settingsField().frame(width: 80)
                            .accessibilityLabel("自定回切时长（0–10080 分钟）")
                    }
                    Picker("空闲后回切", selection: Binding(get: { enteringCustomDelay || invalidCustomDelay ? -1 : idleMinutes }, set: { value in
                        enteringCustomDelay = value == -1
                        if !enteringCustomDelay { idleMinutes = value }
                    })) {
                        Text("5 分钟").tag(5)
                        Text("10 分钟").tag(10)
                        Text("15 分钟").tag(15)
                        Text("30 分钟").tag(30)
                        Text("1 小时").tag(60)
                        Text("2 小时").tag(120)
                        Text("6 小时").tag(360)
                        Text("24 小时").tag(1440)
                        Text("不自动回切").tag(0)
                        if ![0, 5, 10, 15, 30, 60, 120, 360, 1440].contains(idleMinutes) {
                            Text("\(idleMinutes) 分钟（当前）").tag(idleMinutes)
                        }
                        if customIdleMinutes != nil { Text("自定分钟数…").tag(-1) }
                    }.labelsHidden().frame(width: 160)
                }.disabled(mode == "fixed" || loadBalance)
            }
            if invalidCustomDelay {
                InfoNote(text: "回切时长须为 0–10080 之间的整数分钟。", symbol: "exclamationmark.triangle", color: .orange)
            }
            Divider()
            SettingRow(title: "允许重建上下文", subtitle: "无法续接时移除加密块，然后重建上下文。") {
                Toggle("允许重建上下文", isOn: $rebuild).labelsHidden().toggleStyle(.switch).controlSize(.small)
            }
        }
    }
}
