import SwiftUI

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    @State private var days = 1
    @State private var sourceID = ""
    @State private var modelID = ""
    @State private var groupBy = "model"
    @State private var sortBy = "cost"
    @State private var requestLimit = 20
    @State private var expanded = Set<String>()
    @State private var showingPrices = false
    private var groups: [JSONValue] { model.usage["groups"].array }
    private var selected: [JSONValue] { groups.filter { (sourceID.isEmpty || $0["source_id"].string == sourceID) && (modelID.isEmpty || $0["model"].string == modelID) } }
    private var totals: UsageTotals { UsageTotals(rows: selected) }
    private var sources: [UsageComparison] { usageComparisons(groups, key: "source_id", sort: "requests") }
    private var models: [UsageComparison] { usageComparisons(groups, key: "model", sort: "requests") }
    private var comparisons: [UsageComparison] { usageComparisons(selected, key: groupBy, sort: sortBy) }
    private var requests: [UsageItem] {
        model.usage["recent"].array.filter { (sourceID.isEmpty || $0["source_id"].string == sourceID) && (modelID.isEmpty || $0["model"].string == modelID) }.enumerated().map { UsageItem($0.element, index: $0.offset) }
    }
    private var costParts: [(String, String, Color)] { [("input_usd","非缓存输入",.blue),("cached_input_usd","缓存读取",.teal),("cache_write_usd","缓存写入",.indigo),("output_usd","输出",.orange)] }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            PageHeading("用量与费用") {
                HStack(spacing: 10) {
                    Button("API 价格表") { showingPrices = true }.controlSize(.small)
                    Picker("时间范围", selection: $days) { Text("近 24 小时").tag(1); Text("近 7 天").tag(7); Text("近 30 天").tag(30) }.labelsHidden().frame(width: 112)
                    Button { refresh() } label: { Image(systemName: "arrow.clockwise") }.disabled(model.usageRefreshing || !model.running).help("刷新用量")
                }
            }
            filters
            if let error = model.usageError { InfoNote(text: error, color: .orange) }
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    if groups.isEmpty && requests.isEmpty { EmptyState("暂无用量", subtitle: "所选时间范围内没有请求记录。", symbol: "chart.bar.xaxis") }
                    else { overview; costBreakdown; comparison; recentRequests }
                }.padding(.bottom, 4)
            }
        }.settingsPage()
            .sheet(isPresented: $showingPrices) { UsagePriceSheet(catalog: model.usage["pricing"]) }
            .task(id: "\(days):\(sourceID):\(modelID)") {
                requestLimit = 20
                while !Task.isCancelled {
                    await model.loadUsage(days: days, sourceID: sourceID.isEmpty ? nil : sourceID, modelID: modelID.isEmpty ? nil : modelID)
                    do { try await Task.sleep(for: .seconds(1)) } catch { break }
                }
            }
    }
    private func refresh() { model.refreshUsage(days: days, sourceID: sourceID.isEmpty ? nil : sourceID, modelID: modelID.isEmpty ? nil : modelID) }
    private var filters: some View {
        HStack(spacing: 10) {
            Picker("来源", selection: $sourceID) { Text("全部来源").tag(""); ForEach(sources) { Text($0.name).tag($0.id) } }.labelsHidden().frame(maxWidth: 170)
            Picker("模型", selection: $modelID) { Text("全部模型").tag(""); ForEach(models) { Text($0.name).tag($0.id) } }.labelsHidden().frame(maxWidth: 210)
            if !sourceID.isEmpty || !modelID.isEmpty { Button("清除") { sourceID = ""; modelID = "" }.buttonStyle(.plain).foregroundStyle(Color.accentColor) }
            Spacer(minLength: 0)
            HStack(spacing: 5) {
                Circle().fill(model.running ? Color.green : .orange).frame(width: 5, height: 5)
                Text(model.running ? "实时更新" : "未连接")
                if let updated = model.usageUpdatedAt { Text(updated.formatted(date: .omitted, time: .standard)).monospacedDigit() }
            }.foregroundStyle(.secondary)
        }.font(.system(size: 10))
    }
    private var overview: some View {
        VStack(alignment: .leading, spacing: 17) {
            HStack(alignment: .top, spacing: 16) {
                VStack(alignment: .leading, spacing: 7) {
                    Text("参考费用 · USD").font(.system(size: 11)).foregroundStyle(.secondary)
                    Text(usageMoney(totals.cost)).font(.system(size: 30, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.6)
                    Text(totals.costCoverage).font(.system(size: 10)).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading)
                metric("总 Token", usageCount(totals.total, estimated: totals.estimated), "输入 + 输出")
                metric("请求", usageCount(totals.sum("requests")), "失败 \(Int(totals.sum("failures"))) · 进行中 \(Int(totals.sum("active_requests")))")
                metric("平均输出 TPS", usageRate(totals.tps, estimated: totals.estimated), "含首 Token 等待").help("完成请求的输出 Token 总量 ÷ 请求总耗时；包含首 Token 等待。不计输入 Token。")
            }
            Divider()
            HStack(spacing: 12) {
                smallMetric("平均每次费用", usageMoney(totals.averageCost))
                smallMetric("缓存节省", usageMoney(totals.savings))
                smallMetric("成功率", totals.successRate.map { String(format: "%.1f%%", $0) } ?? "—")
                smallMetric("平均首 Token", usageSeconds(totals.firstToken))
                smallMetric("平均耗时", usageSeconds(totals.averageLatency))
            }
            Text("按官方 API 价格折算，非实际账单。" + (totals.sum("unpriced_requests") > 0 ? " \(Int(totals.sum("unpriced_requests"))) 次价格未收录。" : "") + (totals.sum("partial_cost_requests") > 0 ? " \(Int(totals.sum("partial_cost_requests"))) 次仅部分计价。" : "") + (totals.sum("unobserved_requests") > 0 ? " \(Int(totals.sum("unobserved_requests"))) 次无有效计数。" : ""))
                .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                .help("不代表 ChatGPT 订阅扣费或第三方实际账单。不含工具调用、图片、音频、税费及地区附加费。未知价格和缺失计数单独标明。")
        }.padding(18).cardSurface()
    }
    private var costBreakdown: some View {
        VStack(alignment: .leading, spacing: 13) {
            HStack {
                Text("费用构成").font(.system(size: 12, weight: .semibold)); Spacer()
                Text("缓存命中率 \(totals.cacheRate.map { String(format: "%.1f%%", $0) } ?? "—")").font(.system(size: 10)).foregroundStyle(.secondary)
            }
            GeometryReader { proxy in
                HStack(spacing: 0) {
                    ForEach(Array(costParts.enumerated()), id: \.offset) { _, part in
                        Rectangle().fill(part.2.opacity(0.8)).frame(width: totals.cost.map { $0 > 0 ? max(0, proxy.size.width * totals.sum(part.0) / $0) : 0 } ?? 0)
                    }
                }.background(AppPalette.well).clipShape(Capsule())
            }.frame(height: 5)
            HStack(alignment: .top, spacing: 10) {
                ForEach(Array(costParts.enumerated()), id: \.offset) { _, part in
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 5) { Circle().fill(part.2).frame(width: 5, height: 5); Text(part.1).foregroundStyle(.secondary) }
                        Text(usageMoney(totals.cost == nil ? nil : totals.sum(part.0))).fontWeight(.medium).monospacedDigit()
                    }.font(.system(size: 11)).frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }.padding(16).cardSurface()
    }
    private var comparison: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Picker("统计维度", selection: $groupBy) { Text("按模型").tag("model"); Text("按来源").tag("source_id") }.pickerStyle(.segmented).labelsHidden().frame(width: 185)
                Spacer()
                Picker("排序", selection: $sortBy) { Text("费用排序").tag("cost"); Text("Token 排序").tag("tokens"); Text("请求数排序").tag("requests") }.labelsHidden().frame(width: 116)
            }
            VStack(spacing: 0) {
                HStack(spacing: 12) {
                    Text(groupBy == "model" ? "模型" : "来源").frame(maxWidth: .infinity, alignment: .leading)
                    Text("Token / 请求").frame(width: 104, alignment: .trailing)
                    Text("速度 / 缓存").frame(width: 100, alignment: .trailing)
                    Text("参考费用").frame(width: 102, alignment: .trailing)
                }.font(.system(size: 10)).foregroundStyle(.secondary).padding(14)
                Divider()
                ForEach(comparisons) { row in
                    Button { if groupBy == "model" { modelID = modelID == row.id ? "" : row.id } else { sourceID = sourceID == row.id ? "" : row.id } } label: { comparisonRow(row) }.buttonStyle(.plain)
                    if row.id != comparisons.last?.id { Divider().padding(.horizontal, 14) }
                }
            }.cardSurface()
        }
    }
    private func comparisonRow(_ row: UsageComparison) -> some View {
        let data = row.totals
        return HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                Text(row.name).font(.system(size: 12, weight: .medium)).lineLimit(1).help(row.name)
                Text("输入 \(usageCount(data.input)) · 输出 \(usageCount(data.output))").font(.system(size: 10)).foregroundStyle(.secondary)
                if data.sum("unpriced_requests") > 0 { Text("\(Int(data.sum("unpriced_requests"))) 次未计价").font(.system(size: 10)).foregroundStyle(.orange) }
            }.frame(maxWidth: .infinity, alignment: .leading)
            VStack(alignment: .trailing, spacing: 6) { Text(usageCount(data.total, estimated: data.estimated)); Text("\(usageCount(data.sum("requests"))) 次").foregroundStyle(.secondary) }.frame(width: 104, alignment: .trailing)
            VStack(alignment: .trailing, spacing: 6) {
                Text(usageRate(data.tps, estimated: data.estimated) + " TPS")
                Text(data.cacheRate.map { String(format: "%.1f%% 命中", $0) } ?? "缓存未报告").foregroundStyle(.secondary)
            }.frame(width: 100, alignment: .trailing)
            VStack(alignment: .trailing, spacing: 6) {
                Text(usageMoney(data.cost)).fontWeight(.medium)
                Text("均次 \(usageMoney(data.averageCost))").font(.system(size: 9)).foregroundStyle(.secondary)
            }.frame(width: 102, alignment: .trailing)
        }.font(.system(size: 11)).monospacedDigit().padding(14).contentShape(Rectangle())
    }
    private var recentRequests: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack { Text("请求明细").font(.system(size: 12, weight: .semibold)); Spacer(); Text("最近 \(requests.count) 次上游尝试").font(.system(size: 10)).foregroundStyle(.secondary) }.padding(.horizontal, 3)
            VStack(spacing: 0) {
                if requests.isEmpty { Text("当前筛选下暂无请求").font(.system(size: 12)).foregroundStyle(.secondary).padding(18) }
                ForEach(requests.prefix(requestLimit)) { item in
                    requestRow(item)
                    if item.id != requests.prefix(requestLimit).last?.id { Divider().padding(.horizontal, 14) }
                }
                if requests.count > requestLimit {
                    Divider()
                    Button("显示更多（\(min(requestLimit, requests.count)) / \(requests.count)）") { requestLimit += 20 }.buttonStyle(.plain).foregroundStyle(Color.accentColor).font(.system(size: 11)).padding(13)
                }
            }.cardSurface()
        }
    }
    private func requestRow(_ request: UsageItem) -> some View {
        let item = request.value, cost = request.value["cost"], details = request.value["token_details"]
        return VStack(alignment: .leading, spacing: 10) {
            Button { if expanded.contains(request.id) { expanded.remove(request.id) } else { expanded.insert(request.id) } } label: {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(spacing: 8) {
                        Text(item["model"].string ?? "未知模型").font(.system(size: 12, weight: .medium)).lineLimit(1)
                        Text(item["source_name"].string ?? "未知来源").font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1)
                        Spacer(minLength: 0)
                        Text(usageMoney(cost["total_usd"].number)).font(.system(size: 13, weight: .semibold, design: .rounded)).monospacedDigit()
                        StatusPill(text: requestStatus(item["status"].string), color: item["active"].bool == true ? .accentColor : item["status"].string == "success" ? .green : .orange)
                        Image(systemName: expanded.contains(request.id) ? "chevron.up" : "chevron.down").font(.system(size: 9)).foregroundStyle(.tertiary)
                    }
                    HStack(spacing: 12) {
                        Text("入 \(usageCount(item["input_tokens"].number ?? 0, estimated: details["input_estimated"].bool == true))")
                        Text("出 \(details["output_available"].bool == true ? usageCount(item["output_tokens"].number ?? 0, estimated: details["output_estimated"].bool == true) : "—")")
                        Text("缓存 \(details["cached_input_reported"].bool == true ? usageCount(item["cached_input_tokens"].number ?? 0) : "—")")
                        Spacer(minLength: 0)
                        Text(usageRate(item["output_tps"].number, estimated: details["output_estimated"].bool == true) + " TPS")
                        Text(timestampLabel(item["started_at"]))
                    }.font(.system(size: 10)).foregroundStyle(.secondary).monospacedDigit()
                }.contentShape(Rectangle())
            }.buttonStyle(.plain)
            if expanded.contains(request.id) { requestDetails(item) }
        }.padding(14)
    }
    private func requestDetails(_ item: JSONValue) -> some View {
        let cost = item["cost"], details = item["token_details"]
        return VStack(alignment: .leading, spacing: 10) {
            Divider()
            HStack {
                Text(usageCostStatus(cost)).foregroundStyle(cost["total_usd"].number == nil ? Color.orange : .secondary)
                if cost["long_context_waived"].bool == true { Text("已免长上下文加价") }
                else if cost["long_context"].bool == true { Text("长上下文价格") }
                Text(cost["service_tier"].string ?? ""); Spacer()
                Text("首 Token \(usageSeconds(item["first_token_latency_ms"].number)) · 耗时 \(usageSeconds(item["latency_ms"].number))")
            }
            Text("输出速度 \(usageRate(item["output_tps"].number, estimated: details["output_estimated"].bool == true)) TPS（请求总耗时） · 传输阶段 \(usageRate(item["generation_tps"].number)) TPS")
                .help("传输阶段速度从首个可见输出计时；上游缓冲后集中返回时可能很高，不等于模型真实生成速度。")
            HStack(spacing: 16) {
                Text("缓存写入 \(details["cache_write_input_tokens"].number.map { usageCount($0) } ?? "未报告")")
                Text("推理 \(details["reasoning_output_tokens"].number.map { usageCount($0) } ?? "未报告")")
                Spacer(); Text(item["complete"].bool == true ? "上游计数" : "计数未完整报告")
            }
            if cost["total_usd"].number != nil {
                Text(usageBillingBasis(cost) + " · 计价模型 " + (cost["price_model"].string ?? "—"))
                ForEach(Array(costParts.enumerated()), id: \.offset) { _, part in HStack { Text(part.1); Spacer(); Text(usageMoney(cost[part.0].number)) } }
                Text("每百万 Token：输入 \(usageMoney(cost["rates"]["input"].number, estimated: false)) · 缓存 \(usageMoney(cost["rates"]["cached_input"].number, estimated: false)) · 输出 \(usageMoney(cost["rates"]["output"].number, estimated: false))")
                Text("价格版本 \(cost["revision"].string ?? "—")").foregroundStyle(.tertiary)
                if cost["session_scope_estimate"].bool == true { Text("此模型官方按整段会话区分长上下文价格；当前按每次输入量估算。").foregroundStyle(.orange) }
            }
        }.font(.system(size: 10)).foregroundStyle(.secondary).monospacedDigit()
    }
    private func requestStatus(_ status: String?) -> String { switch status { case "connecting": return "连接中"; case "streaming": return "接收中"; case "success": return "完成"; case "failed": return "失败"; case "interrupted": return "已中断"; default: return "未知" } }
    private func metric(_ title: String, _ value: String, _ detail: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 22, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.6)
            Text(detail).font(.system(size: 10)).foregroundStyle(.tertiary)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
    private func smallMetric(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 6) { Text(title).font(.system(size: 10)).foregroundStyle(.secondary); Text(value).font(.system(size: 12, weight: .medium)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.7) }.frame(maxWidth: .infinity, alignment: .leading)
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
