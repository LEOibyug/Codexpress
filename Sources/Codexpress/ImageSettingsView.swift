import SwiftUI
import AppKit

struct ImageSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = ""
    @State private var savedPort = ""
    @State private var sourceID = ""
    @State private var newKey = ""
    @State private var working = false
    @State private var loading = false
    @State private var copyingKey = false
    @State private var exporting = false
    @State private var configNeedsSync = false
    @State private var error: String?
    @State private var confirmReset = false
    private var address: String { status["base_url"].string ?? "—" }
    private var canEdit: Bool { model.running && !working && !loading && !exporting && status != .null }
    private var changed: Bool { !newKey.isEmpty || port != status["port"].number.map({ String(Int($0)) }) }
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("图片生成")
                if !model.running { InfoNote(text: "请先启动本机网关。") }
                if !model.sources.contains(where: { $0.official && $0.enabled }) { InfoNote(text: "请先在来源中添加并登录官方账号。", color: .orange) }
                SettingsCard("官方套餐生图", symbol: "photo", accessory: {
                    StatusPill(text: status["running"].bool == true ? "运行中" : status["enabled"].bool == false ? "已关闭" : "未启动", color: status["running"].bool == true ? .green : .secondary)
                }) {
                    SettingRow(title: "启用接入") {
                        Toggle("启用图片生成", isOn: Binding(get: {status["enabled"].bool ?? true}, set: { update(.object(["enabled":.bool($0)])) })).toggleStyle(.switch).labelsHidden().disabled(!canEdit)
                    }
                    Divider()
                    SettingRow(title: "监听端口") {
                        TextField("17866", text: Binding(get: { port }, set: { port = $0; error = nil }))
                            .font(.system(size: 13, design: .monospaced)).settingsField().frame(width: 130)
                            .multilineTextAlignment(.trailing).disabled(!canEdit)
                    }
                    SettingRow(title: "官方账号") {
                        Picker("官方账号", selection: Binding(get: { sourceID }, set: { update(.object(["source_id": $0.isEmpty ? .null : .string($0)])) })) {
                            Text("按来源优先级").tag("")
                            ForEach(model.sources.filter(\.official)) { Text($0.name + ($0.enabled ? "" : "（已停用）")).tag($0.id) }
                            if !sourceID.isEmpty && !model.sources.contains(where: {$0.id == sourceID}) { Text("原账号已移除").tag(sourceID) }
                        }.labelsHidden().frame(maxWidth: 360, alignment: .trailing).disabled(!canEdit)
                    }
                    SettingRow(title: "新 API Key") {
                        SecureField("留空保留原 Key", text: Binding(get: { newKey }, set: { newKey = $0; error = nil }))
                            .settingsField().frame(maxWidth: 360).disabled(!canEdit)
                    }
                    if !newKey.isEmpty { InfoNote(text: "应用后旧 Key 将失效，请同步更新客户端。", color: .orange) }
                    Divider()
                    HStack(spacing: 12) {
                        Button("复制当前 Key") { copyKey() }.disabled(!model.running || status == .null || working || copyingKey)
                        Button("重新生成…") { confirmReset = true }.disabled(!canEdit)
                        Spacer()
                        if working { ProgressView().controlSize(.small) }
                        Button("应用设置") { save() }.buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                    }
                    if let error { InfoNote(text: error, color: .orange) }
                    if error != nil || status == .null && model.running {
                        Button(loading ? "读取中…" : "重新读取") { Task { await load() } }.disabled(!model.running || loading || working || exporting)
                    }
                    if configNeedsSync {
                        Button("重试更新本机接入配置") { retryExport() }.disabled(!canEdit)
                    }
                    if status["enabled"].bool == true && status["running"].bool == false {
                        HStack {
                            InfoNote(text: "图片入口未启动。", color: .orange)
                            Button("重试启动") { update(.object([:])) }.disabled(!canEdit)
                        }
                    }
                }
                SettingsCard("接入 Codex 或其他 Agent", symbol: "doc.on.clipboard") {
                    HStack {
                        Text("gpt-image-2 · 生成／编辑 · PNG").font(.system(size: 12)).foregroundStyle(.secondary)
                        Spacer()
                        Button("复制接入提示词") { copyPrompt() }.buttonStyle(.borderedProminent).disabled(!canEdit)
                    }
                }
                SettingsCard("其他 Images API 客户端", symbol: "link") {
                    LabeledContent("API 基址") {
                        HStack(spacing: 10) {
                            Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                            Button { copy(address, message: "图片 API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                                .disabled(status == .null).accessibilityLabel("复制图片 API 地址").help("复制图片 API 地址")
                        }
                    }
                }
            }.settingsPage()
        }.task(id: model.running) { await load() }
            .alert("重新生成图片 API Key？", isPresented: $confirmReset) {
                Button("取消", role: .cancel) {}
                Button("重新生成") { update(.object(["regenerate_key": .bool(true)])) }
            } message: { Text("旧 Key 将无法发起新请求，本机接入配置会同步更新。其他客户端需要更换密钥。") }
    }
    private func load() async {
        guard model.running else { status = .null; error = nil; return }
        loading = true
        defer { loading = false }
        do {
            let actual = try await model.remoteCall("images.status")
            guard !Task.isCancelled, model.running else { return }
            apply(actual, updatingPort: port == savedPort)
            error = nil
        } catch { self.error = error.localizedDescription }
    }
    private func apply(_ value: JSONValue, updatingPort: Bool = true) {
        status = value
        savedPort = value["port"].number.map { String(Int($0)) } ?? ""
        if updatingPort { port = savedPort }
        sourceID = value["source_id"].string ?? ""
    }
    private func export() async throws -> URL {
        configNeedsSync = true
        let key = try await model.remoteCall("images.key")
        guard let key = key["key"].string else {throw AppError.message("无法读取图片接入密钥。")}
        let file = try ImageClientAccess.export(baseURL: address, key: key)
        configNeedsSync = false
        return file
    }
    private func save() {
        guard let p = Int(port), (1...65535).contains(p) else {error = "端口须为 1–65535 的整数。"; return}
        var payload: [String: JSONValue] = ["port": .number(Double(p))]
        if !newKey.isEmpty { payload["key"] = .string(newKey) }
        update(.object(payload))
    }
    private func update(_ payload: JSONValue) {
        guard canEdit else {return}; working = true; error = nil
        Task {
            defer {working = false}
            do {
                apply(try await model.remoteCall("images.configure", payload), updatingPort: payload["port"] != .null)
                if payload["key"] != .null { newKey = "" }
                if FileManager.default.fileExists(atPath: ImageClientAccess.file.path),
                   payload["port"] != .null || payload["key"] != .null || payload["regenerate_key"].bool == true {
                    do { _ = try await export() }
                    catch { self.error = "图片设置已生效，本机接入配置更新失败：\(error.localizedDescription)"; return }
                }
                model.notice = "图片接入设置已应用。"
            } catch {
                let message = error.localizedDescription
                if let actual = try? await model.remoteCall("images.status") { apply(actual, updatingPort: false) }
                self.error = message
            }
        }
    }
    private func copyKey() {
        guard model.running, status != .null, !working, !copyingKey else { return }
        copyingKey = true; error = nil
        Task {
            defer { copyingKey = false }
            do {
                let data = try await model.remoteCall("images.key")
                guard let key = data["key"].string else { throw AppError.message("无法读取图片接入密钥。") }
                copy(key, message: "图片 API Key 已复制。")
            } catch { self.error = error.localizedDescription }
        }
    }
    private func copyPrompt() {
        guard canEdit else { return }; exporting = true; error = nil
        Task {
            defer { exporting = false }
            do {
                let skill = Bundle.main.bundleURL.appendingPathComponent("Contents/Resources/skills/codexpress-image/SKILL.md")
                guard FileManager.default.fileExists(atPath: skill.path) else {throw AppError.message("应用内的生图 Skill 不完整，请重新安装 Codexpress。")}
                let config = try await export()
                copy(ImageClientAccess.prompt(bundle: Bundle.main.bundleURL, config: config), message: "接入提示词已复制，可发送给本机 Agent。")
            } catch {self.error = error.localizedDescription}
        }
    }
    private func retryExport() {
        guard canEdit else { return }; exporting = true; error = nil
        Task {
            defer { exporting = false }
            do {
                _ = try await export()
                model.notice = "本机图片接入配置已更新。"
            } catch { self.error = error.localizedDescription }
        }
    }
    private func copy(_ text: String, message: String) { NSPasteboard.general.clearContents();NSPasteboard.general.setString(text,forType:.string);model.notice = message }
}
