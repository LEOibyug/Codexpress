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
    private var prepared: JSONValue = .null
    private var expectedDigest = ""
    private var preparedProfile: SSHRemoteProfile?
    private var preparedPort = ""
    var canApply: Bool { prepared != .null && preparedProfile == profile && preparedPort == port }
    var running: Bool { status["running"].bool == true }
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
        let original = profile; let originalPort = port
        perform("正在读取并预览远端配置…") {
            guard original.codexHome.hasPrefix("/"), !original.gatewayHost.isEmpty else { throw AppError.message("请先填写远端 Codex 目录和本机地址。") }
            // Preview does not start a network listener. Port configuration is applied separately.
            guard self.status["port"].number == Double(originalPort) else { throw AppError.message("入口端口尚未生效，请先开启入口或将端口改回当前值。") }
            let result = try await SSHRemoteClient(profile: original).call("read")
            guard let encoded = result["config"].string, let bytes = Data(base64Encoded: encoded), let config = String(data: bytes, encoding: .utf8) else { throw AppError.message("远端配置不是有效 UTF-8。") }
            let rendered = try await model.remoteCall("remote.preview", .object(["id":.string(original.id),"name":.string(original.username.isEmpty ? original.destination : "\(original.username)@\(original.destination)"),"host":.string(original.gatewayHost),"config":.string(config),"aliases":.bool(original.mapExistingProviders)]))
            guard self.profile == original, self.port == originalPort else { throw AppError.message("设置已变化，请重新预览。") }
            self.prepared = rendered; self.expectedDigest = result["digest"].string ?? ""
            self.preparedProfile = original; self.preparedPort = originalPort
            self.summary = rendered["summary"].string ?? ""; self.aliases = rendered["aliases"].array.compactMap(\.string)
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
        guard canApply else { error = "请先重新预览远端配置。"; return }
        let original = profile; let prepared = prepared; let digest = expectedDigest
        perform("正在验证连通性并备份远端配置…") {
            guard self.running else { throw AppError.message("请先开启远端入口。") }
            let fields: [String:JSONValue] = ["base_url":prepared["base_url"],"key":prepared["key"],"expected_digest":.string(digest),"config":.string(Data((prepared["config"].string ?? "").utf8).base64EncodedString())]
            let result = try await SSHRemoteClient(profile: original).call("apply", fields: fields)
            var saved = original; saved.backupID = result["backup_id"].string
            try self.remember(saved, in: model)
            if self.profile == original { self.profile = saved }
            self.prepared = .null
            self.notice = "已备份并应用远端来源池配置。请重启远端 Codex，已运行的对话不会热更新。"
        }
    }
    func restore(_ model: AppModel) {
        let original = profile
        perform("正在备份当前状态并还原…") {
            guard let backup = original.backupID else { throw AppError.message("此机器没有已记录的备份。") }
            let result = try await SSHRemoteClient(profile: original).call("restore", fields: ["backup_id":.string(backup)])
            var saved = original; saved.backupID = result["recovery_backup_id"].string
            try self.remember(saved, in: model); if self.profile == original { self.profile = saved }
            self.prepared = .null; self.summary = ""
            self.notice = "已还原远端配置和文件认证。请重启远端 Codex；还原前状态也已备份。"
        }
    }
    func revoke(_ id: String, model: AppModel) {
        perform("正在撤销远端 Key…") { self.status = try await model.remoteCall("remote.revoke", .object(["id":.string(id)])); self.prepared = .null; self.summary = "" }
    }
}
