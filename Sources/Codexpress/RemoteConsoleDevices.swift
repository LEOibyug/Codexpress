import AppKit
import SwiftUI

@MainActor
struct RemoteConsoleDevices: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    let target: RemoteConsoleTarget
    @State private var purpose: RemoteIngressPurpose = .codex
    @State private var devices: [RemoteConsoleDevice]?
    @State private var deviceRecord: RemoteIngressKeys.Record?
    @State private var keysVerified = false
    @State private var deviceName = ""
    @State private var handoff: RemoteIngressCredential?
    @State private var confirmation: Confirmation?
    @State private var showingOMP = false
    @State private var operationID: UUID?
    @State private var note: String?
    @State private var error: String?

    private enum Confirmation {
        case issue, other(String), revoke(RemoteConsoleDevice), discard(String)
        var title: String {
            switch self {
            case .issue: "为这台 Mac 签发通用推理密钥？"
            case .other: "为其他设备签发密钥？"
            case .revoke: "撤销设备密钥？"
            case .discard: "清除未确认的签发记录？"
            }
        }
        var detail: String {
            switch self {
            case .issue: "密钥保存在这台 Mac。持有者可使用此网关的全部推理入口。"
            case .other(let name): "设备：\(name)。密钥只在本次交付时可复制，之后无法找回。"
            case .revoke(let device): "\(device.name)（\(device.id)）将无法再使用此密钥。"
            case .discard(let name): "请确认设备列表中没有“\(name)”。清除本机记录不会签发或撤销任何密钥。"
            }
        }
    }

    private var busy: Bool { operationID != nil }
    private var trimmedName: String { deviceName.trimmingCharacters(in: .whitespacesAndNewlines) }

    var body: some View {
        VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
            connectionCard
            if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
            if let note { Text(note).font(AppTypography.caption).foregroundStyle(.secondary) }
            devicesCard
        }
        .onAppear { refresh() }
        .onChange(of: management.fetchedAt) { _, _ in refresh() }
        .onChange(of: purpose) { _, _ in note = nil }
        .onDisappear { operationID = nil; deviceRecord = nil; keysVerified = false; devices = nil; handoff = nil; confirmation = nil; note = nil }
        .alert(confirmation?.title ?? "", isPresented: Binding(get: { confirmation != nil }, set: { if !$0 { confirmation = nil } }), presenting: confirmation) { action in
            Button("取消", role: .cancel) { }
            switch action {
            case .issue, .other: Button("签发密钥") { submit(action) }
            case .revoke: Button("撤销", role: .destructive) { submit(action) }
            case .discard: Button("清除记录", role: .destructive) { submit(action) }
            }
        } message: { Text("\($0.detail)\n\(target.endpoint)") }
        .sheet(isPresented: $showingOMP) {
            VStack(alignment: .trailing) { OMPSettingsView(); Button("完成") { showingOMP = false }.padding() }.frame(width: 660, height: 530)
        }
    }

    private var connectionCard: some View {
        SettingsCard("客户端接入", subtitle: "选择客户端，将地址和密钥分别填入 Base URL 和 API Key。", symbol: "cable.connector") {
            Picker("客户端用途", selection: $purpose) {
                ForEach(RemoteIngressPurpose.allCases, id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 490).disabled(busy)
            Divider()
            SettingRow(title: "Base URL", subtitle: purpose.baseURL(endpoint: target.endpoint)) {
                Button("复制地址") {
                    guard target.current else { return }
                    let board = NSPasteboard.general
                    board.clearContents()
                    if board.setString(purpose.baseURL(endpoint: target.endpoint), forType: .string) { note = "已复制\(purpose.title)的接入地址。" }
                }
            }
            if let record = deviceRecord, let credential = record.credential {
                SettingRow(title: "通用 API Key", subtitle: record.name) {
                    Button("复制密钥") { copyKey(credential.id) }.buttonStyle(.borderedProminent).disabled(busy)
                }
            } else if let pending = deviceRecord {
                SettingRow(title: "签发结果待核对", subtitle: "检查“\(pending.name)”。若已签发但密钥未保存，请在下方撤销该设备。") {
                    Button("清除记录…") { confirmation = .discard(pending.name) }
                        .disabled(busy || devices?.contains { $0.name == pending.name } != false)
                }
            } else {
                SettingRow(title: "通用 API Key", subtitle: keysVerified ? "这台 Mac 尚未签发推理密钥。" : "尚未核实本机密钥。") {
                    Button("签发密钥…") { confirmation = .issue }.buttonStyle(.borderedProminent)
                        .disabled(busy || !keysVerified || !management.capabilities.contains("device.issue"))
                }
            }
            Text("四个推理入口共用这把设备密钥。切换客户端只需更换地址；撤销后四个入口同时失效。")
                .font(AppTypography.caption).foregroundStyle(.secondary)
            if purpose == .omp {
                Divider()
                SettingRow(title: "本机 OMP 扩展", subtitle: "扩展自动连接当前网关，无需复制密钥。") { Button("本机 OMP 接入…") { showingOMP = true } }
            }
        }
    }

    private var devicesCard: some View {
        SettingsCard("已授权设备", subtitle: "服务器只提供设备信息，不回显密钥。", symbol: "laptopcomputer.and.iphone", accessory: {
            Button("刷新列表") { refresh() }.disabled(busy)
        }) {
            if let devices {
                if devices.isEmpty { Text("尚无已授权设备").foregroundStyle(.secondary) }
                ForEach(devices) { device in
                    SettingRow(title: device.name, subtitle: device.id) {
                        Button("撤销…", role: .destructive) { confirmation = .revoke(device) }
                            .disabled(busy || !management.capabilities.contains("device.revoke"))
                    }
                }
            } else if busy { ProgressView("正在核对设备…").controlSize(.small) }
            Divider()
            if let handoff {
                SettingRow(title: "交付新设备的密钥", subtitle: "关闭或离开本页后不能再复制。") {
                    HStack {
                        Button("复制密钥") { copyKey(handoff.id) }.buttonStyle(.borderedProminent).disabled(busy || devices == nil)
                        Button("完成交付") { self.handoff = nil; note = nil }.disabled(busy)
                    }
                }
            } else {
                SettingRow(title: "添加其他设备", subtitle: "为另一台设备签发独立密钥。") {
                    HStack {
                        TextField("设备名称", text: $deviceName).textFieldStyle(.roundedBorder).frame(maxWidth: 180)
                        Button("签发…") { confirmation = .other(trimmedName) }
                            .disabled(busy || devices == nil || trimmedName.isEmpty || trimmedName.utf8.count > 256 || !management.capabilities.contains("device.issue"))
                    }
                }
            }
        }
    }

    private func check(_ id: UUID) throws {
        guard operationID == id, target.current else { throw CancellationError() }
    }

    private func perform(_ work: @escaping @MainActor (UUID) async throws -> Void) {
        guard !busy, target.current else { return }
        let id = UUID(); operationID = id; note = nil; error = nil
        Task { @MainActor in
            defer { if operationID == id { operationID = nil } }
            do { try check(id); try await work(id) }
            catch {
                guard operationID == id, target.current else { return }
                devices = nil; deviceRecord = nil; keysVerified = false
                self.error = "未能确认设备操作。请刷新列表核对结果，不要直接重复签发。"
            }
        }
    }

    private func refresh() {
        guard confirmation == nil else { return }
        perform { id in _ = try await reload(id) }
    }

    @discardableResult private func reload(_ id: UUID) async throws -> [RemoteConsoleDevice] {
        try check(id)
        let listed = try remoteConsoleDecode([RemoteConsoleDevice].self, await target.call("device.list"))
        try check(id)
        let activeIDs = Set(listed.map(\.id))
        guard activeIDs.count == listed.count else { throw RemoteConsoleError.invalid }
        devices = listed
        if let handoff, !activeIDs.contains(handoff.id) { self.handoff = nil }
        do { deviceRecord = try management.ingressKeys(for: target.pool).verified(activeIDs: activeIDs); keysVerified = true }
        catch { deviceRecord = nil; keysVerified = false; self.error = "无法安全读取本机设备密钥。请检查状态目录和密钥文件权限。" }
        return listed
    }

    private func submit(_ action: Confirmation) {
        perform { id in
            let listed = try await reload(id)
            try check(id)
            switch action {
            case .issue:
                guard keysVerified, deviceRecord == nil else { return }
                let store = try management.ingressKeys(for: target.pool)
                let name = try store.reserve()
                let credential = try await issue(name, listed: listed)
                guard target.current else { throw CancellationError() }
                try store.finish(reservation: name, credential: credential)
            case .other(let name):
                guard handoff == nil, !listed.contains(where: { $0.name == name }) else { error = "已有同名设备，请核对列表或换一个设备名称。"; return }
                let credential = try await issue(name, listed: listed)
                try check(id); handoff = credential; deviceName = ""
            case .revoke(let device):
                guard listed.contains(where: { $0.id == device.id }) else { return }
                let result = try await target.call("device.revoke", ["id": .string(device.id), "confirm": .bool(true)])
                guard result["revoked"].bool == true else { throw RemoteConsoleError.invalid }
            case .discard(let name):
                try management.ingressKeys(for: target.pool).discardUnconfirmed(reservation: name, listed: listed)
            }
            try await reload(id)
        }
    }

    private func issue(_ name: String, listed: [RemoteConsoleDevice]) async throws -> RemoteIngressCredential {
        let credential = try RemoteIngressCredential(await target.call("device.issue", ["name": .string(name)]))
        guard !listed.contains(where: { $0.id == credential.id }) else { throw RemoteConsoleError.invalid }
        return credential
    }

    private func copyKey(_ expectedID: String) {
        perform { id in
            let listed = try await reload(id)
            try check(id)
            let credential = handoff?.id == expectedID ? handoff : deviceRecord?.credential
            guard let credential, credential.id == expectedID, listed.contains(where: { $0.id == expectedID }) else {
                error = "无法核实此设备密钥，未复制。"; return
            }
            guard RemoteIngressClipboard.shared.copy(credential.key) else { error = "未能写入本机剪贴板。"; return }
            note = "密钥已复制到本机剪贴板，60 秒后清除。"
        }
    }
}

@MainActor private final class RemoteIngressClipboard: NSObject {
    static let shared = RemoteIngressClipboard()
    private var ownedChange: Int?

    private override init() {
        super.init()
        NotificationCenter.default.addObserver(self, selector: #selector(willTerminate(_:)), name: NSApplication.willTerminateNotification, object: nil)
    }

    func copy(_ key: String) -> Bool {
        let item = NSPasteboardItem()
        guard item.setString(key, forType: .string),
              item.setData(Data(), forType: .init("org.nspasteboard.ConcealedType")),
              item.setData(Data(), forType: .init("org.nspasteboard.TransientType")) else { return false }
        let board = NSPasteboard.general
        board.prepareForNewContents(with: .currentHostOnly)
        guard board.writeObjects([item]) else { return false }
        let change = board.changeCount
        ownedChange = change
        Task {
            try await Task.sleep(for: .seconds(60))
            if ownedChange == change { clearOwnedContents() }
        }
        return true
    }

    @objc private func willTerminate(_ notification: Notification) { clearOwnedContents() }

    private func clearOwnedContents() {
        let board = NSPasteboard.general
        if let ownedChange, board.changeCount == ownedChange { board.prepareForNewContents(with: .currentHostOnly) }
        ownedChange = nil
    }
}
