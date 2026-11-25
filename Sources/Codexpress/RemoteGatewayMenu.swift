import AppKit
import SwiftUI

struct RemoteGatewayMenuHost: View {
    @EnvironmentObject private var management: RemoteGatewayManagement

    var body: some View {
        let pool = management.snapshot.flatMap { $0.loadID == management.targetGeneration ? $0 : nil }
        RemoteGatewayMenu(endpoint: management.endpoint, pool: pool.map { RemoteMenuPool($0, runtime: management.runtimeData) },
                          fetchedAt: management.fetchedAt, busy: management.busy, message: management.message,
                          refresh: refresh, openPage: { page in
                              if management.remoteSelected { AppDelegate.current?.showSettings(destination: page) }
                          })
            .background(WindowOpenRefresh(refresh: refresh))
    }

    private func refresh() { Task { await management.refresh() } }
}

struct RemoteMenuPool {
    struct Row: Identifiable {
        let source: RemotePoolSource
        let health: String?
        let activeRequests: Int?
        let balance: String?
        let quotas: [QuotaWindow]
        let accountStale: Bool
        var id: String { source.id }

        init(_ source: RemotePoolSource, live: JSONValue) {
            self.source = source
            switch live["health"].string {
            case let value? where ["ready", "cooldown", "exhausted", "auth_required", "proxy_error"].contains(value): health = value
            default: health = nil
            }
            activeRequests = live["active_requests"].number.flatMap { $0 >= 0 ? Int(exactly: $0) : nil }
            let amount = live["balance"]
            if source.kind == "api", let value = amount["amount"].number, value.isFinite,
               let currency = amount["currency"].string, !currency.isEmpty {
                balance = (amount["estimated"].bool == true ? "估算余额 " : "余额 ") + balanceMoney(value, currency: currency)
            } else { balance = nil }
            let quota = live["quota"]
            quotas = source.kind == "official" ? Array(QuotaOverview(quota).windows.lazy.filter { $0.remaining != nil }.prefix(2)) : []
            accountStale = (source.kind == "official" ? quota : amount)["stale"].bool == true
        }
    }

    let count: Int
    let routing: String
    let inflight: Int?
    let rows: [Row]

    init(_ pool: RemotePoolSnapshot, runtime: Data?) {
        count = pool.sources.count
        if pool.mode == "fixed" {
            routing = "固定来源 · " + (pool.sources.first { $0.id == pool.fixedSourceID }?.name ?? "未指定")
        } else { routing = "来源池 · 自动切换" }
        let data = runtime.flatMap { try? JSONDecoder().decode(JSONValue.self, from: $0) }
        inflight = data?["inflight"].number.flatMap { $0 >= 0 ? Int(exactly: $0) : nil }
        let live = (data?["sources"].array ?? []).reduce(into: [String: JSONValue]()) { index, record in
            if let id = record["id"].string { index[id] = record }
        }
        rows = pool.sources.sorted { $0.priority < $1.priority }.prefix(4).map { Row($0, live: live[$0.id] ?? .null) }
    }
}

struct RemoteGatewayMenu: View {
    let endpoint: String
    let pool: RemoteMenuPool?
    let fetchedAt: Date?
    let busy: Bool
    let message: String?
    let refresh: () -> Void
    let openPage: (SettingsPage) -> Void

    private var host: String {
        guard let url = URLComponents(string: endpoint), let host = url.host, !host.isEmpty else { return "未配置地址" }
        return url.port.map { "\(host):\($0)" } ?? host
    }

    private var stale: Bool { fetchedAt.map { pool == nil || Date().timeIntervalSince($0) > 60 } ?? false }

    var body: some View {
        VStack(alignment: .leading, spacing: 13) {
            HStack(spacing: 9) {
                Image(systemName: "network").font(.system(size: 16, weight: .medium)).foregroundStyle(Color.accentColor)
                    .frame(width: 34, height: 34)
                    .background(Color.accentColor.opacity(0.11), in: RoundedRectangle(cornerRadius: 9))
                VStack(alignment: .leading, spacing: 3) {
                    Text("远端网关").font(AppTypography.sectionTitle)
                    Text(host).font(AppTypography.small).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle).help(host)
                }
                Spacer(minLength: 4)
                if pool != nil {
                    StatusPill(text: stale ? "待刷新" : "已连接", color: stale ? .orange : AppPalette.accountHealthy, compact: true)
                } else if busy { StatusPill(text: "连接中", color: .accentColor, compact: true) }
                else { StatusPill(text: "未连接", color: .orange, compact: true) }
            }
            HStack(spacing: 8) {
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 6) {
                        Label(pool?.routing ?? "路由策略未知", systemImage: "arrow.triangle.branch").lineLimit(1)
                        if let count = pool?.inflight { Text("· \(count) 个请求").foregroundStyle(.secondary).fixedSize() }
                    }.font(AppTypography.caption.weight(.medium))
                    if let fetchedAt {
                        Text("上次更新 \(fetchedAt.formatted(date: Calendar.current.isDateInToday(fetchedAt) ? .omitted : .abbreviated, time: .standard))")
                            .font(.caption).foregroundStyle(stale ? Color.orange : .secondary)
                    } else { Text("尚未获取快照").font(.caption).foregroundStyle(.secondary) }
                }
                Spacer(minLength: 3)
                Button(action: refresh) { Label("刷新", systemImage: "arrow.clockwise") }
                    .buttonStyle(.bordered).controlSize(.small).disabled(busy || endpoint.isEmpty)
                    .help("刷新远端网关状态和来源池")
            }.padding(10).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
            VStack(spacing: 7) {
                HStack {
                    Text("来源池").font(AppTypography.caption.weight(.semibold))
                    Spacer()
                    let link = pool.flatMap { $0.count > 4 ? "查看全部 \($0.count) 个" : nil } ?? "管理来源"
                    Button(link) { openPage(.sources) }
                        .buttonStyle(.link).font(AppTypography.small)
                }.padding(.horizontal, 2)
                VStack(alignment: .leading, spacing: 0) {
                    if let pool, !pool.rows.isEmpty {
                        ForEach(pool.rows) { row in
                            if row.id != pool.rows.first?.id { Divider() }
                            sourceRow(row)
                        }
                    } else {
                        Label(pool == nil ? "尚无已验证的来源信息" : "远端尚未添加来源", systemImage: "square.stack.3d.up")
                            .font(AppTypography.small).foregroundStyle(.secondary).padding(.vertical, 12)
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 11).cardSurface()
            }
            if let message { InfoNote(text: message, color: .orange) }
            Divider()
            HStack(spacing: 12) {
                Button { openPage(.overview) } label: { Label("概览", systemImage: "macwindow") }
                Button { openPage(.connections) } label: { Label("接入", systemImage: "cable.connector") }
                Spacer(minLength: 4)
                Button { openPage(.usage) } label: { Image(systemName: "chart.bar.xaxis") }
                    .help("查看远端用量").accessibilityLabel("查看远端用量")
                Button { openPage(.general) } label: { Image(systemName: "gearshape") }
                    .help("远端设置").accessibilityLabel("远端设置")
                Button("退出") { NSApp.terminate(nil) }.keyboardShortcut("q").foregroundStyle(.secondary)
            }.buttonStyle(.borderless).font(AppTypography.small)
        }.padding(16).frame(width: 364).background(.regularMaterial)
    }

    private func sourceRow(_ row: RemoteMenuPool.Row) -> some View {
        let source = row.source
        let health = source.enabled ? row.health.map(healthLabel) ?? "状态未知" : "已停用"
        let kind = switch source.kind {
        case "official": "官方账号"
        case "api": "Responses API"
        case "openai": "OpenAI API Platform"
        case "google": "Google AI Studio"
        default: "其他来源"
        }
        return VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                let color = !source.enabled || row.health == nil ? Color.secondary : row.health == "ready" ? AppPalette.accountHealthy : .orange
                Circle().fill(color).frame(width: 6, height: 6)
                Text(source.name).font(AppTypography.caption.weight(.semibold)).lineLimit(1).help(source.name)
                Spacer(minLength: 4)
                Text(health).font(.caption).foregroundStyle(.secondary)
            }
            HStack(spacing: 5) {
                Text(kind)
                if let active = row.activeRequests { Text("· 并发 \(active)").monospacedDigit() }
            }.font(.caption).foregroundStyle(.secondary).padding(.leading, 12)
            if row.balance != nil || !row.quotas.isEmpty {
                HStack(spacing: 9) {
                    if let balance = row.balance { Text(balance) }
                    ForEach(row.quotas) { quota in
                        Text("\(quota.title)剩余 \(quota.remainingLabel)").foregroundStyle(AccountLevel.quota(quota.remaining).color)
                    }
                    if row.accountStale { Image(systemName: "clock").foregroundStyle(.orange).help("显示上次获取的数值").accessibilityLabel("上次数值") }
                }.font(AppTypography.small.weight(.medium)).monospacedDigit().lineLimit(1).padding(.leading, 12)
            }
        }.padding(.vertical, 8).accessibilityElement(children: .combine)
    }
}
