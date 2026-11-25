import SwiftUI

/// Captured by editors and confirmation sheets, never rebuilt from the current selection on submit.
@MainActor struct RemoteConsoleTarget {
    let management: RemoteGatewayManagement
    let pool: RemotePoolSnapshot
    let generation: UUID
    let endpoint: String

    init(_ management: RemoteGatewayManagement, _ pool: RemotePoolSnapshot) {
        self.management = management; self.pool = pool
        generation = management.targetGeneration; endpoint = management.endpoint
    }

    var current: Bool {
        management.remoteSelected && management.targetGeneration == generation &&
        pool.loadID == generation && management.snapshot?.revision == pool.revision
    }

    func call(_ name: String, _ fields: [String: JSONValue] = [:]) async throws -> JSONValue {
        guard current else { throw CancellationError() }
        let result = try await management.operation(name, payload: .object(fields), on: pool)
        try Task.checkCancellation()
        guard generation == management.targetGeneration, management.remoteSelected else { throw CancellationError() }
        return result
    }
}

struct RemoteGatewayConsole: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let page: SettingsPage

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                PageHeading(page.rawValue, subtitle: management.endpoint) {
                    Button("刷新") { Task { await management.refresh() } }.disabled(management.busy)
                }
                RemoteGatewayFreshness()
                if let message = management.message { InfoNote(text: message, color: .orange) }
                if let pool = management.snapshot, pool.loadID == management.targetGeneration {
                    let target = RemoteConsoleTarget(management, pool)
                    if page == .overview { RemoteRuntimeSummary(data: management.runtimeData) }
                    if management.capabilities.contains(page == .connections ? "device.list" : "config.get") {
                        Group {
                            switch page {
                            case .overview: RemoteConsoleOverview(target: target)
                            case .usage: RemoteConsoleUsage(target: target)
                            case .connections: RemoteConsoleDevices(target: target).id(pool.revision)
                            case .sources, .models, .general: EmptyView()
                            }
                        }.id(page).id(management.targetGeneration)
                    } else { InfoNote(text: page == .connections ? "此网关不支持设备密钥管理。请更新远端网关。" : "此网关仅支持来源池管理。请更新远端网关以使用更多功能。") }
                } else if management.busy { ProgressView("正在验证远端网关…") }
                else { InfoNote(text: "尚无已验证的远端快照。请刷新或重新连接。") }
            }.settingsPage()
        }.background(WindowOpenRefresh(refresh: { Task { await management.refresh() } }))
    }
}

struct RemoteConsoleDevice: Identifiable, Decodable {
    let id: String
    let name: String
    let host: String?
    let port: Int?
}


enum RemoteConsoleError: Error { case invalid }

func remoteConsoleDecode<T: Decodable>(_ type: T.Type, _ value: JSONValue) throws -> T {
    try JSONDecoder().decode(type, from: JSONEncoder().encode(value))
}
