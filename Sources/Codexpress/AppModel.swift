import AppKit
import Foundation
import SwiftUI
import ServiceManagement

@MainActor final class AppModel: ObservableObject {
    static let shared = AppModel()
    @Published private(set) var settings = AppSettings()
    @Published private(set) var configuredModelIDs: [String] = []
    @Published private(set) var modelInformation: [String: JSONValue] = [:]
    @Published private(set) var modelInformationRefreshing = false
    @Published private(set) var modelInformationError: String?
    @Published private(set) var modelSettingsRevision = ModelSettingsRevision.empty
    @Published private(set) var localModelSettingsSyncError: String?
    @Published private(set) var status: JSONValue = .null
    @Published private(set) var usage: JSONValue = .null
    @Published private(set) var usageRefreshing = false
    @Published private(set) var usageError: String?
    @Published private(set) var usageUpdatedAt: Date?
    @Published var usageSelection = UsageSelection()
    @Published var usageActivityPeriod: UsageActivityPeriod = .month
    @Published var usagePerformanceSourceID = ""
    @Published private var usageAnalyticsCache: [UsageQuery.CacheKey: UsageAnalyticsSnapshot] = [:]
    @Published private(set) var historyStatus: JSONValue = .null
    @Published var error: String?
    @Published var notice: String?
    @Published private(set) var busy = false
    @Published private(set) var running = false
    @Published private(set) var gatewayVersion: String?
    @Published private(set) var installedGatewayUpdate: GatewayExecutable.Update?
    @Published private var pendingGatewayUpdate: GatewayUpdate?
    var gatewayUpdateWaiting: Bool { pendingGatewayUpdate != nil }
    @Published private(set) var settingsWindowActive = false
    @Published var loginSource: String?
    @Published var loginFlow: String?
    @Published var callbackURL = ""
    @Published var loginMessage: String?
    @Published var preview: JSONValue = .null
    @Published var creditResetRequest: CreditResetRequest?
    @Published var consuming = Set<String>()
    @Published private(set) var launchAtLogin = false
    @Published private(set) var metadataLoadFailed = false
    @Published private(set) var pendingSync = false
    @Published private(set) var syncing = false
    @Published private(set) var syncError: String?
    @Published private(set) var refreshingModels = Set<String>()
    @Published var refreshingQuotas = Set<String>()
    @Published private(set) var apiBalances: [String: JSONValue] = [:]
    @Published private(set) var refreshingBalances = Set<String>()
    @Published private(set) var accountRefreshGeneration = 0
    @Published private(set) var modelErrors: [String: String] = [:]
    @Published private(set) var codexConnectionPhase: String?
    @Published private(set) var configurationBackups: [JSONValue] = []
    @Published private(set) var credentialStorage: JSONValue = .null
    @Published private(set) var environmentSyncWorking = false
    @Published private(set) var environmentSynced = false
    @Published private(set) var environmentSyncError: String?
    let directory: URL
    let gatewayManagement: RemoteGatewayManagement
    let environmentAccess: ChatbotEnvironmentAccess
    private let metadata: MetadataStore
    var credentialSnapshot = CredentialSnapshot()
    let client: GatewayClient
    private var helper: Process?
    private var stagedGateway: URL?
    private var environmentSyncTask: Task<Void, Never>?
    private var environmentSyncGeneration: UInt64 = 0
    private var backgroundSyncTask: Task<Void, Never>?
    private var backgroundSyncSleepTask: Task<Void, Never>?
    private var backgroundSyncBypassDebounce = false
    private var backgroundSyncGeneration: UInt64 = 0
    private var backgroundSyncNeedsHistory = false
    private var backgroundSyncSuccessNotice: String?
    private var lastSyncedHistoryCache: HistoryCacheSettings?
    var showsPendingSyncWarning: Bool { pendingSync && (!running || (!syncing && syncError != nil)) }
    var hasPendingBackgroundSync: Bool { backgroundSyncTask != nil || syncing }
    private struct GatewayUpdate {
        let version: String
        let process: Process
        let candidate: URL
    }
    private var logPipe: Pipe?
    private var timerTask: Task<Void, Never>?
    private var accountTimerTask: Task<Void, Never>?
    private var officialAccountTimerTask: Task<Void, Never>?
    private var officialBackgroundWakeAt: Date?
    private var officialBackgroundStartedAt = Date()
    private var restarting = false
    private var restartCount = 0
    private var shuttingDown = false
    @Published var operations: [String: ResetOperation] = [:]
    var officialRefreshSchedule = OfficialRefreshSchedule()
    var officialQueries: [String: (includeCredits: Bool, task: Task<Void, Error>)] = [:]
    private var weeklyResetRevocationPending = false
    var controlResetPollTask: Task<Void, Never>?
    var controlResetPollingError: String?
    var presentedControlResetRequest: CreditResetRequest?
    private var startInProgress = false
    private var starting = false
    var lastModelRefresh: [String: Date] = [:]
    private var modelRefreshTask: Task<Void, Never>?
    private var usageDays = 30
    private var usageSourceID: String?
    private var usageModelID: String?
    private var usageAnalyticsRecency: [UsageQuery.CacheKey] = []
    private var usageAnalyticsTasks: [UsageQuery.CacheKey: Task<JSONValue, Error>] = [:]
    private var lastPollingError: String?
    private var balanceWarnings: BalanceWarnings?
    var sources: [Source] { settings.sources.sorted { $0.priority < $1.priority } }
    var baseURL: String { status["base_url"].string ?? "http://127.0.0.1:\(settings.port)/v1" }
    var routingMode: String { settings.mode }
    var fixedSourceID: String? { settings.fixed_source_id }
    var fixedName: String { sources.first { $0.id == fixedSourceID }?.name ?? "未选择" }
    var portNeedsRestart: Bool { status["port"].number.map { Int($0) != settings.port } ?? false }
    var connectionMode: String { status["connection_mode"].string ?? "unmanaged" }
    var localModelSettingsNeedsSync: Bool {
        localModelSettingsSyncError != nil || (connectionMode != "unmanaged" && (pendingSync || settings.localCodexAppliedModelSettingsRevision != modelSettingsRevision))
    }
    var showsLocalModelSettingsWarning: Bool { connectionMode != "official" && localModelSettingsNeedsSync }
    var localModelSettingsStatus: String {
        if connectionMode == "official" { return "本地 Codex 当前直接连接官方。" }
        if let failure = localModelSettingsSyncError { return "本地 Codex 模型设置待同步：\(failure)" }
        if connectionMode == "unmanaged" { return "本地 Codex 未连接，请先完成接入。" }
        if localModelSettingsNeedsSync { return "模型设置已变更，请手动同步到本地 Codex。" }
        return "本地模型配置已更新；重新载入 Codex 或下次启动后新建会话生效。"
    }
    var credentialStorageMode: String { credentialStorage["mode"].string ?? "keychain" }
    var canManageCredentials: Bool { helper?.isRunning == true && !metadataLoadFailed }
    var localCredentialFile: URL {
        let standard = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Codexpress", isDirectory: true)
        let home = directory.standardizedFileURL == standard.standardizedFileURL ? FileManager.default.homeDirectoryForCurrentUser : directory
        return home.appendingPathComponent(".codexpress/credentials.json")
    }
    init(stateDirectory: URL? = nil, environmentAccess: ChatbotEnvironmentAccess? = nil) {
        let override = ProcessInfo.processInfo.environment["CODEXPRESS_STATE_DIR"]
        directory = stateDirectory ?? override.map { URL(fileURLWithPath: $0, isDirectory: true) } ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Codexpress", isDirectory: true)
        gatewayManagement = RemoteGatewayManagement(directory: directory)
        if let environmentAccess {
            self.environmentAccess = environmentAccess
        } else if stateDirectory == nil && override == nil {
            self.environmentAccess = .live
        } else {
            self.environmentAccess = ChatbotEnvironmentAccess(homeDirectory: directory.appendingPathComponent("env-home", isDirectory: true), launchctl: .noop)
        }
        metadata = MetadataStore(directory: directory)
        client = GatewayClient(socketPath: directory.appendingPathComponent("control.sock").path)
        do {
            let loaded = try metadata.load()
            modelSettingsRevision = try ModelSettingsRevision.value(for: loaded)
            settings = loaded
        } catch { metadataLoadFailed = true; self.error = "来源文件无法读取，已进入只读保护。原文件未改动，请从备份修复后重新启动应用。" }
        environmentSynced = self.environmentAccess.inspect().synced
        configuredModelIDs = Self.modelIDs(in: settings)
        do {
            balanceWarnings = try BalanceWarnings(directory: directory)
            if !metadataLoadFailed { try balanceWarnings?.retain(sources: settings.sources) }
        } catch { notice = "无法读取余额预警记录，余额预警暂不可用。"; balanceWarnings = nil }
        // Display only; model updates require an existing managed attachment.
        if settings.codexHome.isEmpty { settings.codexHome = ProcessInfo.processInfo.environment["CODEX_HOME"] ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".codex").path }
        do {
            operations = try ResetOperationStore(directory: directory).load()
        } catch { self.error = "重置卡操作记录读取失败，已停用手动提交。"; consuming.insert("all") }
        launchAtLogin = SMAppService.mainApp.status == .enabled
    }
    func start() async {
        guard !gatewayManagement.remoteSelected, timerTask == nil, !starting else { return }
        starting = true
        defer { starting = false }
        await startHelper()
        timerTask = Task { [weak self] in
            var tick = 0
            while !Task.isCancelled {
                let interval = self?.settingsWindowActive == true || self?.gatewayHasActivity == true || self?.gatewayUpdateWaiting == true ? 1 : 3
                try? await Task.sleep(for: .seconds(interval))
                guard !Task.isCancelled, let self else { break }
                tick = (tick + 1) % 5
                await self.poll(refreshCredentials: tick == 0)
                if tick == 0 { await self.refreshInstalledGatewayUpdate() }
                await self.applyPendingGatewayUpdateIfIdle()
            }
        }
        accountTimerTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(1800)) } catch { break }
                guard !Task.isCancelled, let self else { break }
                await self.refreshAccountInfo(sourceIDs: self.sources.filter(\.supportsBalanceQuery).map(\.id), foreground: false)
            }
        }
        officialBackgroundStartedAt = Date()
        scheduleOfficialAccountRefresh()
    }
    func startHelper(port: Int? = nil, expectedVersion: String? = nil, requiredEntries: [String] = []) async {
        guard !metadataLoadFailed, !shuttingDown else { return }
        guard !startInProgress else { return }
        startInProgress = true
        defer { startInProgress = false }
        var verifiedProcessID: Int32?
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
            // Do not adopt a possibly unrelated live helper without the metadata we own.
            if helper?.isRunning != true {
                if (try? await client.call("status")) != nil { throw AppError.message("此状态目录已有网关运行，已拒绝接管其他实例。请先退出另一个 Codexpress 实例。") }
                guard !shuttingDown else { return }
                if stagedGateway == nil {
                    let source = try ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"].map { URL(fileURLWithPath: $0) } ?? GatewayExecutable.bundled()
                    stagedGateway = try gatewayExecutables.stage(source, replacing: "gateway-a")
                }
                guard let executable = stagedGateway else { throw AppError.message("未找到网关可执行文件。") }
                let process = Process(); process.executableURL = executable
                process.arguments = ["--state-dir", directory.path, "--port", String(port ?? settings.port)]
                if expectedVersion != nil { process.arguments?.append("--hold-ingress") }
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
                process.terminationHandler = { [weak self] stopped in
                    let pid = stopped.processIdentifier
                    Task { @MainActor in await self?.helperStopped(pid: pid) }
                }
            }
            for _ in 0..<30 {
                if let result = try? await client.call("status") {
                    guard let child = helper, child.isRunning, result["process_id"].number == Double(child.processIdentifier) else { throw AppError.message("网关实例不匹配，已拒绝同步来源。") }
                    status = result; verifiedProcessID = child.processIdentifier; break
                }
                guard helper?.isRunning == true else { throw AppError.message("网关启动后退出，请检查端口是否被占用。") }
                try? await Task.sleep(for: .milliseconds(100))
            }
            guard verifiedProcessID != nil else { throw AppError.message("网关未能启动。请检查端口冲突和受保护的 gateway.log。") }
            let info = try await client.call("gateway.update.info")
            if let expectedVersion {
                guard info["protocol"].number == 1, info["version"].string == expectedVersion else {
                    throw AppError.message("新版网关与当前应用不兼容。")
                }
            }
            running = true
            lastSyncedHistoryCache = nil
            credentialStorage = try await client.call("credentials.storage.status")
            credentialSnapshot = CredentialSnapshot()
            try await loadCredentialSnapshot(settings.sources.map(\.id))
            try await sync()
            self.error = nil
            for entry in requiredEntries {
                guard try await client.call(entry)["running"].bool == true else {
                    throw AppError.message("网关未能恢复原先运行的接入端口。")
                }
            }
            if expectedVersion != nil { _ = try await client.call("gateway.update.release") }
            gatewayVersion = info["version"].string
            await poll()
            _ = await syncChatbotEnvironmentIfEnabled(showSuccessNotice: false)
        } catch {
            // A rejected sync still leaves our verified helper available for
            // repairing source settings. Never adopt an unverified instance.
            running = helper?.isRunning == true && helper?.processIdentifier == verifiedProcessID
            if running { pendingSync = true; syncError = error.localizedDescription }
            self.error = error.localizedDescription
        }
    }
    private func helperStopped(pid: Int32) async {
        guard helper?.processIdentifier == pid else { return }
        running = false
        guard !shuttingDown, !restarting else { return }
        guard restartCount < 3 else { error = "网关反复退出，已停止自动重启。请检查本地端口后手动重试。"; return }
        restarting = true; restartCount += 1
        try? await Task.sleep(for: .seconds(restartCount))
        await startHelper(); restarting = false
    }
    private var gatewayExecutables: GatewayExecutable {
        GatewayExecutable(directory: directory.appendingPathComponent("gateway-runtime", isDirectory: true))
    }
    private func refreshInstalledGatewayUpdate() async {
        let update = gatewayVersion.flatMap { try? GatewayExecutable.installedUpdate(after: $0) }
        guard update != installedGatewayUpdate else { return }
        // A bundle being copied may expose its version before its signed contents are ready.
        if let update, await Task.detached(priority: .utility, operation: {
            (try? GatewayExecutable.validateInstalledBundle()) != nil
        }).value {
            installedGatewayUpdate = update
        } else {
            installedGatewayUpdate = nil
        }
    }
    func updateGateway() {
        guard pendingGatewayUpdate == nil else { return }
        perform {
            guard self.running, !self.restarting, let old = self.helper, old.isRunning,
                  let previous = self.stagedGateway, let currentVersion = self.gatewayVersion else {
                throw AppError.message("请先启动网关。")
            }
            guard let update = try GatewayExecutable.installedUpdate(after: currentVersion) else {
                self.notice = "当前没有可应用的新版网关。请先替换磁盘上的 Codexpress.app。"
                return
            }
            guard !update.requiresRestart else { throw AppError.message("此版本需重启 Codexpress 后生效。") }
            try GatewayExecutable.validateInstalledBundle()
            let candidate = try self.gatewayExecutables.stage(GatewayExecutable.bundled(), replacing: previous.lastPathComponent == "gateway-a" ? "gateway-b" : "gateway-a")
            try GatewayExecutable.validate(candidate)
            guard try GatewayExecutable.installedUpdate(after: currentVersion) == update else {
                throw AppError.message("磁盘上的 Codexpress.app 已变更，请重新应用更新。")
            }
            self.pendingGatewayUpdate = GatewayUpdate(version: update.version, process: old, candidate: candidate)
        }
    }
    func cancelGatewayUpdate() {
        if !restarting { pendingGatewayUpdate = nil }
    }
    private var gatewayUpdateUIIdle: Bool {
        loginFlow == nil && creditResetRequest == nil && consuming.subtracting(["all"]).isEmpty
    }
    private func applyPendingGatewayUpdateIfIdle() async {
        guard !shuttingDown, let update = pendingGatewayUpdate else { return }
        guard helper === update.process, update.process.isRunning else {
            pendingGatewayUpdate = nil
            error = "网关已停止或重新启动，请重新应用更新。"
            return
        }
        guard running, !busy, !restarting, !startInProgress, gatewayUpdateUIIdle,
              status["inflight"].number == 0, let currentPort = status["port"].number,
              let previous = stagedGateway, let currentVersion = gatewayVersion else { return }
        busy = true
        defer { busy = false }
        do {
            var requiredEntries: [String] = []
            for entry in ["chatbot.status", "images.status", "control.status"] {
                if try await client.call(entry)["running"].bool == true { requiredEntries.append(entry) }
            }
            guard !shuttingDown, pendingGatewayUpdate != nil, gatewayUpdateUIIdle else { return }
            guard helper === update.process, update.process.isRunning else {
                throw AppError.message("网关已停止或重新启动，请重新应用更新。")
            }
            restarting = true
            defer { restarting = false }
            do {
                guard let stopping = try await client.call("gateway.update.stop_when_idle")["stopping"].bool else {
                    throw AppError.message("无法确认网关是否已空闲。")
                }
                guard stopping else { return }
            } catch {
                if !update.process.isRunning { await startHelper(port: Int(currentPort)) }
                throw error
            }
            pendingGatewayUpdate = nil
            guard !shuttingDown else { return }
            try await waitForGatewayExit(update.process)
            guard !shuttingDown else { return }
            try await switchGateway(to: update, replacing: previous, version: currentVersion, port: Int(currentPort), requiredEntries: requiredEntries)
        } catch {
            pendingGatewayUpdate = nil
            if !shuttingDown { self.error = error.localizedDescription }
        }
    }
    private func switchGateway(to update: GatewayUpdate, replacing previous: URL, version currentVersion: String, port: Int, requiredEntries: [String]) async throws {
        stagedGateway = update.candidate
        await startHelper(port: port, expectedVersion: update.version, requiredEntries: requiredEntries)
        if running, !pendingSync, gatewayVersion == update.version, helper?.isRunning == true {
            restartCount = 0
            notice = "网关已更新至 \(update.version)。"
            installedGatewayUpdate = nil
            return
        }
        guard !shuttingDown else { return }
        let failure = error ?? "新版网关未就绪。"
        stagedGateway = previous
        if let failed = helper { try await waitForGatewayExit(failed, terminate: true) }
        guard !shuttingDown else { return }
        await startHelper(port: port, expectedVersion: currentVersion, requiredEntries: requiredEntries)
        if running, !pendingSync, gatewayVersion == currentVersion, helper?.isRunning == true {
            throw AppError.message("更新失败，已恢复网关 \(currentVersion)：\(failure)")
        }
        if let failed = helper { try await waitForGatewayExit(failed, terminate: true) }
        throw AppError.message("更新失败，旧网关未能恢复：\(error ?? failure)")
    }
    private func waitForGatewayExit(_ process: Process, terminate: Bool = false) async throws {
        if terminate && process.isRunning { process.terminate() }
        for _ in 0..<30 where process.isRunning { try await Task.sleep(for: .milliseconds(100)) }
        guard !process.isRunning else { throw AppError.message("网关未退出，未继续切换版本。") }
        running = false
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
    func stop() { shuttingDown = true; cancelBackgroundSync(); timerTask?.cancel(); accountTimerTask?.cancel(); officialAccountTimerTask?.cancel(); modelRefreshTask?.cancel(); controlResetPollTask?.cancel(); helper?.terminate(); logPipe?.fileHandleForReading.readabilityHandler = nil }
    func retry() {
        restartCount = 0
        if running, helper?.isRunning == true {
            perform {
                try await self.sync()
                self.error = nil
            }
        } else {
            Task { await startHelper() }
        }
    }
    func requireWritableMetadata() throws {
        guard !metadataLoadFailed else { throw AppError.message("来源文件无法读取，已禁止保存。请先从备份修复 sources.json，再重新启动应用；原文件保持不变。") }
    }
    func persistSettings(_ next: AppSettings) throws {
        try requireWritableMetadata()
        var next = next
        if next.codexHome != settings.codexHome { next.localCodexAppliedModelSettingsRevision = nil }
        if let enrolled = next.autoResetWeeklyAccounts {
            let retained = Set(next.weeklyResetAccountIDs)
            next.autoResetWeeklyAccounts = enrolled.filter { retained.contains($0.key) }
        }
        let revision: String
        if next.modelCompaction == settings.modelCompaction, next.modelPolicies == settings.modelPolicies {
            revision = modelSettingsRevision
        } else {
            revision = try ModelSettingsRevision.value(for: next)
        }
        try metadata.save(next)
        if let prior = settings.autoResetWeeklyAccounts,
           prior.keys.contains(where: { next.autoResetWeeklyAccounts?[$0] == nil }) {
            weeklyResetRevocationPending = true
        }
        for source in settings.sources where !next.sources.contains(where: {
            $0.id == source.id && $0.kind == source.kind && $0.base_url == source.base_url
                && $0.proxy == source.proxy && $0.account_id == source.account_id
        }) { apiBalances[source.id] = nil }
        if next.codexHome != settings.codexHome { configurationBackups = [] }
        if next.codexHome != settings.codexHome || revision != modelSettingsRevision { localModelSettingsSyncError = nil }
        let previousOfficial = settings.sources.filter(\.official)
        let nextOfficial = next.sources.filter(\.official)
        let invalidated = previousOfficial.filter { previous in
            !nextOfficial.contains { $0.id == previous.id && $0.hasSameOfficialBinding(as: previous) }
        }
        for source in invalidated { officialRefreshSchedule.reset(source.id) }
        let officialScheduleChanged = !invalidated.isEmpty || next.officialQuotaRefreshInterval != settings.officialQuotaRefreshInterval
            || Set(previousOfficial.filter(\.enabled).map(\.id)) != Set(nextOfficial.filter(\.enabled).map(\.id))
        if next.sources.map(\.models) != settings.sources.map(\.models)
            || next.sources.map(\.official) != settings.sources.map(\.official)
            || next.modelPolicies?.map(\.model) != settings.modelPolicies?.map(\.model)
            || next.modelCompaction?.map(\.model) != settings.modelCompaction?.map(\.model) {
            configuredModelIDs = Self.modelIDs(in: next)
        }
        settings = next
        modelSettingsRevision = revision
        if officialScheduleChanged { scheduleOfficialAccountRefresh() }
        do { try balanceWarnings?.retain(sources: next.sources) }
        catch { notice = "无法更新余额预警记录，请检查应用数据目录的写入权限。" }
    }
    private func updateSettingsAndSync(_ update: (inout AppSettings) -> Void) async throws {
        var next = settings
        update(&next)
        try persistSettings(next)
        try await sync()
    }
    private func updateSettingsImmediately(
        syncHistory: Bool = false,
        _ mutate: (inout AppSettings) throws -> Void
    ) {
        guard !metadataLoadFailed, !busy else { return }
        do {
            var next = settings
            try mutate(&next)
            try persistSettings(next)
            error = nil
            scheduleBackgroundSync(syncHistory: syncHistory)
        } catch {
            self.error = error.localizedDescription
        }
    }
    private func cancelBackgroundSync() {
        backgroundSyncSleepTask?.cancel()
        backgroundSyncSleepTask = nil
        backgroundSyncTask?.cancel()
        backgroundSyncTask = nil
        backgroundSyncBypassDebounce = false
        syncing = false
        lastSyncedHistoryCache = nil
    }
    private func awaitPendingBackgroundSync() async {
        guard backgroundSyncTask != nil else { return }
        backgroundSyncBypassDebounce = true
        backgroundSyncSleepTask?.cancel()
        await backgroundSyncTask?.value
    }
    private func scheduleBackgroundSync(syncHistory: Bool = false, successNotice: String? = nil) {
        backgroundSyncGeneration &+= 1
        backgroundSyncNeedsHistory = backgroundSyncNeedsHistory || syncHistory
        if let successNotice { backgroundSyncSuccessNotice = successNotice }
        pendingSync = true
        syncError = nil
        guard running, !shuttingDown else { return }
        if backgroundSyncTask == nil {
            backgroundSyncTask = Task { @MainActor [weak self] in
                await self?.runBackgroundSyncLoop()
            }
        }
    }
    private func waitForBackgroundSyncDebounce() async {
        var observed = backgroundSyncGeneration
        for _ in 0..<5 {
            if Task.isCancelled || backgroundSyncBypassDebounce { break }
            let sleepTask = Task<Void, Never> {
                _ = try? await Task.sleep(nanoseconds: 180_000_000)
            }
            backgroundSyncSleepTask = sleepTask
            await sleepTask.value
            backgroundSyncSleepTask = nil
            if Task.isCancelled || backgroundSyncBypassDebounce || backgroundSyncGeneration == observed {
                break
            }
            observed = backgroundSyncGeneration
        }
        backgroundSyncBypassDebounce = false
    }
    private func runBackgroundSyncLoop() async {
        defer {
            syncing = false
            backgroundSyncTask = nil
        }
        while running && pendingSync && !Task.isCancelled && !shuttingDown {
            await waitForBackgroundSyncDebounce()
            if Task.isCancelled || !running || shuttingDown { break }
            let targetGeneration = backgroundSyncGeneration
            let needsHistory = backgroundSyncNeedsHistory
            let notice = backgroundSyncSuccessNotice
            backgroundSyncNeedsHistory = false
            backgroundSyncSuccessNotice = nil
            syncing = true
            do {
                try await performGatewaySync(syncHistory: needsHistory, refreshModelsInfo: false)
                syncing = false
                if backgroundSyncGeneration == targetGeneration {
                    pendingSync = weeklyResetRevocationPending
                    syncError = nil
                    error = nil
                    if let notice { self.notice = notice }
                    break
                }
            } catch {
                syncing = false
                if !Task.isCancelled && !shuttingDown {
                    pendingSync = true
                    syncError = error.localizedDescription
                    self.error = error.localizedDescription
                }
                break
            }
        }
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
    func loadCredentialSnapshot(_ ids: [String]) async throws {
        guard !ids.isEmpty else { return }
        let previous = credentialSnapshot
        let response = try await client.call("credentials.read", .object(["source_ids": .array(ids.map(JSONValue.string))]))
        try credentialSnapshot.apply(response, requested: ids)
        for id in ids where previous.load(id) != credentialSnapshot.load(id) { apiBalances[id] = nil }
    }
    private func mergeCredentials(_ data: JSONValue) async throws {
        try requireWritableMetadata()
        let changed = data["updated_secrets"].array.compactMap { item -> String? in
            guard let id = item["source_id"].string, settings.sources.contains(where: { $0.id == id && $0.official }) else { return nil }
            return credentialSnapshot.load(id) == item["secret"] ? nil : id
        }
        try await loadCredentialSnapshot(Array(Set(changed)).sorted())
        var next = settings
        next.sources = try OfficialCredentialMirror.mergedSources(settings.sources, response: data, snapshot: credentialSnapshot)
        if next.sources != settings.sources { try persistSettings(next) }
    }
    @discardableResult func refreshCredentials() async throws -> JSONValue {
        let notification = try await client.call("credentials")
        try await mergeCredentials(notification)
        return notification
    }
    func updateStatus() async throws {
        let balanceSources = settings.sources
        let balanceCredentials = credentialSnapshot
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
            if let durable = credentialSnapshot.load(next.sources[i].id), let account = durable["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
            else if let live = fresh["sources"].array.first(where: { $0["id"].string == next.sources[i].id }), let account = live["account_id"].string, !account.isEmpty { next.sources[i].account_id = account }
        }
        if next.sources != settings.sources { try persistSettings(next) }
        var officialScheduleChanged = false
        for live in status["sources"].array {
            guard let id = live["id"].string,
                  let source = settings.sources.first(where: { $0.id == id && $0.enabled }),
                  source.matchesOfficialStatus(live) else { continue }
            if officialRefreshSchedule.observeQuota(id, snapshot: live["quota"]) { officialScheduleChanged = true }
        }
        if officialScheduleChanged { scheduleOfficialAccountRefresh() }
        for before in balanceSources where before.supportsBalanceQuery {
            guard let current = settings.sources.first(where: { $0.id == before.id }), current.sameConfiguration(as: before),
                  balanceCredentials.load(before.id) == credentialSnapshot.load(before.id) else { continue }
            let live = fresh["sources"].array.first(where: { $0["id"].string == before.id }) ?? .null
            guard live == .null || (live["base_url"].string == current.base_url
                && live["proxy"]["mode"].string == current.proxy.mode && live["proxy"]["url"].string == current.proxy.url) else {
                apiBalances[before.id] = nil
                continue
            }
            let balance = live["balance"]
            guard (apiBalances[before.id] ?? .null) != balance else { continue }
            apiBalances[before.id] = balance == .null ? nil : balance
            await checkBalanceWarning(source: current, balance: balance, credential: credentialSnapshot.load(current.id))
        }
    }
    func setSettingsWindowActive(_ active: Bool) {
        if settingsWindowActive != active { settingsWindowActive = active }
    }
    func sync() async throws {
        if backgroundSyncTask != nil {
            if !syncing {
                backgroundSyncSleepTask?.cancel()
                backgroundSyncSleepTask = nil
                backgroundSyncTask?.cancel()
                backgroundSyncTask = nil
                backgroundSyncBypassDebounce = false
            } else {
                await backgroundSyncTask?.value
            }
        }
        let targetGeneration = backgroundSyncGeneration
        let notice = backgroundSyncSuccessNotice
        backgroundSyncNeedsHistory = false
        backgroundSyncSuccessNotice = nil
        try await performGatewaySync(syncHistory: true, refreshModelsInfo: true)
        if backgroundSyncGeneration == targetGeneration {
            pendingSync = weeklyResetRevocationPending
            syncError = nil
            if let notice { self.notice = notice }
        }
    }
    private func performGatewaySync(syncHistory: Bool, refreshModelsInfo: Bool) async throws {
        try requireWritableMetadata()
        pendingSync = true
        // Always retrieve helper-authoritative rotated tokens before building a source snapshot.
        let notification = try await refreshCredentials()
        let helperOwnedIDs = Set(notification["updated_secrets"].array.lazy.compactMap { $0["source_id"].string })
        var values: [JSONValue] = []
        var next = settings
        for index in next.sources.indices {
            let sourceID = next.sources[index].id
            let secret = credentialSnapshot.load(sourceID) ?? (next.sources[index].official ? .object(["access_token": .string(""), "refresh_token": .string(""), "id_token": .string(""), "expires_at": .number(0)]) : .object([:]))
            // After a crash, durable storage may be newer than the metadata mirror.
            if next.sources[index].official, let account = secret["account_id"].string, !account.isEmpty { next.sources[index].account_id = account }
            guard case .object(var value) = try JSONValue.from(next.sources[index]) else { continue }
            value["secret"] = next.sources[index].official && helperOwnedIDs.contains(sourceID) ? .object([:]) : secret
            values.append(.object(value))
        }
        if next.sources != settings.sources { try persistSettings(next) }
        let enrolled = settings.weeklyResetAccountIDs
        let historySettings = settings.historyCacheSettings
        do {
            let response = try await client.call("sync", .object(["sources": .array(values), "mode": .string(settings.mode), "fixed_source_id": settings.fixed_source_id.map(JSONValue.string) ?? .null, "official_low_quota_percent":.number(Double(settings.lowQuotaPercent)), "auto_reset_weekly_accounts":.array(enrolled.map(JSONValue.string)), "degraded_context_fallback":.bool(settings.degradedContextFallback ?? true), "load_balance":.bool(settings.loadBalanceEnabled), "idle_return_minutes":.number(Double(settings.idleReturnDelay)), "upstream_attempts":.number(Double(settings.upstreamAttemptLimit)), "upstream_retry_delay_seconds":.number(Double(settings.upstreamRetryDelay)), "model_compaction":try JSONValue.from(settings.modelCompaction ?? []), "model_policies":try JSONValue.from(settings.modelPolicies ?? [])]))
            try await mergeCredentials(response)
            if syncHistory || lastSyncedHistoryCache != historySettings {
                historyStatus = try await client.call("history.configure", historyPayload(historySettings))
                lastSyncedHistoryCache = historySettings
            }
            try await updateStatus()
            if enrolled == settings.weeklyResetAccountIDs { weeklyResetRevocationPending = false }
            if refreshModelsInfo { await refreshModelInformation() }
        } catch {
            pendingSync = true
            syncError = error.localizedDescription
            throw error
        }
    }
    func poll(refreshCredentials: Bool = true) async {
        guard running, !busy, !restarting, !syncing else { return }
        do {
            try await updateStatus()
            if refreshCredentials {
                try await self.refreshCredentials()
                if let id = loginSource, let flow = loginFlow {
                    let result = try await client.call("official.login.status", .object(["source_id": .string(id), "flow_id": .string(flow)]))
                    if loginSource == id && loginFlow == flow {
                        if result["state"].string == "complete" { try await finishLogin(id) }
                        else if result["state"].string == "failed" { loginMessage = result["error"].string ?? "登录失败，请重新发起。"; loginFlow = nil }
                    }
                }
                scheduleModelRefresh()
            }
            if weeklyResetRevocationPending, !busy, !hasPendingBackgroundSync { try await sync() }
            reportPollingError(nil)
            scheduleControlResetPoll()
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
    func scheduleOfficialAccountRefresh() {
        guard timerTask != nil, !shuttingDown else { return }
        let delay = officialBackgroundDelay()
        let wakeAt = Date().addingTimeInterval(delay)
        // New failures can bring the wakeup forward; unrelated refreshes cannot postpone it.
        if let scheduled = officialBackgroundWakeAt, scheduled <= wakeAt { return }
        officialBackgroundWakeAt = wakeAt
        officialAccountTimerTask?.cancel()
        officialAccountTimerTask = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(delay)) } catch { return }
            guard !Task.isCancelled, let self else { return }
            self.officialBackgroundWakeAt = nil
            self.dispatchOfficialBackgroundRefresh()
            self.scheduleOfficialAccountRefresh()
        }
    }
    private func dispatchOfficialBackgroundRefresh() {
        guard running, !busy, !shuttingDown else { return }
        var slots = 4 - officialQueries.count
        guard slots > 0 else { return }
        let interval = Double(settings.officialQuotaRefreshInterval) * 60
        // Give failed accounts priority without letting a slow account hold up other retries.
        for pass in 0..<2 {
            for source in settings.sources where source.enabled && source.official {
                let id = source.id
                guard officialRefreshSchedule.isRetrying(id) == (pass == 0), !refreshingQuotas.contains(id),
                      officialRefreshSchedule.isDue(id, interval: interval, since: officialBackgroundStartedAt),
                      credentialSnapshot.load(id)?["access_token"].string?.isEmpty == false else { continue }
                Task { await refreshAccount(id, foreground: false) }
                slots -= 1
                if slots == 0 { return }
            }
        }
    }
    private func officialBackgroundDelay() -> TimeInterval {
        officialRefreshSchedule.backgroundDelay(for: settings.sources.lazy.filter { $0.enabled && $0.official }.map(\.id),
            since: officialBackgroundStartedAt, interval: Double(settings.officialQuotaRefreshInterval) * 60)
    }
    func refreshAccountInfo(sourceIDs: [String]? = nil, foreground: Bool = true) async {
        guard running, !busy, !Task.isCancelled else { return }
        let selected = sources.filter { $0.enabled && ($0.official || $0.supportsBalanceQuery) && (sourceIDs?.contains($0.id) ?? true) }
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
    func refreshAccount(_ id: String, foreground: Bool, force: Bool = false) async {
        guard running, !busy, !shuttingDown, !Task.isCancelled, let before = settings.sources.first(where: { $0.id == id }), before.enabled || foreground else { return }
        if before.official {
            guard before.enabled, !refreshingQuotas.contains(id), officialRefreshSchedule.isDue(id, foreground: foreground, interval: Double(settings.officialQuotaRefreshInterval) * 60, since: officialBackgroundStartedAt), let secret = credentialSnapshot.load(id), secret["access_token"].string?.isEmpty == false else { return }
            let includeCredits = !officialRefreshSchedule.isRetrying(id) && officialRefreshSchedule.needsCredits(id)
            try? await queryOfficial(id, includeCredits: includeCredits)
        } else if before.supportsBalanceQuery {
            guard !refreshingBalances.contains(id) else { return }
            if !force, let cached = apiBalances[id], (cached["failure_count"].number ?? 0) > 0,
               let retry = cached["next_retry_at"].number, retry > Date().timeIntervalSince1970 { return }
            let credential = credentialSnapshot.load(id)
            refreshingBalances.insert(id)
            defer { refreshingBalances.remove(id) }
            do {
                let balance = try await client.call("balance.query", .object(["source_id": .string(id), "force": .bool(force)]))
                guard running, let current = settings.sources.first(where: { $0.id == id }), current.base_url == before.base_url, current.proxy == before.proxy,
                      credentialSnapshot.load(id) == credential else { return }
                apiBalances[id] = balance
                await checkBalanceWarning(source: current, balance: balance, credential: credential)
            } catch {
                guard settings.sources.contains(where: { $0.id == id && $0.base_url == before.base_url && $0.proxy == before.proxy }),
                      credentialSnapshot.load(id) == credential else { return }
                if case .object(var cached) = apiBalances[id], cached["amount"]?.number != nil {
                    cached["stale"] = .bool(true)
                    cached["error"] = .string(error.localizedDescription)
                    apiBalances[id] = .object(cached)
                } else { apiBalances[id] = .object(["status": .string("unavailable"), "error": .string(error.localizedDescription)]) }
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
        Task {
            defer { self.busy = false }
            await self.awaitPendingBackgroundSync()
            do { try await action() } catch { self.error = error.localizedDescription }
        }
    }
    func saveSource(_ source: Source, apiKey: String) async throws {
        try requireWritableMetadata()
        try source.validate()
        var previous = settings.sources.first { $0.id == source.id }
        if let previous, previous.kind != source.kind { throw AppError.message("不能更改已有来源的类型，请新建来源。") }
        guard previous != nil || settings.sources.count < 200 else { throw AppError.message("最多支持 200 个来源。") }
        let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        if !source.official, key.isEmpty, credentialSnapshot.load(source.id) == nil { throw AppError.message("请输入 API 密钥。") }
        if source.official {
            try await refreshCredentials()
            try await updateStatus()
            previous = settings.sources.first { $0.id == source.id }
        }
        var source = source
        if source.official { source = source.preservingOfficialIdentity(current: previous, live: state(source.id)) }
        let connectionChanged = previous?.kind != source.kind || previous?.base_url != source.base_url
            || previous?.proxy != source.proxy || previous?.account_id != source.account_id
            || (!source.official && !key.isEmpty)
        try requireUniqueOfficialAccount(source)
        if !source.official, !key.isEmpty {
            _ = try await client.call("credentials.save_api", .object(["source_id": .string(source.id), "api_key": .string(key)]))
            try await loadCredentialSnapshot([source.id])
        }
        try await updateSettingsAndSync { next in
            if let i = next.sources.firstIndex(where: { $0.id == source.id }) { next.sources[i] = source }
            else { var item = source; item.priority = next.sources.count; next.sources.append(item) }
            if next.fixed_source_id == source.id && !source.enabled { next.mode = "auto"; next.fixed_source_id = nil }
            Self.applyDefaultDisabledPolicies(for: source, previousModels: previous?.models ?? [], in: &next)
        }
        if connectionChanged {
            modelErrors[source.id] = nil
            apiBalances[source.id] = nil
            lastModelRefresh[source.id] = nil
        }
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
            let secret = key.isEmpty ? credentialSnapshot.load(source.id) : .object(["api_key": .string(key)])
            guard let secret, secret["api_key"].string?.isEmpty == false else { throw AppError.message("请填写 API 密钥后获取模型。") }
            guard case .object(var value) = try JSONValue.from(source) else { throw AppError.message("来源配置无效。") }
            value["secret"] = secret
            payload = .object(["source": .object(value)])
        }
        let result = try await client.call("models.list", payload)
        let models = result["models"].array.compactMap(\.string)
        guard !models.isEmpty else { throw AppError.message("供应商未返回可用模型。") }
        await refreshModelInformation()
        return models
    }
    func scheduleModelRefresh(maximumAge: TimeInterval = 3600) {
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
        let credential = credentialSnapshot.load(id)
        refreshingModels.insert(id); lastModelRefresh[id] = Date(); modelErrors[id] = nil
        defer { refreshingModels.remove(id) }
        do {
            let models = try await fetchModels(for: before)
            // Discard a response if the editor, login or deletion changed its source.
            guard !Task.isCancelled, running, !busy, let index = settings.sources.firstIndex(where: { $0.id == id }), settings.sources[index] == before,
                  credentialSnapshot.load(id) == credential else { lastModelRefresh[id] = nil; return }
            guard before.models != models || before.automaticModelDiscovery != true else { return }
            try await updateSettingsAndSync { next in
                next.sources[index].automaticModelDiscovery = true
                next.sources[index].models = models
                Self.applyDefaultDisabledPolicies(for: next.sources[index], previousModels: before.models, in: &next)
            }
        } catch {
            if !Task.isCancelled, settings.sources.contains(where: { $0 == before }),
               credentialSnapshot.load(id) == credential { modelErrors[id] = error.localizedDescription }
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
        updateSettingsImmediately { next in
            if mode == "fixed", !next.sources.contains(where: { $0.id == fixed && $0.enabled }) { throw AppError.message("请选择已启用的固定来源。") }
            next.mode = mode; next.fixed_source_id = fixed
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
        updateSettingsImmediately { next in
            guard (1...99).contains(value) else { throw AppError.message("余量阈值须为 1–99%。") }
            next.officialLowQuotaPercent = value
        }
    }
    private func confirmedWeeklyResetAccount(for source: Source) -> String? {
        guard source.official, let account = source.account_id, !account.isEmpty,
              let current = settings.sources.first(where: { $0.id == source.id }),
              current.hasSameOfficialBinding(as: source), source.matchesOfficialStatus(state(source.id)) else { return nil }
        return account
    }
    func canChangeAutoResetWeeklyQuota(for source: Source) -> Bool {
        running && !busy && !metadataLoadFailed && loginSource != source.id && confirmedWeeklyResetAccount(for: source) != nil
    }
    func changeAutoResetWeeklyQuota(_ enabled: Bool, for source: Source) {
        updateSettingsImmediately { next in
            guard self.running, self.loginSource != source.id, let account = self.confirmedWeeklyResetAccount(for: source) else {
                throw AppError.message("账号或网络设置已变化，请刷新来源后重试。")
            }
            var enrolled = next.autoResetWeeklyAccounts ?? [:]
            enrolled[account] = enabled ? WeeklyResetEndpoint(source) : nil
            next.autoResetWeeklyAccounts = enrolled
        }
    }
    func changeContextFallback(_ enabled: Bool) {
        updateSettingsImmediately { $0.degradedContextFallback = enabled }
    }
    func changeLoadBalance(_ enabled: Bool) {
        updateSettingsImmediately { $0.loadBalance = enabled }
    }
    func changeIdleReturnDelay(_ minutes: Int) {
        updateSettingsImmediately { next in
            guard (0...10080).contains(minutes) else { throw AppError.message("闲置回切时间须为 0–10080 分钟。") }
            next.conversationStickyMinutes = minutes
        }
    }
    func changeUpstreamRetry(attempts: Int, delaySeconds: Int) {
        updateSettingsImmediately { next in
            guard (1...10).contains(attempts), (0...300).contains(delaySeconds) else {
                throw AppError.message("尝试次数须为 1–10，等待时间须为 0–300 秒。")
            }
            next.upstreamAttempts = attempts
            next.upstreamRetryDelaySeconds = delaySeconds
        }
    }
    func move(_ offsets: IndexSet, to destination: Int) {
        var order = sources.map(\.id); order.move(fromOffsets: offsets, toOffset: destination)
        updateSettingsImmediately { next in
            var reordered = order.compactMap { id in next.sources.first { $0.id == id } }
            guard reordered.count == next.sources.count else { throw AppError.message("来源已变化，请重新排序。") }
            for i in reordered.indices { reordered[i].priority = i }
            next.sources = reordered
        }
    }
    func toggle(_ source: Source) {
        var becameEnabled = false
        updateSettingsImmediately { next in
            guard let index = next.sources.firstIndex(where: { $0.id == source.id }) else {
                throw AppError.message("来源不存在，请刷新后重试。")
            }
            next.sources[index].enabled.toggle()
            becameEnabled = next.sources[index].enabled
            if next.fixed_source_id == source.id && !next.sources[index].enabled {
                next.mode = "auto"
                next.fixed_source_id = nil
            }
        }
        if becameEnabled {
            scheduleModelRefresh()
            accountPageOpened()
        }
    }
    func toggleShowInCmux(_ source: Source) {
        updateSettingsImmediately { next in
            guard let index = next.sources.firstIndex(where: { $0.id == source.id }) else {
                throw AppError.message("来源不存在，请刷新后重试。")
            }
            next.sources[index].show_in_cmux = !next.sources[index].showInCmux
        }
    }
    func state(_ id: String) -> JSONValue { status["sources"].array.first { $0["id"].string == id } ?? .null }
    var gatewayHasActivity: Bool { running && (status["inflight"].number ?? 0) > 0 }
    func sourceHasActivity(_ id: String) -> Bool { running && (state(id)["active_requests"].number ?? 0) > 0 }
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
    func cachedUsageAnalytics(_ query: UsageQuery?) -> UsageAnalyticsSnapshot? {
        guard let query else { return nil }
        return usageAnalyticsCache[query.cacheKey]
    }
    func usageAnalytics(_ query: UsageQuery, refresh: Bool = false) async throws -> JSONValue {
        let key = query.cacheKey
        if usageAnalyticsCache[key] != nil { touchUsageAnalytics(key) }
        if let task = usageAnalyticsTasks[key] {
            return try await task.value
        }
        if !refresh, let snapshot = usageAnalyticsCache[key], Date().timeIntervalSince(snapshot.updatedAt) < 5 {
            return snapshot.value
        }
        // This task owns the request and cache write, independently of any page's cancellation.
        let task = Task {
            defer { usageAnalyticsTasks[key] = nil }
            let value = try await client.call("usage.analytics", .object(query.payload))
            usageAnalyticsCache[key] = UsageAnalyticsSnapshot(query: query, value: value)
            touchUsageAnalytics(key)
            if usageAnalyticsCache.count > 8 {
                let oldest = usageAnalyticsRecency.removeFirst()
                usageAnalyticsCache.removeValue(forKey: oldest)
            }
            return value
        }
        usageAnalyticsTasks[key] = task
        return try await task.value
    }
    private func touchUsageAnalytics(_ key: UsageQuery.CacheKey) {
        usageAnalyticsRecency.removeAll { $0 == key }
        usageAnalyticsRecency.append(key)
    }
    func usageRequests(_ query: UsageQuery, status: String = "", search: String = "", before: Double? = nil) async throws -> JSONValue {
        var payload = query.payload
        payload["limit"] = .number(50)
        if !status.isEmpty { payload["status"] = .string(status) }
        if !search.isEmpty { payload["search"] = .string(search) }
        if let before { payload["before"] = .number(before) }
        return try await client.call("usage.requests", .object(payload))
    }
    func usageDetail(attemptID: String, bodyOffset: Double = 0, recordOnly: Bool = false) async throws -> JSONValue {
        try await client.call("usage.detail", .object(["attempt_id": .string(attemptID), "body_offset": .number(bodyOffset), "body_limit": .number(65_536), "record_only": .bool(recordOnly)]))
    }
    func savePreferences(port: Int, codexHome: String) {
        perform {
            guard (1024...65535).contains(port) else { throw AppError.message("端口须在 1024–65535 之间。") }
            let home = (codexHome.trimmingCharacters(in: .whitespacesAndNewlines) as NSString).expandingTildeInPath
            guard home.hasPrefix("/") else { throw AppError.message("请填写 Codex 配置目录，可使用绝对路径或 ~/ 开头的路径。") }
            let changed = self.settings.port != port
            if changed, self.running {
                let chatbot = try await self.client.call("chatbot.status")
                guard chatbot["port"].number != Double(port) else { throw AppError.message("此端口已用于聊天客户端接入，请选择不同端口。") }
                let images = try await self.client.call("images.status")
                guard images["port"].number != Double(port) else { throw AppError.message("此端口已用于图片生成，请选择不同端口。") }
                let control = try await self.client.call("control.status")
                guard control["port"].number != Double(port) else { throw AppError.message("此端口已用于管理 API，请选择不同端口。") }
            }
            if changed, FileManager.default.fileExists(atPath: self.directory.appendingPathComponent("config-attachment.json").path) { throw AppError.message("请先在「接入 → Codex」恢复原连接，再更改网关端口。") }
            var next = self.settings; next.port = port; if next.codexHome != home { next.legacyProviderOrigins = nil }; next.codexHome = home
            try self.persistSettings(next); self.preview = .null
            if changed { self.notice = "端口已保存，重新启动 Codexpress 后生效。" }
            else { self.notice = "设置已保存。" }
        }
    }
    func saveOfficialQuotaRefresh(minutes: String) {
        guard !metadataLoadFailed, !busy else { return }
        do {
            guard let value = Int(minutes.trimmingCharacters(in: .whitespacesAndNewlines)), value > 0 else {
                throw AppError.message("官方额度后台查询间隔须为正整数分钟。")
            }
            var next = settings; next.officialQuotaRefreshMinutes = value
            try persistSettings(next)
            error = nil
            notice = "官方额度后台查询间隔已保存，立即生效。"
        } catch {
            self.error = error.localizedDescription
        }
    }
    private func historyPayload(_ settings: HistoryCacheSettings) -> JSONValue {
        .object(["enabled": .bool(settings.enabled), "max_bytes": .number(Double(settings.maxBytes))])
    }
    func saveHistoryCache(_ preferences: HistoryCacheSettings) {
        updateSettingsImmediately(syncHistory: true) { next in
            _ = try HistoryCacheSettings.parse(enabled: preferences.enabled, gigabytes: String(Double(preferences.maxBytes) / 1_000_000_000))
            next.historyCache = preferences
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
    func refreshModelInformation() async {
        guard running, !modelInformationRefreshing else { return }
        modelInformationRefreshing = true
        defer { modelInformationRefreshing = false }
        do {
            let result = try await client.call("models.info")
            var information: [String: JSONValue] = [:]
            for item in result["models"].array {
                if let id = item["id"].string { information[id] = item }
            }
            if information != modelInformation { modelInformation = information }
            modelInformationError = nil
        } catch { modelInformationError = error.localizedDescription }
    }
    private static func modelIDs(in settings: AppSettings) -> [String] {
        var ids = Set(settings.sources.flatMap(\.models))
        ids.formUnion((settings.modelPolicies ?? []).map(\.model))
        ids.formUnion((settings.modelCompaction ?? []).map(\.model))
        if settings.sources.contains(where: \.official) { ids.insert("gpt-image-2") }
        return ids.sorted()
    }
    static func applyDefaultDisabledPolicies(for source: Source, previousModels: [String] = [], in settings: inout AppSettings) {
        guard source.defaultsDiscoveredModelsDisabled, source.discoversModels, !source.models.isEmpty else { return }
        let previousSet = Set(previousModels)
        var policies = settings.modelPolicies ?? []
        lazy var otherSources = settings.sources.lazy.filter { $0.id != source.id && !$0.defaultsDiscoveredModelsDisabled }.sorted { $0.priority < $1.priority }
        for modelID in source.models where !previousSet.contains(modelID) {
            let index = policies.firstIndex { $0.model == modelID }
            if let index, policies[index].sourceIDs != nil { continue }
            let sourceIDs = otherSources.compactMap { $0.models.isEmpty || $0.models.contains(modelID) ? $0.id : nil }
            if let index {
                policies[index].sourceIDs = sourceIDs
            } else {
                policies.append(ModelPolicy(model: modelID, sourceIDs: sourceIDs, clients: ModelPolicy.applicableClients(for: modelID)))
            }
        }
        if !policies.isEmpty { settings.modelPolicies = policies }
    }
    func setModelsEnabled(_ ids: [String], enabled: Bool) throws {
        try updateModelPolicies(successMessage: ids.count > 1 ? (enabled ? "已启用所选模型。" : "已停用所选模型。") : nil) { next in
            try ModelPolicy.setModelsEnabled(ids, enabled: enabled, in: &next)
        }
    }
    func setProviderModelsEnabled(sourceID: String, modelIDs: [String], enabled: Bool) throws {
        try updateModelPolicies(successMessage: modelIDs.count > 1 ? (enabled ? "已启用所选来源模型。" : "已停用所选来源模型。") : nil) { next in
            try ModelPolicy.setProviderModelsEnabled(sourceID: sourceID, modelIDs: modelIDs, enabled: enabled, in: &next)
        }
    }
    private func updateModelPolicies(successMessage: String?, mutate: (inout AppSettings) throws -> Void) throws {
        guard !busy else { throw AppError.message("请等待当前操作完成后重试。") }
        try requireWritableMetadata()
        var next = settings
        try mutate(&next)
        try persistSettings(next)
        preview = .null
        error = nil
        scheduleBackgroundSync(successNotice: successMessage)
        if !running {
            notice = "模型设置已保存在本机，启动网关后生效。"
        }
    }
    func saveModelSettings(model id: String, policy: ModelPolicy?, compaction: ModelCompactionRule?) async throws {
        guard !busy else { throw AppError.message("请等待当前操作完成后重试。") }
        busy = true
        defer { busy = false }
        try requireWritableMetadata()
        try ModelPolicy.validateModelID(id)
        guard policy == nil || policy?.model == id, compaction == nil || compaction?.model == id else {
            throw AppError.message("模型 ID 已变化，请重新编辑。")
        }
        let compaction = compaction.flatMap { $0.auto_compact_token_limit != nil || $0.context_window != nil ? $0 : nil }
        if let compaction {
            _ = try ModelCompactionRule.parse(model: id,
                threshold: compaction.auto_compact_token_limit.map(String.init) ?? "",
                window: compaction.context_window.map(String.init) ?? "",
                defaultWindow: (modelInformation[id]?["default_max_context_window"].number ?? modelInformation[id]?["default_context_window"].number).map(Int.init))
        }
        var next = settings
        var policies = next.modelPolicies ?? []
        policies.removeAll { $0.model == id }
        if let policy { policies.append(policy) }
        var rules = next.modelCompaction ?? []
        rules.removeAll { $0.model == id }
        if let compaction { rules.append(compaction) }
        try ModelPolicy.validate(policies)
        try ModelCompactionRule.validate(rules)
        next.modelPolicies = policies; next.modelCompaction = rules
        try persistSettings(next)
        preview = .null
        pendingSync = true
        do {
            guard running else { throw AppError.message("网关尚未运行，请启动后重试模型同步。") }
            try await sync()
        } catch {
            throw AppError.message("模型设置已保存在本机，但同步网关失败：\(error.localizedDescription) 请保留当前选择并重试保存。")
        }
        let codexSyncNote = connectionMode == "official" ? "远端 Codex 请在接入页手动同步。" : "本地和远端 Codex 请在接入页手动同步。"
        notice = "模型设置已保存，OMP 会自动同步预算。\(codexSyncNote)聊天客户端请刷新模型列表。"
    }
    func retryLocalModelSettingsSync() {
        perform {
            guard self.running else { throw AppError.message("请先启动网关后重试模型同步。") }
            if self.pendingSync { try await self.sync() }
            try await self.applyLocalModelSettings()
        }
    }
    func applyLocalModelSettings() async throws {
        try requireWritableMetadata()
        let home = settings.codexHome
        let revision = modelSettingsRevision
        let payload: JSONValue = .object(["codex_home": .string(home), "model_compaction": try JSONValue.from(settings.modelCompaction ?? [])])
        do {
            let result = try await client.call("config.models.apply", payload)
            guard settings.codexHome == home, modelSettingsRevision == revision else { return }
            let applied = result["applied"].bool == true
            var next = settings
            next.localCodexAppliedModelSettingsRevision = applied ? revision : nil
            try persistSettings(next)
            localModelSettingsSyncError = !applied && result["reason"].string == "missing" ? "配置目录或文件不存在，请先检查目录并完成接入。" : nil
        } catch {
            localModelSettingsSyncError = error.localizedDescription
            throw AppError.message("本地 Codex 模型设置同步失败：\(error.localizedDescription) 请检查配置后重试。")
        }
    }
    func configPayload() throws -> JSONValue {
        .object(["codex_home": .string(settings.codexHome), "model_compaction": try JSONValue.from(settings.modelCompaction ?? []), "provider_origins":try JSONValue.from(settings.legacyProviderOrigins ?? [])])
    }
    func saveLegacyOrigins(_ values: [String:String]) {
        perform {
            var next = self.settings
            next.legacyProviderOrigins = try values.filter { !$0.value.isEmpty }.map { provider, id in
                guard let source = self.sources.first(where: {$0.id == id && !$0.official && !$0.isGoogle}), let identity = self.state(id)["identity_digest"].string else { throw AppError.message("来源状态已变化，请刷新后重新选择。") }
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
            if operation == "config.attach" || operation == "config.official" {
                try await self.applyLocalModelSettings()
            } else {
                var next = self.settings
                next.localCodexAppliedModelSettingsRevision = nil
                try self.persistSettings(next)
                self.localModelSettingsSyncError = nil
            }
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
        let home = settings.codexHome
        do {
            let result = try await client.call("config.backups", .object(["codex_home":.string(home)]))
            guard !Task.isCancelled, settings.codexHome == home else { return }
            configurationBackups = result["backups"].array
        } catch { if !Task.isCancelled, settings.codexHome == home { self.error = error.localizedDescription } }
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
            }
            try await self.sync()
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
    func refreshEnvironmentSyncInspection() {
        environmentSynced = environmentAccess.inspect().synced
    }
    func setAutoExportChatbotEnvironment(_ enabled: Bool) {
        do {
            try requireWritableMetadata()
            var next = settings
            next.autoExportChatbotEnvironment = enabled
            try persistSettings(next)
            environmentSyncError = nil
            if enabled {
                _ = enqueueEnvironmentOperation { generation in
                    guard self.settings.autoExportChatbotEnvironmentEnabled else { return }
                    guard self.running else {
                        self.environmentSynced = self.environmentAccess.inspect().synced
                        if self.environmentSyncGeneration == generation {
                            self.notice = "已开启环境变量自动同步，网关启动后将写入配置。"
                        }
                        return
                    }
                    await self.applyChatbotEnvironment(generation: generation, showSuccessNotice: true)
                }
            } else {
                _ = enqueueEnvironmentOperation { generation in
                    await self.removeChatbotEnvironment(generation: generation, showSuccessNotice: true)
                }
            }
        } catch {
            self.error = error.localizedDescription
        }
    }
    @discardableResult
    func syncChatbotEnvironmentIfEnabled(showSuccessNotice: Bool = false) async -> Bool {
        guard settings.autoExportChatbotEnvironmentEnabled else {
            refreshEnvironmentSyncInspection()
            return true
        }
        let task = enqueueEnvironmentOperation { generation in
            await self.applyChatbotEnvironment(generation: generation, showSuccessNotice: showSuccessNotice)
        }
        await task.value
        return environmentSyncError == nil
    }
    func syncChatbotEnvironmentNow() {
        Task { _ = await syncChatbotEnvironmentIfEnabled(showSuccessNotice: true) }
    }
    func retryChatbotEnvironmentCleanup() {
        guard !settings.autoExportChatbotEnvironmentEnabled else { return }
        _ = enqueueEnvironmentOperation { generation in
            await self.removeChatbotEnvironment(generation: generation, showSuccessNotice: true)
        }
    }
    private func enqueueEnvironmentOperation(_ operation: @escaping @MainActor (UInt64) async -> Void) -> Task<Void, Never> {
        environmentSyncGeneration &+= 1
        let generation = environmentSyncGeneration
        environmentSyncWorking = true
        let previous = environmentSyncTask
        let task = Task { @MainActor [weak self] in
            await previous?.value
            guard let self else { return }
            await operation(generation)
            if self.environmentSyncGeneration == generation {
                self.environmentSyncWorking = false
            }
        }
        environmentSyncTask = task
        return task
    }
    private func applyChatbotEnvironment(generation: UInt64, showSuccessNotice: Bool) async {
        guard environmentSyncGeneration == generation, settings.autoExportChatbotEnvironmentEnabled else { return }
        guard running else {
            environmentSynced = environmentAccess.inspect().synced
            environmentSyncError = "请先启动本机网关。"
            return
        }
        let access = environmentAccess
        do {
            let statusResult = try await client.call("chatbot.status")
            let keyResult = try await client.call("chatbot.key")
            guard let baseURL = statusResult["base_url"].string, !baseURL.isEmpty else {
                throw AppError.message("无法读取聊天客户端接口地址。")
            }
            guard let key = keyResult["key"].string, !key.isEmpty else {
                throw AppError.message("无法读取聊天客户端 Key。")
            }
            guard environmentSyncGeneration == generation, settings.autoExportChatbotEnvironmentEnabled else { return }
            try await Task.detached(priority: .utility) {
                try access.apply(baseURL: baseURL, key: key)
            }.value
            guard environmentSyncGeneration == generation else { return }
            environmentSynced = access.inspect().synced
            environmentSyncError = nil
            if showSuccessNotice { notice = "聊天客户端环境变量已同步。" }
        } catch {
            guard environmentSyncGeneration == generation else { return }
            environmentSynced = access.inspect().synced
            environmentSyncError = error.localizedDescription
        }
    }
    private func removeChatbotEnvironment(generation: UInt64, showSuccessNotice: Bool) async {
        guard environmentSyncGeneration == generation, !settings.autoExportChatbotEnvironmentEnabled else { return }
        let access = environmentAccess
        do {
            try await Task.detached(priority: .utility) {
                try access.remove()
            }.value
            guard environmentSyncGeneration == generation else { return }
            environmentSynced = access.inspect().synced
            environmentSyncError = nil
            if showSuccessNotice { notice = "已关闭并清理聊天客户端环境变量。" }
        } catch {
            guard environmentSyncGeneration == generation else { return }
            environmentSynced = access.inspect().synced
            environmentSyncError = error.localizedDescription
        }
    }
}
