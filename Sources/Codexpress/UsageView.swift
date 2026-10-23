import SwiftUI

struct UsageSelection: Equatable {
    var days = 30
    var start = Calendar.current.date(byAdding: .day, value: -29, to: Date()) ?? Date()
    var end = Date()
    var sourceID = ""
    var modelID = ""
    var query: UsageQuery? {
        if days == 0 { return try? UsageQuery(start: start, end: end, sourceID: sourceID, modelID: modelID) }
        return UsageQuery.last(days: days, sourceID: sourceID, modelID: modelID)
    }
}

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    @State private var selection = UsageSelection()
    @State private var snapshot: JSONValue = .null
    @State private var snapshotQuery: UsageQuery?
    @State private var loadError: String?
    @State private var updatedAt: Date?
    @State private var showingLogs = false
    @State private var showingPrices = false
    private var totals: UsageTotals { UsageTotals(rows: snapshot["groups"].array) }
    private var daily: [UsageDaily] { snapshot["daily"].array.map(UsageDaily.init) }

    var body: some View {
        Group {
            if showingLogs {
                UsageLogsView(selection: $selection, options: snapshot["options"]) { showingLogs = false }
            } else { overview }
        }
        .sheet(isPresented: $showingPrices) { UsagePriceSheet(catalog: model.usage["pricing"]).task { await model.loadUsage(days: 30) } }
    }
    private var overview: some View {
        VStack(alignment: .leading, spacing: 16) {
            PageHeading("用量与费用") {
                Button { showingLogs = true } label: { Label("连接日志", systemImage: "list.bullet.rectangle") }.controlSize(.small)
            }
            UsageFilters(selection: $selection, options: snapshot["options"])
            if selection.query == nil { InfoNote(text: "请选择不超过 366 天的日期范围，结束日期须晚于或等于开始日期。", color: .orange) }
            if let loadError { InfoNote(text: loadError, color: .orange) }
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    summary
                    breakdown
                    UsageActivityCard(selection: selection, snapshot: snapshot, overviewQuery: snapshotQuery)
                    UsageTrendChart(days: daily)
                    HStack {
                        Button("API 价格表") { showingPrices = true }.buttonStyle(.link)
                        Spacer()
                        if let updatedAt { Text("更新于 " + updatedAt.formatted(date: .omitted, time: .standard)).foregroundStyle(.tertiary).monospacedDigit() }
                    }.font(.system(size: 10)).padding(.horizontal, 3)
                }.padding(.bottom, 4)
            }
        }.settingsPage()
        .task(id: selection) {
            guard selection.query != nil else { return }
            while !Task.isCancelled {
                guard let query = selection.query else { break }
                do {
                    let next = try await model.usageAnalytics(query)
                    guard !Task.isCancelled else { break }
                    snapshot = next; snapshotQuery = query; loadError = nil; updatedAt = Date()
                } catch {
                    guard !Task.isCancelled else { break }
                    loadError = error.localizedDescription
                }
                do { try await Task.sleep(for: .milliseconds(1500)) } catch { break }
            }
        }
    }
    private var breakdown: some View {
        HStack(alignment: .top, spacing: 0) {
            UsageDistributionChart(groups: snapshot["groups"].array).frame(maxWidth: .infinity)
            Divider().padding(.vertical, 16)
            UsagePerformanceCard(selection: selection, snapshot: snapshot).frame(maxWidth: .infinity)
        }.frame(height: 342).cardSurface()
    }
    private var summary: some View {
        HStack(alignment: .top, spacing: 0) {
            summaryMetric("参考费用 · USD", value: usageMoney(totals.cost), detail: "API 参考价估算", prominent: true)
            Divider().padding(.vertical, 3).padding(.horizontal, 14)
            summaryMetric("总 Token", value: usageCount(totals.total, estimated: totals.estimated), detail: "输入 + 输出")
            summaryMetric("请求", value: usageCount(totals.sum("requests")), detail: "失败 \(usageCount(totals.sum("failures")))")
            summaryMetric("成功率", value: totals.successRate.map { String(format: "%.1f%%", $0) } ?? "—", detail: "进行中 \(usageCount(totals.sum("active_requests")))")
        }.padding(18).frame(height: 108).cardSurface()
    }
    private func summaryMetric(_ title: String, value: String, detail: String, prominent: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: prominent ? 25 : 23, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.65)
            Text(detail).font(.system(size: 9)).foregroundStyle(.tertiary).lineLimit(1)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

}

struct UsageFilters: View {
    @Binding var selection: UsageSelection
    let options: JSONValue
    var sourceTitle = "全部来源"
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 9) {
                Picker("时间范围", selection: $selection.days) {
                    Text("近 7 天").tag(7); Text("近 30 天").tag(30); Text("近 90 天").tag(90); Text("近 365 天").tag(365); Text("自定义").tag(0)
                }.labelsHidden().frame(width: 108)
                Picker("来源", selection: $selection.sourceID) {
                    Text(sourceTitle).tag("")
                    if !selection.sourceID.isEmpty && !options["sources"].array.contains(where: { $0["id"].string == selection.sourceID }) { Text("已选来源").tag(selection.sourceID) }
                    ForEach(Array(options["sources"].array.enumerated()), id: \.offset) { _, item in Text(item["name"].string ?? "已移除来源").tag(item["id"].string ?? "") }
                }.labelsHidden().frame(maxWidth: 180)
                Picker("模型", selection: $selection.modelID) {
                    Text("全部模型").tag("")
                    if !selection.modelID.isEmpty && !options["models"].array.compactMap(\.string).contains(selection.modelID) { Text(selection.modelID).tag(selection.modelID) }
                    ForEach(options["models"].array.compactMap(\.string), id: \.self) { Text($0).tag($0) }
                }.labelsHidden().frame(maxWidth: 210)
                Spacer(minLength: 0)
                if !selection.sourceID.isEmpty || !selection.modelID.isEmpty {
                    Button { selection.sourceID = ""; selection.modelID = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }.buttonStyle(.plain).help("清除来源与模型筛选")
                }
            }.controlSize(.small)
            if selection.days == 0 {
                HStack(spacing: 8) {
                    DatePicker("从", selection: $selection.start, displayedComponents: .date).fixedSize()
                    DatePicker("至", selection: $selection.end, displayedComponents: .date).fixedSize()
                    Spacer(minLength: 0)
                    Text("最多 366 天").font(.system(size: 10)).foregroundStyle(.secondary)
                }.controlSize(.small).font(.system(size: 11))
            }
        }
    }
}

private struct UsagePriceSheet: View {
    let catalog: JSONValue
    @Environment(\.dismiss) private var dismiss
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                VStack(alignment: .leading, spacing: 5) {
                    Text("官方 API 价格表").font(.system(size: 19, weight: .semibold))
                    Text("USD / 百万 Token · 标准模式 · 核对于 \(catalog["verified_at"].string ?? "—")").font(.system(size: 11)).foregroundStyle(.secondary)
                }
                Spacer(); Button("完成") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    HStack { Text("模型").frame(maxWidth: .infinity, alignment: .leading); Text("输入").frame(width: 75, alignment: .trailing); Text("缓存读取").frame(width: 85, alignment: .trailing); Text("输出").frame(width: 75, alignment: .trailing) }.font(.system(size: 10)).foregroundStyle(.secondary).padding(.vertical, 10)
                    ForEach(Array(catalog["models"].array.enumerated()), id: \.offset) { _, row in
                        Divider()
                        VStack(alignment: .leading, spacing: 7) {
                            HStack {
                                Text(row["id"].string ?? "").frame(maxWidth: .infinity, alignment: .leading)
                                Text(usageMoney(row["input"].number, estimated: false)).frame(width: 75, alignment: .trailing)
                                Text(usageMoney(row["cached_input"].number, estimated: false)).frame(width: 85, alignment: .trailing)
                                Text(usageMoney(row["output"].number, estimated: false)).frame(width: 75, alignment: .trailing)
                            }.font(.system(size: 11)).monospacedDigit()
                            if let limit = row["long_context_over"].number {
                                Text("输入 > \(usageCount(limit))：输入／缓存 ×\(String(format: "%.1f", row["long_input_multiplier"].number ?? 1))，输出 ×\(String(format: "%.1f", row["long_output_multiplier"].number ?? 1))" + (row["session_scope"].bool == true ? "（按会话，逐请求估算）" : "（整条请求）")).font(.system(size: 10)).foregroundStyle(.secondary)
                            }
                            if let write = row["cache_write_30m"].number {
                                Text("缓存写入：30 分钟 \(usageMoney(write, estimated: false))" + (row["cache_write_12h"].number.map { " · 12 小时 \(usageMoney($0, estimated: false))" } ?? "")).font(.system(size: 10)).foregroundStyle(.secondary)
                            }
                        }.padding(.vertical, 12)
                    }
                }
            }
            Text("官方订阅的 GPT‑6.1 Sol、已开启“订阅来源”的 API 均免长上下文加价。费用按请求时保存的参考价与可用 Token 折算；未收录的模型与计费模式不计价。").font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            HStack {
                if let raw = catalog["source_url"].string, let url = URL(string: raw) { Link("查看 OpenAI 官方价格", destination: url) }
                Spacer()
                if let raw = catalog["subscription_source_url"].string, let url = URL(string: raw) { Link("订阅豁免说明", destination: url) }
            }.font(.system(size: 11))
        }.padding(24).frame(width: 690, height: 630)
    }
}
