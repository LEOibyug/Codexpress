import Foundation
import SwiftUI

@MainActor final class SSHRemoteModel: ObservableObject {
    @Published var profile = SSHRemoteProfile()
    @Published var port = "17864"
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
    private var prepared: JSONValue = .null
    private var expectedDigest = ""
    private var preparedProfile: SSHRemoteProfile?
    private var preparedPort = ""
    private var preparedRules: [ModelCompactionRule] = []
    private var expectedCatalogDigest = ""
    private var expectedCompactionStateDigest = ""
    private var expectedFiles: JSONValue = .array([])
    func canApply(_ model: AppModel) -> Bool { prepared != .null && preparedProfile == profile && preparedPort == port && preparedRules == (model.settings.modelCompaction ?? []) }
    var running: Bool { status["running"].bool == true }
    var canRestore: Bool { !profile.destination.isEmpty && profile.codexHome.hasPrefix("/") }
    private func perform(_ label: String, action: @escaping () async throws -> Void) {
        guard !busy else { return }; busy = true; phase = label; error = nil; notice = nil
        Task { defer { busy = false; phase = "" }; do { try await action() } catch { self.error = error.localizedDescription } }
    }
    func refresh(_ model: AppModel) async {
        guard model.running, !busy else { return }
        do { status = try await model.remoteCall("remote.status"); if let value = status["port"].number { port = String(Int(value)) } }
        catch { self.error = error.localizedDescription }
    }
    func select(_ value: SSHRemoteProfile) { profile = value; summary = ""; prepared = .null; candidates = [] }
    func loadSSHConfig() async {
        guard !readingSSHConfig else { return }
        readingSSHConfig = true
        let catalog = await Task.detached(priority: .utility) { SSHConfigCatalog.load() }.value
        configuredHosts = catalog.hosts
        sshConfigMessage = catalog.incomplete ? "部分 SSH 配置未能读取，可手动填写别名。" : !catalog.exists ? "未找到 ~/.ssh/config，可手动填写连接。" : catalog.hosts.isEmpty ? "配置中没有明确的 Host 别名，可手动填写。" : nil
        readingSSHConfig = false
    }
    func selectConfiguredHost(_ alias: String, saved: [SSHRemoteProfile]) {
        var next = saved.first { $0.destination.caseInsensitiveCompare(alias) == .orderedSame } ?? SSHRemoteProfile()
        next.destination = alias
        next.username = ""; next.sshPort = ""; next.identityFile = ""
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
            self.candidates = result["candidates"].array.compactMap(\.string)
            if self.profile.codexHome.isEmpty { self.profile.codexHome = self.candidates.first ?? "" }
            if self.profile.gatewayHost.isEmpty { self.profile.gatewayHost = result["client_ip"].string ?? "" }
            try self.remember(self.profile, in: model)
            self.notice = self.candidates.isEmpty ? "未找到 Codex 目录，请手动填写远端绝对路径。" : "已找到远端目录和 SSH 连接地址，请核对后预览。"
        }
    }
    func preview(_ model: AppModel) {
        let original = profile; let originalPort = port; let rules = model.settings.modelCompaction ?? []
        perform("正在读取并预览远端配置…") {
            try ModelCompactionRule.validate(rules)
            guard original.codexHome.hasPrefix("/"), !original.gatewayHost.isEmpty else { throw AppError.message("请先填写远端 Codex 目录和本机地址。") }
            // Preview does not start a network listener. Port configuration is applied separately.
            guard self.status["port"].number == Double(originalPort) else { throw AppError.message("入口端口尚未生效，请先开启入口或将端口改回当前值。") }
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
            guard self.profile == original, self.port == originalPort, (model.settings.modelCompaction ?? []) == rules else { throw AppError.message("设置已变化，请重新预览。") }
            self.prepared = rendered; self.expectedDigest = result["digest"].string ?? ""
            self.expectedCatalogDigest = result["catalog_digest"].string ?? ""
            self.expectedCompactionStateDigest = result["compaction_state_digest"].string ?? ""
            self.expectedFiles = checks; self.preparedRules = rules
            self.preparedProfile = original; self.preparedPort = originalPort
            var summary = rendered["summary"].string ?? ""
            if rules.isEmpty { summary += base == nil ? "\n模型压缩 = 保留远端设置" : "\n模型压缩 = 恢复远端原设置" }
            else {
                summary += "\n模型压缩（\(rules.count)）"
                for rule in rules { summary += "\n\(rule.model)：压缩 \(rule.auto_compact_token_limit.formatted()) Token" + (rule.context_window.map { "，窗口 \($0.formatted())" } ?? "") }
            }
            self.summary = summary; self.aliases = rendered["aliases"].array.compactMap(\.string)
            self.status = try await model.remoteCall("remote.status")
            try self.remember(original, in: model)
        }
    }
    func start(_ model: AppModel) {
        let text = port
        perform("正在开启远端入口…") {
            guard let value = Int(text), (1...65535).contains(value), value != model.settings.port else { throw AppError.message("入口端口须为 1–65535，且不能与本机网关端口相同。") }
            self.status = try await model.remoteCall("remote.start", .object(["port":.number(Double(value))]))
            self.prepared = .null; self.summary = ""
        }
    }
    func stop(_ model: AppModel) {
        perform("正在关闭远端入口…") { self.status = try await model.remoteCall("remote.stop") }
    }
    func apply(_ model: AppModel) {
        guard canApply(model) else { error = "请先重新预览远端配置。"; return }
        let original = profile; let prepared = prepared; let digest = expectedDigest
        perform("正在验证连通性并备份远端配置…") {
            guard self.running else { throw AppError.message("请先开启远端入口。") }
            let fields: [String:JSONValue] = ["base_url":prepared["base_url"],"key":prepared["key"],"expected_digest":.string(digest),"config":.string(Data((prepared["config"].string ?? "").utf8).base64EncodedString()),"catalog_action":prepared["catalog_action"],"catalog":prepared["catalog"],"expected_catalog_digest":.string(self.expectedCatalogDigest),"expected_compaction_state_digest":.string(self.expectedCompactionStateDigest),"expected_files":self.expectedFiles]
            let result = try await SSHRemoteClient(profile: original).call("apply", fields: fields)
            var saved = original; saved.backupID = result["backup_id"].string
            saved.originalBackupID = result["original_backup_id"].string
            try self.remember(saved, in: model)
            if self.profile == original { self.profile = saved }
            self.prepared = .null
            self.notice = "已备份并应用远端来源池配置。请重启远端 Codex，已运行的对话不会热更新。"
        }
    }
    func restore(_ model: AppModel) {
        let original = profile
        perform("正在备份当前状态并还原接入前设置…") {
            guard self.canRestore else { throw AppError.message("请先填写 SSH 连接和远端 Codex 目录。") }
            let result = try await SSHRemoteClient(profile: original).call("restore")
            var saved = original; saved.backupID = result["recovery_backup_id"].string
            saved.originalBackupID = result["original_backup_id"].string
            try self.remember(saved, in: model); if self.profile == original { self.profile = saved }
            self.prepared = .null; self.summary = ""
            self.notice = "已还原首次接入前的配置和文件认证。请重启远端 Codex；还原前状态另有备份。"
        }
    }
    func revoke(_ id: String, model: AppModel) {
        perform("正在撤销远端 Key…") { self.status = try await model.remoteCall("remote.revoke", .object(["id":.string(id)])); self.prepared = .null; self.summary = "" }
    }
}
