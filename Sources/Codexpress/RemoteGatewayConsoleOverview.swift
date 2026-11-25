import SwiftUI

struct RemoteConsoleOverview: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    @State private var runtime: [String: JSONValue] = [:]
    @State private var operations: JSONValue = .null
    @State private var events: [JSONValue] = []
    @State private var failed = false
    @State private var busy = false
    @State private var accounts: RemoteConsoleAccountSession?

    var body: some View {
        VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
            if failed { InfoNote(text: "无法确认远端操作。请先刷新状态再决定是否操作。", color: .orange) }
            SettingsCard("来源余量与手动分配", symbol: "arrow.triangle.branch") {
                if let id = operations["manual_route"]["id"].string {
                    let sourceID = operations["manual_route"]["source_id"].string
                    HStack {
                        Text("正在尝试接管：\(target.pool.sources.first { $0.id == sourceID }?.name ?? "已移除来源")")
                        Spacer(); Button("停止接管") { act("routing.cancel", ["id": .string(id)]) }
                    }.disabled(busy)
                }
                if target.pool.sources.isEmpty { Text("尚无远端来源").foregroundStyle(.secondary) }
                ForEach(target.pool.sources) { source in
                    Divider()
                    sourceRow(source)
                }
            }
            if let running = operations["scheduler"]["running"].bool {
                let schedule = operations["scheduler"]
                SettingsCard("服务器后台维护", symbol: "clock") {
                    LabeledContent("状态", value: running ? "正在刷新" : "等待下次刷新")
                    if let last = schedule["last_finished_at"].number, last > 0 { LabeledContent("最近完成", value: timestampLabel(.number(last))) }
                    if let completed = schedule["completed"].number, let failed = schedule["failed"].number {
                        LabeledContent("已完成 / 失败", value: usageCount(completed) + " / " + usageCount(failed))
                    }
                }
            }
            if !events.isEmpty {
                SettingsCard("服务器通知", symbol: "bell") {
                    ForEach(Array(events.enumerated()), id: \.offset) { _, event in
                        if let id = event["id"].string {
                            HStack {
                                VStack(alignment: .leading) {
                                    Text(event["title"].string ?? "网关通知")
                                    if let sourceID = event["source_id"].string, let source = target.pool.sources.first(where: { $0.id == sourceID }) {
                                        Text(source.name).font(.caption)
                                    }
                                    Text(event["body"].string ?? "").font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer(); Button("标为已读") { act("notifications.ack", ["ids": .array([.string(id)])]) }.disabled(busy)
                            }
                        }
                    }
                }
            }
        }.task(id: management.fetchedAt) {
            let live = management.runtimeData.flatMap { try? JSONDecoder().decode(JSONValue.self, from: $0) }
            runtime = (live?["sources"].array ?? []).reduce(into: [:]) { if let id = $1["id"].string { $0[id] = $1 } }
            await load()
        }
        .sheet(item: $accounts) { RemoteConsoleAccount(session: $0) }
    }

    private func sourceRow(_ source: RemotePoolSource) -> some View {
        let live = runtime[source.id] ?? .null
        return VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(source.name).font(.headline); Spacer()
                Button(source.kind == "official" ? "额度与卡片…" : "余额详情…") { accounts = .init(target: target, source: source) }
            }
            if source.kind == "official" {
                ForEach(QuotaOverview(live["quota"]).mainWindows) { window in
                    LabeledContent(window.title, value: "\(window.remainingLabel) · \(window.resetLabel)")
                }
            } else if let amount = live["balance"]["amount"].number, let currency = live["balance"]["currency"].string {
                LabeledContent(live["balance"]["estimated"].bool == true ? "估算余额" : "余额", value: balanceMoney(amount, currency: currency))
            } else { Text("尚无余额数据").foregroundStyle(.secondary) }
            HStack {
                Button("尝试接管") { act("routing.try", ["source_id": .string(source.id)]) }
                    .disabled(!source.enabled || live["health"].string == "exhausted")
                if ["cooldown", "proxy_error"].contains(live["health"].string ?? "") {
                    Button("解除冷却") { act("routing.recover", ["source_id": .string(source.id)]) }.disabled(!source.enabled)
                }
            }.disabled(busy)
        }
    }

    private func load() async {
        do {
            operations = try await target.call("operations.status")
            events = try await target.call("notifications.list")["events"].array; failed = false
        } catch { if !Task.isCancelled { failed = true } }
    }
    private func act(_ name: String, _ fields: [String: JSONValue]) {
        guard !busy else { return }; busy = true
        Task { defer { busy = false }; do {
            _ = try await target.call(name, fields); await load(); await management.refresh()
        } catch { failed = true } }
    }
}
