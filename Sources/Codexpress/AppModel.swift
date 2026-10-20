import AppKit
import Foundation
import SwiftUI
import ServiceManagement

@MainActor final class AppModel: ObservableObject {
    static let shared = AppModel()
    @Published private(set) var settings = AppSettings()
    @Published private(set) var status: JSONValue = .null
    @Published private(set) var usage: JSONValue = .null
    @Published var error: String?
    @Published var notice: String?
    @Published private(set) var busy = false
    @Published private(set) var running = false
    @Published private(set) var loginSource: String?
    @Published private(set) var loginFlow: String?
    @Published var callbackURL = ""
    @Published var loginMessage: String?
    @Published var preview: JSONValue = .null
    @Published private(set) var consuming = Set<String>()
    @Published private(set) var resetResults: [String: String] = [:]
    @Published private(set) var launchAtLogin = false
    @Published private(set) var metadataLoadFailed = false
    @Published private(set) var pendingSync = false
    let directory: URL
    private let metadata: MetadataStore
    private let keychain: KeychainStore
    private let client: GatewayClient
    private var helper: Process?
    private var logPipe: Pipe?
    private var timerTask: Task<Void, Never>?
    private var restarting = false
    private var restartCount = 0
    private var shuttingDown = false
    private var operations: [String: ResetOperation] = [:]
    private var lastQuotaRefresh: [String: Date] = [:]
    private var refreshingOfficial = false
    private var startInProgress = false
    private var starting = false
    var sources: [Source] { settings.sources.sorted { $0.priority < $1.priority } }
    var baseURL: String { status["base_url"].string ?? "http://127.0.0.1:\(settings.port)/v1" }
    var routingMode: String { status["mode"].string ?? settings.mode }
    var fixedSourceID: String? { status == .null ? settings.fixed_source_id : status["fixed_source_id"].string }
    var fixedName: String { sources.first { $0.id == fixedSourceID }?.name ?? "未选择" }
    var portNeedsRestart: Bool { status["port"].number.map { Int($0) != settings.port } ?? false }
    init(stateDirectory: URL? = nil) {
        let override = ProcessInfo.processInfo.environment["CODEXPRESS_STATE_DIR"]
        directory = stateDirectory ?? override.map { URL(fileURLWithPath: $0, isDirectory: true) } ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Codexpress", isDirectory: true)
        metadata = MetadataStore(directory: directory); keychain = KeychainStore(stateDirectory: directory)
        client = GatewayClient(socketPath: directory.appendingPathComponent("control.sock").path)
        do { settings = try metadata.load() } catch { metadataLoadFailed = true; self.error = "来源文件无法读取，已进入只读保护。原文件未改动，请从备份修复后重新启动应用。" }
        // Merely display the path; reading/modifying it requires explicit preview/attach actions.
        if settings.codexHome.isEmpty { settings.codexHome = ProcessInfo.processInfo.environment["CODEX_HOME"] ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".codex").path }
        do {
            operations = try ResetOperationStore(directory: directory).load()
            resetResults = operations.mapValues { $0.result }
        } catch { self.error = "重置卡操作记录读取失败，已停用手动提交。"; consuming.insert("all") }
        launchAtLogin = SMAppService.mainApp.status == .enabled
    }
    func start() async {
        guard timerTask == nil, !starting else { return }
        starting = true
        defer { starting = false }
        await startHelper()
        timerTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(5))
                guard !Task.isCancelled, let self else { break }
                await self.poll()
            }
        }
    }
    func startHelper() async {
        guard !metadataLoadFailed else { return }
        guard !startInProgress else { return }
        startInProgress = true
        defer { startInProgress = false }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
            // Do not adopt a possibly unrelated live helper without the metadata we own.
            if helper?.isRunning != true {
                if (try? await client.call("status")) != nil { throw AppError.message("此状态目录已有网关运行，已拒绝接管其他实例。请先退出另一个 Codexpress 实例。") }
                let path = ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"] ?? Bundle.main.executableURL!.deletingLastPathComponent().appendingPathComponent("codexpress-gateway").path
                guard FileManager.default.isExecutableFile(atPath: path) else { throw AppError.message("未找到随应用打包的网关。请从 Codexpress.app 启动，或配置 CODEXPRESS_GATEWAY_PATH。") }
                let process = Process(); process.executableURL = URL(fileURLWithPath: path); process.arguments = ["--state-dir", directory.path, "--port", String(settings.port)]
                let pipe = Pipe(); process.standardError = pipe; process.standardOutput = FileHandle.nullDevice
                // Helper errors are bounded and protected; never append IPC payloads.
                let logURL = directory.appendingPathComponent("gateway.log")
                pipe.fileHandleForReading.readabilityHandler = { handle in
                    let data = handle.availableData
                    guard !data.isEmpty else { handle.readabilityHandler = nil; return }
                    // The helper contract guarantees secret-free diagnostics; cap retention at 256 KiB.
                    var previous = (try? Data(contentsOf: logURL)) ?? Data()
                    previous.append(data)
                    if previous.count > 262_144 { previous = previous.suffix(262_144) }
                    let temp = logURL.deletingLastPathComponent().appendingPathComponent(".gateway-log-\(UUID().uuidString)")
                    if FileManager.default.createFile(atPath: temp.path, contents: previous, attributes: [.posixPermissions: 0o600]) { _ = rename(temp.path, logURL.path) }
                }
                try process.run(); helper = process; logPipe = pipe
                process.terminationHandler = { [weak self] _ in Task { @MainActor in await self?.helperStopped() } }
            }
            var connected = false
            for _ in 0..<30 {
                if let result = try? await client.call("status") {
                    guard let child = helper, child.isRunning, result["process_id"].number == Double(child.processIdentifier) else { throw AppError.message("网关实例不匹配，已拒绝同步来源。") }
                    status = result; connected = true; break
                }
                guard helper?.isRunning == true else { throw AppError.message("网关启动后退出，请检查端口是否被占用。") }
                try? await Task.sleep(for: .milliseconds(100))
            }
            guard connected else { throw AppError.message("网关未能启动。请检查端口冲突和受保护的 gateway.log。") }
            running = true
            try await sync()
            await poll()
        } catch { running = false; self.error = error.localizedDescription }
    }
    private func helperStopped() async {
        running = false
        guard !shuttingDown, !restarting else { return }
        guard restartCount < 3 else { error = "网关反复退出，已停止自动重启。请检查本地端口后手动重试。"; return }
        restarting = true; restartCount += 1
        try? await Task.sleep(for: .seconds(restartCount))
        await startHelper(); restarting = false
    }
    func shutdown() async {
        if running { try? mergeCredentials(try await client.call("credentials")) }
        stop()
    }
    func stop() { shuttingDown = true; timerTask?.cancel(); helper?.terminate(); logPipe?.fileHandleForReading.readabilityHandler = nil }
    func retry() { restartCount = 0; Task { await startHelper() } }
    func requireWritableMetadata() throws {
        guard !metadataLoadFailed else { throw AppError.message("来源文件无法读取，已禁止保存。请先从备份修复 sources.json，再重新启动应用；原文件保持不变。") }
    }
    func persistSettings(_ next: AppSettings) throws {
        try requireWritableMetadata()
        try metadata.save(next)
        settings = next
    }
    private func mergeCredentials(_ data: JSONValue) throws {
        try requireWritableMetadata()
        var next = settings
        next.sources = try OfficialCredentialMirror.mergedSources(settings.sources, response: data, loadDurable: keychain.load)
        if next.sources != settings.sources { try persistSettings(next) }
    }
    private func updateStatus() async throws {
        let fresh = try await client.call("status")
        guard let child = helper, child.isRunning, fresh["process_id"].number == Double(child.processIdentifier) else { running = false; throw AppError.message("网关实例已改变，停止发送管理请求。") }
        status = fresh
        var next = settings
        for i in next.sources.indices where next.sources[i].official {
            if let durable = try keychain.load(next.sources[i].id), let account = durable["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
            else if let live = fresh["sources"].array.first(where: { $0["id"].string == next.sources[i].id }), let account = live["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
        }
        if next.sources != settings.sources { try persistSettings(next) }
    }
    private func sync() async throws {
        try requireWritableMetadata()
        pendingSync = true
        // Always retrieve helper-authoritative rotated tokens before building a source snapshot.
        let notification = try await client.call("credentials")
        try mergeCredentials(notification)
        let helperOwnedIDs = Set(notification["updated_secrets"].array.compactMap { $0["source_id"].string })
        var values: [JSONValue] = []
        var next = settings
        for index in next.sources.indices {
            let sourceID = next.sources[index].id
            let secret = try keychain.load(sourceID) ?? (next.sources[index].official ? .object(["access_token": .string(""), "refresh_token": .string(""), "id_token": .string(""), "expires_at": .number(0)]) : .object([:]))
            // After a crash, Keychain may be newer than the last metadata mirror.
            if next.sources[index].official, let account = secret["account_id"].string, !account.isEmpty { next.sources[index].account_id = account }
            guard case .object(var value) = try JSONValue.from(next.sources[index]) else { continue }
            value["secret"] = next.sources[index].official && helperOwnedIDs.contains(sourceID) ? .object([:]) : secret
            values.append(.object(value))
        }
        if next.sources != settings.sources { try persistSettings(next) }
        do {
            try mergeCredentials(try await client.call("sync", .object(["sources": .array(values), "mode": .string(settings.mode), "fixed_source_id": settings.fixed_source_id.map(JSONValue.string) ?? .null])))
            try await updateStatus()
            pendingSync = false
        } catch { pendingSync = true; throw error }
    }
    func poll() async {
        guard running, !busy else { return }
        do {
            try await updateStatus()
            try mergeCredentials(try await client.call("credentials"))
            if let id = loginSource, let flow = loginFlow {
                let result = try await client.call("official.login.status", .object(["source_id": .string(id), "flow_id": .string(flow)]))
                if result["state"].string == "complete" { try finishLogin(id) }
                else if result["state"].string == "failed" { loginMessage = result["error"].string ?? "登录失败，请重新发起。"; loginFlow = nil }
            }
            await refreshOfficialIfDue()
        } catch { self.error = error.localizedDescription }
    }
    private func refreshOfficialIfDue() async {
        guard !refreshingOfficial else { return }
        refreshingOfficial = true
        defer { refreshingOfficial = false }
        for source in sources where source.official && source.enabled {
            guard Date().timeIntervalSince(lastQuotaRefresh[source.id] ?? .distantPast) >= 300, let secret = try? keychain.load(source.id), secret["access_token"].string?.isEmpty == false else { continue }
            lastQuotaRefresh[source.id] = Date()
            _ = try? await client.call("official.refresh", .object(["source_id": .string(source.id)]))
            try? mergeCredentials(try await client.call("credentials"))
        }
        try? await updateStatus()
    }
    func perform(_ action: @escaping () async throws -> Void) {
        guard !busy else { return }; busy = true; error = nil
        Task { do { try await action() } catch { self.error = error.localizedDescription }; busy = false }
    }
    func saveSource(_ source: Source, apiKey: String) async throws {
        try requireWritableMetadata()
        try source.validate()
        if let current = settings.sources.first(where: { $0.id == source.id }), current.kind != source.kind { throw AppError.message("不能更改已有来源的类型，请新建来源。") }
        guard settings.sources.contains(where: { $0.id == source.id }) || settings.sources.count < 200 else { throw AppError.message("最多支持 200 个来源。") }
        if !source.official, apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, try keychain.load(source.id) == nil { throw AppError.message("请输入 API 密钥。") }
        try mergeCredentials(try await client.call("credentials"))
        try await updateStatus()
        var source = source
        if source.official { source = source.preservingOfficialIdentity(current: settings.sources.first { $0.id == source.id }, live: state(source.id)) }
        if !source.official, !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { try keychain.saveAPIKey(apiKey, for: source.id) }
        var updated = settings
        if let i = updated.sources.firstIndex(where: { $0.id == source.id }) { updated.sources[i] = source }
        else { var item = source; item.priority = updated.sources.count; updated.sources.append(item) }
        if updated.fixed_source_id == source.id && !source.enabled { updated.mode = "auto"; updated.fixed_source_id = nil }
        try persistSettings(updated)
        try await sync(); try await updateStatus()
    }
    func removeSource(_ id: String) {
        perform {
            try self.requireWritableMetadata()
            try self.mergeCredentials(try await self.client.call("credentials"))
            var next = self.settings; next.sources.removeAll { $0.id == id }
            if next.fixed_source_id == id { next.fixed_source_id = nil; next.mode = "auto" }
            try self.persistSettings(next)
            try await self.sync(); try self.keychain.delete(id)
            if self.loginSource == id { self.loginSource = nil; self.loginFlow = nil; self.callbackURL = "" }
            try await self.updateStatus()
        }
    }
    func changeRouting(mode: String, fixed: String?) {
        perform {
            if mode == "fixed", !self.settings.sources.contains(where: { $0.id == fixed && $0.enabled }) { throw AppError.message("请选择已启用的固定来源。") }
            var next = self.settings; next.mode = mode; next.fixed_source_id = fixed
            try self.persistSettings(next); try await self.sync(); try await self.updateStatus()
        }
    }
    func move(_ offsets: IndexSet, to destination: Int) {
        var order = sources.map(\.id); order.move(fromOffsets: offsets, toOffset: destination)
        perform {
            var next = order.compactMap { id in self.settings.sources.first { $0.id == id } }
            guard next.count == self.settings.sources.count else { throw AppError.message("来源已变化，请重新排序。") }
            for i in next.indices { next[i].priority = i }
            var settings = self.settings; settings.sources = next
            try self.persistSettings(settings); try await self.sync(); try await self.updateStatus()
        }
    }
    func toggle(_ source: Source) { var next = source; next.enabled.toggle(); perform { try await self.saveSource(next, apiKey: "") } }
    func state(_ id: String) -> JSONValue { status["sources"].array.first { $0["id"].string == id } ?? .null }
    func login(_ source: Source) {
        perform {
            try await self.sync()
            let result = try await self.client.call("official.login.start", .object(["source_id": .string(source.id)]))
            guard let urlString = result["auth_url"].string, let url = URL(string: urlString), url.scheme == "https", url.host == "auth.openai.com", let flow = result["flow_id"].string else { throw AppError.message("网关返回了无效的登录地址。") }
            self.loginSource = source.id; self.loginFlow = flow; self.loginMessage = "已打开系统浏览器，等待授权回调。"; self.callbackURL = ""
            NSWorkspace.shared.open(url)
        }
    }
    func cancelLogin() {
        callbackURL = ""; loginSource = nil; loginFlow = nil; loginMessage = nil
    }
    func completeLogin() {
        guard let id = loginSource, let flow = loginFlow else { return }
        perform {
            _ = try await self.client.call("official.login.complete", .object(["source_id": .string(id), "flow_id": .string(flow), "callback_url": .string(self.callbackURL)]))
            try self.finishLogin(id)
        }
    }
    private func finishLogin(_ id: String) throws {
        // The helper durably installs official credentials before returning; never replay an IPC token snapshot.
        let durable = try OfficialCredentialMirror.requireDurable(keychain.load(id))
        var next = settings
        if let i = next.sources.firstIndex(where: { $0.id == id }) { next.sources[i].account_id = durable["account_id"].string }
        try persistSettings(next); callbackURL = ""; loginSource = nil; loginFlow = nil; loginMessage = "官方账号已连接。"
        lastQuotaRefresh[id] = nil
        Task { await refreshOfficialIfDue() }
    }
    func refresh(_ id: String) { perform { self.lastQuotaRefresh[id] = Date(); _ = try await self.client.call("official.refresh", .object(["source_id": .string(id)])); try self.mergeCredentials(try await self.client.call("credentials")); try await self.updateStatus() } }
    func refreshUsage(days: Int) { perform { self.usage = try await self.client.call("usage", .object(["days": .number(Double(days))])) } }
    func prepareCredit(source: Source, credit: JSONValue) async throws -> CreditConfirmation {
        try requireWritableMetadata()
        guard let before = settings.sources.first(where: { $0.id == source.id }), before == source else { throw AppError.message("来源已删除或发生变化，请重新选择重置卡。") }
        _ = try await client.call("official.refresh", .object(["source_id": .string(source.id)]))
        try mergeCredentials(try await client.call("credentials"))
        try await updateStatus()
        guard let current = settings.sources.first(where: { $0.id == source.id }), current.sameConfiguration(as: before) else { throw AppError.message("来源已删除或网络策略发生变化，请重新选择重置卡。") }
        return try CreditConfirmation.make(source: current, live: state(source.id), creditID: credit["id"].string)
    }
    func consume(_ confirmation: CreditConfirmation) {
        let source = confirmation.source
        let credit = confirmation.credit
        guard let creditID = credit["id"].string, !creditID.isEmpty else { return }
        let key = source.id + ":" + creditID
        guard !consuming.contains("all"), !consuming.contains(key), resetResults[key] == nil else { return }
        do {
            try requireWritableMetadata()
            guard let currentSource = settings.sources.first(where: { $0.id == source.id }), currentSource == source else { throw AppError.message("来源或账号已变化，请重新确认。") }
            try confirmation.validate(live: state(source.id))
        } catch { self.error = error.localizedDescription; return }
        let operation = UUID().uuidString
        operations[key] = ResetOperation(sourceID: source.id, creditID: creditID, operationID: operation, createdAt: Date().timeIntervalSince1970, result: "结果待核实，请刷新额度和卡片。", accountID: confirmation.accountID)
        do { try ResetOperationStore(directory: directory).save(operations) } catch { self.error = error.localizedDescription; return }
        consuming.insert(key)
        Task {
            defer {
                consuming.remove(key)
                if let result = resetResults[key] { operations[key]?.result = result; do { try ResetOperationStore(directory: directory).save(operations) } catch { self.error = error.localizedDescription } }
            }
            do {
                let result = try await client.call("official.credits.consume", confirmation.consumePayload(operationID: operation))
                let labels = ["reset": "已重置；以刷新后的窗口为准", "nothing_to_reset": "当前无需重置", "no_credit": "无可用重置卡", "already_redeemed": "卡片已使用", "unknown": "结果待核实，请刷新，勿重复提交", "pending_verification": "结果待核实，请刷新，勿重复提交"]
                resetResults[key] = labels[result["code"].string ?? "unknown"] ?? "结果待核实，请刷新"
                try await updateStatus()
            } catch { resetResults[key] = "结果待核实，请先刷新额度和卡片。"; self.error = error.localizedDescription }
        }
    }
    func savePreferences(port: Int, codexHome: String) {
        perform {
            guard (1024...65535).contains(port) else { throw AppError.message("端口须在 1024–65535 之间。") }
            guard codexHome.hasPrefix("/") else { throw AppError.message("请填写 Codex 配置目录的绝对路径。") }
            let changed = self.settings.port != port
            if changed, FileManager.default.fileExists(atPath: self.directory.appendingPathComponent("config-attachment.json").path) { throw AppError.message("Codex 连接仍由本应用接管。请先预览并恢复接管前连接，再更改本地端口。") }
            var next = self.settings; next.port = port; next.codexHome = codexHome
            try self.persistSettings(next); self.preview = .null
            if changed { self.notice = "端口已保存，重新启动应用后生效；随后请重新预览连接配置。" }
            else { self.notice = "设置已保存。" }
        }
    }
    func requireCurrentPort() throws { try ConfigConnectionPolicy.requireCurrentPort(saved: settings.port, live: status["port"].number) }
    func saveModelCompaction(_ rules: [ModelCompactionRule]) {
        perform {
            try ModelCompactionRule.validate(rules)
            var next = self.settings; next.modelCompaction = rules
            try self.persistSettings(next); self.preview = .null
            self.notice = "模型规则已保存。请预览连接配置并应用，再重启 Codex 使规则生效。"
        }
    }
    func configPayload() throws -> JSONValue {
        .object(["codex_home": .string(settings.codexHome), "model_compaction": try JSONValue.from(settings.modelCompaction ?? [])])
    }
    func configPreview() {
        perform {
            try self.requireCurrentPort()
            self.preview = try await self.client.call("config.preview", self.configPayload())
        }
    }
    func configAction(_ operation: String) {
        perform {
            try self.requireWritableMetadata()
            if operation != "config.restore" { try self.requireCurrentPort() }
            let result = try await self.client.call(operation, self.configPayload())
            self.notice = result["message"].string ?? "连接配置已更新。"
            if !self.portNeedsRestart { self.preview = try await self.client.call("config.preview", self.configPayload()) } else { self.preview = .null }
        }
    }
    func setLaunchAtLogin(_ value: Bool) {
        do { if value { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }; launchAtLogin = SMAppService.mainApp.status == .enabled }
        catch { self.error = "无法更新登录启动设置：\(error.localizedDescription)" }
    }
}
