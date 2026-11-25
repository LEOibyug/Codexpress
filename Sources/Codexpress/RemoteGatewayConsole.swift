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
                    if management.capabilities.contains("config.get") {
                        Group {
                            switch page {
                            case .overview: RemoteConsoleOverview(target: target)
                            case .models, .general: RemoteConsolePreferences(target: target, page: page)
                            case .usage: RemoteConsoleUsage(target: target)
                            case .connections: RemoteConsoleDevices(target: target)
                            case .sources: EmptyView()
                            }
                        }.id(page).id(management.targetGeneration)
                    } else { InfoNote(text: "此网关仅支持来源池管理。请更新远端网关以使用更多功能。") }
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

private struct RemoteConsoleIssuedKey: Identifiable {
    let id: String
    let key: String
}

private struct RemoteConsoleDevices: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    @State private var devices: [RemoteConsoleDevice] = []
    @State private var name = ""
    @State private var busy = false
    @State private var failed = false
    @State private var loaded = false
    @State private var issued: RemoteConsoleIssuedKey?
    @State private var revoking: RemoteConsoleDevice?
    @State private var confirmingTarget: RemoteConsoleTarget?
    @State private var showingOMP = false

    var body: some View {
        let base = target.endpoint.hasSuffix("/") ? String(target.endpoint.dropLast()) : target.endpoint
        SettingsCard("客户端接入", symbol: "point.3.connected.trianglepath.dotted") {
            LabeledContent("Codex", value: base + "/v1").textSelection(.enabled)
            LabeledContent("聊天客户端", value: base + "/chat/v1").textSelection(.enabled)
            LabeledContent("图片客户端", value: base + "/images/v1").textSelection(.enabled)
            LabeledContent("OMP", value: base + "/omp/v1").textSelection(.enabled)
            Text("将设备密钥填入客户端的 API Key，不要使用网关管理令牌。").foregroundStyle(.secondary)
            Button("本机 OMP 接入…") { showingOMP = true }
        }
        SettingsCard("设备密钥", symbol: "key") {
            if failed { InfoNote(text: "无法确认设备操作。请先刷新列表核对，不要重复签发。", color: .orange) }
            if loaded && devices.isEmpty { Text("尚未签发设备密钥").foregroundStyle(.secondary) }
            ForEach(devices) { device in
                HStack {
                    VStack(alignment: .leading) {
                        Text(device.name); Text(device.id).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                    Spacer(); Button("撤销…", role: .destructive) { revoking = device; confirmingTarget = target }
                }
            }
            Divider()
            HStack {
                TextField("设备名称", text: $name).textFieldStyle(.roundedBorder)
                Button("签发密钥") { issue() }
                    .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || name.utf8.count > 256 || !loaded)
            }
        }.disabled(busy)
        .task(id: management.fetchedAt) { await list() }
        .alert("撤销设备密钥？", isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }), presenting: revoking) { device in
            Button("取消", role: .cancel) { }
            Button("撤销", role: .destructive) {
                guard let scope = confirmingTarget else { return }
                busy = true
                Task { defer { busy = false }; do {
                    _ = try await scope.call("device.revoke", ["id": .string(device.id), "confirm": .bool(true)]); await list()
                } catch { failed = true } }
            }
        } message: { Text("\($0.name)（\($0.id)）将立即无法连接 \(target.endpoint)。") }
        .sheet(item: $issued) { device in
            VStack(alignment: .leading, spacing: 16) {
                Text("设备密钥仅显示一次").font(.title2)
                Text("设备：\(device.id) · \(target.endpoint)")
                Text(device.key).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                Button("已保存，关闭") { issued = nil }.keyboardShortcut(.defaultAction)
            }.padding(24).frame(width: 520)
        }
        .sheet(isPresented: $showingOMP) {
            VStack(alignment: .trailing) { OMPSettingsView(); Button("完成") { showingOMP = false }.padding() }.frame(width: 660, height: 530)
        }
    }

    private func list() async {
        do {
            let result = try await target.call("device.list")
            devices = try remoteConsoleDecode([RemoteConsoleDevice].self, result); failed = false; loaded = true
        } catch { if !Task.isCancelled { failed = true; loaded = false } }
    }

    private func issue() {
        let deviceName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !busy, loaded, !deviceName.isEmpty else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let value = try await target.call("device.issue", ["name": .string(deviceName)])
                guard let key = value["key"].string, key.hasPrefix("cxr-"), key.utf8.count == 68,
                      key.utf8.dropFirst(4).allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
                      let id = value["id"].string, UUID(uuidString: id) != nil else { throw RemoteConsoleError.invalid }
                issued = .init(id: id, key: key); name = ""; await list()
            } catch { failed = true; loaded = false }
        }
    }
}

enum RemoteConsoleError: Error { case invalid }

func remoteConsoleDecode<T: Decodable>(_ type: T.Type, _ value: JSONValue) throws -> T {
    try JSONDecoder().decode(type, from: JSONEncoder().encode(value))
}
