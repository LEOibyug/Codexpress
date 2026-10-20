import SwiftUI

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    @State private var days = 1
    @State private var sourceID = ""
    @State private var requestLimit = 20
    private let estimateHelp = "字符估算：中文、日文、韩文等约 1 字符/Token，其他文字约 4 字符/Token。不计图片、音频和加密内容；返回上游计数后使用上游值。"
    private let tpsHelp = "输出 Token ÷ 首个流式文字/工具参数至结束的秒数。平均 TPS 按完成请求的输出量和生成时长加权；一次性 JSON、缺少流式计时的记录不参与。"
    private var rows: [JSONValue] { model.usage["rows"].array }
    private var selectedRows: [JSONValue] { sourceID.isEmpty ? rows : rows.filter { $0["source_id"].string == sourceID } }
    private var totals: UsageTotals { UsageTotals(rows: selectedRows) }
    private var requests: [UsageItem] { model.usage["recent"].array.filter { sourceID.isEmpty || $0["source_id"].string == sourceID }.enumerated().map { UsageItem($0.element, index: $0.offset) } }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PageHeading("用量") {
                HStack(spacing: 8) {
                    Picker("来源", selection: $sourceID) {
                        Text("全部来源").tag("")
                        ForEach(rows.enumerated().map { UsageItem($0.element, index: $0.offset, key: "source_id") }) { row in
                            Text(row.value["source_name"].string ?? "已移除来源").tag(row.id)
                        }
                    }.labelsHidden().frame(width: 135)
                    Picker("时间范围", selection: $days) {
                        Text("近 24 小时").tag(1); Text("近 7 天").tag(7); Text("近 30 天").tag(30)
                    }.labelsHidden().frame(width: 108)
                    Button { model.refreshUsage(days: days, sourceID: sourceID.isEmpty ? nil : sourceID) } label: { Image(systemName: "arrow.clockwise") }
                        .disabled(model.usageRefreshing || !model.running).help("刷新用量").accessibilityLabel("刷新用量")
                }
            }
            liveStatus
            if let error = model.usageError { InfoNote(text: error, color: .orange) }
            if rows.isEmpty && requests.isEmpty {
                if model.usageRefreshing && model.usage == .null { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
                else { EmptyState("暂无用量", subtitle: "所选时间范围内没有请求记录。", symbol: "chart.bar.xaxis") }
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 20) {
                        overview
                        sourceComparison
                        recentRequests
                    }.padding(.bottom, 2)
                }
            }
        }.settingsPage()
            .task(id: "\(days):\(sourceID)") {
                requestLimit = 20
                let selected = sourceID.isEmpty ? nil : sourceID
                while !Task.isCancelled {
                    await model.loadUsage(days: days, sourceID: selected)
                    do { try await Task.sleep(for: .seconds(1)) } catch { break }
                }
            }
    }

    private var liveStatus: some View {
        HStack(spacing: 6) {
            Circle().fill(model.running ? Color.green : .orange).frame(width: 5, height: 5)
            Text(model.running ? "实时更新" : "网关未连接")
            if let updated = model.usageUpdatedAt {
                Text("· \(updated.formatted(date: .omitted, time: .standard))").monospacedDigit()
            }
            Spacer()
            if totals.sum("active_requests") > 0 { Text("\(Int(totals.sum("active_requests"))) 个请求进行中") }
        }.font(.system(size: 10)).foregroundStyle(.secondary)
    }

    private var overview: some View {
        VStack(spacing: 14) {
            HStack(spacing: 0) {
                metric("总 Token", usageCount(totals.total, estimated: totals.estimated), detail: "输入 + 输出")
                metric("请求", usageCount(totals.sum("requests")), detail: "失败 \(usageCount(totals.sum("failures")))")
                metric("平均 TPS", usageRate(totals.tps, estimated: totals.estimated), detail: "输出 Token / 秒", help: tpsHelp)
                metric("成功率", totals.successRate.map { String(format: "%.1f%%", $0) } ?? "—", detail: "已结束请求")
            }
            Divider()
            HStack(spacing: 0) {
                smallMetric("输入", usageCount(totals.input, estimated: totals.estimated))
                smallMetric("输出", usageCount(totals.output, estimated: totals.estimated))
                smallMetric("平均首 Token", usageSeconds(totals.firstToken))
                smallMetric("平均耗时", usageSeconds(totals.averageLatency))
            }
            if totals.estimated {
                HStack(spacing: 5) {
                    Image(systemName: "function")
                    Text("含字符估算 \(usageCount(totals.sum("estimated_requests"))) 次")
                    Spacer()
                }.font(.system(size: 10)).foregroundStyle(.orange).help(estimateHelp)
            }
        }.padding(18).cardSurface()
    }

    private var sourceComparison: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("来源统计").font(.system(size: 12, weight: .semibold))
                Spacer()
                if !sourceID.isEmpty { Button("显示全部") { sourceID = "" }.buttonStyle(.plain).font(.system(size: 11)).foregroundStyle(Color.accentColor) }
            }.padding(.horizontal, 4)
            VStack(spacing: 0) {
                HStack {
                    Text("来源 / Token 占比").frame(maxWidth: .infinity, alignment: .leading)
                    Text("请求 / 成功率").frame(width: 92, alignment: .trailing)
                    Text("平均 TPS").frame(width: 72, alignment: .trailing)
                    Text("首 Token").frame(width: 76, alignment: .trailing)
                }.font(.system(size: 10)).foregroundStyle(.secondary).padding(.horizontal, 16).padding(.vertical, 11)
                Divider()
                ForEach(rows.enumerated().map { UsageItem($0.element, index: $0.offset, key: "source_id") }) { source in
                    sourceRow(source)
                    if source.id != rows.last?["source_id"].string { Divider().padding(.horizontal, 16) }
                }
            }.cardSurface()
        }
    }

    private func sourceRow(_ source: UsageItem) -> some View {
        let row = source.value
        let data = UsageTotals(rows: [row])
        let allTokens = UsageTotals(rows: rows).total
        let share = allTokens > 0 ? data.total / allTokens : 0
        return Button { sourceID = sourceID == source.id ? "" : source.id } label: {
            VStack(alignment: .leading, spacing: 10) {
                HStack(alignment: .top) {
                    VStack(alignment: .leading, spacing: 5) {
                        HStack(spacing: 6) {
                            Text(row["source_name"].string ?? "已移除来源").font(.system(size: 12, weight: .medium)).lineLimit(1)
                            if data.sum("active_requests") > 0 { Circle().fill(Color.accentColor).frame(width: 5, height: 5) }
                        }
                        Text(usageCount(data.total, estimated: data.estimated) + " Token · " + String(format: "%.0f%%", share * 100))
                            .font(.system(size: 10)).foregroundStyle(.secondary).monospacedDigit()
                        GeometryReader { proxy in
                            Capsule().fill(Color.accentColor.opacity(0.1))
                                .overlay(alignment: .leading) { Capsule().fill(Color.accentColor.opacity(0.65)).frame(width: max(0, proxy.size.width * share)) }
                        }.frame(maxWidth: 220).frame(height: 3).padding(.top, 2)
                    }.frame(maxWidth: .infinity, alignment: .leading)
                    VStack(alignment: .trailing, spacing: 5) {
                        Text(usageCount(data.sum("requests")))
                        Text(data.successRate.map { String(format: "%.1f%%", $0) } ?? "—").foregroundStyle(.secondary)
                    }.frame(width: 92, alignment: .trailing)
                    Text(usageRate(data.tps, estimated: data.estimated)).frame(width: 72, alignment: .trailing).help(tpsHelp)
                    Text(usageSeconds(data.firstToken)).frame(width: 76, alignment: .trailing)
                }.font(.system(size: 11)).monospacedDigit()
                HStack(spacing: 14) {
                    Text("输入 \(usageCount(data.input, estimated: data.estimated))")
                    Text("输出 \(usageCount(data.output, estimated: data.estimated))")
                    Text("缓存命中 \(data.cacheRate.map { String(format: "%.0f%%", $0) } ?? "—")")
                    Spacer(minLength: 0)
                    Text("耗时 \(usageSeconds(data.averageLatency))")
                }.font(.system(size: 10)).foregroundStyle(.secondary).monospacedDigit()
            }.padding(16).background(sourceID == source.id ? Color.accentColor.opacity(0.06) : Color.clear)
                .contentShape(Rectangle())
        }.buttonStyle(.plain)
    }

    private var recentRequests: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("请求明细").font(.system(size: 12, weight: .semibold))
                Spacer()
                Text(sourceID.isEmpty ? "全部来源" : (selectedRows.first?["source_name"].string ?? "所选来源")).font(.system(size: 10)).foregroundStyle(.secondary)
            }.padding(.horizontal, 4)
            if requests.isEmpty { Text("此来源暂无请求").font(.system(size: 12)).foregroundStyle(.secondary).padding(18).frame(maxWidth: .infinity, alignment: .leading).cardSurface() }
            else {
                LazyVStack(spacing: 0) {
                    ForEach(requests.prefix(requestLimit)) { item in
                        requestRow(item.value)
                        if item.id != requests.prefix(requestLimit).last?.id { Divider().padding(.horizontal, 16) }
                    }
                    if requests.count > requestLimit {
                        Divider()
                        Button("显示更多（\(min(requestLimit, requests.count)) / \(requests.count)）") { requestLimit += 20 }
                            .buttonStyle(.plain).foregroundStyle(Color.accentColor).font(.system(size: 11)).padding(13)
                    }
                }.cardSurface()
            }
        }
    }

    private func requestRow(_ item: JSONValue) -> some View {
        let details = item["token_details"]
        let estimated = details["input_estimated"].bool == true || details["output_estimated"].bool == true
        let active = item["active"].bool == true
        let total = (item["input_tokens"].number ?? 0) + (item["output_tokens"].number ?? 0)
        return VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text(item["source_name"].string ?? "未知来源").font(.system(size: 12, weight: .medium))
                Text(item["model"].string ?? "未知模型").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary).lineLimit(1)
                Spacer(minLength: 4)
                StatusPill(text: requestStatus(item["status"].string), color: active ? .accentColor : item["status"].string == "success" ? .green : .orange)
            }
            HStack(spacing: 16) {
                tokenMetric("输入", value: item["input_tokens"], estimated: details["input_estimated"].bool == true, available: details["legacy"].bool != true || details["input_available"].bool == true)
                tokenMetric("输出", value: item["output_tokens"], estimated: details["output_estimated"].bool == true, available: details["output_available"].bool == true)
                tokenMetric("合计", value: .number(total), estimated: estimated, available: details["output_available"].bool == true && (details["legacy"].bool != true || details["input_available"].bool == true))
                Spacer(minLength: 0)
                Text(usageRate(item["output_tps"].number, estimated: details["output_estimated"].bool == true) + " TPS")
                    .font(.system(size: 12, weight: .semibold, design: .rounded)).monospacedDigit().help(tpsHelp)
            }
            HStack(spacing: 14) {
                Text("缓存命中 \(details["cached_input_reported"].bool == true ? optionalCount(item["cached_input_tokens"]) : "未报告")")
                Text("缓存写入 \(optionalCount(details["cache_write_input_tokens"]))")
                Text("推理 \(optionalCount(details["reasoning_output_tokens"]))")
                Spacer(minLength: 0)
                Text(estimated ? "字符估算" : item["complete"].bool == true ? "上游计数" : "未完整报告")
                    .foregroundStyle(estimated ? Color.orange : .secondary).help(estimateHelp)
            }.font(.system(size: 10)).foregroundStyle(.secondary)
            HStack(spacing: 12) {
                Text(timestampLabel(item["started_at"]))
                Spacer()
                Text("首 Token \(usageSeconds(item["first_token_latency_ms"].number))")
                Text((active ? "已持续 " : "耗时 ") + usageSeconds(item["latency_ms"].number))
            }.font(.system(size: 10)).foregroundStyle(.tertiary).monospacedDigit()
        }.padding(16)
    }

    private func optionalCount(_ value: JSONValue) -> String { value.number.map { usageCount($0) } ?? "未报告" }
    private func requestStatus(_ status: String?) -> String {
        switch status { case "connecting": return "连接中"; case "streaming": return "接收中"; case "success": return "完成"; case "failed": return "失败"; case "interrupted": return "已中断"; default: return "未知" }
    }
    private func tokenMetric(_ title: String, value: JSONValue, estimated: Bool, available: Bool) -> some View {
        HStack(spacing: 4) {
            Text(title).foregroundStyle(.secondary)
            Text(available ? value.number.map { usageCount($0, estimated: estimated) } ?? "—" : "—").fontWeight(.medium).monospacedDigit()
        }.font(.system(size: 11)).help(available ? "\(title)：\(estimated ? "≈" : "")\(countLabel(value)) Token\n" + (estimated ? estimateHelp : "上游报告的 Token 计数") : "未报告")
    }
    private func metric(_ title: String, _ value: String, detail: String, help: String = "") -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 23, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.65)
            Text(detail).font(.system(size: 10)).foregroundStyle(.tertiary)
        }.frame(maxWidth: .infinity, alignment: .leading).help(help)
    }
    private func smallMetric(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 12, weight: .medium)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.7)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
