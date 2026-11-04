import Foundation
import SwiftUI

@MainActor final class SSHRemoteModel: ObservableObject {
    @Published var profile = SSHRemoteProfile()
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
    @Published var ompDraft = OMPConfiguration()
    @Published private(set) var ompConfiguration: JSONValue = .null
    @Published private(set) var ompConfigurationRevision = ""
    @Published private(set) var ompDraftRefresh = 0
    @Published var localOMPDirectory = ""
    private var ompConfigurationProfile: SSHRemoteProfile?
    private var ompConfigurationDigest = ""
    private var preparedOMPChanges: [OMPConfiguration.Field: String] = [:]
    var hasOMPConfiguration: Bool {
        guard ompConfiguration != .null, let loaded = ompConfigurationProfile else { return false }
        return loaded.kind == profile.kind && loaded.destination == profile.destination
            && loaded.username == profile.username && loaded.sshPort == profile.sshPort
            && loaded.identityFile == profile.identityFile && loaded.directory == profile.directory
    }
    private var prepared: JSONValue = .null
    private var expectedDigest = ""
    private var preparedProfile: SSHRemoteProfile?
    private var preparedPort = ""
    private var preparedRules: [ModelCompactionRule] = []
    private var expectedCatalogDigest = ""
    private var expectedCompactionStateDigest = ""
    private var expectedFiles: JSONValue = .array([])
    func canApply(_ model: AppModel) -> Bool {
        prepared != .null && preparedProfile == profile && preparedPort == port
            && (profile.kind == .omp ? preparedOMPChanges == ompDraft.changes : preparedRules == (model.settings.modelCompaction ?? []))
    }
    var running: Bool { status["running"].bool == true }
    var canRestore: Bool { !profile.destination.isEmpty && profile.directory.hasPrefix("/") }
    private func perform(_ label: String, action: @escaping () async throws -> Void) {
        guard !busy else { return }; busy = true; phase = label; error = nil; notice = nil
        Task { defer { busy = false; phase = "" }; do { try await action() } catch { self.error = error.localizedDescription } }
    }
    func refresh(_ model: AppModel) async {
        guard model.running, !busy else { return }
        let kind = profile.kind
        do {
            let result = try await model.remoteCall(kind == .omp ? "chatbot.status" : "remote.status")
            if profile.kind == kind { status = result }
        }
        catch { self.error = error.localizedDescription }
    }
    func select(_ value: SSHRemoteProfile) { profile = value; summary = ""; prepared = .null; candidates = []; clearOMPConfiguration() }
    func selectKind(_ kind: SSHRemoteKind) {
        guard kind != profile.kind else { return }
        profile.clientKind = kind; profile.id = UUID().uuidString
        profile.backupID = nil; profile.originalBackupID = nil
        status = .null; prepared = .null; summary = ""; candidates = []; aliases = []
        error = nil; notice = nil
        clearOMPConfiguration()
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
        var next = saved.first { $0.kind == kind && $0.destination.caseInsensitiveCompare(alias) == .orderedSame } ?? SSHRemoteProfile()
        next.clientKind = kind
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
            if original.kind == .omp {
                if self.profile.directory.isEmpty { self.profile.ompAgentDirectory = self.candidates.first ?? "" }
            } else if self.profile.codexHome.isEmpty { self.profile.codexHome = self.candidates.first ?? "" }
            if self.profile.gatewayHost.isEmpty { self.profile.gatewayHost = result["client_ip"].string ?? "" }
            try self.remember(self.profile, in: model)
            self.notice = self.candidates.isEmpty ? "未找到 \(original.kind.title) 目录，请手动填写远端绝对路径。" : "已找到远端目录和 SSH 连接地址，请核对后预览。"
        }
    }
    func preview(_ model: AppModel) {
        if profile.kind == .omp { previewOMP(model); return }
        let original = profile; let originalPort = port; let rules = model.settings.modelCompaction ?? []
        perform("正在读取并预览远端配置…") {
            try ModelCompactionRule.validate(rules)
            guard original.codexHome.hasPrefix("/"), !original.gatewayHost.isEmpty else { throw AppError.message("请先填写远端 Codex 目录和本机地址。") }
            guard self.running, self.status["port"].number == model.status["port"].number else { throw AppError.message("网关端口已变化，请重新打开页面后预览。") }
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
    func apply(_ model: AppModel) {
        if profile.kind == .omp { applyOMP(model); return }
        guard canApply(model) else { error = "请先重新预览远端配置。"; return }
        let original = profile; let prepared = prepared; let digest = expectedDigest
        perform("正在验证连通性并备份远端配置…") {
            guard self.running else { throw AppError.message("请先启动本地网关。") }
            let fields: [String:JSONValue] = ["base_url":prepared["base_url"],"key":prepared["key"],"expected_digest":.string(digest),"config":.string(Data((prepared["config"].string ?? "").utf8).base64EncodedString()),"catalog_action":prepared["catalog_action"],"catalog":prepared["catalog"],"expected_catalog_digest":.string(self.expectedCatalogDigest),"expected_compaction_state_digest":.string(self.expectedCompactionStateDigest),"expected_files":self.expectedFiles]
            let result = try await SSHRemoteClient(profile: original).call("apply", fields: fields)
            var saved = original; saved.backupID = result["backup_id"].string
            saved.originalBackupID = result["original_backup_id"].string
            try self.remember(saved, in: model)
            if self.profile == original { self.profile = saved }
            self.prepared = .null
            self.notice = "已备份并应用远端来源池配置。请重启远端 Codex；使用 VS Code 时，请重载对应的远端窗口。"
        }
    }
    func restore(_ model: AppModel) {
        if profile.kind == .omp { restoreOMP(model); return }
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
        guard profile.kind == .codex else { return }
        perform("正在撤销远端 Key…") { self.status = try await model.remoteCall("remote.revoke", .object(["id":.string(id)])); self.prepared = .null; self.summary = "" }
    }
    private func clearOMPConfiguration() {
        ompConfiguration = .null; ompDraft = OMPConfiguration()
        ompConfigurationRevision = ""
        ompConfigurationProfile = nil; ompConfigurationDigest = ""; ompDraftRefresh += 1
    }
    func detectLocalOMPDirectory() async {
        guard localOMPDirectory.isEmpty else { return }
        let installation = await OMPInstallation.detect()
        if localOMPDirectory.isEmpty { localOMPDirectory = installation.agentDirectory }
    }
    private func renderOMP(_ operation: String, original: SSHRemoteProfile, bundle: JSONValue, edits: [JSONValue] = [], model: AppModel) async throws -> JSONValue {
        try await model.remoteCall(operation, .object([
            "agent_dir": bundle["agent_dir"], "files": bundle["files"], "snapshot": bundle["snapshot"],
            "agent_db_exists": bundle["agent_db_exists"], "model_id": .string(original.ompDefaultModel ?? ""),
            "host": .string(original.gatewayHost), "configuration_edits": .array(edits)
        ]))
    }
    private func requireOMPConnection(_ original: SSHRemoteProfile) throws {
        guard original.directory.hasPrefix("/") else { throw AppError.message("请填写远端 OMP 配置目录的绝对路径。") }
        guard !original.gatewayHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw AppError.message("请填写远端可访问的本机地址后预览连接。")
        }
        guard !(original.ompDefaultModel ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw AppError.message("请填写默认模型后预览连接。")
        }
    }
    func loadOMPConfiguration(_ model: AppModel) {
        let original = profile
        perform("正在读取远端 OMP 配置…") {
            guard original.directory.hasPrefix("/") else { throw AppError.message("请填写远端 OMP 配置目录的绝对路径。") }
            let bundle = try await SSHRemoteClient(profile: original).call("read")
            let rendered = try await self.renderOMP("omp.remote.read", original: original, bundle: bundle, model: model)
            guard self.profile == original else { throw AppError.message("设置已变化，请重新读取远端配置。") }
            self.ompConfiguration = rendered["configuration"]
            self.ompConfigurationRevision = rendered["configuration_revision"].string ?? ""
            self.ompDraft = OMPConfiguration(stored: rendered["configuration"])
            if (self.profile.ompDefaultModel ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
               let selected = rendered["model_id"].string { self.profile.ompDefaultModel = selected }
            self.ompConfigurationProfile = self.profile; self.ompConfigurationDigest = bundle["digest"].string ?? ""
            self.ompDraftRefresh += 1; self.prepared = .null; self.summary = ""
            self.notice = "已读取远端 OMP 配置，尚未写入任何更改。"
        }
    }
    func importLocalOMPConfiguration(_ model: AppModel) {
        guard hasOMPConfiguration else { error = "请先读取远端 OMP 配置。"; return }
        let original = profile
        let directory = (localOMPDirectory as NSString).expandingTildeInPath
        perform("正在读取本地已保存的 OMP 配置…") {
            guard directory.hasPrefix("/") else { throw AppError.message("请填写本地 OMP 配置目录的绝对路径。") }
            let value = try await model.remoteCall("omp.read", .object(["agent_dir": .string(directory)]))
            guard self.profile == original, self.hasOMPConfiguration else { throw AppError.message("设置已变化，请重新读取远端配置。") }
            let skipped = self.ompDraft.importSaved(value["configuration"])
            self.ompDraftRefresh += 1
            self.notice = skipped.isEmpty ? "已填入本地压缩与模型覆盖，请预览后应用。" : "已填入本地配置；远端没有这些模型，未导入：" + skipped.joined(separator: "、")
        }
    }
    func useOpenAIOMPConfiguration() {
        guard hasOMPConfiguration, !busy else { return }
        ompDraft.applyOpenAIPreset(); ompDraftRefresh += 1
        notice = "已填入 OpenAI 推荐配置，请预览后应用。"
    }
    private func previewOMP(_ model: AppModel) {
        if !hasOMPConfiguration { loadOMPConfiguration(model); return }
        guard let edits = ompDraft.edits() else { error = "请修正 OMP 配置中标出的字段后预览。"; return }
        let original = profile; let changes = ompDraft.changes; let digest = ompConfigurationDigest
        perform("正在预览远端 OMP 连接与配置…") {
            try self.requireOMPConnection(original)
            let connection = try await model.remoteCall("chatbot.status")
            self.status = connection
            guard self.running else { throw AppError.message("请先启用聊天客户端接入。") }
            let originalPort = self.port
            let bundle = try await SSHRemoteClient(profile: original).call("read")
            guard bundle["digest"].string == digest else { throw AppError.message("远端 OMP 配置已变化，请重新读取后编辑。") }
            let rendered = try await self.renderOMP("omp.remote.preview", original: original, bundle: bundle, edits: edits, model: model)
            let current = try await model.remoteCall("chatbot.status")
            guard self.profile == original, self.ompDraft.changes == changes,
                  current["port"] == connection["port"], current["running"].bool == true else {
                throw AppError.message("设置或聊天客户端端口已变化，请重新预览。")
            }
            self.status = current; self.prepared = rendered; self.expectedDigest = digest
            self.preparedProfile = original; self.preparedPort = originalPort; self.preparedOMPChanges = changes
            self.summary = rendered["summary"].string ?? ""; self.aliases = []
            try self.remember(original, in: model)
        }
    }
    private func applyOMP(_ model: AppModel) {
        guard canApply(model) else { error = "请先重新预览远端 OMP 配置。"; return }
        let original = profile; let rendered = prepared; let digest = expectedDigest; let originalPort = port
        perform("正在验证聊天客户端入口并备份远端 OMP 配置…") {
            let connection = try await model.remoteCall("chatbot.status")
            self.status = connection
            guard self.profile == original, self.running, self.port == originalPort else {
                throw AppError.message("设置或聊天客户端端口已变化，请重新预览。")
            }
            let result = try await SSHRemoteClient(profile: original).call("apply", fields: [
                "base_url": rendered["base_url"], "key": rendered["key"], "files": rendered["files"],
                "snapshot": rendered["snapshot"], "expected_digest": .string(digest)
            ])
            var saved = original; saved.ompAgentDirectory = result["agent_dir"].string ?? original.directory
            saved.backupID = result["backup_id"].string
            try self.remember(saved, in: model); if self.profile == original { self.profile = saved }
            self.prepared = .null
            self.clearOMPConfiguration()
            self.notice = "已备份并应用远端 OMP 配置。使用聊天客户端共享 Key（不是每台机器独立的 Key），请重启远端 OMP。"
        }
    }
    private func restoreOMP(_ model: AppModel) {
        let original = profile
        perform("正在还原远端 OMP 接入设置…") {
            guard self.canRestore else { throw AppError.message("请先填写 SSH 连接和远端 OMP 目录。") }
            let bundle = try await SSHRemoteClient(profile: original).call("read")
            let rendered = try await self.renderOMP("omp.remote.restore", original: original, bundle: bundle, model: model)
            guard self.profile == original else { throw AppError.message("设置已变化，请重新还原。") }
            let result = try await SSHRemoteClient(profile: original).call("restore", fields: [
                "files": rendered["files"], "snapshot": rendered["snapshot"], "expected_digest": bundle["digest"]
            ])
            var saved = original; saved.ompAgentDirectory = result["agent_dir"].string ?? original.directory
            saved.backupID = result["backup_id"].string
            try self.remember(saved, in: model); if self.profile == original { self.profile = saved }
            self.prepared = .null; self.summary = ""
            self.clearOMPConfiguration()
            self.notice = "已撤回远端 OMP 的 Codexpress 接入设置，保留无关修改；还原前配置已另行备份，请重启远端 OMP。"
        }
    }
}
