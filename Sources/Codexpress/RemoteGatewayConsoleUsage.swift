import SwiftUI

struct RemoteConsoleUsageFilter: Equatable {
    var selection = UsageSelection()
    var device = ""
    var ingress = ""
    var status = ""
    var search = ""

    func fields(_ query: UsageQuery) -> [String: JSONValue] {
        var fields = query.payload
        for (key, value) in [("remote_peer_id", device), ("ingress", ingress), ("status", status), ("search", search.trimmingCharacters(in: .whitespacesAndNewlines))] where !value.isEmpty {
            fields[key] = .string(value)
        }
        return fields
    }
}

private struct RemoteUsageLoad: Equatable {
    let filter: RemoteConsoleUsageFilter
    let fetchedAt: Date?
}

private struct RemoteConsoleDetailSession: Identifiable {
    let id: String
    let target: RemoteConsoleTarget
    let devices: [RemoteConsoleDevice]
}

struct RemoteConsoleUsage: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    @State private var filter = RemoteConsoleUsageFilter()
    @State private var completed: RemoteConsoleUsageFilter?
    @State private var analytics: JSONValue = .null
    @State private var options: JSONValue = .null
    @State private var devices: [RemoteConsoleDevice] = []
    @State private var records: [UsageItem] = []
    @State private var nextCursor: Double?
    @State private var loadedQuery: UsageQuery?
    @State private var total = 0.0
    @State private var failed = false
    @State private var more = false
    @State private var billing: JSONValue = .null
    @State private var billingBusy = false
    @State private var billingFailed = false
    @State private var detail: RemoteConsoleDetailSession?
    @State private var showRequests = false
    @State private var generation = UUID()

    var body: some View {
        VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
            UsageFilters(selection: $filter.selection, options: options)
            filters
            Picker("用量视图", selection: $showRequests) {
                Text("统计与账单").tag(false); Text("请求记录").tag(true)
            }.pickerStyle(.segmented).labelsHidden().frame(width: 260)
            if filter.selection.query == nil { InfoNote(text: "请选择 1–366 天的有效日期范围。", color: .orange) }
            else if failed { InfoNote(text: "无法读取所选范围的服务器用量，请刷新重试。", color: .orange) }
            else if completed != filter { ProgressView("正在读取用量…") }
            else if showRequests { logRows }
            else { metrics; billingCard }
        }
        .task(id: RemoteUsageLoad(filter: filter, fetchedAt: management.fetchedAt)) { await load() }
        .task(id: management.fetchedAt) {
            do {
                let listed = try await target.call("device.list")
                devices = try remoteConsoleDecode([RemoteConsoleDevice].self, listed)
            } catch { if !Task.isCancelled { devices = [] } }
            await loadBilling()
        }
        .sheet(item: $detail) { RemoteConsoleRequestDetail(session: $0) }
    }

    private var filters: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Picker("设备", selection: $filter.device) {
                    Text("全部设备").tag("")
                    ForEach(devices) { Text($0.name).tag($0.id) }
                    let known = Set(devices.map(\.id))
                    let observed = Set(records.compactMap { $0.value["token_details"]["remote_peer_id"].string }).union([filter.device])
                    ForEach(observed.subtracting(known).filter { !$0.isEmpty }.sorted(), id: \.self) { Text($0).tag($0) }
                }
                Picker("入口", selection: $filter.ingress) {
                    Text("全部入口").tag("")
                    ForEach(["native", "ssh", "omp", "chatbot", "image", "unknown"], id: \.self) { Text(usageIngressName($0)).tag($0) }
                }
                Picker("结果", selection: $filter.status) {
                    Text("全部").tag(""); Text("成功").tag("success"); Text("未成功").tag("unsuccessful"); Text("进行中").tag("active")
                }
            }
            TextField("搜索请求 ID、模型或来源", text: $filter.search).settingsField()
        }.padding(14).cardSurface()
    }

    @ViewBuilder private var metrics: some View {
        let groups = analytics["groups"].array
        let totals = UsageTotals(rows: groups)
        SettingsCard("用量与性能", symbol: "chart.bar") {
            LabeledContent("费用 · USD", value: usageMoney(totals.cost, estimated: totals.costEstimated))
            LabeledContent("输入 / 输出 Token", value: usageCount(totals.input, estimated: totals.estimated) + " / " + usageCount(totals.output, estimated: totals.estimated))
            LabeledContent("请求 / 失败", value: usageCount(totals.sum("requests")) + " / " + usageCount(totals.sum("failures")))
            LabeledContent("成功率", value: totals.successRate.map { String(format: "%.1f%%", $0) } ?? "—")
            LabeledContent("输出速度 · TPS", value: usageRate(totals.tps))
            LabeledContent("首 Token · P50 / P95", value: usageSeconds(analytics["performance"]["p50_first_token_ms"].number) + " / " + usageSeconds(analytics["performance"]["p95_first_token_ms"].number))
            LabeledContent("请求耗时 · P50 / P95", value: usageSeconds(analytics["performance"]["p50_latency_ms"].number) + " / " + usageSeconds(analytics["performance"]["p95_latency_ms"].number))
        }
        UsageDistributionChart(groups: groups).cardSurface()
        UsageTrendChart(days: analytics["daily"].array.map(UsageDaily.init), intraday: loadedQuery?.isIntraday == true)
    }

    private var billingCard: some View {
        SettingsCard("上游账单核对", symbol: "creditcard") {
            if billingFailed { InfoNote(text: "无法确认账单核对状态，请刷新核实。", color: .orange) }
            if let running = billing["running"].bool { LabeledContent("状态", value: running ? "正在核对" : "空闲") }
            if let pages = billing["pages"].number { LabeledContent("已检查页数", value: usageCount(pages)) }
            if let updated = billing["updated"].number { LabeledContent("已更新记录", value: usageCount(updated)) }
            if let errors = billing["errors"].number { LabeledContent("失败数", value: usageCount(errors)) }
            Button(filter.selection.sourceID.isEmpty ? "核对所有来源账单" : "核对所选来源账单") {
                let source = filter.selection.sourceID
                billingBusy = true
                Task { defer { billingBusy = false }; do {
                    _ = try await target.call("usage.reconcile", source.isEmpty ? [:] : ["source_id": .string(source)]); await loadBilling()
                } catch { billingFailed = true } }
            }.disabled(billingBusy || billing["running"].bool != false)
        }
    }

    private var logRows: some View {
        SettingsCard("请求记录 · \(usageCount(total))", symbol: "list.bullet.rectangle") {
            if records.isEmpty { Text("没有匹配的请求").foregroundStyle(.secondary) }
            ForEach(records) { item in
                let row = item.value
                HStack {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(row["model"].string ?? "未记录模型")
                        Text("\(row["source_name"].string ?? "已移除来源") · \(usageIngressName(row["ingress"].string)) · \(timestampLabel(row["started_at"]))")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer(); Text(usageRequestStatus(row)); Text(usageCostMoney(row["cost"]))
                    Button("详情…") { detail = .init(id: item.id, target: target, devices: devices) }
                }
            }
            if nextCursor != nil { Button("载入更多") { Task { await loadMore() } }.disabled(more) }
        }
    }

    private func load() async {
        let requested = filter, tag = UUID()
        generation = tag; failed = false
        guard let query = requested.selection.query else { return }
        do {
            try await Task.sleep(for: .milliseconds(220))
            let args = requested.fields(query)
            let summary = try await target.call("usage.analytics", args)
            let page = try await target.call("usage.requests", args)
            guard filter == requested, generation == tag else { return }
            let incoming = page["records"].array.compactMap { $0["attempt_id"].string == nil ? nil : UsageItem($0) }
            let same = completed == requested && records.count > 50
            let merged = usageMergeFirstPage(previous: same ? records : [], incoming: incoming, previousCursor: nextCursor, incomingCursor: page["next_cursor"].number, preserving: same, capacity: max(50, records.count))
            records = merged.records; nextCursor = merged.nextCursor; total = page["total"].number ?? 0
            analytics = summary; options = summary["options"]; loadedQuery = query; completed = requested
        } catch { if generation == tag && filter == requested && !Task.isCancelled { failed = true } }
    }
    private func loadMore() async {
        guard !more, completed == filter, let cursor = nextCursor, let query = loadedQuery else { return }
        let requested = filter, tag = generation
        var fields = requested.fields(query); fields["before"] = .number(cursor)
        more = true; defer { more = false }
        do {
            let page = try await target.call("usage.requests", fields)
            guard generation == tag, filter == requested else { return }
            var seen = Set(records.map(\.id))
            records += page["records"].array.compactMap { value in
                guard let id = value["attempt_id"].string, seen.insert(id).inserted else { return nil }; return UsageItem(value)
            }
            nextCursor = page["next_cursor"].number
        } catch { if generation == tag, filter == requested { failed = true } }
    }
    private func loadBilling() async {
        do { billing = try await target.call("usage.reconcile.status"); billingFailed = false } catch { if !Task.isCancelled { billingFailed = true } }
    }
}

private struct RemoteConsoleRequestDetail: View {
    @Environment(\.dismiss) private var dismiss
    let session: RemoteConsoleDetailSession
    @State private var record: JSONValue = .null
    @State private var failed = false
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("请求详情").font(.title2); Text(session.target.endpoint).foregroundStyle(.secondary)
            if failed { InfoNote(text: "无法读取此请求的详情。", color: .orange) }
            else if record == .null { ProgressView() }
            else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 12) {
                        SettingsCard("执行信息", symbol: "list.bullet") {
                            LabeledContent("请求 ID", value: record["request_id"].string ?? "—").textSelection(.enabled)
                            LabeledContent("尝试 ID", value: session.id).textSelection(.enabled)
                            LabeledContent("模型", value: record["model"].string ?? "—")
                            LabeledContent("上游", value: record["source_name"].string ?? "—")
                            LabeledContent("入口", value: usageIngressName(record["ingress"].string))
                            if let peer = record["token_details"]["remote_peer_id"].string {
                                LabeledContent("设备", value: session.devices.first { $0.id == peer }.map { "\($0.name) · \(peer)" } ?? peer).textSelection(.enabled)
                            }
                            LabeledContent("时间", value: timestampLabel(record["started_at"]))
                            LabeledContent("结果", value: usageRequestStatus(record)); LabeledContent("耗时", value: usageSeconds(record["latency_ms"].number))
                            if let status = record["http_status"].number { LabeledContent("HTTP 状态", value: String(format: "%.0f", status)) }
                        }
                        SettingsCard("Token 与计价", symbol: "dollarsign.circle") {
                            LabeledContent("输入 / 输出", value: (record["input_tokens"].number.map { usageCount($0) } ?? "—") + " / " + (record["output_tokens"].number.map { usageCount($0) } ?? "—"))
                            LabeledContent("缓存读取", value: record["cached_input_tokens"].number.map { usageCount($0) } ?? "—")
                            let cost = record["cost"]
                            LabeledContent("费用 · USD", value: usageCostMoney(cost))
                            if cost != .null { LabeledContent("计价状态", value: usageCostStatus(cost)); LabeledContent("计价依据", value: usageBillingBasis(cost)) }
                        }
                    }
                }
            }
            HStack { Spacer(); Button("关闭") { dismiss() }.keyboardShortcut(.cancelAction) }
        }.padding(22).frame(width: 560, height: 590)
        .task {
            do {
                let detail = try await session.target.call("usage.detail", ["attempt_id": .string(session.id)])
                guard detail["record"]["attempt_id"].string == session.id else { throw RemoteConsoleError.invalid }
                record = detail["record"]
            } catch { if !Task.isCancelled { failed = true } }
        }
    }
}
