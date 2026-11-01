import SwiftUI

struct RoutingView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                PageHeading("分配策略")
                SettingsCard("分配方式", symbol: "arrow.triangle.branch") {
                    SettingRow(title: "路由模式") {
                        Picker("路由模式", selection: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: model.settings.fixed_source_id ?? model.sources.first?.id) })) {
                            Text("自动切换").tag("auto")
                            Text("固定来源").tag("fixed")
                        }.pickerStyle(.segmented).labelsHidden().frame(width: 230)
                            .disabled(model.busy || model.sources.isEmpty)
                    }
                    Divider()
                    if model.routingMode == "fixed" {
                        SettingRow(title: "固定来源", subtitle: "来源不可用时暂停请求") {
                            Picker("固定来源", selection: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) })) {
                                ForEach(model.sources.filter(\.enabled)) { Text($0.name).tag($0.id) }
                            }.labelsHidden().frame(width: 230).disabled(model.busy)
                        }
                    } else {
                        SettingRow(title: "负载均衡", subtitle: model.settings.loadBalanceEnabled ? "按活跃对话数分配新对话" : "按优先级依次尝试可用来源") {
                            Toggle("负载均衡", isOn: Binding(get: { model.settings.loadBalanceEnabled }, set: { model.changeLoadBalance($0) }))
                                .labelsHidden().toggleStyle(.switch).controlSize(.small).disabled(model.busy)
                        }
                    }
                    if model.settings.loadBalanceEnabled {
                        Divider()
                        SettingRow(title: "官方余量保护", subtitle: "低于阈值时为已有对话保留余量；额度归零后跳过") {
                            Stepper(value: Binding(get: { model.settings.lowQuotaPercent }, set: { model.changeLowQuotaThreshold($0) }), in: 1...99) {
                                Text("\(model.settings.lowQuotaPercent)%").monospacedDigit().frame(width: 46, alignment: .trailing)
                            }.fixedSize().disabled(model.busy)
                        }
                    }
                }
                SettingsCard("上游重试", symbol: "arrow.clockwise") {
                    SettingRow(title: "每个来源最多尝试", subtitle: "包含首次请求") {
                        Stepper(value: Binding(get: { model.settings.upstreamAttemptLimit }, set: { model.changeUpstreamRetry(attempts: $0, delaySeconds: model.settings.upstreamRetryDelay) }), in: 1...10) {
                            Text("\(model.settings.upstreamAttemptLimit) 次").monospacedDigit().frame(width: 62, alignment: .trailing)
                        }.fixedSize()
                    }
                    Divider()
                    SettingRow(title: "重试间隔", subtitle: "全部重试失败后，再按路由规则切换来源") {
                        Stepper(value: Binding(get: { model.settings.upstreamRetryDelay }, set: { model.changeUpstreamRetry(attempts: model.settings.upstreamAttemptLimit, delaySeconds: $0) }), in: 0...300) {
                            Text("\(model.settings.upstreamRetryDelay) 秒").monospacedDigit().frame(width: 62, alignment: .trailing)
                        }.fixedSize()
                    }
                }.disabled(model.busy)
                SettingsCard("对话接续", symbol: "arrow.triangle.2.circlepath") {
                    SettingRow(title: "来源粘性", subtitle: model.settings.loadBalanceEnabled ? "负载均衡下，已有对话保留来源" : "接续后刷新计时，到期后从最高优先级重新尝试") {
                        Picker("粘性保持时间", selection: Binding(get: { model.settings.idleReturnDelay }, set: { model.changeIdleReturnDelay($0) })) {
                            Text("5 分钟").tag(5)
                            Text("10 分钟").tag(10)
                            Text("15 分钟").tag(15)
                            Text("30 分钟").tag(30)
                            Text("1 小时").tag(60)
                            Text("2 小时").tag(120)
                            Text("6 小时").tag(360)
                            Text("24 小时").tag(1440)
                            Text("不自动回切").tag(0)
                        }.labelsHidden().frame(width: 160)
                            .disabled(model.busy || model.routingMode == "fixed" || model.settings.loadBalanceEnabled)
                    }
                    Divider()
                    Toggle("无法续接时移除加密块并重建上下文", isOn: Binding(get: { model.settings.degradedContextFallback ?? true }, set: { model.changeContextFallback($0) }))
                        .disabled(model.busy)
                    Text("可能缺失隐藏推理或压缩摘要，触发时发送系统通知。关闭后无法续接将返回错误。")
                        .font(AppTypography.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }.settingsPage()
        }
    }

}
