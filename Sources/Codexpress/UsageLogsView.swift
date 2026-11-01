import SwiftUI

private struct UsageLogFilter: Equatable {
    let selection: UsageSelection
    let ingress: String
    let status: String
    let search: String
    var query: UsageQuery? {
        guard var query = selection.query else { return nil }
        query.ingress = ingress
        return query
    }
}

struct UsageLogsView: View {
    @EnvironmentObject var model: AppModel
    @Binding var selection: UsageSelection
    let options: JSONValue
    let onBack: () -> Void
    @State private var logOptions: JSONValue = .null
    @State private var ingress = ""
    @State private var status = ""
    @State private var search = ""
    @State private var records: [UsageItem] = []
    @State private var nextCursor: Double?
    @State private var total = 0
    @State private var activeCount = 0
    @State private var loadError: String?
    @State private var loadedFilter: UsageLogFilter?
    @State private var loadedQuery: UsageQuery?
    @State private var loadedPages = 1
    @State private var loadingMore = false
    @State private var selected: UsageItem?
    private var availableOptions: JSONValue { logOptions == .null ? options : logOptions }
    private var filter: UsageLogFilter { UsageLogFilter(selection: selection, ingress: ingress, status: status, search: search.trimmingCharacters(in: .whitespacesAndNewlines)) }

    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            UsageFilters(selection: $selection, options: availableOptions, sourceTitle: "全部上游")
            HStack(spacing: 9) {
                Picker("入口", selection: $ingress) {
                    Text("全部入口").tag("")
                    if !ingress.isEmpty && !availableOptions["ingresses"].array.compactMap(\.string).contains(ingress) { Text(usageIngressName(ingress)).tag(ingress) }
                    ForEach(availableOptions["ingresses"].array.compactMap(\.string), id: \.self) { Text(usageIngressName($0)).tag($0) }
                }.frame(width: 164)
                Picker("状态", selection: $status) {
                    Text("全部结果").tag(""); Text("成功").tag("success"); Text("未成功").tag("unsuccessful"); Text("进行中").tag("active")
                }.frame(width: 152)
                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("搜索模型、来源或错误代码", text: $search).textFieldStyle(.plain)
                    if !search.isEmpty { Button { search = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }.buttonStyle(.plain) }
                }.font(.system(size: 13)).padding(.horizontal, 9).padding(.vertical, 6).background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 6)).overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(nsColor: .separatorColor)))
            }.controlSize(.regular).font(.system(size: 13))
            if selection.query == nil { InfoNote(text: "请选择不超过 366 天的日期范围。", color: .orange) }
            if let loadError { InfoNote(text: loadError, color: .orange) }
            HStack {
                Text("请求记录").font(.system(size: 13, weight: .semibold))
                Spacer()
                Text("\(total) 次请求" + (activeCount > 0 ? " · \(activeCount) 次进行中" : ""))
                    .font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit()
            }
            ScrollView {
                LazyVStack(spacing: 0) {
                    if records.isEmpty {
                        EmptyState("暂无连接记录", subtitle: "当前筛选下没有请求", symbol: "list.bullet.rectangle").frame(height: 310)
                    }
                    ForEach(records) { item in
                        Button { selected = item } label: { requestRow(item) }.buttonStyle(.plain)
                        if item.id != records.last?.id { Divider().padding(.horizontal, 15) }
                    }
                    if nextCursor != nil {
                        Divider()
                        Button("显示更多") { Task { await loadMore() } }.buttonStyle(.plain).foregroundStyle(Color.accentColor).font(.system(size: 13)).padding(16).disabled(loadingMore)
                    }
                }.cardSurface().padding(.bottom, 4)
            }
        }
        .sheet(item: $selected) { item in UsageRequestDetailView(item: item) }
        .task(id: UsageRefreshKey(selection: filter, active: model.settingsWindowActive)) {
            guard model.settingsWindowActive else { return }
            let requested = filter
            guard requested.query != nil else { return }
            do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
            while !Task.isCancelled {
                guard let query = requested.query else { break }
                do {
                    if loadedQuery?.from != query.from || loadedQuery?.to != query.to {
                        if let analytics = try? await model.usageAnalytics(query), !Task.isCancelled { logOptions = analytics["options"] }
                    }
                    let result = try await model.usageRequests(query, status: requested.status, search: requested.search)
                    guard !Task.isCancelled else { break }
                    let incoming = result["records"].array.enumerated().map { UsageItem($0.element, index: $0.offset) }
                    if !loadingMore {
                        let merged = usageMergeFirstPage(previous: records, incoming: incoming, previousCursor: nextCursor, incomingCursor: result["next_cursor"].number, preserving: loadedFilter == requested && loadedQuery == query && loadedPages > 1, capacity: loadedPages * 50)
                        records = merged.records; nextCursor = merged.nextCursor
                        if !merged.preservedHistory { loadedPages = 1 }
                        loadedQuery = query
                    }
                    total = Int(result["total"].number ?? 0); activeCount = Int(result["active_count"].number ?? 0)
                    loadedFilter = requested; loadError = nil
                } catch {
                    guard !Task.isCancelled else { break }
                    loadError = error.localizedDescription
                }
                do { try await Task.sleep(for: .milliseconds(1500)) } catch { break }
            }
        }
    }
    private func requestRow(_ request: UsageItem) -> some View {
        let item = request.value
        return VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .center, spacing: 10) {
                Circle().fill(statusColor(item)).frame(width: 7, height: 7)
                VStack(alignment: .leading, spacing: 4) {
                    Text(item["model"].string ?? "未知模型").font(.system(size: 13, weight: .semibold)).lineLimit(1)
                    Text(item["source_name"].string ?? "已移除来源").font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 12)
                VStack(alignment: .trailing, spacing: 4) {
                    Text(usageCostMoney(item["cost"])).font(.system(size: 14, weight: .semibold)).monospacedDigit()
                    if item["cost"]["total_usd"].number != nil {
                        Text(item["cost"]["billing_basis"].string == "upstream_charge" ? "实际扣费" : "参考价估算")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                }
                StatusPill(text: usageRequestStatus(item), color: statusColor(item), active: item["active"].bool == true)
                Image(systemName: "chevron.right").font(.system(size: 11, weight: .medium)).foregroundStyle(.secondary)
            }
            HStack(spacing: 14) {
                Text(Date(timeIntervalSince1970: item["started_at"].number ?? 0).formatted(.dateTime.month().day().hour().minute().second()))
                Text(usageIngressName(item["ingress"].string))
                Text("入 " + usageCount(item["input_tokens"].number ?? 0) + " · 出 " + usageCount(item["output_tokens"].number ?? 0))
                Spacer(minLength: 0)
                if let code = item["error_code"].string { Text(code).foregroundStyle(.orange).lineLimit(1).help(code) }
                else if let status = item["http_status"].number { Text("HTTP \(Int(status))") }
                Text(usageSeconds(item["latency_ms"].number)).fixedSize()
            }.font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit()
        }.padding(18).contentShape(Rectangle())
    }
    private func statusColor(_ item: JSONValue) -> Color {
        item["active"].bool == true ? .accentColor : item["status"].string == "success" ? .green : .orange
    }
    private func loadMore() async {
        let requested = filter
        guard let cursor = nextCursor, let query = requested.query, !loadingMore else { return }
        loadingMore = true; defer { loadingMore = false }
        do {
            let result = try await model.usageRequests(query, status: requested.status, search: requested.search, before: cursor)
            guard requested == filter, requested.query == query, loadedQuery == query else { return }
            let ids = Set(records.map(\.id))
            records += result["records"].array.enumerated().map { UsageItem($0.element, index: $0.offset) }.filter { !ids.contains($0.id) }
            loadedPages += 1
            nextCursor = result["next_cursor"].number; loadError = nil
        } catch { if requested == filter { loadError = error.localizedDescription } }
    }
}
