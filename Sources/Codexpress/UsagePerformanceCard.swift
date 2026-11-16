import SwiftUI

private struct UsagePerformanceSelection: Equatable {
    let selection: UsageSelection
    let sourceID: String
}

struct UsagePerformanceCard: View {
    let selection: UsageSelection
    let snapshot: JSONValue
    @EnvironmentObject var model: AppModel
    private var sourceID: String { model.usagePerformanceSourceID }
    private var query: UsageQuery? {
        guard var query = selection.query else { return nil }
        query.sourceID = sourceID
        return query
    }
    @State private var loadError: String?
    @State private var loadingSource = false
    private var data: JSONValue { sourceID.isEmpty ? snapshot : model.cachedUsageAnalytics(query)?.value ?? .null }
    private var totals: UsageTotals { UsageTotals(rows: data["groups"].array) }
    private var options: [JSONValue] { snapshot["options"]["sources"].array }
    private var refreshID: UsagePerformanceSelection { UsagePerformanceSelection(selection: selection, sourceID: sourceID) }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 7) {
                Text("性能").font(.system(size: 14, weight: .semibold))
                if loadingSource { ProgressView().controlSize(.mini) }
                Spacer(minLength: 4)
                Picker("性能来源", selection: $model.usagePerformanceSourceID) {
                    Text("汇总").tag("")
                    if !sourceID.isEmpty && !options.contains(where: { $0["id"].string == sourceID }) { Text("已选来源").tag(sourceID) }
                    ForEach(Array(options.enumerated()), id: \.offset) { _, source in Text(source["name"].string ?? "已移除来源").tag(source["id"].string ?? "") }
                }.labelsHidden().controlSize(.regular).frame(maxWidth: 160)
            }
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(), alignment: .leading), count: 2), alignment: .leading, spacing: 22) {
                metric("平均输出速度", value: usageRate(totals.tps, estimated: totals.estimated) + " TPS")
                metric("缓存命中率", value: totals.cacheRate.map { String(format: "%.1f%%", $0) } ?? "—")
                metric("首 Token · P50", value: usageSeconds(data["performance"]["p50_first_token_ms"].number), detail: "P95 " + usageSeconds(data["performance"]["p95_first_token_ms"].number))
                metric("请求耗时 · P50", value: usageSeconds(data["performance"]["p50_latency_ms"].number), detail: "P95 " + usageSeconds(data["performance"]["p95_latency_ms"].number))
                metric("平均每次费用", value: usageMoney(totals.averageCost, estimated: totals.costEstimated))
                metric("缓存节省", value: usageMoney(totals.savings))
            }.padding(.top, 2)
            if let loadError { Text(loadError).font(.system(size: 11)).foregroundStyle(.orange).lineLimit(2).help(loadError) }
        }.padding(20)
        .task(id: UsageRefreshKey(selection: refreshID, active: model.settingsWindowActive)) {
            let requested = refreshID
            guard model.settingsWindowActive, !sourceID.isEmpty else { loadError = nil; loadingSource = false; return }
            loadError = nil; loadingSource = model.cachedUsageAnalytics(query) == nil
            defer { if !Task.isCancelled, requested == refreshID { loadingSource = false } }
            var refresh = false
            while !Task.isCancelled {
                guard var query = requested.selection.query else { break }
                query.sourceID = requested.sourceID
                do {
                    _ = try await model.usageAnalytics(query, refresh: refresh)
                    guard !Task.isCancelled, requested == refreshID else { break }
                    loadError = nil; loadingSource = false
                } catch {
                    guard !Task.isCancelled, requested == refreshID else { break }
                    loadError = error.localizedDescription; loadingSource = false
                }
                refresh = true
                do { try await Task.sleep(for: .milliseconds(1500)) } catch { break }
            }
        }
    }
    private func metric(_ title: String, value: String, detail: String? = nil) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(2)
            Text(value).font(.system(size: 20, weight: .semibold)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.85)
            if let detail { Text(detail).font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
