import Combine
import CryptoKit
import Foundation
import Darwin

struct RemotePoolSource: Identifiable, Codable, Equatable {
    var id: String
    var name: String
    var kind: String
    var enabled: Bool
    var priority: Int
}

struct RemotePoolSnapshot: Codable, Equatable {
    var instanceID: String
    var revision: String
    var mode: String
    var fixedSourceID: String?
    var sources: [RemotePoolSource]
    // 不上网、不落盘：即使两个地址报告相同实例和修订，也不能互用草稿。
    var loadID: UUID? = nil

    enum CodingKeys: String, CodingKey {
        case instanceID = "instance_id", revision, mode, fixedSourceID = "fixed_source_id", sources
    }

    func encode(to encoder: Encoder) throws {
        var fields = encoder.container(keyedBy: CodingKeys.self)
        try fields.encode(instanceID, forKey: .instanceID)
        try fields.encode(revision, forKey: .revision)
        try fields.encode(mode, forKey: .mode)
        try fields.encode(fixedSourceID, forKey: .fixedSourceID)
        try fields.encode(sources, forKey: .sources)
    }
}

struct RemotePoolAddition: Encodable {
    var id: String
    var name: String
    var baseURL: String
    var apiKey: String
    var models: [String]

    enum CodingKeys: String, CodingKey { case id, name, baseURL = "base_url", apiKey = "api_key", models }
}

private struct RemotePoolUpdate: Encodable {
    let draft: RemotePoolSnapshot
    let additions: [RemotePoolAddition]
    private enum CodingKeys: String, CodingKey { case additions }

    func encode(to encoder: Encoder) throws {
        try draft.encode(to: encoder)
        if !additions.isEmpty {
            var fields = encoder.container(keyedBy: CodingKeys.self)
            try fields.encode(additions, forKey: .additions)
        }
    }
}

private struct RemoteStatus: Decodable {
    let instanceID: String
    let protocolVersion: Int
    let version: String
    let capabilities: [String]?
    let runtime: [String: JSONValue]?
    enum CodingKeys: String, CodingKey { case instanceID = "instance_id", protocolVersion = "protocol", version, capabilities, runtime }
}

private struct RemoteManagementError: LocalizedError {
    let message: String
    let invalidatesIdentity: Bool
    var errorDescription: String? { message }
    init(_ message: String, invalidatesIdentity: Bool = false) {
        self.message = message
        self.invalidatesIdentity = invalidatesIdentity
    }
}

struct RemoteManagementFile {
    let directory: URL
    var name = "remote-management.json"
    private var unsafe: RemoteManagementError { RemoteManagementError("无法安全读写远端管理配置。请检查目录权限和文件类型。") }

    // 按目录描述符创建缺少的层级；不通过任何路径分量上的符号链接创建配置。
    private func openDirectory(_ url: URL, create: Bool) throws -> Int32 {
        let flags = O_RDONLY | O_DIRECTORY | O_NOFOLLOW_ANY | O_CLOEXEC
        let existing = open(url.path, flags)
        if existing >= 0 { return existing }
        guard errno == ENOENT else { throw unsafe }
        guard create else { throw CocoaError(.fileReadNoSuchFile) }
        let parent = try openDirectory(url.deletingLastPathComponent(), create: true)
        defer { close(parent) }
        let leaf = url.lastPathComponent
        guard mkdirat(parent, leaf, 0o700) == 0 || errno == EEXIST else { throw unsafe }
        let fd = openat(parent, leaf, flags)
        guard fd >= 0 else { throw unsafe }
        return fd
    }

    private func directoryFD(create: Bool) throws -> Int32 {
        let fd = try openDirectory(directory, create: create)
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_uid == geteuid(),
              !create || info.st_mode & 0o7777 == 0o700 || fchmod(fd, 0o700) == 0 else {
            close(fd); throw unsafe
        }
        return fd
    }

    private func isPrivateFile(_ info: stat) -> Bool {
        (info.st_mode & S_IFMT) == S_IFREG && info.st_mode & 0o7777 == 0o600 && info.st_nlink == 1 && info.st_uid == geteuid()
    }

    func read() throws -> Data {
        let dir = try directoryFD(create: false)
        defer { close(dir) }
        let fd = openat(dir, name, O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC)
        if fd < 0, errno == ENOENT { throw CocoaError(.fileReadNoSuchFile) }
        guard fd >= 0 else { throw unsafe }
        defer { close(fd) }
        var info = stat()
        guard fstat(dir, &info) == 0, info.st_mode & 0o7777 == 0o700,
              fstat(fd, &info) == 0, isPrivateFile(info) else { throw unsafe }
        return try FileHandle(fileDescriptor: fd, closeOnDealloc: false).readToEnd() ?? Data()
    }

    func replace(with data: Data) throws {
        let dir = try directoryFD(create: true)
        defer { close(dir) }
        var info = stat()
        if fstatat(dir, name, &info, AT_SYMLINK_NOFOLLOW) == 0 {
            guard isPrivateFile(info) else { throw unsafe }
        } else if errno != ENOENT { throw unsafe }
        let temp = ".remote-management-\(UUID().uuidString)"
        let fd = openat(dir, temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw unsafe }
        defer { close(fd); unlinkat(dir, temp, 0) }
        guard fchmod(fd, 0o600) == 0, fstat(fd, &info) == 0, isPrivateFile(info) else { throw unsafe }
        try FileHandle(fileDescriptor: fd, closeOnDealloc: false).write(contentsOf: data)
        guard fsync(fd) == 0, renameat(dir, temp, dir, name) == 0 else { throw unsafe }
    }
}

private final class RemoteManagementHTTP: NSObject, URLSessionTaskDelegate {
    static let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.urlCache = nil
        config.urlCredentialStorage = nil
        config.httpCookieStorage = nil
        config.httpShouldSetCookies = false
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        config.timeoutIntervalForRequest = 15
        config.timeoutIntervalForResource = 30
        return URLSession(configuration: config, delegate: RemoteManagementHTTP(), delegateQueue: nil)
    }()

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

@MainActor
final class RemoteGatewayManagement: ObservableObject {
    @Published private(set) var remoteSelected = false
    @Published private(set) var endpoint = ""
    @Published private(set) var version: String?
    @Published private(set) var snapshot: RemotePoolSnapshot?
    @Published private(set) var capabilities = Set<String>()
    var targetGeneration: UUID { targetID }
    private enum Activity { case idle, refreshing, exclusive }
    @Published private var activity: Activity = .idle
    var busy: Bool { activity != .idle }
    var targetSwitchBlocked: Bool { activity == .exclusive }
    @Published private(set) var message: String?
    @Published private(set) var fetchedAt: Date?
    @Published private(set) var runtimeData: Data?
    @Published private(set) var ompMessage: String?

    private struct Profile: Codable {
        let endpoint: String
        let instanceID: String
        let token: String
        let binding: String
    }
    private struct Selection: Codable {
        let remoteSelected: Bool
        let profile: Profile?
        var ompIssuanceAttempts: Set<String>?
    }

    private let file: RemoteManagementFile
    private let integrations: GatewayIntegrationFiles
    typealias Transport = @MainActor (URLRequest) async throws -> (Data, URLResponse)
    private let send: Transport
    private var profile: Profile?
    private var ompIssuanceAttempts = Set<String>()
    private var operationID = UUID()
    private var targetID = UUID()
    private var refreshTask: Task<Void, Never>?
    private var monitoring = false
    private var pollTimer: Timer?
    private var prepareOMPOnLoad = true

    convenience init(directory: URL) {
        self.init(directory: directory) { request in
            try await RemoteManagementHTTP.session.data(for: request)
        }
    }

    init(directory: URL, send: @escaping Transport) {
        self.file = RemoteManagementFile(directory: directory)
        self.integrations = GatewayIntegrationFiles(directory: directory)
        self.send = send
        do {
            let selection = try JSONDecoder().decode(Selection.self, from: file.read())
            if let saved = selection.profile {
                guard try Self.root(saved.endpoint).absoluteString == saved.endpoint, !saved.instanceID.isEmpty,
                      saved.binding == binding(endpoint: saved.endpoint, instance: saved.instanceID) else {
                    throw RemoteManagementError("远端配置无效。")
                }
                profile = saved
                endpoint = saved.endpoint
            } else if selection.remoteSelected { throw RemoteManagementError("缺少远端配置。") }
            remoteSelected = selection.remoteSelected
            ompIssuanceAttempts = selection.ompIssuanceAttempts ?? []
        } catch let error as CocoaError where error.code == .fileReadNoSuchFile {
            // 初次启动不创建管理令牌文件。
        } catch {
            remoteSelected = true
            message = "无法读取管理目标。请重新连接远端，或手动切换到本机。"
            do { try integrations.invalidate() }
            catch { message = "管理目标和共享文件均不可用。请检查状态目录权限后重新选择网关。" }
            return
        }
        do {
            if remoteSelected, let profile { try integrations.selectRemote(endpoint: profile.endpoint, instanceID: profile.instanceID) }
            else { try integrations.selectLocal() }
        } catch { message = "无法初始化网关共享文件。请检查状态目录权限后重新选择网关。" }
    }

    func startMonitoring() {
        guard !monitoring else { return }
        monitoring = true
        if remoteSelected {
            schedulePolling()
            Task { if monitoring { await refresh() } }
        }
    }

    // Run-loop ticks do not wait for the preceding network request. A slow request is joined, not duplicated.
    private func schedulePolling() {
        guard monitoring, pollTimer == nil, remoteSelected, profile != nil else { return }
        let timer = Timer(timeInterval: 30, repeats: true) { [weak self] _ in
            Task { @MainActor in if let self, self.monitoring { await self.refresh() } }
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    func stopMonitoring() {
        monitoring = false
        pollTimer?.invalidate(); pollTimer = nil
        supersede()
        activity = .idle
    }

    @discardableResult
    func selectLocal() -> Bool {
        supersede()
        activity = .idle
        do {
            if remoteSelected {
                try invalidateSharedTarget()
                try persist(remote: false, profile: profile)
            }
            remoteSelected = false
            pollTimer?.invalidate(); pollTimer = nil
            targetID = UUID()
            snapshot = nil
            runtimeData = nil
            fetchedAt = nil
            version = nil
            ompMessage = nil
            try integrations.selectLocal()
            message = nil
            return true
        } catch {
            snapshot = nil
            version = nil
            runtimeData = nil
            message = "管理目标或共享文件保存失败。请检查状态目录权限后重试。"
            return false
        }
    }

    @discardableResult
    func connect(url: String, token: String) async -> Bool {
        await perform(begin()) { id in
            let root = try Self.root(url)
            let (status, pool) = try await self.load(root, token: token, expecting: nil, id: id)
            try self.check(id)
            let next = Profile(endpoint: root.absoluteString, instanceID: status.instanceID, token: token,
                               binding: self.binding(endpoint: root.absoluteString, instance: status.instanceID))
            try self.adopt(next)
            try await self.accept(status, pool, id: id)
        }
    }

    @discardableResult
    func selectRemote() async -> Bool {
        if let refreshTask {
            let id = operationID
            await refreshTask.value
            return operationID == id && snapshot != nil
        }
        return await restore(begin())
    }

    private func restore(_ operation: UUID, retryOMP: Bool = false) async -> Bool {
        await perform(operation, checkingSelection: true) { id in
            guard let profile = self.profile else { throw RemoteManagementError("请先输入远端地址和管理令牌。") }
            let root = try Self.root(profile.endpoint)
            let (status, pool) = try await self.load(root, token: profile.token, expecting: profile.instanceID, id: id)
            try self.check(id)
            if !self.remoteSelected { try self.adopt(profile) }
            try await self.accept(status, pool, id: id, retryOMP: retryOMP)
        }
    }

    private func adopt(_ next: Profile) throws {
        let changingTarget = !remoteSelected || profile?.binding != next.binding
        if changingTarget { try invalidateSharedTarget() }
        try persist(remote: true, profile: next)
        if changingTarget {
            targetID = UUID()
            snapshot = nil
            runtimeData = nil
            fetchedAt = nil
            version = nil
            ompMessage = nil
            prepareOMPOnLoad = true
        }
        profile = next
        endpoint = next.endpoint
        remoteSelected = true
        schedulePolling()
    }

    private func invalidateSharedTarget() throws {
        capabilities.removeAll()
        prepareOMPOnLoad = ompMessage == nil
        try integrations.invalidate()
    }

    func refresh() async {
        if let refreshTask { await refreshTask.value; return }
        guard remoteSelected, profile != nil, !busy else { return }
        let id = begin(.refreshing)
        let task = Task {
            _ = await restore(id)
            if operationID == id { refreshTask = nil }
        }
        refreshTask = task
        await task.value
    }

    func retryOMP() async {
        guard remoteSelected, ompMessage != nil, !busy else { return }
        _ = await restore(begin(), retryOMP: true)
    }

    private func accept(_ status: RemoteStatus, _ pool: RemotePoolSnapshot, id: UUID, retryOMP: Bool = false) async throws {
        try check(id)
        guard let profile, remoteSelected, profile.instanceID == pool.instanceID else { throw CancellationError() }
        var verified = pool
        verified.loadID = targetID
        snapshot = verified
        capabilities = Set(status.capabilities ?? [])
        version = status.version
        let now = Date()
        fetchedAt = now
        runtimeData = try status.runtime.map { try JSONEncoder().encode($0) }
        do { try integrations.selectRemote(endpoint: profile.endpoint, instanceID: profile.instanceID) }
        catch { message = "远端数据已更新，但无法保存共享目标。请检查状态目录权限并刷新。"; return }
        do { try integrations.publish(pool: verified, runtime: runtimeData, fetchedAt: now) }
        catch {
            // The server already accepted a PUT; a cache error must not prompt a second write.
            message = "远端数据已更新，但共享快照未更新。请检查状态目录权限并刷新。"
        }
        guard prepareOMPOnLoad || retryOMP else { return }
        activity = .exclusive
        prepareOMPOnLoad = false
        let failure = await integrations.prepareOMP(endpoint: profile.endpoint, instanceID: profile.instanceID) {
            try self.check(id)
            // Record intent before the non-idempotent POST, even across target switches and restarts.
            if self.ompIssuanceAttempts.insert(profile.binding).inserted {
                do { try self.persist(remote: true, profile: profile) }
                catch { self.ompIssuanceAttempts.remove(profile.binding); throw error }
            } else if !retryOMP { throw RemoteManagementError("需要手动确认后再签发。") }
            struct Device: Decodable { let id: String; let key: String }
            let device: Device = try await self.request(try Self.root(profile.endpoint), path: "admin/v1/devices", token: profile.token,
                                                        body: Data(#"{"name":"OMP on Mac"}"#.utf8), method: "POST", id: id)
            try self.check(id)
            guard device.key != profile.token else { throw RemoteManagementError("设备凭据无效。") }
            return (device.id, device.key)
        }
        try check(id)
        ompMessage = failure
    }

    /// Read-only polling may run concurrently. Mutation admission is checked again on the server under its pool lock.
    func operation(_ name: String, payload: JSONValue = .object([:]), on target: RemotePoolSnapshot) async throws -> JSONValue {
        func matchesTarget() -> Bool {
            remoteSelected && target.loadID == targetID && snapshot?.revision == target.revision && profile?.instanceID == target.instanceID
        }
        guard matchesTarget(), let profile else { throw RemoteManagementError("网关或来源池已改变，请刷新后重新操作。") }
        guard capabilities.contains(name), !name.isEmpty, name.count <= 64,
              name.utf8.allSatisfy({ (97...122).contains($0) || $0 == 46 || $0 == 95 }) else {
            throw RemoteManagementError("当前网关不支持此操作。")
        }
        let body = try JSONEncoder().encode(JSONValue.object(["instance_id": .string(target.instanceID), "revision": .string(target.revision), "payload": payload]))
        try Task.checkCancellation()
        let result: JSONValue
        do {
            result = try await exchange(try Self.root(profile.endpoint), path: "admin/v1/operations/\(name)", token: profile.token, body: body, method: "POST")
        } catch {
            guard matchesTarget() else { throw CancellationError() }
            if let failure = error as? RemoteManagementError { throw failure }
            throw RemoteManagementError("未收到网关确认。请先刷新核对结果，不要直接重复提交。")
        }
        guard remoteSelected, targetID == target.loadID, self.profile?.instanceID == target.instanceID else { throw CancellationError() }
        if name == "config.save" {
            // A GET issued before this write may still be in flight. Never join that stale GET after saving.
            if activity == .refreshing { supersede(); activity = .idle }
        } else if snapshot?.revision != target.revision { throw CancellationError() }
        try Task.checkCancellation()
        return result
    }

    @discardableResult
    func save(_ draft: RemotePoolSnapshot, additions: [RemotePoolAddition]) async -> Bool {
        guard !busy else { return false }
        guard remoteSelected, let profile, let current = snapshot, draft.loadID == current.loadID,
              draft.instanceID == profile.instanceID, draft.revision == current.revision else {
            message = "草稿不属于当前远端快照。请刷新后重新编辑。"
            return false
        }
        let kinds = Dictionary(uniqueKeysWithValues: current.sources.map { ($0.id, $0.kind) })
        guard draft.sources.count == current.sources.count, Set(draft.sources.map(\.id)).count == kinds.count,
              draft.sources.allSatisfy({ kinds[$0.id] == $0.kind }) else {
            message = "必须保留所有现有来源及其类型。"
            return false
        }
        return await perform(begin(), checkingSelection: true) { id in
            let root = try Self.root(profile.endpoint)
            let status = try await self.status(root, token: profile.token, expecting: profile.instanceID, id: id)
            var ordered = draft
            for i in ordered.sources.indices { ordered.sources[i].priority = i }
            let body = try JSONEncoder().encode(RemotePoolUpdate(draft: ordered, additions: additions))
            let updated = try await self.pool(root, token: profile.token, instanceID: profile.instanceID, body: body, id: id)
            try await self.accept(status, updated, id: id)
        }
    }

    private func supersede() {
        refreshTask?.cancel()
        refreshTask = nil
        operationID = UUID()
    }

    private func begin(_ next: Activity = .exclusive) -> UUID {
        supersede()
        activity = next
        message = nil
        return operationID
    }

    private func perform(_ id: UUID, checkingSelection: Bool = false, _ action: (UUID) async throws -> Void) async -> Bool {
        guard operationID == id else { return false }
        defer { if operationID == id { activity = .idle } }
        do {
            try await action(id)
            return operationID == id
        } catch {
            guard operationID == id else { return false }
            snapshot = nil
            runtimeData = nil
            version = nil
            let failure = error as? RemoteManagementError
            message = failure?.message ?? "远端管理操作未完成。请检查连接并刷新。"
            if checkingSelection, remoteSelected, failure?.invalidatesIdentity == true {
                targetID = UUID()
                do { try invalidateSharedTarget() }
                catch { message = "远端身份未通过验证，且共享文件无法停用。请检查状态目录权限并重新连接。" }
            }
            return false
        }
    }

    private func check(_ id: UUID) throws {
        guard id == operationID, !Task.isCancelled else { throw CancellationError() }
    }

    // 防止只改地址、实例或搬动配置目录后把原令牌交给另一个目标；文件本身依靠所有者权限保护。
    private func binding(endpoint: String, instance: String) -> String {
        let identity = [file.directory.path, endpoint, instance].joined(separator: "\0")
        return Data(SHA256.hash(data: Data(identity.utf8))).base64EncodedString()
    }

    private func persist(remote: Bool, profile: Profile?) throws {
        let data = try JSONEncoder().encode(Selection(remoteSelected: remote, profile: profile, ompIssuanceAttempts: ompIssuanceAttempts))
        try file.replace(with: data)
    }

    private static func root(_ text: String) throws -> URL {
        let invalid = RemoteManagementError("请输入 HTTPS 根地址，不含账号、密码、路径、查询参数或片段。")
        guard text.utf8.allSatisfy({ (33...126).contains($0) }), !text.contains("%"), !text.contains("\\"),
              let url = URL(string: text, encodingInvalidCharacters: false),
              var parts = URLComponents(url: url, resolvingAgainstBaseURL: false), parts.scheme?.lowercased() == "https",
              let host = parts.host, !host.isEmpty, parts.user == nil, parts.password == nil,
              parts.query == nil, parts.fragment == nil, parts.path == "" || parts.path == "/" else { throw invalid }
        let authority = text.dropFirst("https://".count).prefix { $0 != "/" }
        if authority.last != "]", let colon = authority.lastIndex(of: ":") {
            let port = authority[authority.index(after: colon)...]
            guard port.utf8.allSatisfy({ (48...57).contains($0) }), let value = Int(port),
                  (1...65535).contains(value), parts.port == value else { throw invalid }
        }
        parts.scheme = "https"
        parts.host = host.lowercased()
        if parts.port == 443 { parts.port = nil }
        parts.path = ""
        guard let root = parts.url else { throw invalid }
        return root
    }

    private func load(_ root: URL, token: String, expecting: String?, id: UUID) async throws -> (RemoteStatus, RemotePoolSnapshot) {
        let status = try await status(root, token: token, expecting: expecting, id: id)
        let pool = try await pool(root, token: token, instanceID: status.instanceID, body: nil, id: id)
        return (status, pool)
    }

    private func status(_ root: URL, token: String, expecting: String?, id: UUID) async throws -> RemoteStatus {
        let status: RemoteStatus = try await request(root, path: "admin/v1/status", token: token, body: nil, id: id)
        try check(id)
        if expecting == nil, remoteSelected, let profile, profile.endpoint == root.absoluteString,
           profile.instanceID != status.instanceID {
            // The old instance is gone even if admission of its replacement fails at the pool request.
            targetID = UUID()
            snapshot = nil
            version = nil
            runtimeData = nil
            try invalidateSharedTarget()
        }
        guard status.protocolVersion == 1, status.capabilities?.contains("pool.edit") == true else {
            throw RemoteManagementError("此网关不支持协议 1 的来源池编辑。", invalidatesIdentity: true)
        }
        guard !status.version.isEmpty, !status.instanceID.isEmpty, expecting == nil || expecting == status.instanceID else {
            throw RemoteManagementError("远端实例与已保存的目标不符。请核实地址并重新连接。", invalidatesIdentity: true)
        }
        return status
    }

    private func pool(_ root: URL, token: String, instanceID: String, body: Data?, id: UUID) async throws -> RemotePoolSnapshot {
        let pool: RemotePoolSnapshot = try await request(root, path: "admin/v1/pool", token: token, body: body, method: body == nil ? "GET" : "PUT", id: id)
        guard pool.instanceID == instanceID, !pool.revision.isEmpty, ["auto", "fixed"].contains(pool.mode),
              Set(pool.sources.map(\.id)).count == pool.sources.count else {
            throw RemoteManagementError("来源池快照无效或不属于已验证的远端实例。", invalidatesIdentity: true)
        }
        return pool
    }

    private func request<T: Decodable>(_ root: URL, path: String, token: String, body: Data?, method: String = "GET", id: UUID) async throws -> T {
        try check(id)
        let value: T = try await exchange(root, path: path, token: token, body: body, method: method)
        try check(id)
        return value
    }

    private func exchange<T: Decodable>(_ root: URL, path: String, token: String, body: Data?, method: String) async throws -> T {
        guard !token.isEmpty, token.utf8.allSatisfy({ (33...126).contains($0) }) else {
            throw RemoteManagementError("请输入有效的管理令牌。")
        }
        var request = URLRequest(url: root.appendingPathComponent(path), cachePolicy: .reloadIgnoringLocalCacheData)
        request.httpMethod = method
        request.httpShouldHandleCookies = false
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let (data, response) = try await send(request)
        guard let http = response as? HTTPURLResponse, http.url == request.url else { throw RemoteManagementError("远端响应地址无效。", invalidatesIdentity: true) }
        switch http.statusCode {
        case 200: break
        case 300..<400: throw RemoteManagementError("远端返回重定向，已拒绝跟随。请使用最终 HTTPS 根地址。", invalidatesIdentity: true)
        case 401, 403: throw RemoteManagementError("远端拒绝管理令牌。请核实管理权限后重新连接。", invalidatesIdentity: true)
        case 409: throw RemoteManagementError("远端来源池已变更。请刷新后重新编辑，未覆盖服务器配置。")
        case 404: throw RemoteManagementError("当前网关没有这个项目或功能。")
        case 413: throw RemoteManagementError("提交的数据超过网关限制。")
        case 429: throw RemoteManagementError("网关繁忙，请稍后手动刷新。")
        case 400, 422: throw RemoteManagementError("网关拒绝了输入，请检查字段、凭据和操作条件。")
        default: throw RemoteManagementError("远端管理请求失败（HTTP \(http.statusCode)）。请刷新后重试。")
        }
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw RemoteManagementError("远端管理响应格式不兼容。", invalidatesIdentity: true) }
    }
}
