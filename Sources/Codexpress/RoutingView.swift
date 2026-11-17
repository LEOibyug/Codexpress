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
                SettingsCard("分配方式", symbol: "arrow.triangle.branch") {
                    SettingRow(title: "路由模式", subtitle: enabledSources.isEmpty ? "启用至少一个来源后可固定分配。" : nil) {
                        Picker("路由模式", selection: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: preferredFixedSourceID) })) {
                            Text("自动切换").tag("auto")
                            Text("固定来源").tag("fixed").disabled(enabledSources.isEmpty)
                        }.pickerStyle(.segmented).labelsHidden().frame(width: 230)
                            .disabled(model.busy)
                    }
                    Divider()
                    if model.routingMode == "fixed" {
                        SettingRow(title: "固定来源") {
                            Picker("固定来源", selection: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) })) {
                                ForEach(enabledSources) { Text($0.name).tag($0.id) }
                            }.labelsHidden().frame(width: 230).disabled(model.busy)
                        }
                    } else {
                        SettingRow(title: "负载均衡", subtitle: "按来源余量分配请求。") {
                            Toggle("负载均衡", isOn: Binding(get: { model.settings.loadBalanceEnabled }, set: { model.changeLoadBalance($0) }))
                                .labelsHidden().toggleStyle(.switch).controlSize(.small).disabled(model.busy)
                        }
                    }
                    if model.routingMode != "fixed" && model.settings.loadBalanceEnabled {
                        Divider()
                        SettingRow(title: "官方余量保护") {
                            Stepper(value: Binding(get: { model.settings.lowQuotaPercent }, set: { model.changeLowQuotaThreshold($0) }), in: 1...99) {
                                Text("\(model.settings.lowQuotaPercent)%").monospacedDigit().frame(width: 46, alignment: .trailing)
                            }.fixedSize().disabled(model.busy)
                        }
                    }
                }
                SettingsCard("官方额度", symbol: "gauge.with.dots.needle.50percent") {
                    SettingRow(title: "自动使用重置卡", subtitle: "周额度耗尽时，自动消耗最早到期的可用卡片。") {
                        Toggle("自动使用重置卡", isOn: Binding(get: { model.settings.autoResetWeeklyQuotaEnabled }, set: { model.changeAutoResetWeeklyQuota($0) }))
                            .labelsHidden().toggleStyle(.switch).controlSize(.small).disabled(model.busy)
                    }
                }
                SettingsCard("上游重试", symbol: "arrow.clockwise") {
                    SettingRow(title: "最多尝试次数（含首次）") {
                        Stepper(value: Binding(get: { model.settings.upstreamAttemptLimit }, set: { model.changeUpstreamRetry(attempts: $0, delaySeconds: model.settings.upstreamRetryDelay) }), in: 1...10) {
                            Text("\(model.settings.upstreamAttemptLimit) 次").monospacedDigit().frame(width: 62, alignment: .trailing)
                        }.fixedSize()
                    }
                    Divider()
                    SettingRow(title: "重试间隔") {
                        Stepper(value: Binding(get: { model.settings.upstreamRetryDelay }, set: { model.changeUpstreamRetry(attempts: model.settings.upstreamAttemptLimit, delaySeconds: $0) }), in: 0...300) {
                            Text("\(model.settings.upstreamRetryDelay) 秒").monospacedDigit().frame(width: 62, alignment: .trailing)
                        }.fixedSize()
                    }
                }.disabled(model.busy)
                SettingsCard("对话接续", symbol: "arrow.triangle.2.circlepath") {
                    SettingRow(title: "空闲后回切", subtitle: model.routingMode == "fixed" ? "固定来源时使用所选来源。" : model.settings.loadBalanceEnabled ? "负载均衡按各来源余量分配。" : "对话空闲达到所选时长后，可回到优先来源。") {
                        Picker("空闲后回切", selection: Binding(get: { model.settings.idleReturnDelay }, set: { model.changeIdleReturnDelay($0) })) {
                            Text("5 分钟").tag(5)
                            Text("10 分钟").tag(10)
                            Text("15 分钟").tag(15)
                            Text("30 分钟").tag(30)
                            Text("1 小时").tag(60)
                            Text("2 小时").tag(120)
                            Text("6 小时").tag(360)
                            Text("24 小时").tag(1440)
                            Text("不自动回切").tag(0)
                            if ![0, 5, 10, 15, 30, 60, 120, 360, 1440].contains(model.settings.idleReturnDelay) {
                                Text("\(model.settings.idleReturnDelay) 分钟（当前）").tag(model.settings.idleReturnDelay)
                            }
                        }.labelsHidden().frame(width: 160)
                            .disabled(model.busy || model.routingMode == "fixed" || model.settings.loadBalanceEnabled)
                    }
                    Divider()
                    SettingRow(title: "允许重建上下文", subtitle: "无法续接时移除加密块，然后重建上下文。") {
                        Toggle("允许重建上下文", isOn: Binding(get: { model.settings.degradedContextFallback ?? true }, set: { model.changeContextFallback($0) }))
                            .labelsHidden().toggleStyle(.switch).controlSize(.small).disabled(model.busy)
                    }
                }
            }.settingsPage().disabled(!model.running || model.metadataLoadFailed)
        }
    }

}
