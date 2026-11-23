import SwiftUI

enum UsagePeriod: Hashable {
    case hours(Int), days(Int), custom
}

struct UsageSelection: Equatable {
    var period = UsagePeriod.days(30)
    var start = Calendar.current.date(byAdding: .day, value: -29, to: Date()) ?? Date()
    var end = Date()
    var sourceID = ""
    var modelID = ""
    var query: UsageQuery? {
        switch period {
        case .hours(let hours): return UsageQuery(lastHours: hours, sourceID: sourceID, modelID: modelID)
        case .days(let days): return UsageQuery.last(days: days, sourceID: sourceID, modelID: modelID)
        case .custom: return try? UsageQuery(start: start, end: end, sourceID: sourceID, modelID: modelID)
        }
    }
}

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    private var selection: UsageSelection { model.usageSelection }
    private var cachedSnapshot: UsageAnalyticsSnapshot? { model.cachedUsageAnalytics(selection.query) }
    private var snapshot: JSONValue { cachedSnapshot?.value ?? .null }
    private var snapshotQuery: UsageQuery? { cachedSnapshot?.query }
    @State private var loadError: String?
    private var updatedAt: Date? { cachedSnapshot?.updatedAt }
    @State private var showingLogs = false
    @State private var logIngress = ""
    @State private var logStatus = ""
    @State private var logSearch = ""
    @State private var showingPrices = false
    private var totals: UsageTotals { UsageTotals(rows: snapshot["groups"].array) }
    private var daily: [UsageDaily] { cachedSnapshot?.daily ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            PageHeading("用量", subtitle: showingLogs ? "查看单次请求、重试及错误详情" : "对比用量、费用与输出性能") {
                Picker("用量视图", selection: $showingLogs) {
                    Text("统计概览").tag(false)
                    Text("连接日志").tag(true)
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                .controlSize(.regular)
                .frame(width: 250)
            }
            Group {
                if showingLogs {
                    UsageLogsView(selection: $model.usageSelection, options: snapshot["options"],
                                  ingress: $logIngress, status: $logStatus, search: $logSearch)
                } else { overview }
            }
        }
        .settingsPage()
        .sheet(isPresented: $showingPrices) { UsagePriceSheet(catalog: model.usage["pricing"]).task { await model.loadUsage(days: 30) } }
    }
    private var overview: some View {
        VStack(alignment: .leading, spacing: 16) {
            UsageFilters(selection: $model.usageSelection, options: snapshot["options"])
            if selection.query == nil { InfoNote(text: "请选择不超过 366 天的日期范围，结束日期须晚于或等于开始日期。", color: .orange) }
            if let loadError { InfoNote(text: loadError, color: .orange) }
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    summary
                    breakdown
                    UsageActivityCard(selection: selection)
                    UsageTrendChart(days: daily, intraday: snapshotQuery?.isIntraday == true)
                    HStack {
                        Button { showingPrices = true } label: { Label("API 价格表", systemImage: "tablecells") }.buttonStyle(.link)
                        Spacer()
                        if let updatedAt { Text("更新于 " + updatedAt.formatted(date: .omitted, time: .standard)).foregroundStyle(.secondary).monospacedDigit() }
                    }.font(.system(size: 11)).padding(.horizontal, 2).padding(.top, 2)
                }.padding(.bottom, 4)
            }
        }
        .task(id: UsageRefreshKey(selection: selection, active: model.settingsWindowActive)) {
            let requested = selection
            loadError = nil
            guard model.settingsWindowActive, requested.query != nil else { return }
            var refresh = false
            while !Task.isCancelled {
                guard let query = requested.query else { break }
                do {
                    _ = try await model.usageAnalytics(query, refresh: refresh)
                    guard !Task.isCancelled, requested == selection else { break }
                    loadError = nil
                } catch {
                    guard !Task.isCancelled, requested == selection else { break }
                    loadError = error.localizedDescription
                }
                refresh = true
                do { try await Task.sleep(for: .milliseconds(1500)) } catch { break }
            }
        }
    }
    private var breakdown: some View {
        HStack(alignment: .top, spacing: 0) {
            UsageDistributionChart(groups: snapshot["groups"].array).frame(maxWidth: .infinity, maxHeight: .infinity)
            UsagePerformanceCard(selection: selection, snapshot: snapshot).frame(maxWidth: .infinity)
        }
        .fixedSize(horizontal: false, vertical: true)
        .overlay { Rectangle().fill(Color(nsColor: .separatorColor)).frame(width: 1).padding(.vertical, 18).allowsHitTesting(false) }
        .cardSurface()
    }
    private var summary: some View {
        HStack(alignment: .top, spacing: 0) {
            summaryMetric("费用 · USD", value: usageMoney(totals.cost, estimated: totals.costEstimated), detail: totals.cost == nil ? "暂无已计价请求" : totals.costEstimated ? "参考价估算" : "实际扣费", prominent: true)
            Rectangle().fill(Color(nsColor: .separatorColor)).frame(width: 1, height: 64).padding(.horizontal, 20)
            summaryMetric("总 Token", value: usageCount(totals.total, estimated: totals.estimated), detail: "输入输出比 \(totals.inputOutputRatio)")
                .help("输入 Token 总量 ÷ 输出 Token 总量；输入含缓存读取，输出为零时不计算。")
            summaryMetric("请求", value: usageCount(totals.sum("requests")), detail: "失败 \(usageCount(totals.sum("failures")))")
            summaryMetric("成功率", value: totals.successRate.map { String(format: "%.1f%%", $0) } ?? "—", detail: "进行中 \(usageCount(totals.sum("active_requests")))")
        }.padding(20).cardSurface()
    }
    private func summaryMetric(_ title: String, value: String, detail: String? = nil, prominent: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.system(size: 12)).foregroundStyle(.secondary)
            Text(value).font(.system(size: prominent ? 28 : 25, weight: .semibold)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.85)
            if let detail { Text(detail).font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

}

struct UsageFilters: View {
    @Binding var selection: UsageSelection
    let options: JSONValue
    var sourceTitle = "全部来源"
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .bottom, spacing: 14) {
                VStack(alignment: .leading, spacing: 6) {
                    filterLabel("时间范围")
                    Picker("时间范围", selection: $selection.period) {
                        Text("近 1 小时").tag(UsagePeriod.hours(1)); Text("近 5 小时").tag(UsagePeriod.hours(5))
                        Text("近 1 天").tag(UsagePeriod.days(1))
                        Text("近 7 天").tag(UsagePeriod.days(7)); Text("近 30 天").tag(UsagePeriod.days(30)); Text("近 90 天").tag(UsagePeriod.days(90)); Text("近 365 天").tag(UsagePeriod.days(365)); Text("自定义").tag(UsagePeriod.custom)
                    }.labelsHidden().frame(maxWidth: .infinity)
                }.frame(width: 138)
                VStack(alignment: .leading, spacing: 6) {
                    filterLabel("来源")
                    Picker("来源", selection: $selection.sourceID) {
                        Text(sourceTitle).tag("")
                        if !selection.sourceID.isEmpty && !options["sources"].array.contains(where: { $0["id"].string == selection.sourceID }) { Text("已选来源").tag(selection.sourceID) }
                        ForEach(Array(options["sources"].array.enumerated()), id: \.offset) { _, item in Text(item["name"].string ?? "已移除来源").tag(item["id"].string ?? "") }
                    }.labelsHidden().frame(maxWidth: .infinity)
                }.frame(maxWidth: .infinity, alignment: .leading)
                VStack(alignment: .leading, spacing: 6) {
                    filterLabel("模型")
                    Picker("模型", selection: $selection.modelID) {
                        Text("全部模型").tag("")
                        if !selection.modelID.isEmpty && !options["models"].array.compactMap(\.string).contains(selection.modelID) { Text(selection.modelID).tag(selection.modelID) }
                        ForEach(options["models"].array.compactMap(\.string), id: \.self) { Text($0).tag($0) }
                    }.labelsHidden().frame(maxWidth: .infinity)
                }.frame(maxWidth: .infinity, alignment: .leading)
                Button {
                    var cleared = selection
                    cleared.sourceID = ""
                    cleared.modelID = ""
                    selection = cleared
                } label: {
                    Text("清除")
                }.accessibilityLabel("清除来源与模型筛选").help("清除来源与模型筛选")
                    .disabled(selection.sourceID.isEmpty && selection.modelID.isEmpty)
            }.controlSize(.regular).font(.system(size: 13))
            if selection.period == .custom {
                HStack(spacing: 16) {
                    DatePicker("从", selection: $selection.start, displayedComponents: .date).fixedSize()
                    DatePicker("至", selection: $selection.end, displayedComponents: .date).fixedSize()
                    Spacer(minLength: 0)
                    Text("最多 366 天").font(.system(size: 11)).foregroundStyle(.secondary)
                }.controlSize(.regular).font(.system(size: 13))
            }
        }.padding(13).cardSurface()
    }
    private func filterLabel(_ title: String) -> some View {
        Text(title).font(.system(size: 11, weight: .medium)).foregroundStyle(.secondary)
    }
}

private struct UsagePriceSheet: View {
    let catalog: JSONValue
    @Environment(\.dismiss) private var dismiss
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                VStack(alignment: .leading, spacing: 5) {
                    Text("官方 API 价格表").font(.system(size: 22, weight: .semibold))
                    Text("USD / 百万 Token · 标准模式 · 核对于 \(catalog["verified_at"].string ?? "—")").font(.system(size: 12)).foregroundStyle(.secondary)
                }
                Spacer(); Button("完成") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    HStack { Text("模型").frame(maxWidth: .infinity, alignment: .leading); Text("输入").frame(width: 86, alignment: .trailing); Text("缓存读取").frame(width: 96, alignment: .trailing); Text("输出").frame(width: 86, alignment: .trailing) }.font(.system(size: 12, weight: .medium)).foregroundStyle(.secondary).padding(.vertical, 12)
                    ForEach(Array(catalog["models"].array.enumerated()), id: \.offset) { _, row in
                        Divider()
                        VStack(alignment: .leading, spacing: 7) {
                            HStack {
                                Text(row["id"].string ?? "").frame(maxWidth: .infinity, alignment: .leading)
                                Text(usageMoney(row["input"].number, estimated: false)).frame(width: 86, alignment: .trailing)
                                Text(usageMoney(row["cached_input"].number, estimated: false)).frame(width: 96, alignment: .trailing)
                                Text(usageMoney(row["output"].number, estimated: false)).frame(width: 86, alignment: .trailing)
                            }.font(.system(size: 13)).monospacedDigit()
                            if let limit = row["long_context_over"].number {
                                Text("输入 > \(usageCount(limit))：输入／缓存 ×\(String(format: "%.1f", row["long_input_multiplier"].number ?? 1))，输出 ×\(String(format: "%.1f", row["long_output_multiplier"].number ?? 1))" + (row["session_scope"].bool == true ? "（按会话）" : "（整条请求）")).font(.system(size: 12)).foregroundStyle(.secondary)
                            }
                            if let write = row["cache_write_30m"].number {
                                Text("缓存写入：30 分钟 \(usageMoney(write, estimated: false))" + (row["cache_write_12h"].number.map { " · 12 小时 \(usageMoney($0, estimated: false))" } ?? "")).font(.system(size: 12)).foregroundStyle(.secondary)
                            }
                        }.padding(.vertical, 12)
                    }
                }
            }
            HStack {
                if let raw = catalog["source_url"].string, let url = URL(string: raw) { Link("查看 OpenAI 官方价格", destination: url) }
                Spacer()
                if let raw = catalog["subscription_source_url"].string, let url = URL(string: raw) { Link("订阅豁免说明", destination: url) }
            }.font(.system(size: 13))
        }.padding(24).frame(minWidth: 740, idealWidth: 800, maxWidth: 940, minHeight: 620, idealHeight: 700, maxHeight: 860)
    }
}
