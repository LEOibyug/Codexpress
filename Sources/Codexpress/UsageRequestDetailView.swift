import SwiftUI
import AppKit

struct UsageRequestDetailView: View {
    let item: UsageItem
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var detail: JSONValue = .null
    @State private var bodyText = ""
    @State private var nextOffset: Double?
    @State private var loadError: String?
    @State private var loading = false
    @State private var copying = false
    @State private var copied = false
    @State private var selectedID: String?
    @State private var copyTask: Task<Void, Never>?
    @State private var isVisible = false
    private var attemptID: String { selectedID ?? item.id }
    private var record: JSONValue { detail["record"] == .null ? item.value : detail["record"] }
    private var diagnostic: JSONValue { detail["diagnostic"] }
    private var failed: Bool { ["failed", "interrupted"].contains(record["status"].string ?? "") }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 5) {
                    Text("请求详情").font(.system(size: 21, weight: .semibold))
                    Text(record["model"].string ?? "未知模型").font(.system(size: 12)).foregroundStyle(.secondary)
                }
                Spacer()
                Button(copied ? "已复制" : "复制错误反馈") { copyTask?.cancel(); copyTask = Task { await copyFeedback() } }.disabled(!failed || loading || copying || detail == .null)
                Button("完成") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            if let loadError { InfoNote(text: loadError, color: .orange) }
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    metadata
                    tokenAndPricing
                    if failed { errorFeedback }
                    if diagnostic != .null {
                        VStack(alignment: .leading, spacing: 13) {
                            DisclosureGroup("响应头") { codeBlock(usagePrettyJSON(diagnostic["response_headers"])) }
                            if !diagnostic["adapted_parameters"].array.isEmpty {
                                DisclosureGroup("参数调整") { codeBlock(usagePrettyJSON(diagnostic["adapted_parameters"])) }
                            }
                        }.font(.system(size: 11)).padding(16).cardSurface()
                    }
                    if !detail["related"].array.isEmpty { relatedAttempts }
                }.padding(.bottom, 4)
            }
        }.padding(24).frame(width: 740, height: 690)
        .onAppear { isVisible = true }
        .onDisappear { isVisible = false; copyTask?.cancel(); copyTask = nil }
        .task(id: attemptID) { copyTask?.cancel(); await load() }
    }
    private var metadata: some View {
        VStack(alignment: .leading, spacing: 13) {
            HStack {
                Text(record["source_name"].string ?? "已移除来源").font(.system(size: 13, weight: .semibold))
                Spacer()
                StatusPill(text: usageRequestStatus(record), color: record["status"].string == "success" ? .green : record["active"].bool == true ? .accentColor : .orange)
            }
            Grid(alignment: .leading, horizontalSpacing: 28, verticalSpacing: 10) {
                GridRow { field("时间", Date(timeIntervalSince1970: record["started_at"].number ?? 0).formatted(date: .numeric, time: .standard)); field("入口", usageIngressName(record["ingress"].string ?? diagnostic["ingress"].string)) }
                GridRow { field("HTTP 状态", (record["http_status"].number ?? diagnostic["http_status"].number).map { String(Int($0)) } ?? "—"); field("请求耗时", usageSeconds(record["latency_ms"].number)) }
                GridRow { field("输入 / 输出", usageCount(record["input_tokens"].number ?? 0) + " / " + usageCount(record["output_tokens"].number ?? 0)); field("首 Token", usageSeconds(record["first_token_latency_ms"].number)) }
                GridRow { field("参考费用", usageMoney(record["cost"]["total_usd"].number)); field("输出速度", usageRate(record["output_tps"].number) + " TPS") }
            }
            if let path = diagnostic["path"].string {
                Divider()
                Text((diagnostic["method"].string ?? "POST") + " " + path).font(.system(size: 10, design: .monospaced)).textSelection(.enabled)
            }
            if let url = diagnostic["upstream_url"].string { Text(url).font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled).fixedSize(horizontal: false, vertical: true) }
        }.padding(16).cardSurface()
    }
    private var tokenAndPricing: some View {
        let tokens = record["token_details"], cost = record["cost"]
        return DisclosureGroup("Token 与计价") {
            VStack(alignment: .leading, spacing: 11) {
                detailRow("输入 Token", usageCount(record["input_tokens"].number ?? 0, estimated: tokens["input_estimated"].bool == true) + (tokens["input_estimated"].bool == true ? " · 估算" : ""))
                detailRow("输出 Token", tokens["output_available"].bool == true ? usageCount(record["output_tokens"].number ?? 0, estimated: tokens["output_estimated"].bool == true) + (tokens["output_estimated"].bool == true ? " · 估算" : "") : "未报告")
                detailRow("缓存读取", tokens["cached_input_reported"].bool == true ? usageCount(record["cached_input_tokens"].number ?? 0) : "未报告")
                detailRow("缓存写入", tokens["cache_write_input_tokens"].number.map { usageCount($0) } ?? "未报告")
                detailRow("推理 Token", tokens["reasoning_output_tokens"].number.map { usageCount($0) } ?? "未报告")
                detailRow("上游报告总量", tokens["reported_total_tokens"].number.map { usageCount($0) } ?? "未报告")
                detailRow("计数状态", record["complete"].bool == true ? "完整" : "未完整报告")
                detailRow("传输阶段速度", usageRate(record["generation_tps"].number) + " TPS")
                    .help("从首个可见输出开始计时。上游缓冲后集中返回时，此速度可能高于实际生成速度。")
                Divider()
                if cost == .null { Text("此记录未保存计价详情。").foregroundStyle(.secondary) }
                else {
                    detailRow("计价状态", usageCostStatus(cost))
                    detailRow("计价依据", usageBillingBasis(cost))
                    detailRow("计价模型", cost["price_model"].string ?? "—")
                    if let tier = cost["service_tier"].string { detailRow("计费模式", tier) }
                    detailRow("非缓存输入", usageMoney(cost["input_usd"].number))
                    detailRow("缓存读取", usageMoney(cost["cached_input_usd"].number))
                    detailRow("缓存写入", usageMoney(cost["cache_write_usd"].number))
                    detailRow("输出", usageMoney(cost["output_usd"].number))
                    detailRow("合计", usageMoney(cost["total_usd"].number))
                    detailRow("缓存节省", usageMoney(cost["cache_savings_usd"].number))
                    if cost["rates"] != .null {
                        Text("每百万 Token").font(.system(size: 10)).foregroundStyle(.secondary).padding(.top, 3)
                        detailRow("输入 / 缓存 / 输出", usageMoney(cost["rates"]["input"].number, estimated: false) + " / " + usageMoney(cost["rates"]["cached_input"].number, estimated: false) + " / " + usageMoney(cost["rates"]["output"].number, estimated: false))
                    }
                    if cost["long_context_waived"].bool == true { detailRow("长上下文", "已免加价") }
                    else if cost["long_context"].bool == true { detailRow("长上下文", "已按长上下文价格折算") }
                    if let revision = cost["revision"].string { detailRow("价格版本", revision) }
                    if cost["session_scope_estimate"].bool == true { InfoNote(text: "此模型按整段会话区分长上下文价格；此处按本次输入量估算。", color: .orange) }
                }
            }.padding(.top, 13)
        }.font(.system(size: 11)).padding(16).cardSurface()
    }
    private func detailRow(_ title: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 18) {
            Text(title).foregroundStyle(.secondary)
            Spacer(minLength: 8)
            Text(value).monospacedDigit().multilineTextAlignment(.trailing).textSelection(.enabled)
        }
    }
    private func field(_ name: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(name).font(.system(size: 10)).foregroundStyle(.secondary).frame(width: 70, alignment: .leading)
            Text(value).font(.system(size: 11)).monospacedDigit().textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
        }
    }
    private var errorFeedback: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack { Text("错误反馈").font(.system(size: 13, weight: .semibold)); Spacer(); if loading { ProgressView().controlSize(.small) } }
            if diagnostic == .null {
                Text(loading ? "正在读取…" : "此记录未保存错误详情。较早的请求仅保留计数与性能数据。").font(.system(size: 11)).foregroundStyle(.secondary)
            } else {
                if let message = diagnostic["error_message"].string, !message.isEmpty { Text(message).font(.system(size: 12)).textSelection(.enabled).fixedSize(horizontal: false, vertical: true) }
                HStack(spacing: 14) {
                    if let kind = diagnostic["error_kind"].string { Text(kind) }
                    if let code = diagnostic["error_code"].string { Text("代码 " + code) }
                    if let param = diagnostic["error_param"].string { Text("参数 " + param) }
                }.font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                if let error = diagnostic["body_decode_error"].string { InfoNote(text: error, color: .orange) }
                else if diagnostic["body_evicted"].bool == true {
                    InfoNote(text: "错误正文已过保留期限或已清理。请求状态与性能记录仍可查看。")
                } else if !bodyText.isEmpty {
                    ScrollView { codeBlock(bodyText) }
                        .frame(height: min(280, max(70, CGFloat(bodyText.split(separator: "\n", omittingEmptySubsequences: false).count) * 15 + 24)))
                }
                else if !loading { Text("上游未返回错误正文。").font(.system(size: 11)).foregroundStyle(.secondary) }
                if diagnostic["body_truncated"].bool == true { InfoNote(text: "上游错误正文超过保存上限，已保留前 " + byteCount(diagnostic["body_saved_bytes"].number) + "。", color: .orange) }
                if diagnostic["body_complete"].bool == false && diagnostic["body_truncated"].bool != true && diagnostic["body_evicted"].bool != true { InfoNote(text: "连接结束前未收到完整错误正文。", color: .orange) }
                if nextOffset != nil {
                    HStack { Text("已读取 " + byteCount(Double(bodyText.utf8.count)) + " / " + byteCount(diagnostic["body_saved_bytes"].number)).foregroundStyle(.secondary); Spacer(); Button("读取更多") { Task { await loadMore() } }.disabled(loading || copying) }.font(.system(size: 10))
                }
            }
        }.padding(16).cardSurface()
    }
    private var relatedAttempts: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("相关尝试").font(.system(size: 13, weight: .semibold))
            ForEach(Array(detail["related"].array.enumerated()), id: \.offset) { _, related in
                if let id = related["attempt_id"].string {
                    Button { selectedID = id } label: {
                        HStack { Text(related["source_name"].string ?? "已移除来源"); Text(related["model"].string ?? "").foregroundStyle(.secondary); Spacer(); Text(usageRequestStatus(related)); Image(systemName: "chevron.right").foregroundStyle(.tertiary) }.font(.system(size: 11)).contentShape(Rectangle())
                    }.buttonStyle(.plain)
                }
            }
        }.padding(16).cardSurface()
    }
    private func codeBlock(_ text: String) -> some View {
        Text(text).font(.system(size: 10, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding(12).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 7)).fixedSize(horizontal: false, vertical: true)
    }
    private func byteCount(_ value: Double?) -> String { value.map { ByteCountFormatter.string(fromByteCount: Int64($0), countStyle: .file) } ?? "—" }
    private func load() async {
        let id = attemptID
        loading = true; copied = false; bodyText = ""; nextOffset = nil; detail = .null; loadError = nil
        defer { if id == attemptID { loading = false } }
        do {
            let result = try await model.usageDetail(attemptID: id)
            guard !Task.isCancelled, id == attemptID else { return }
            detail = result; bodyText = result["diagnostic"]["body_text"].string ?? ""; nextOffset = result["diagnostic"]["body_next_offset"].number
        } catch { if !Task.isCancelled, id == attemptID { loadError = error.localizedDescription } }
    }
    @discardableResult private func loadMore() async -> Bool {
        guard !Task.isCancelled, isVisible else { return false }
        guard let offset = nextOffset else { return true }
        let id = attemptID
        loading = true; defer { if id == attemptID { loading = false } }
        do {
            let result = try await model.usageDetail(attemptID: id, bodyOffset: offset)
            guard !Task.isCancelled, id == attemptID else { return false }
            let next = result["diagnostic"]["body_next_offset"].number
            guard next == nil || next! > offset else { loadError = "读取错误正文失败，请关闭详情后重试。"; return false }
            bodyText += result["diagnostic"]["body_text"].string ?? ""; nextOffset = next
            return true
        } catch { if id == attemptID { loadError = error.localizedDescription }; return false }
    }
    private func copyFeedback() async {
        let id = attemptID
        guard !Task.isCancelled, isVisible else { return }
        copying = true; defer { copying = false }
        while nextOffset != nil {
            guard !Task.isCancelled, isVisible, id == attemptID else { return }
            if !(await loadMore()) { return }
        }
        guard !Task.isCancelled, isVisible, id == attemptID, detail != .null else { return }
        var values = ["Codexpress 错误反馈", "时间：" + Date(timeIntervalSince1970: record["started_at"].number ?? 0).formatted(date: .numeric, time: .standard), "来源：" + (record["source_name"].string ?? "—"), "模型：" + (record["model"].string ?? "—"), "状态：" + usageRequestStatus(record)]
        if diagnostic != .null {
            values += ["HTTP：" + (diagnostic["http_status"].number.map { String(Int($0)) } ?? "—"), "入口：" + usageIngressName(diagnostic["ingress"].string), "上游：" + (diagnostic["upstream_url"].string ?? "—"), "错误：" + (diagnostic["error_message"].string ?? "—"), "代码：" + (diagnostic["error_code"].string ?? "—"), "响应头：\n" + usagePrettyJSON(diagnostic["response_headers"]), "参数调整：\n" + usagePrettyJSON(diagnostic["adapted_parameters"]), "错误正文：\n" + bodyText]
            if diagnostic["body_truncated"].bool == true { values.append("正文超过保存上限，已截断。") }
            if diagnostic["body_evicted"].bool == true { values.append("错误正文已清理。") }
        } else { values.append("此记录未保存错误详情。") }
        if let error = diagnostic["body_decode_error"].string { values.append(error) }
        guard !Task.isCancelled, isVisible, id == attemptID else { return }
        NSPasteboard.general.clearContents(); NSPasteboard.general.setString(values.joined(separator: "\n"), forType: .string); copied = true
    }
}
