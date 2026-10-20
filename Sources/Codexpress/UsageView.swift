import SwiftUI

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    @State private var days = 30
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                VStack(alignment: .leading, spacing: 4) { Text("用量").font(.largeTitle.bold()); Text("仅统计经过本地网关的请求，不保存对话正文。").foregroundStyle(.secondary) }
                Spacer()
                Picker("时间范围", selection: $days) { Text("近24小时").tag(1); Text("7 天").tag(7); Text("30 天").tag(30) }.labelsHidden().frame(width: 112)
                Button { model.refreshUsage(days: days) } label: { Image(systemName: "arrow.clockwise") }.disabled(model.busy || !model.running).help("刷新用量")
            }
            if model.usage["rows"].array.isEmpty {
                ContentUnavailableView("暂无网关用量", systemImage: "chart.bar.xaxis", description: Text("请求通过 Codexpress 后会在这里显示。未知 Token 用量不会伪装成零。")).frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        ForEach(Array(model.usage["rows"].array.enumerated()), id: \.offset) { _, row in
                            VStack(alignment: .leading, spacing: 12) {
                                Text(row["source_name"].string ?? "已移除来源").font(.headline)
                                HStack(spacing: 24) {
                                    metric("请求", row["requests"]); metric("输入 Token", row["input_tokens"]); metric("输出 Token", row["output_tokens"]); metric("失败", row["failures"])
                                }
                                Text("缓存输入（包含在输入中）：\(countLabel(row["cached_input_tokens"]))  ·  用量不完整：\(countLabel(row["incomplete_requests"]))").font(.caption).foregroundStyle(.secondary)
                            }.padding(16).frame(maxWidth: .infinity, alignment: .leading).background(.background, in: RoundedRectangle(cornerRadius: 10)).overlay(RoundedRectangle(cornerRadius: 10).stroke(.quaternary))
                        }
                        if !model.usage["recent"].array.isEmpty {
                            Text("最近请求").font(.headline).padding(.top, 8)
                            ForEach(Array(model.usage["recent"].array.prefix(30).enumerated()), id: \.offset) { _, item in
                                VStack(alignment: .leading, spacing: 4) {
                                    HStack { Text(item["source_name"].string ?? "未知来源"); Spacer(); Text(item["status"].string ?? item["status"].number.map { String(Int($0)) } ?? "未知").foregroundStyle(.secondary) }
                                    HStack { Text(item["model"].string ?? "未知模型"); Text("·"); Text(timestampLabel(item["started_at"])); Spacer(); Text(item["latency_ms"].number.map { "\(Int($0)) ms" } ?? "延迟未知") }.font(.caption).foregroundStyle(.secondary)
                                    if item["complete"].bool != true { Text("用量未报告或不完整").font(.caption).foregroundStyle(.orange) }
                                }.padding(.vertical, 6)
                                Divider()
                            }
                        }
                    }
                }
            }
            Text("总 Token = 输入 + 输出。缓存输入是输入的子集；此页面不是供应商账单。").font(.caption).foregroundStyle(.secondary)
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading).onAppear { if model.running { model.refreshUsage(days: days) } }.onChange(of: days) { _, value in model.refreshUsage(days: value) }
    }
    private func metric(_ title: String, _ value: JSONValue) -> some View {
        VStack(alignment: .leading, spacing: 4) { Text(countLabel(value)).font(.title3.monospacedDigit()); Text(title).font(.caption).foregroundStyle(.secondary) }
    }
}
