import Foundation
import SwiftUI

@MainActor final class SSHRemoteModel: ObservableObject {
    @Published var profile = SSHRemoteProfile() {
        didSet {
            guard !Self.sameTarget(oldValue, profile) else { return }
            if !selectingProfile, oldValue.id == profile.id {
                profile.id = UUID().uuidString
                profile.backupID = nil; profile.originalBackupID = nil
                profile.appliedModelSettingsRevision = nil; profile.codexConnectionRestored = nil
            }
            prepared = .null; summary = ""; candidates = []; aliases = []
            error = nil; notice = nil
            ompStatus = .null; ompStatusProfile = nil
        }
    }
    var port: String { status["port"].number.map { String(Int($0)) } ?? (profile.kind == .omp ? "17865" : "") }
    @Published private(set) var status: JSONValue = .null
    @Published private(set) var busy = false
    @Published private(set) var phase = ""
    @Published var error: String?
    @Published var notice: String?
    @Published private(set) var candidates: [String] = []
    @Published private(set) var summary = ""
    @Published private(set) var aliases: [String] = []
    @Published private(set) var configuredHosts: [String] = []
    @Published private(set) var readingSSHConfig = false
    @Published private(set) var sshConfigMessage: String?
    @Published private(set) var ompStatus: JSONValue = .null
    private var ompStatusProfile: SSHRemoteProfile?
    private var selectingProfile = false
    private static func sameTarget(_ lhs: SSHRemoteProfile, _ rhs: SSHRemoteProfile) -> Bool {
        lhs.kind == rhs.kind && lhs.destination == rhs.destination && lhs.username == rhs.username
            && lhs.sshPort == rhs.sshPort && lhs.identityFile == rhs.identityFile && lhs.directory == rhs.directory
    }
    var hasOMPStatus: Bool {
        guard ompStatus != .null, let loaded = ompStatusProfile else { return false }
        return Self.sameTarget(loaded, profile)
    }
    var ompInstalled: Bool { hasOMPStatus && ompStatus["installed"].bool == true }
    var ompAddressChanged: Bool {
        guard hasOMPStatus, let port = Int(port), let requested = try? profile.ompBaseURL(port: port) else { return false }
        return ompStatus["base_url"].string != requested
    }
    private var prepared: JSONValue = .null
    private var expectedDigest = ""
    private var expectedCodexHome = ""
    private var preparedProfile: SSHRemoteProfile?
    private var preparedPort = ""
    private var preparedModelSettingsRevision: String?
    private var expectedCatalogDigest = ""
    private var expectedCompactionStateDigest = ""
    private var expectedFiles: JSONValue = .array([])
    func canApply(_ model: AppModel) -> Bool {
        !model.pendingSync && prepared != .null && preparedProfile == profile && preparedPort == port
            && preparedModelSettingsRevision == model.modelSettingsRevision
    }
    var running: Bool { status["running"].bool == true }
    var canRestore: Bool { !profile.destination.isEmpty && profile.directory.hasPrefix("/") }
    private func perform(_ label: String, action: @escaping () async throws -> Void) {
        guard !busy else { return }; busy = true; phase = label; error = nil; notice = nil
        Task { defer { busy = false; phase = "" }; do { try await action() } catch { self.error = error.localizedDescription } }
    }
    func refresh(_ model: AppModel) async {
        guard model.running else { status = .null; return }
        guard !busy else { return }
        let kind = profile.kind
        do {
            let result = try await model.remoteCall(kind == .omp ? "chatbot.status" : "remote.status")
            if profile.kind == kind, !busy { status = result }
        }
        catch { self.error = error.localizedDescription }
    }
    func select(_ value: SSHRemoteProfile) {
        selectingProfile = true
        profile = value
        selectingProfile = false
    }
    func selectKind(_ kind: SSHRemoteKind) {
        guard kind != profile.kind else { return }
        profile.clientKind = kind; profile.id = UUID().uuidString
        profile.backupID = nil; profile.originalBackupID = nil
        profile.appliedModelSettingsRevision = nil; profile.codexConnectionRestored = nil
        status = .null; prepared = .null; summary = ""; candidates = []; aliases = []
        ompStatus = .null; ompStatusProfile = nil
        error = nil; notice = nil
    }
    func loadSSHConfig() async {
        guard !readingSSHConfig else { return }
        readingSSHConfig = true
        let catalog = await Task.detached(priority: .utility) { SSHConfigCatalog.load() }.value
        configuredHosts = catalog.hosts
        sshConfigMessage = catalog.incomplete ? "部分 SSH 配置未能读取，可手动填写别名。" : !catalog.exists ? "未找到 ~/.ssh/config，可手动填写连接。" : catalog.hosts.isEmpty ? "配置中没有明确的 Host 别名，可手动填写。" : nil
        readingSSHConfig = false
    }
    func selectConfiguredHost(_ alias: String, saved: [SSHRemoteProfile]) {
        let kind = profile.kind
        if let saved = saved.first(where: { $0.kind == kind && $0.destination.caseInsensitiveCompare(alias) == .orderedSame }) {
            select(saved); return
        }
        var next = SSHRemoteProfile()
        next.clientKind = kind; next.destination = alias
        select(next); error = nil; notice = nil
    }
    private func remember(_ value: SSHRemoteProfile, in model: AppModel) throws {
        var profiles = model.settings.sshRemoteProfiles ?? []; profiles.removeAll { $0.id == value.id }; profiles.append(value)
        try model.saveRemoteProfiles(profiles)
    }
    func discover(_ model: AppModel) {
        let original = profile
        perform("正在连接并查找目录…") {
            let result = try await SSHRemoteClient(profile: original).call("discover")
            guard self.profile == original else { throw AppError.message("连接信息已变化，请重新探测。") }
            let candidates = result["candidates"].array.compactMap(\.string)
            if original.kind == .omp {
                if self.profile.directory.isEmpty { self.profile.ompAgentDirectory = candidates.first ?? "" }
            } else if self.profile.codexHome.isEmpty { self.profile.codexHome = candidates.first ?? "" }
            if self.profile.gatewayHost.isEmpty { self.profile.gatewayHost = result["client_ip"].string ?? "" }
            self.candidates = candidates
            if original.kind == .omp {
                let selected = self.profile
                let installed = try await SSHRemoteClient(profile: selected).call("status")
                guard self.profile == selected else { throw AppError.message("连接信息已变化，请重新探测。") }
                self.ompStatus = installed; self.ompStatusProfile = selected
            }
            try self.remember(self.profile, in: model)
            self.notice = self.candidates.isEmpty ? "未找到 \(original.kind.title) 目录，请手动填写远端绝对路径。" : "已找到远端目录和 SSH 连接地址。"
        }
    }
    func preview(_ model: AppModel) {
        perform("正在读取并预览远端配置…") { try await self.prepareCodex(model) }
    }
    private func prepareCodex(_ model: AppModel) async throws {
            let original = profile; let rules = model.settings.modelCompaction ?? []
            let revision = model.modelSettingsRevision
            guard !model.pendingSync else { throw AppError.message("请先将已保存的模型设置同步到本地网关，再预览远端配置。") }
            try ModelCompactionRule.validate(rules)
            guard original.codexHome.hasPrefix("/"), !original.gatewayHost.isEmpty else { throw AppError.message("请先填写远端 Codex 目录和本机地址。") }
            let connection = try await model.remoteCall("remote.status")
            self.status = connection
            guard self.running else { throw AppError.message("请先启动本地网关。") }
            let originalPort = port
            let result = try await SSHRemoteClient(profile: original).call("read")
            guard let encoded = result["config"].string, let bytes = Data(base64Encoded: encoded), let config = String(data: bytes, encoding: .utf8) else { throw AppError.message("远端配置不是有效 UTF-8。") }
            var base: String?
            if let encoded = result["compaction_base"].string {
                guard let bytes = Data(base64Encoded: encoded), let text = String(data: bytes, encoding: .utf8) else { throw AppError.message("远端原模型配置不是有效 UTF-8。") }
                base = text
            }
            var catalog: JSONValue = .null; var checks: JSONValue = .array([])
            if !rules.isEmpty {
                let source = try await model.remoteCall("remote.compaction.source", .object(["config": .string(base ?? config)]))
                let fetched = try await SSHRemoteClient(profile: original).call("catalog", fields: ["catalog_path": source["catalog_path"]])
                catalog = fetched["catalog"]; checks = fetched["expected_files"]
            }
            let rendered = try await model.remoteCall("remote.preview", .object(["id":.string(original.id),"name":.string(original.username.isEmpty ? original.destination : "\(original.username)@\(original.destination)"),"host":.string(original.gatewayHost),"config":.string(config),"aliases":.bool(original.mapExistingProviders),"codex_home":result["codex_home"],"model_compaction":try JSONValue.from(rules),"compaction_base":base.map(JSONValue.string) ?? .null,"catalog":catalog]))
            let current = try await model.remoteCall("remote.status")
            guard self.profile == original, current["port"] == connection["port"], current["running"].bool == true,
                  !model.pendingSync, model.modelSettingsRevision == revision else { throw AppError.message("模型设置或网关端口已变化，请重新预览。") }
            self.prepared = rendered; self.expectedDigest = result["digest"].string ?? ""
            self.expectedCodexHome = result["codex_home"].string ?? ""
            self.expectedCatalogDigest = result["catalog_digest"].string ?? ""
            self.expectedCompactionStateDigest = result["compaction_state_digest"].string ?? ""
            self.expectedFiles = checks; self.preparedModelSettingsRevision = revision
            self.preparedProfile = original; self.preparedPort = originalPort
            var summary = rendered["summary"].string ?? ""
            if rules.isEmpty { summary += base == nil ? "\n模型压缩 = 保留远端设置" : "\n模型压缩 = 恢复远端原设置" }
            else {
                summary += "\n模型压缩（\(rules.count)）"
                for rule in rules { summary += "\n\(rule.model)：压缩 " + (rule.auto_compact_token_limit.map { $0.formatted() + " Token" } ?? "默认") + (rule.context_window.map { "，最大输入 \($0.formatted())" } ?? "") }
            }
            self.summary = summary; self.aliases = rendered["aliases"].array.compactMap(\.string)
            self.status = current
    }
    func apply(_ model: AppModel) {
        let original = profile
        perform("正在验证连通性并备份远端配置…") {
            guard self.canApply(model), let revision = self.preparedModelSettingsRevision else {
                throw AppError.message("请先预览当前模型设置，再应用到远端。")
            }
            let prepared = self.prepared; let digest = self.expectedDigest
            let current = try await model.remoteCall("remote.status")
            self.status = current
            guard self.profile == original, self.canApply(model), self.running else { throw AppError.message("设置或网关端口已变化，请重试。") }
            let fields: [String:JSONValue] = ["base_url":prepared["base_url"],"key":prepared["key"],"expected_home":.string(self.expectedCodexHome),"expected_digest":.string(digest),"config":.string(Data((prepared["config"].string ?? "").utf8).base64EncodedString()),"catalog_action":prepared["catalog_action"],"catalog":prepared["catalog"],"expected_catalog_digest":.string(self.expectedCatalogDigest),"expected_compaction_state_digest":.string(self.expectedCompactionStateDigest),"expected_files":self.expectedFiles]
            let result = try await SSHRemoteClient(profile: original).call("apply", fields: fields)
            var saved = original; saved.backupID = result["backup_id"].string
            saved.originalBackupID = result["original_backup_id"].string
            saved.appliedModelSettingsRevision = revision; saved.codexConnectionRestored = false
            try self.remember(saved, in: model)
            if self.profile == original { self.select(saved) }
            self.prepared = .null
            self.notice = "已备份并应用远端来源池配置。请重启远端 Codex；使用 VS Code 时，请重载对应的远端窗口。"
        }
    }
    func restore(_ model: AppModel) {
        let original = profile
        perform("正在备份当前状态并还原接入前设置…") {
            guard self.canRestore else { throw AppError.message("请先填写 SSH 连接和远端 Codex 目录。") }
            let result = try await SSHRemoteClient(profile: original).call("restore")
            var saved = original; saved.backupID = result["recovery_backup_id"].string
            saved.originalBackupID = result["original_backup_id"].string
            saved.appliedModelSettingsRevision = nil; saved.codexConnectionRestored = true
            try self.remember(saved, in: model); if self.profile == original { self.select(saved) }
            self.prepared = .null; self.summary = ""
            self.notice = "已还原首次接入前的配置和文件认证。请重启远端 Codex；还原前状态另有备份。"
        }
    }
    func revoke(_ id: String, model: AppModel) {
        guard profile.kind == .codex else { return }
        perform("正在撤销远端 Key…") { self.status = try await model.remoteCall("remote.revoke", .object(["id":.string(id)])); self.prepared = .null; self.summary = "" }
    }
    func loadOMPStatus() {
        let original = profile
        perform("正在读取远端扩展状态…") {
            guard original.directory.hasPrefix("/") else { throw AppError.message("请填写远端 OMP 配置目录的绝对路径。") }
            let result = try await SSHRemoteClient(profile: original).call("status")
            guard self.profile == original else { throw AppError.message("连接信息已变化，请重新读取。") }
            self.ompStatus = result; self.ompStatusProfile = original
        }
    }
    func installOMPExtension(_ model: AppModel) { updateOMPConnection(model, install: true) }
    func updateOMPAddress(_ model: AppModel) { updateOMPConnection(model, install: false) }
    private func updateOMPConnection(_ model: AppModel, install: Bool) {
        let original = profile
        perform(install ? "正在安装远端扩展并连接…" : "正在更新远端连接地址…") {
            guard original.directory.hasPrefix("/") else { throw AppError.message("请填写远端 OMP 配置目录的绝对路径。") }
            var fields: [String: JSONValue] = [:]
            if install {
                guard let package = Bundle.main.url(forResource: "omp-extension", withExtension: "tgz") else {
                    throw AppError.message("内置扩展包缺失，请重新安装 Codexpress。")
                }
                fields["package"] = .string(try Data(contentsOf: package).base64EncodedString())
            }
            let binding = try await model.remoteCall("omp.bind")
            guard binding["running"].bool == true,
                  let local = binding["base_url"].string,
                  let port = URLComponents(string: local)?.port,
                  let key = binding["key"].string else { throw AppError.message("无法取得本机 OMP 接入信息，请检查网关。") }
            let baseURL = try original.ompBaseURL(port: port)
            fields["base_url"] = .string(baseURL)
            fields["key"] = .string(key)
            let client = SSHRemoteClient(profile: original)
            if install {
                let current = try await client.call("status")
                guard current["omp_executable"].string != nil else { throw AppError.message("远端未找到 OMP，请先安装官方版 OMP。") }
                _ = try await client.call("probe", fields: ["base_url": .string(baseURL), "key": .string(key)])
                try await self.migrateLegacyOMP(client, model: model)
            }
            let result = try await client.call(install ? "install" : "configure", fields: fields, timeout: install ? 180 : 60)
            guard self.profile == original else { throw AppError.message("远端操作已完成，但连接信息已变化，请重新读取状态。") }
            self.ompStatus = result; self.ompStatusProfile = original
            try self.remember(original, in: model)
            self.status = try await model.remoteCall("chatbot.status")
            if self.status["port"].number != Double(port) {
                self.notice = "远端操作已完成，但本机端口已变化，请更新连接地址。"
            } else if result["environment_override"].bool == true {
                self.notice = "连接已保存；远端仍有旧的 CODEXPRESS_BASE_URL 环境覆盖，请移除后启动 OMP。"
            } else {
                self.notice = install ? "远端扩展已安装并绑定。重启远端 OMP，在 /model 中选择 Codexpress 模型。" : "远端连接地址已更新，已有会话将在下一轮使用新地址。"
            }
        }
    }
    private func migrateLegacyOMP(_ client: SSHRemoteClient, model: AppModel) async throws {
        let previous = try await client.call("read")
        guard previous["snapshot"] != .null else { return }
        let restored = try await model.remoteCall("omp.remote.restore", .object([
            "agent_dir": previous["agent_dir"], "files": previous["files"],
            "snapshot": previous["snapshot"], "agent_db_exists": previous["agent_db_exists"]
        ]))
        _ = try await client.call("restore", fields: [
            "files": restored["files"], "snapshot": restored["snapshot"], "expected_digest": previous["digest"]
        ])
    }
}
