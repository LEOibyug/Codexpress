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
    @Binding var ingress: String
    @Binding var status: String
    @Binding var search: String
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
    private var availableOptions: JSONValue { model.cachedUsageAnalytics(filter.query)?.value["options"] ?? options }
    private var filter: UsageLogFilter { UsageLogFilter(selection: selection, ingress: ingress, status: status, search: search.trimmingCharacters(in: .whitespacesAndNewlines)) }
    private var showsCurrentResults: Bool { loadedFilter == filter }

    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            UsageFilters(selection: $selection, options: availableOptions, sourceTitle: "全部上游")
            HStack(spacing: 9) {
                Picker("入口", selection: $ingress) {
                    Text("全部入口").tag("")
                    if !ingress.isEmpty && !availableOptions["ingresses"].array.compactMap(\.string).contains(ingress) { Text(usageIngressName(ingress)).tag(ingress) }
                    ForEach(availableOptions["ingresses"].array.compactMap(\.string), id: \.self) { Text(usageIngressName($0)).tag($0) }
                }.labelsHidden().frame(width: 142)
                Picker("状态", selection: $status) {
                    Text("全部结果").tag(""); Text("成功").tag("success"); Text("未成功").tag("unsuccessful"); Text("进行中").tag("active")
                }.labelsHidden().frame(width: 132)
                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("搜索模型、来源或错误代码", text: $search).textFieldStyle(.plain)
                    if !search.isEmpty {
                        Button { search = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                            .buttonStyle(.plain).accessibilityLabel("清除搜索")
                    }
                }.padding(.horizontal, 10).padding(.vertical, 7)
                    .background(AppPalette.card, in: RoundedRectangle(cornerRadius: 7))
                    .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(AppPalette.border).allowsHitTesting(false))
            }.controlSize(.regular).font(.system(size: 13))
            if let loadError, showsCurrentResults { InfoNote(text: loadError, color: .orange) }
            HStack {
                Text("请求记录").font(.system(size: 13, weight: .semibold))
                Spacer()
                if showsCurrentResults {
                    Text("\(total.formatted()) 次请求" + (activeCount > 0 ? " · \(activeCount.formatted()) 次进行中" : ""))
                        .font(AppTypography.caption).foregroundStyle(.secondary).monospacedDigit()
                }
            }
            ScrollView {
                LazyVStack(spacing: 0) {
                    if !showsCurrentResults {
                        if selection.query == nil {
                            EmptyState("日期范围无效", subtitle: "请在筛选栏调整起止日期。", symbol: "calendar").frame(height: 230)
                        } else if let loadError {
                            EmptyState("暂时无法读取请求", subtitle: loadError, symbol: "exclamationmark.circle").frame(height: 230)
                        } else if !model.running {
                            EmptyState("网关未连接", subtitle: "启动网关后即可读取请求记录。", symbol: "network.slash").frame(height: 230)
                        } else {
                            ProgressView("正在载入请求…").controlSize(.small).frame(maxWidth: .infinity, minHeight: 230)
                        }
                    } else if records.isEmpty {
                        EmptyState("没有匹配的请求", subtitle: "试试更长的时间范围或清除筛选。", symbol: "list.bullet.rectangle").frame(height: 230)
                    }
                    ForEach(showsCurrentResults ? records : []) { item in
                        Button { selected = item } label: { requestRow(item) }.buttonStyle(.plain)
                        if item.id != records.last?.id { Divider().padding(.horizontal, 15) }
                    }
                    if showsCurrentResults && nextCursor != nil {
                        Divider()
                        Button(loadingMore ? "正在载入…" : "载入更多请求") { Task { await loadMore() } }
                            .buttonStyle(.link).padding(16).disabled(loadingMore)
                    }
                }.cardSurface().padding(.bottom, 4)
            }
        }
        .sheet(item: $selected) { item in UsageRequestDetailView(item: item) }
        .task(id: UsageRefreshKey(selection: filter, active: model.settingsWindowActive)) {
            guard model.settingsWindowActive else { return }
            let requested = filter
            guard model.cachedUsageAnalytics(requested.query) == nil else { return }
            do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
            guard !Task.isCancelled, let query = requested.query else { return }
            _ = try? await model.usageAnalytics(query)
        }
        .task(id: UsageRefreshKey(selection: filter, active: model.settingsWindowActive)) {
            guard model.settingsWindowActive else { return }
            let requested = filter
            loadError = nil
            guard requested.query != nil else { return }
            do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
            while !Task.isCancelled {
                guard let query = requested.query else { break }
                do {
                    let result = try await model.usageRequests(query, status: requested.status, search: requested.search)
                    guard !Task.isCancelled else { break }
                    let incoming = result["records"].array.enumerated().map { UsageItem($0.element, index: $0.offset) }
                    if !loadingMore || loadedFilter != requested {
                        let preserving = loadedFilter == requested && loadedPages > 1
                        let previous = preserving ? records.filter { (query.from..<query.to).contains($0.value["started_at"].number ?? 0) } : []
                        let merged = usageMergeFirstPage(previous: previous, incoming: incoming, previousCursor: nextCursor, incomingCursor: result["next_cursor"].number, preserving: preserving, capacity: loadedPages * 50)
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
        guard let cursor = nextCursor, let query = loadedQuery, loadedFilter == requested, !loadingMore else { return }
        loadingMore = true; defer { loadingMore = false }
        do {
            let result = try await model.usageRequests(query, status: requested.status, search: requested.search, before: cursor)
            guard requested == filter, loadedQuery == query else { return }
            let ids = Set(records.map(\.id))
            records += result["records"].array.enumerated().map { UsageItem($0.element, index: $0.offset) }.filter { !ids.contains($0.id) }
            loadedPages += 1
            nextCursor = result["next_cursor"].number; loadError = nil
        } catch { if requested == filter { loadError = error.localizedDescription } }
    }
}
