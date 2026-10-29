import AppKit
import Foundation
import SwiftUI
import ServiceManagement

@MainActor final class AppModel: ObservableObject {
    static let shared = AppModel()
    @Published private(set) var settings = AppSettings()
    @Published private(set) var status: JSONValue = .null
    @Published private(set) var usage: JSONValue = .null
    @Published private(set) var usageRefreshing = false
    @Published private(set) var usageError: String?
    @Published private(set) var usageUpdatedAt: Date?
    @Published private(set) var historyStatus: JSONValue = .null
    @Published var error: String?
    @Published var notice: String?
    @Published private(set) var busy = false
    @Published private(set) var running = false
    @Published private(set) var settingsWindowActive = false
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
    @Published private(set) var refreshingModels = Set<String>()
    @Published private(set) var refreshingQuotas = Set<String>()
    @Published private(set) var apiBalances: [String: JSONValue] = [:]
    @Published private(set) var refreshingBalances = Set<String>()
    @Published private(set) var accountRefreshGeneration = 0
    @Published private(set) var modelErrors: [String: String] = [:]
    @Published private(set) var codexConnectionPhase: String?
    @Published private(set) var configurationBackups: [JSONValue] = []
    @Published private(set) var credentialStorage: JSONValue = .null
    let directory: URL
    private let metadata: MetadataStore
    private var credentialSnapshot = CredentialSnapshot()
    private let client: GatewayClient
    private var helper: Process?
    private var logPipe: Pipe?
    private var timerTask: Task<Void, Never>?
    private var accountTimerTask: Task<Void, Never>?
    private var restarting = false
    private var restartCount = 0
    private var shuttingDown = false
    private var operations: [String: ResetOperation] = [:]
    private var officialRefreshSchedule = OfficialRefreshSchedule()
    private var startInProgress = false
    private var starting = false
    private var lastModelRefresh: [String: Date] = [:]
    private var modelRefreshTask: Task<Void, Never>?
    private var usageDays = 30
    private var usageSourceID: String?
    private var usageModelID: String?
    private var lastPollingError: String?
    private var balanceWarnings: BalanceWarnings?
    var sources: [Source] { settings.sources.sorted { $0.priority < $1.priority } }
    var baseURL: String { status["base_url"].string ?? "http://127.0.0.1:\(settings.port)/v1" }
    var routingMode: String { status["mode"].string ?? settings.mode }
    var fixedSourceID: String? { status == .null ? settings.fixed_source_id : status["fixed_source_id"].string }
    var fixedName: String { sources.first { $0.id == fixedSourceID }?.name ?? "未选择" }
    var portNeedsRestart: Bool { status["port"].number.map { Int($0) != settings.port } ?? false }
    var connectionMode: String { status["connection_mode"].string ?? "unmanaged" }
    var credentialStorageMode: String { credentialStorage["mode"].string ?? "keychain" }
    var canManageCredentials: Bool { helper?.isRunning == true && !metadataLoadFailed }
    var localCredentialFile: URL {
        let standard = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Codexpress", isDirectory: true)
        let home = directory.standardizedFileURL == standard.standardizedFileURL ? FileManager.default.homeDirectoryForCurrentUser : directory
        return home.appendingPathComponent(".codexpress/credentials.json")
    }
    init(stateDirectory: URL? = nil) {
        let override = ProcessInfo.processInfo.environment["CODEXPRESS_STATE_DIR"]
        directory = stateDirectory ?? override.map { URL(fileURLWithPath: $0, isDirectory: true) } ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Codexpress", isDirectory: true)
        metadata = MetadataStore(directory: directory)
        client = GatewayClient(socketPath: directory.appendingPathComponent("control.sock").path)
        do { settings = try metadata.load() } catch { metadataLoadFailed = true; self.error = "来源文件无法读取，已进入只读保护。原文件未改动，请从备份修复后重新启动应用。" }
        do {
            balanceWarnings = try BalanceWarnings(directory: directory)
            if !metadataLoadFailed { try balanceWarnings?.retain(sources: settings.sources) }
        } catch { notice = "无法读取余额预警记录，余额预警暂不可用。"; balanceWarnings = nil }
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
            var tick = 0
            while !Task.isCancelled {
                let interval = self?.settingsWindowActive == true || self?.gatewayHasActivity == true ? 1 : 3
                try? await Task.sleep(for: .seconds(interval))
                guard !Task.isCancelled, let self else { break }
                tick = (tick + 1) % 5
                await self.poll(refreshCredentials: tick == 0)
            }
        }
        accountTimerTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(1800)) } catch { break }
                guard !Task.isCancelled, let self else { break }
                await self.refreshAccountInfo(foreground: false)
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
            credentialStorage = try await client.call("credentials.storage.status")
            credentialSnapshot = CredentialSnapshot()
            try await loadCredentialSnapshot(settings.sources.map(\.id))
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
        if running { _ = try? await refreshCredentials() }
        stop()
    }
    func activeRequestsBeforeTermination() async throws -> Int {
        guard let helper, helper.isRunning else { return 0 }
        let fresh = try await client.call("status")
        guard fresh["process_id"].number == Double(helper.processIdentifier),
              let count = fresh["inflight"].number, count.isFinite, count >= 0,
              count < Double(Int.max), count.rounded(.down) == count else {
            throw AppError.message("无法确认当前网关的活动状态。")
        }
        return Int(count)
    }
    func stop() { shuttingDown = true; timerTask?.cancel(); accountTimerTask?.cancel(); modelRefreshTask?.cancel(); helper?.terminate(); logPipe?.fileHandleForReading.readabilityHandler = nil }
    func retry() { restartCount = 0; Task { await startHelper() } }
    func requireWritableMetadata() throws {
        guard !metadataLoadFailed else { throw AppError.message("来源文件无法读取，已禁止保存。请先从备份修复 sources.json，再重新启动应用；原文件保持不变。") }
    }
    func persistSettings(_ next: AppSettings) throws {
        try requireWritableMetadata()
        try metadata.save(next)
        settings = next
        do { try balanceWarnings?.retain(sources: next.sources) }
        catch { notice = "无法更新余额预警记录，请检查应用数据目录的写入权限。" }
    }
    private func updateSettingsAndSync(_ update: (inout AppSettings) -> Void) async throws {
        var next = settings
        update(&next)
        try persistSettings(next)
        try await sync()
    }
    func refreshCredentialStorage() async {
        guard canManageCredentials, !busy else { return }
        do { credentialStorage = try await client.call("credentials.storage.status") }
        catch { self.error = error.localizedDescription }
    }
    func changeCredentialStorage(_ mode: String) {
        perform {
            try self.requireWritableMetadata()
            guard self.canManageCredentials else { throw AppError.message("请先启动网关，再切换凭据存储。") }
            let result = try await self.client.call("credentials.storage.switch", .object(["mode": .string(mode), "file_path": .string(self.localCredentialFile.path), "source_ids": .array(self.settings.sources.map { .string($0.id) })]))
            self.credentialStorage = result
            try await self.loadCredentialSnapshot(self.settings.sources.map(\.id))
            try await self.updateStatus()
            self.running = true
            try await self.sync()
            self.notice = mode == "file" ? "已改用本地文件保存凭据，旧钥匙串副本保留。" : "已改用钥匙串保存凭据，旧本地文件副本保留。"
        }
    }
    private func loadCredentialSnapshot(_ ids: [String]) async throws {
        guard !ids.isEmpty else { return }
        let response = try await client.call("credentials.read", .object(["source_ids": .array(ids.map(JSONValue.string))]))
        try credentialSnapshot.apply(response, requested: ids)
    }
    private func mergeCredentials(_ data: JSONValue) async throws {
        try requireWritableMetadata()
        let changed = data["updated_secrets"].array.compactMap { item -> String? in
            guard let id = item["source_id"].string, settings.sources.contains(where: { $0.id == id && $0.official }) else { return nil }
            return (try? credentialSnapshot.load(id)) == item["secret"] ? nil : id
        }
        try await loadCredentialSnapshot(Array(Set(changed)).sorted())
        var next = settings
        next.sources = try OfficialCredentialMirror.mergedSources(settings.sources, response: data, loadDurable: credentialSnapshot.load)
        if next.sources != settings.sources { try persistSettings(next) }
    }
    @discardableResult private func refreshCredentials() async throws -> JSONValue {
        let notification = try await client.call("credentials")
        try await mergeCredentials(notification)
        return notification
    }
    private func updateStatus() async throws {
        let fresh = try await client.call("status")
        guard let child = helper, child.isRunning, fresh["process_id"].number == Double(child.processIdentifier) else { running = false; throw AppError.message("网关实例已改变，停止发送管理请求。") }
        if status != fresh { status = fresh }
        if Bundle.main.bundleIdentifier == "local.codexpress.mac" {
            let events = try await client.call("notifications.list")
            let result = await LocalNotifications.shared.deliver(events["events"].array)
            if let text = result.undelivered { notice = text }
            if !result.ids.isEmpty { _ = try await client.call("notifications.ack", .object(["ids":.array(result.ids.map(JSONValue.string))])) }
        }
        var next = settings
        for i in next.sources.indices where next.sources[i].official {
            if let durable = try credentialSnapshot.load(next.sources[i].id), let account = durable["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
            else if let live = fresh["sources"].array.first(where: { $0["id"].string == next.sources[i].id }), let account = live["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
        }
        if next.sources != settings.sources { try persistSettings(next) }
    }
    func setSettingsWindowActive(_ active: Bool) {
        if settingsWindowActive != active { settingsWindowActive = active }
    }
    private func sync() async throws {
        try requireWritableMetadata()
        pendingSync = true
        // Always retrieve helper-authoritative rotated tokens before building a source snapshot.
        let notification = try await refreshCredentials()
        let helperOwnedIDs = Set(notification["updated_secrets"].array.compactMap { $0["source_id"].string })
        var values: [JSONValue] = []
        var next = settings
        for index in next.sources.indices {
            let sourceID = next.sources[index].id
            let secret = try credentialSnapshot.load(sourceID) ?? (next.sources[index].official ? .object(["access_token": .string(""), "refresh_token": .string(""), "id_token": .string(""), "expires_at": .number(0)]) : .object([:]))
            // After a crash, durable storage may be newer than the metadata mirror.
            if next.sources[index].official, let account = secret["account_id"].string, !account.isEmpty { next.sources[index].account_id = account }
            guard case .object(var value) = try JSONValue.from(next.sources[index]) else { continue }
            value["secret"] = next.sources[index].official && helperOwnedIDs.contains(sourceID) ? .object([:]) : secret
            values.append(.object(value))
        }
        if next.sources != settings.sources { try persistSettings(next) }
        do {
            try await mergeCredentials(try await client.call("sync", .object(["sources": .array(values), "mode": .string(settings.mode), "fixed_source_id": settings.fixed_source_id.map(JSONValue.string) ?? .null, "official_low_quota_percent":.number(Double(settings.lowQuotaPercent)), "degraded_context_fallback":.bool(settings.degradedContextFallback ?? true), "load_balance":.bool(settings.loadBalanceEnabled), "idle_return_minutes":.number(Double(settings.idleReturnDelay)), "upstream_attempts":.number(Double(settings.upstreamAttemptLimit)), "upstream_retry_delay_seconds":.number(Double(settings.upstreamRetryDelay))])))
            historyStatus = try await client.call("history.configure", historyPayload(settings.historyCacheSettings))
            try await updateStatus()
            pendingSync = false
        } catch { pendingSync = true; throw error }
    }
    func poll(refreshCredentials: Bool = true) async {
        guard running, !busy else { return }
        do {
            try await updateStatus()
            if refreshCredentials {
                try await self.refreshCredentials()
                if let id = loginSource, let flow = loginFlow {
                    let result = try await client.call("official.login.status", .object(["source_id": .string(id), "flow_id": .string(flow)]))
                    if result["state"].string == "complete" { try await finishLogin(id) }
                    else if result["state"].string == "failed" { loginMessage = result["error"].string ?? "登录失败，请重新发起。"; loginFlow = nil }
                }
                scheduleModelRefresh()
            }
            reportPollingError(nil)
        } catch { reportPollingError(error.localizedDescription) }
    }
    func reportPollingError(_ message: String?) {
        defer { lastPollingError = message }
        guard let message, message != lastPollingError else { return }
        error = message
    }
    func dismissError(_ message: String) {
        if error == message { error = nil }
    }
    func accountPageOpened() { accountRefreshGeneration += 1 }
    func refreshAccountInfo(sourceIDs: [String]? = nil, foreground: Bool = true) async {
        guard running, !busy, !Task.isCancelled else { return }
        let selected = sources.filter { $0.enabled && (sourceIDs?.contains($0.id) ?? true) }
        // Bounded concurrency keeps many configured sources from monopolizing
        // the management socket; no account request lives in the status poll.
        await withTaskGroup(of: Void.self) { group in
            for (index, source) in selected.enumerated() {
                if index >= 4 { await group.next() }
                guard !Task.isCancelled else { break }
                group.addTask { await self.refreshAccount(source.id, foreground: foreground) }
            }
        }
    }
    private func refreshAccount(_ id: String, foreground: Bool) async {
        guard running, !busy, !Task.isCancelled, let before = settings.sources.first(where: { $0.id == id }), before.enabled else { return }
        if before.official {
            guard !refreshingQuotas.contains(id), officialRefreshSchedule.isDue(id, foreground: foreground), let secret = try? credentialSnapshot.load(id), secret["access_token"].string?.isEmpty == false else { return }
            try? await queryOfficial(id, includeCredits: officialRefreshSchedule.needsCredits(id))
        } else {
            guard !refreshingBalances.contains(id) else { return }
            let credential = try? credentialSnapshot.load(id)
            refreshingBalances.insert(id)
            defer { refreshingBalances.remove(id) }
            do {
                let balance = try await client.call("balance.query", .object(["source_id": .string(id)]))
                guard running, let current = settings.sources.first(where: { $0.id == id }), current.base_url == before.base_url, current.proxy == before.proxy,
                      (try? credentialSnapshot.load(id)) == credential else { return }
                apiBalances[id] = balance
                await checkBalanceWarning(source: current, balance: balance, credential: credential)
            } catch {
                guard settings.sources.contains(where: { $0.id == id && $0.base_url == before.base_url && $0.proxy == before.proxy }),
                      (try? credentialSnapshot.load(id)) == credential else { return }
                if case .object(var cached) = apiBalances[id], cached["amount"]?.number != nil {
                    cached["stale"] = .bool(true); apiBalances[id] = .object(cached)
                } else { apiBalances[id] = .object(["status": .string("unavailable")]) }
            }
        }
    }
    private func checkBalanceWarning(source: Source, balance: JSONValue, credential: JSONValue?) async {
        guard Bundle.main.bundleIdentifier == "local.codexpress.mac", let key = credential?["api_key"].string, !key.isEmpty else { return }
        do {
            guard let event = try balanceWarnings?.evaluate(source: source, balance: balance, apiKey: key) else { return }
            let result = await LocalNotifications.shared.deliver([event])
            if let text = result.undelivered { notice = text }
            try balanceWarnings?.acknowledge(result.ids)
        } catch { notice = "无法保存余额预警记录，请检查应用数据目录的写入权限。" }
    }
    func perform(_ action: @escaping () async throws -> Void) {
        guard !busy else { return }; busy = true; error = nil; notice = nil
        Task { do { try await action() } catch { self.error = error.localizedDescription }; busy = false }
    }
    func saveSource(_ source: Source, apiKey: String) async throws {
        try requireWritableMetadata()
        try source.validate()
        if let current = settings.sources.first(where: { $0.id == source.id }), current.kind != source.kind { throw AppError.message("不能更改已有来源的类型，请新建来源。") }
        guard settings.sources.contains(where: { $0.id == source.id }) || settings.sources.count < 200 else { throw AppError.message("最多支持 200 个来源。") }
        if !source.official, apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, try credentialSnapshot.load(source.id) == nil { throw AppError.message("请输入 API 密钥。") }
        try await refreshCredentials()
        try await updateStatus()
        var source = source
        if source.official { source = source.preservingOfficialIdentity(current: settings.sources.first { $0.id == source.id }, live: state(source.id)) }
        if !source.official, !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            _ = try await client.call("credentials.save_api", .object(["source_id": .string(source.id), "api_key": .string(apiKey.trimmingCharacters(in: .whitespacesAndNewlines))]))
            try await loadCredentialSnapshot([source.id])
        }
        try await updateSettingsAndSync { next in
            if let i = next.sources.firstIndex(where: { $0.id == source.id }) { next.sources[i] = source }
            else { var item = source; item.priority = next.sources.count; next.sources.append(item) }
            if next.fixed_source_id == source.id && !source.enabled { next.mode = "auto"; next.fixed_source_id = nil }
        }
        if source.official { officialRefreshSchedule.reset(source.id) }
        modelErrors[source.id] = nil
        apiBalances[source.id] = nil
        lastModelRefresh[source.id] = nil
        scheduleModelRefresh()
        accountPageOpened()
    }
    func fetchModels(for draft: Source, apiKey: String = "") async throws -> [String] {
        guard running else { throw AppError.message("请先启动网关。") }
        var source = draft
        if source.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { source.name = "API" }
        try source.validate()
        let payload: JSONValue
        if source.official {
            guard let current = settings.sources.first(where: { $0.id == source.id }),
                  current.proxy == source.proxy else { throw AppError.message("请先保存网络设置，再获取模型。") }
            guard current.account_id != nil else { throw AppError.message("请先登录此账号。") }
            payload = .object(["source_id": .string(source.id)])
        } else {
            let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
            if key.isEmpty, settings.sources.first(where: { $0.id == source.id })?.base_url != source.base_url {
                throw AppError.message("请填写 API 密钥后获取模型。")
            }
            let secret = key.isEmpty ? try credentialSnapshot.load(source.id) : .object(["api_key": .string(key)])
            guard let secret, secret["api_key"].string?.isEmpty == false else { throw AppError.message("请填写 API 密钥后获取模型。") }
            guard case .object(var value) = try JSONValue.from(source) else { throw AppError.message("来源配置无效。") }
            value["secret"] = secret
            payload = .object(["source": .object(value)])
        }
        let result = try await client.call("models.list", payload)
        let models = result["models"].array.compactMap(\.string)
        guard !models.isEmpty else { throw AppError.message("供应商未返回可用模型。") }
        return models
    }
    private func scheduleModelRefresh(maximumAge: TimeInterval = 3600) {
        guard modelRefreshTask == nil, running, !metadataLoadFailed else { return }
        modelRefreshTask = Task {
            defer { modelRefreshTask = nil }
            for source in sources where source.enabled && source.discoversModels {
                guard !busy, Date().timeIntervalSince(lastModelRefresh[source.id] ?? .distantPast) >= maximumAge else { continue }
                if source.official && source.account_id == nil { continue }
                await refreshModels(source.id)
            }
        }
    }
    func refreshVisibleModels() { scheduleModelRefresh(maximumAge: 300) }
    func refreshModels(_ id: String) async {
        guard !refreshingModels.contains(id), let before = settings.sources.first(where: { $0.id == id }), before.discoversModels else { return }
        refreshingModels.insert(id); lastModelRefresh[id] = Date(); modelErrors[id] = nil
        defer { refreshingModels.remove(id) }
        do {
            let models = try await fetchModels(for: before)
            // Discard a response if the editor, login or deletion changed its source.
            guard !Task.isCancelled, running, !busy, let index = settings.sources.firstIndex(where: { $0.id == id }), settings.sources[index] == before else { lastModelRefresh[id] = nil; return }
            guard before.models != models || before.automaticModelDiscovery != true else { return }
            try await updateSettingsAndSync { next in
                next.sources[index].automaticModelDiscovery = true
                next.sources[index].models = models
            }
        } catch {
            if settings.sources.contains(where: { $0 == before }) { modelErrors[id] = error.localizedDescription }
        }
    }
    func removeSource(_ id: String) {
        perform {
            try self.requireWritableMetadata()
            try await self.refreshCredentials()
            try await self.updateSettingsAndSync { next in
                next.sources.removeAll { $0.id == id }
                if next.fixed_source_id == id { next.fixed_source_id = nil; next.mode = "auto" }
            }
            _ = try await self.client.call("credentials.delete", .object(["source_id": .string(id)]))
            try await self.loadCredentialSnapshot([id])
            if self.loginSource == id { self.loginSource = nil; self.loginFlow = nil; self.callbackURL = "" }
            try await self.updateStatus()
            self.apiBalances[id] = nil
        }
    }
    func changeRouting(mode: String, fixed: String?) {
        perform {
            if mode == "fixed", !self.settings.sources.contains(where: { $0.id == fixed && $0.enabled }) { throw AppError.message("请选择已启用的固定来源。") }
            try await self.updateSettingsAndSync { next in next.mode = mode; next.fixed_source_id = fixed }
        }
    }
    func recoverSource(_ id: String) {
        performSourceRouting("routing.recover", sourceID: id, notice: "已解除临时冷却。")
    }
    func startSourceTakeover(_ id: String) {
        performSourceRouting("routing.try", sourceID: id, notice: "已开始抢占后续对话请求。")
    }
    private func performSourceRouting(_ operation: String, sourceID: String, notice: String) {
        perform {
            guard self.running else { throw AppError.message("请先启动网关。") }
            _ = try await self.client.call(operation, .object(["source_id": .string(sourceID)]))
            try await self.updateStatus()
            self.notice = notice
        }
    }
    func cancelSourceTakeover() {
        guard let id = status["manual_route"]["id"].string else { return }
        perform {
            let result = try await self.client.call("routing.cancel", .object(["id": .string(id)]))
            try await self.updateStatus()
            self.notice = result["cancelled"].bool == true ? "已停止来源抢占。" : "抢占已结束或已切换来源。"
        }
    }
    func changeLowQuotaThreshold(_ value: Int) {
        perform {
            guard (1...99).contains(value) else { throw AppError.message("余量阈值须为 1–99%。") }
            try await self.updateSettingsAndSync { $0.officialLowQuotaPercent = value }
        }
    }
    func changeContextFallback(_ enabled: Bool) {
        perform {
            try await self.updateSettingsAndSync { $0.degradedContextFallback = enabled }
        }
    }
    func changeLoadBalance(_ enabled: Bool) {
        perform {
            try await self.updateSettingsAndSync { $0.loadBalance = enabled }
        }
    }
    func changeIdleReturnDelay(_ minutes: Int) {
        perform {
            guard (0...10080).contains(minutes) else { throw AppError.message("闲置回切时间须为 0–10080 分钟。") }
            try await self.updateSettingsAndSync { $0.conversationStickyMinutes = minutes }
        }
    }
    func changeUpstreamRetry(attempts: Int, delaySeconds: Int) {
        perform {
            guard (1...10).contains(attempts), (0...300).contains(delaySeconds) else {
                throw AppError.message("尝试次数须为 1–10，等待时间须为 0–300 秒。")
            }
            try await self.updateSettingsAndSync { next in
                next.upstreamAttempts = attempts
                next.upstreamRetryDelaySeconds = delaySeconds
            }
        }
    }
    func move(_ offsets: IndexSet, to destination: Int) {
        var order = sources.map(\.id); order.move(fromOffsets: offsets, toOffset: destination)
        perform {
            var next = order.compactMap { id in self.settings.sources.first { $0.id == id } }
            guard next.count == self.settings.sources.count else { throw AppError.message("来源已变化，请重新排序。") }
            for i in next.indices { next[i].priority = i }
            try await self.updateSettingsAndSync { $0.sources = next }
        }
    }
    func toggle(_ source: Source) { var next = source; next.enabled.toggle(); perform { try await self.saveSource(next, apiKey: "") } }
    func state(_ id: String) -> JSONValue { status["sources"].array.first { $0["id"].string == id } ?? .null }
    var gatewayHasActivity: Bool { running && (status["inflight"].number ?? 0) > 0 }
    func sourceHasActivity(_ id: String) -> Bool { running && (state(id)["active_requests"].number ?? 0) > 0 }
    func login(_ source: Source) {
        perform {
            try await self.sync()
            let result = try await self.client.call("official.login.start", .object(["source_id": .string(source.id)]))
            guard let urlString = result["auth_url"].string, let url = URL(string: urlString), url.scheme == "https", url.host == "auth.openai.com", let flow = result["flow_id"].string else { throw AppError.message("网关返回了无效的登录地址。") }
            self.loginSource = source.id; self.loginFlow = flow; self.loginMessage = "请在浏览器中完成登录。"; self.callbackURL = ""
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
            try await self.finishLogin(id)
        }
    }
    private func finishLogin(_ id: String) async throws {
        // The helper durably installs official credentials before returning; never replay an IPC token snapshot.
        try await loadCredentialSnapshot([id])
        let durable = try OfficialCredentialMirror.requireDurable(credentialSnapshot.load(id))
        var next = settings
        if let i = next.sources.firstIndex(where: { $0.id == id }) { next.sources[i].account_id = durable["account_id"].string }
        try persistSettings(next); callbackURL = ""; loginSource = nil; loginFlow = nil; loginMessage = "官方账号已连接。"
        officialRefreshSchedule.reset(id)
        lastModelRefresh[id] = nil
        scheduleModelRefresh()
        Task { await refreshAccount(id, foreground: true) }
    }
    private func queryOfficial(_ id: String, includeCredits: Bool) async throws {
        guard !refreshingQuotas.contains(id) else { throw AppError.message("此账号正在刷新，请稍后重试。") }
        refreshingQuotas.insert(id)
        defer { refreshingQuotas.remove(id) }
        officialRefreshSchedule.record(id, includeCredits: includeCredits)
        _ = try await client.call("official.refresh", .object(["source_id": .string(id), "include_credits": .bool(includeCredits)]))
        try await refreshCredentials()
        try await updateStatus()
    }
    func refresh(_ id: String) { perform { try await self.queryOfficial(id, includeCredits: true) } }
    func loadUsage(days: Int, sourceID: String? = nil, modelID: String? = nil) async {
        if usageDays != days { usage = .null; usageUpdatedAt = nil }
        usageDays = days; usageSourceID = sourceID; usageModelID = modelID
        guard running, !usageRefreshing else { return }
        usageRefreshing = true
        defer { usageRefreshing = false }
        do {
            let snapshot = try await client.call("usage", .object(["days": .number(Double(days)), "source_id": sourceID.map(JSONValue.string) ?? .null, "model": modelID.map(JSONValue.string) ?? .null]))
            guard !Task.isCancelled, usageDays == days, usageSourceID == sourceID, usageModelID == modelID else { return }
            usage = snapshot; usageError = nil; usageUpdatedAt = Date()
        } catch {
            if !Task.isCancelled, usageDays == days, usageSourceID == sourceID, usageModelID == modelID { usageError = error.localizedDescription }
        }
    }
    func usageAnalytics(_ query: UsageQuery) async throws -> JSONValue {
        try await client.call("usage.analytics", .object(query.payload))
    }
    func usageRequests(_ query: UsageQuery, status: String = "", search: String = "", before: Double? = nil) async throws -> JSONValue {
        var payload = query.payload
        payload["limit"] = .number(50)
        if !status.isEmpty { payload["status"] = .string(status) }
        if !search.isEmpty { payload["search"] = .string(search) }
        if let before { payload["before"] = .number(before) }
        return try await client.call("usage.requests", .object(payload))
    }
    func usageDetail(attemptID: String, bodyOffset: Double = 0) async throws -> JSONValue {
        try await client.call("usage.detail", .object(["attempt_id": .string(attemptID), "body_offset": .number(bodyOffset), "body_limit": .number(65_536)]))
    }
    func prepareCredit(source: Source, credit: JSONValue) async throws -> CreditConfirmation {
        try requireWritableMetadata()
        guard let before = settings.sources.first(where: { $0.id == source.id }), before == source else { throw AppError.message("来源已删除或发生变化，请重新选择重置卡。") }
        try await queryOfficial(source.id, includeCredits: true)
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
            if changed, self.running {
                let chatbot = try await self.client.call("chatbot.status")
                guard chatbot["port"].number != Double(port) else { throw AppError.message("此端口已用于聊天客户端接入，请选择不同端口。") }
                let images = try await self.client.call("images.status")
                guard images["port"].number != Double(port) else { throw AppError.message("此端口已用于图片生成，请选择不同端口。") }
            }
            if changed, FileManager.default.fileExists(atPath: self.directory.appendingPathComponent("config-attachment.json").path) { throw AppError.message("Codex 连接仍由本应用接管。请先预览并恢复接管前连接，再更改本地端口。") }
            var next = self.settings; next.port = port; if next.codexHome != codexHome { next.legacyProviderOrigins = nil }; next.codexHome = codexHome
            try self.persistSettings(next); self.preview = .null
            if changed { self.notice = "端口已保存，重新启动应用后生效；随后请重新预览连接配置。" }
            else { self.notice = "设置已保存。" }
        }
    }
    private func historyPayload(_ settings: HistoryCacheSettings) -> JSONValue {
        .object(["enabled": .bool(settings.enabled), "max_bytes": .number(Double(settings.maxBytes))])
    }
    func saveHistoryCache(_ preferences: HistoryCacheSettings) {
        perform {
            _ = try HistoryCacheSettings.parse(enabled: preferences.enabled, gigabytes: String(Double(preferences.maxBytes) / 1_000_000_000))
            var next = self.settings; next.historyCache = preferences
            try self.persistSettings(next)
            if self.running { self.historyStatus = try await self.client.call("history.configure", self.historyPayload(preferences)) }
        }
    }
    func refreshHistoryStatus() async {
        guard running, !busy else { return }
        do { historyStatus = try await client.call("history.status") }
        catch { self.error = "无法读取原文缓存状态。" }
    }
    func clearHistoryCache() {
        perform { self.historyStatus = try await self.client.call("history.clear") }
    }
    func remoteCall(_ operation: String, _ payload: JSONValue = .object([:])) async throws -> JSONValue {
        guard running else { throw AppError.message("请先启动本机网关。") }
        return try await client.call(operation, payload)
    }
    func saveRemoteProfiles(_ profiles: [SSHRemoteProfile]) throws {
        var next = settings; next.sshRemoteProfiles = profiles
        try persistSettings(next)
    }
    func requireCurrentPort() throws { try ConfigConnectionPolicy.requireCurrentPort(saved: settings.port, live: status["port"].number) }
    func saveModelCompaction(_ rules: [ModelCompactionRule]) {
        perform {
            try ModelCompactionRule.validate(rules)
            var next = self.settings; next.modelCompaction = rules
            try self.persistSettings(next); self.preview = .null
            self.notice = "模型规则已保存。请在本机或远端预览并应用连接配置，再重启对应的 Codex。"
        }
    }
    func configPayload() throws -> JSONValue {
        .object(["codex_home": .string(settings.codexHome), "model_compaction": try JSONValue.from(settings.modelCompaction ?? []), "provider_origins":try JSONValue.from(settings.legacyProviderOrigins ?? [])])
    }
    func saveLegacyOrigins(_ values: [String:String]) {
        perform {
            var next = self.settings
            next.legacyProviderOrigins = try values.filter { !$0.value.isEmpty }.map { provider, id in
                guard let source = self.sources.first(where: {$0.id == id && !$0.official}), let identity = self.state(id)["identity_digest"].string else { throw AppError.message("来源状态已变化，请刷新后重新选择。") }
                return LegacyProviderOrigin(provider_id: provider, source_id: source.id, source_identity: identity)
            }.sorted { $0.provider_id < $1.provider_id }
            try self.persistSettings(next)
            self.preview = try await self.client.call("config.preview", self.configPayload())
        }
    }
    func configPreview() {
        perform {
            try self.requireCurrentPort()
            self.preview = try await self.client.call("config.preview", self.configPayload())
        }
    }
    private func transitionCodexConnection(target: CodexConnectionTarget, payload: JSONValue, preflight: String, operation: String) async throws -> JSONValue {
        var result: JSONValue = .null
        try await CodexConnectionTransition.run(controller: NativeCodexApplication(), codexHome: settings.codexHome, target: target, preflight: {
            _ = try await self.client.call(preflight, payload)
        }, apply: {
            result = try await self.client.call(operation, payload)
        }, phase: { self.codexConnectionPhase = $0 })
        preview = .null
        try await updateStatus()
        await refreshConfigurationBackups()
        return result
    }
    func configAction(_ operation: String) {
        perform {
            try self.requireWritableMetadata()
            if operation != "config.restore" { try self.requireCurrentPort() }
            let payload = try self.configPayload()
            self.codexConnectionPhase = "检查连接设置…"
            defer { self.codexConnectionPhase = nil }
            let result = try await self.transitionCodexConnection(target: operation == "config.restore" ? .restored : .gateway, payload: payload, preflight: operation == "config.restore" ? "config.restore.preview" : "config.preview", operation: operation)
            self.notice = operation == "config.restore" ? "已恢复原连接并重启 Codex。" : "旧配置与文件认证已备份，连接设置已应用并重启 Codex。"
            if let warning = result["warning"].string { self.notice = warning }
        }
    }
    func refreshConfigurationBackups() async {
        guard running else { return }
        do {
            let result = try await client.call("config.backups", .object(["codex_home":.string(settings.codexHome)]))
            configurationBackups = result["backups"].array
        } catch { self.error = error.localizedDescription }
    }
    func restoreConfigurationBackup(_ id: String) {
        perform {
            try self.requireWritableMetadata()
            let payload: JSONValue = .object(["codex_home":.string(self.settings.codexHome),"backup_id":.string(id)])
            self.codexConnectionPhase = "检查备份…"
            defer { self.codexConnectionPhase = nil }
            _ = try await self.transitionCodexConnection(target: .restored, payload: payload, preflight: "config.backup.preview", operation: "config.backup.restore")
            self.notice = "配置与文件认证已还原，Codex 已重启；还原前状态也已备份。"
        }
    }
    func switchCodexConnection(_ target: CodexConnectionTarget) {
        perform {
            try self.requireWritableMetadata()
            guard self.running else { throw AppError.message("请先启动网关以更新连接设置。") }
            if target == .gateway {
                try self.requireCurrentPort()
                guard self.sources.contains(where: \.enabled) else { throw AppError.message("请先添加并启用一个来源。") }
                try await self.sync()
            }
            let payload = try self.configPayload()
            self.codexConnectionPhase = "检查连接设置…"
            defer { self.codexConnectionPhase = nil }
            do {
                let result = try await self.transitionCodexConnection(target: target, payload: payload, preflight: target == .official ? "config.official.preview" : "config.preview", operation: target == .official ? "config.official" : "config.attach")
                self.notice = target == .official ? "Codex 已重启并切换为官方连接；如未登录，请在 Codex 中完成登录。" : "Codex 已重启并切换为来源池。"
                if let warning = result["warning"].string { self.notice = warning }
            } catch {
                self.preview = .null
                try? await self.updateStatus()
                throw error
            }
        }
    }
    func setLaunchAtLogin(_ value: Bool) {
        do { if value { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }; launchAtLogin = SMAppService.mainApp.status == .enabled }
        catch { self.error = "无法更新登录启动设置：\(error.localizedDescription)" }
    }
}
