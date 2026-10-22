import SwiftUI

struct RoutingView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                PageHeading("路由")
                SettingsCard("路由模式", symbol: "arrow.triangle.branch") {
                    Picker("分配方式", selection: Binding(get: { model.routingMode }, set: { model.changeRouting(mode: $0, fixed: model.settings.fixed_source_id ?? model.sources.first?.id) })) {
                        Text("自动切换").tag("auto")
                        Text("固定来源").tag("fixed")
                    }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 330).disabled(model.busy || model.sources.isEmpty)
                    if model.routingMode == "fixed" {
                        Picker("固定来源", selection: Binding(get: { model.fixedSourceID ?? "" }, set: { model.changeRouting(mode: "fixed", fixed: $0) })) {
                            ForEach(model.sources.filter(\.enabled)) { Text($0.name).tag($0.id) }
                        }.disabled(model.busy)
                        InfoNote(text: "来源不可用时暂停请求。")
                    } else {
                        Toggle("负载均衡", isOn: Binding(get: { model.settings.loadBalanceEnabled }, set: { model.changeLoadBalance($0) }))
                            .disabled(model.busy)
                        Text("关闭时按优先级分配新对话；开启时按活跃对话数分配。已有对话保留来源。")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                }
                SettingsCard("官方余量保护", symbol: "shield.lefthalf.filled") {
                    HStack {
                        Text("保留余量阈值").font(.system(size: 12))
                        Spacer()
                        Stepper(value: Binding(get: { model.settings.lowQuotaPercent }, set: { model.changeLowQuotaThreshold($0) }), in: 1...99) {
                            Text("\(model.settings.lowQuotaPercent)%").monospacedDigit().frame(width: 42, alignment: .trailing)
                        }.fixedSize().disabled(model.busy)
                    }
                    Text("任一短周期或周额度不高于此值时，为已有活跃对话保留余量；额度归零后跳过此来源。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                SettingsCard("对话接续", symbol: "arrow.triangle.2.circlepath") {
                    Picker("闲置回切时间", selection: Binding(get: { model.settings.idleReturnDelay }, set: { model.changeIdleReturnDelay($0) })) {
                        Text("15 分钟").tag(15)
                        Text("30 分钟").tag(30)
                        Text("1 小时").tag(60)
                        Text("2 小时").tag(120)
                        Text("6 小时").tag(360)
                        Text("24 小时").tag(1440)
                        Text("不自动回切").tag(0)
                    }.disabled(model.busy || model.routingMode == "fixed" || model.settings.loadBalanceEnabled)
                    Text(model.settings.loadBalanceEnabled ? "开启负载均衡时，已有对话保留来源，不按优先级自动回切。" : "从上次请求结束计时。闲置到期后按当前优先级回到可用来源，官方与 API 使用同一排序。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                    Divider().padding(.vertical, 3)
                    Toggle("无法续接时移除加密块并重建上下文", isOn: Binding(get: { model.settings.degradedContextFallback ?? true }, set: { model.changeContextFallback($0) }))
                        .disabled(model.busy)
                    Text("跨来源接续加密上下文时使用，可能缺失隐藏推理或压缩摘要。触发时发送系统通知，本地对话记录保留。关闭后，含此类状态的对话保留当前来源。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                if model.sources.isEmpty {
                    EmptyState("暂无来源", subtitle: "添加来源后可设置优先级。", symbol: "arrow.triangle.branch")
                } else {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Text("来源优先级").font(.system(size: 12, weight: .semibold))
                            Spacer()
                            Text("拖动或使用箭头排序").font(.system(size: 11)).foregroundStyle(.tertiary)
                        }.padding(.horizontal, 4)
                        List {
                            ForEach(Array(model.sources.enumerated()), id: \.element.id) { index, source in
                                HStack(spacing: 12) {
                                    Text("\(index + 1)").font(.system(size: 11, weight: .medium, design: .monospaced))
                                        .foregroundStyle(.secondary).frame(width: 26, height: 26).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 7))
                                    VStack(alignment: .leading, spacing: 4) {
                                        Text(source.name).font(.system(size: 13, weight: .medium))
                                        Text(source.policyLabel).font(.system(size: 11)).foregroundStyle(.secondary)
                                        if model.state(source.id)["admission"]["capacity_active_conversations"].number != nil || source.max_active_conversations != nil {
                                            SourceCapacityLabel(source: source, admission: model.state(source.id)["admission"], showsRoutingNote: true)
                                                .font(.system(size: 10))
                                        } else if let active = model.state(source.id)["admission"]["active_conversations"].number {
                                            Text("\(Int(active)) 个活跃对话" + (source.official ? " · \(model.state(source.id)["admission"]["routing_note"].string ?? "")" : ""))
                                                .font(.system(size: 10)).foregroundStyle(.secondary)
                                        }
                                    }
                                    Spacer()
                                    StatusPill(text: source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用")
                                    HStack(spacing: 8) {
                                        Button { model.move(IndexSet(integer: index), to: index - 1) } label: { Image(systemName: "chevron.up") }.disabled(index == 0 || model.busy).help("提高优先级")
                                        Button { model.move(IndexSet(integer: index), to: index + 2) } label: { Image(systemName: "chevron.down") }.disabled(index == model.sources.count - 1 || model.busy).help("降低优先级")
                                    }.buttonStyle(.borderless).foregroundStyle(.secondary)
                                }.padding(.vertical, 9).listRowBackground(Color.clear)
                            }.onMove { model.move($0, to: $1) }
                        }.listStyle(.plain).scrollContentBackground(.hidden)
                            .frame(height: CGFloat(min(max(model.sources.count * 80 + 14, 120), 334)))
                            .padding(7).cardSurface().disabled(model.busy)
                    }
                }
            }
        }.settingsPage()
    }
}
