import SwiftUI

struct UsageView: View {
    @EnvironmentObject var model: AppModel
    @State private var days = 30

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            PageHeading("用量") {
                HStack(spacing: 8) {
                    Picker("时间范围", selection: $days) {
                        Text("近 24 小时").tag(1); Text("近 7 天").tag(7); Text("近 30 天").tag(30)
                    }.labelsHidden().frame(width: 108)
                    Button { model.refreshUsage(days: days) } label: { Image(systemName: "arrow.clockwise") }
                        .disabled(model.busy || !model.running).help("刷新用量").accessibilityLabel("刷新用量")
                }
            }
            if model.usage["rows"].array.isEmpty {
                EmptyState("暂无用量", subtitle: "所选时间范围内没有请求记录。", symbol: "chart.bar.xaxis")
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        ForEach(Array(model.usage["rows"].array.enumerated()), id: \.offset) { _, row in
                            VStack(alignment: .leading, spacing: 20) {
                                HStack(spacing: 8) {
                                    Image(systemName: "server.rack").foregroundStyle(.secondary)
                                    Text(row["source_name"].string ?? "已移除来源").font(.system(size: 13, weight: .semibold))
                                }
                                HStack(spacing: 0) {
                                    metric("请求", row["requests"])
                                    metric("输入 Token", row["input_tokens"])
                                    metric("输出 Token", row["output_tokens"])
                                    metric("失败", row["failures"])
                                }
                                Divider()
                                HStack {
                                    Text("缓存输入 \(countLabel(row["cached_input_tokens"]))")
                                    Spacer()
                                    Text("用量不完整 \(countLabel(row["incomplete_requests"])) 次")
                                }.font(.system(size: 10)).foregroundStyle(.secondary)
                            }.padding(18).cardSurface()
                        }
                        if !model.usage["recent"].array.isEmpty { recentRequests }
                    }.padding(.bottom, 2)
                }
            }
        }.settingsPage()
            .onAppear { if model.running { model.refreshUsage(days: days) } }
            .onChange(of: days) { _, value in model.refreshUsage(days: value) }
    }

    private var recentRequests: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("最近请求").font(.system(size: 12, weight: .semibold)).padding(.horizontal, 4).padding(.top, 8)
            VStack(spacing: 0) {
                ForEach(Array(model.usage["recent"].array.prefix(30).enumerated()), id: \.offset) { index, item in
                    if index > 0 { Divider().padding(.horizontal, 16) }
                    VStack(alignment: .leading, spacing: 7) {
                        HStack {
                            Text(item["source_name"].string ?? "未知来源").font(.system(size: 12, weight: .medium))
                            Spacer()
                            Text(item["status"].string ?? item["status"].number.map { String(Int($0)) } ?? "未知")
                                .font(.system(size: 10)).foregroundStyle(.secondary)
                        }
                        HStack(spacing: 7) {
                            Text(item["model"].string ?? "未知模型").font(.system(size: 10, design: .monospaced)).lineLimit(1)
                            Text("·")
                            Text(timestampLabel(item["started_at"]))
                            Spacer(minLength: 0)
                            Text(item["latency_ms"].number.map { "\(Int($0)) ms" } ?? "延迟未知").monospacedDigit()
                        }.font(.system(size: 10)).foregroundStyle(.secondary)
                        if item["complete"].bool != true { InfoNote(text: "用量未报告或不完整", color: .orange) }
                    }.padding(16)
                }
            }.cardSurface()
        }
    }

    private func metric(_ title: String, _ value: JSONValue) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(countLabel(value)).font(.system(size: 22, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.7)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
