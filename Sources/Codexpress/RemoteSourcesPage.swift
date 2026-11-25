import SwiftUI

struct RemoteSourcesPage: View {
    @ObservedObject var draft: RemotePreferencesDraft
    @EnvironmentObject private var management: RemoteGatewayManagement
    @State private var rows: [String: JSONValue] = [:]
    // nil: not verified; .null: the server confirmed there is no takeover.
    @State private var manualRoute: JSONValue?
    @State private var routingBusy = false
    @State private var notice: String?

    var body: some View {
        if let target = draft.target {
            let pool = RemotePoolDraft(draft, target: target)
            switch Result(catching: { try pool.list() }) {
            case .success(let sources):
                let context = RemoteSourcesContext(pool: pool,
                    savedIDs: pool.savedDestinationIDs(for: sources),
                    rows: rows, manualRoute: manualRoute, routingBusy: routingBusy, notice: notice,
                    route: { source, action in perform(action, on: source, pool: pool) })
                SourcesPage(sources: sources, remote: context)
                    .task(id: management.fetchedAt) { await readStatus(target) }
            case .failure:
                InfoNote(text: "无法读取服务器的来源配置，请重新载入。", color: .orange).settingsPage()
            }
        }
    }

    private func readStatus(_ target: RemoteConsoleTarget) async {
        guard target.current else { rows = [:]; manualRoute = nil; return }
        let value = management.runtimeData.flatMap { try? JSONDecoder().decode(JSONValue.self, from: $0) }
        rows = (value?["sources"].array ?? []).reduce(into: [:]) { result, row in
            if let id = row["id"].string { result[id] = row }
        }
        guard management.capabilities.contains("operations.status") else { manualRoute = nil; return }
        do {
            let response = try await target.call("operations.status")
            guard case .object(let fields) = response, let route = fields["manual_route"],
                  route == .null || (route["id"].string.flatMap(UUID.init(uuidString:)) != nil && route["source_id"].string?.isEmpty == false) else { throw RemoteConsoleError.invalid }
            guard target.current, !Task.isCancelled else { return }
            manualRoute = route; notice = nil
        } catch {
            guard !Task.isCancelled else { return }
            manualRoute = nil; notice = "未能核对服务器的抢占状态。"
        }
    }

    private func perform(_ action: RemoteSourceRoute, on source: Source, pool: RemotePoolDraft) {
        guard !routingBusy, pool.isSavedDestination(source), management.capabilities.contains(action.operation) else { return }
        routingBusy = true
        Task {
            defer { routingBusy = false }
            do {
                guard pool.isSavedDestination(source) else { throw CancellationError() }
                switch action {
                case .cancel(let id):
                    guard manualRoute?["id"].string == id, manualRoute?["source_id"].string == source.id else { throw RemoteConsoleError.invalid }
                    let reply = try await pool.target.call(action.operation, ["id": .string(id)])
                    guard reply["cancelled"].bool == true else { throw RemoteConsoleError.invalid }
                    manualRoute = .null
                case .takeover, .recover:
                    guard source.enabled, pool.target.pool.sources.contains(where: { $0.id == source.id && $0.enabled }) else { throw RemoteConsoleError.invalid }
                    let reply = try await pool.target.call(action.operation, ["source_id": .string(source.id)])
                    if case .takeover = action {
                        guard reply["source_id"].string == source.id, reply["id"].string.flatMap(UUID.init(uuidString:)) != nil else { throw RemoteConsoleError.invalid }
                        manualRoute = reply
                    } else if reply["recovered"].bool != true { throw RemoteConsoleError.invalid }
                }
                notice = nil
            } catch {
                guard pool.target.current else { return }
                manualRoute = nil; notice = "未能确认远端分配操作，请先核对服务器状态。"
            }
            if pool.target.current { await management.refresh() }
        }
    }
}

enum RemoteSourceRoute {
    case takeover, recover, cancel(String)
    var operation: String {
        switch self { case .takeover: "routing.try"; case .recover: "routing.recover"; case .cancel: "routing.cancel" }
    }
}

@MainActor struct RemoteSourcesContext {
    let pool: RemotePoolDraft
    let savedIDs: Set<String>
    let rows: [String: JSONValue]
    let manualRoute: JSONValue?
    let routingBusy: Bool
    let notice: String?
    let route: (Source, RemoteSourceRoute) -> Void

    func supports(_ operation: String) -> Bool { pool.target.management.capabilities.contains(operation) }
    var canLogin: Bool {
        pool.target.management.capabilities.isSuperset(of: ["official.login.start", "official.login.complete", "official.login.status", "official.login.cancel"])
    }
    func live(_ source: Source) -> JSONValue { savedIDs.contains(source.id) ? rows[source.id] ?? .null : .null }
    func takingOver(_ source: Source) -> Bool { savedIDs.contains(source.id) && manualRoute?["source_id"].string == source.id }
    func missingCredential(_ source: Source) -> Bool {
        let live = live(source)
        return live["health"].string == "auth_required" || ["需要登录", "缺少密钥"].contains(live["admission"]["routing_note"].string ?? "")
    }
    func canUse(_ source: Source) -> Bool { savedIDs.contains(source.id) && pool.target.current && !pool.draft.needsReload && !pool.draft.submitting }
    func session(_ source: Source) -> RemoteConsoleAccountSession? {
        guard canUse(source), pool.isSavedDestination(source), let saved = pool.target.pool.sources.first(where: { $0.id == source.id }) else { return nil }
        return .init(target: pool.target, source: saved)
    }
}

struct RemoteSourceRoutingActions: View {
    let source: Source
    let remote: RemoteSourcesContext

    var body: some View {
        let live = remote.live(source)
        let health = live["health"].string
        let exhausted = health == "exhausted" || live["admission"]["routing_note"].string == "额度耗尽" || (source.official && QuotaOverview(live["quota"]).isExhausted)
        let enabled = source.enabled && remote.pool.target.pool.sources.contains { $0.id == source.id && $0.enabled }
        let unavailable = remote.routingBusy || !remote.canUse(source)
        if remote.supports("routing.recover"), health == "cooldown" || health == "proxy_error" {
            Button { remote.route(source, .recover) } label: { Label("立即恢复", systemImage: "arrow.clockwise") }
                .disabled(unavailable || !enabled || exhausted)
        }
        if remote.supports("routing.try") {
            Button { remote.route(source, .takeover) } label: { Label("立即抢占", systemImage: "bolt.fill") }
                .disabled(unavailable || !enabled || health == nil || remote.manualRoute == nil || remote.takingOver(source) || exhausted || remote.missingCredential(source))
        }
        if remote.supports("routing.cancel"), remote.takingOver(source), let id = remote.manualRoute?["id"].string {
            Button { remote.route(source, .cancel(id)) } label: { Label("停止抢占", systemImage: "xmark.circle") }.disabled(unavailable)
        }
    }
}

struct RemoteSourceCards: View {
    let source: Source
    let live: JSONValue
    let remote: RemoteSourcesContext
    let open: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 4) {
                Image(systemName: "ticket"); Text("重置卡")
                Text(CreditInventory(live["credits"]).countLabel).font(.system(size: 18, weight: .semibold)).monospacedDigit()
                if live["credits"]["stale"].bool == true { Image(systemName: "clock").help("卡片信息待更新") }
            }.font(AppTypography.small).foregroundStyle(.secondary)
            if remote.supports("official.snapshot") {
                Button("查看卡片…", action: open).buttonStyle(.bordered).controlSize(.small)
                    .disabled(!remote.canUse(source) || remote.missingCredential(source))
            }
        }
    }
}

struct RemoteSourceBalance: View {
    let source: Source
    let live: JSONValue
    let remote: RemoteSourcesContext
    let open: () -> Void

    var body: some View {
        let balance = live["balance"]
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text(balance["estimated"].bool == true ? "估算余额" : "余额")
            if let amount = balance["amount"].number, let currency = balance["currency"].string {
                Text(balanceMoney(amount, currency: currency)).font(AccountTypography.value).monospacedDigit()
                    .foregroundStyle(AccountLevel.balance(amount, threshold: source.balanceWarningThreshold).color)
                if balance["stale"].bool == true { Image(systemName: "clock").help("余额待更新") }
            } else { Text("暂无数据") }
            if remote.supports("balance.query") {
                Button("余额详情…", action: open).buttonStyle(.borderless)
                    .disabled(!remote.canUse(source) || remote.missingCredential(source))
            }
        }.font(AppTypography.caption).foregroundStyle(.secondary)
    }
}

struct RemoteSourceModelRefresh: View {
    let source: Source
    let remote: RemoteSourcesContext
    @State private var refreshing = false
    @State private var failed = false

    var body: some View {
        if failed { Text("模型查询失败").foregroundStyle(.orange) }
        if refreshing { ProgressView().controlSize(.mini) }
        Button("刷新模型") { refreshing = true }.buttonStyle(.borderless)
            .disabled(refreshing || !remote.canUse(source) || remote.missingCredential(source))
            .task(id: refreshing) {
                guard refreshing else { return }
                defer { refreshing = false }
                let pool = remote.pool
                do {
                    guard pool.valid, pool.isSavedDestination(source) else { return }
                    let reply = try await pool.target.call("models.list", ["source_id": .string(source.id)])
                    let models = try remoteConsoleDecode([String].self, reply["models"])
                    guard !Task.isCancelled, pool.valid, pool.isSavedDestination(source), !pool.draft.sourceEditorOpen,
                          try pool.list().first(where: { $0.id == source.id }) == source else { return }
                    if source.models != models || source.automaticModelDiscovery != true {
                        var changed = source; changed.models = models; changed.automaticModelDiscovery = true
                        try pool.stage(changed)
                    }
                    failed = false
                } catch { if !Task.isCancelled { failed = true } }
            }
    }
}

struct RemoteSourceMeasurements: View {
    let source: Source
    let live: JSONValue

    var body: some View {
        let admission = live["admission"]
        if let count = live["active_requests"].number.flatMap(Int.init(exactly:)) {
            LabeledContent("进行中的请求", value: "\(count)")
        }
        if let active = admission["capacity_active_conversations"].number.flatMap(Int.init(exactly:)) {
            if let limit = admission["max_active_conversations"].number.flatMap(Int.init(exactly:)) {
                LabeledContent("对话容量", value: "\(active)/\(limit)" + (admission["capacity_available"].bool == false ? " · 已满" : ""))
            } else { LabeledContent("活跃对话", value: "\(active)") }
        } else if let limit = source.max_active_conversations { LabeledContent("对话上限", value: "\(limit)") }
        if let note = admission["routing_note"].string { LabeledContent("分配状态", value: note) }
        if let until = live["cooldown_until"].number, (1...253402300799).contains(until) {
            LabeledContent("冷却至", value: timestampLabel(.number(until)))
        }
    }
}
