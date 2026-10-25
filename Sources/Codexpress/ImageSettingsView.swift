import SwiftUI
import AppKit

struct ImageSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = ""
    @State private var sourceID = ""
    @State private var newKey = ""
    @State private var working = false
    @State private var error: String?
    @State private var confirmReset = false
    private var address: String { status["base_url"].string ?? "—" }
    private var canEdit: Bool { model.running && !working && !model.busy && status != .null }
    private var changed: Bool { !newKey.isEmpty || port != status["port"].number.map({String(Int($0))}) || sourceID != (status["source_id"].string ?? "") }
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                PageHeading("图片生成")
                if !model.sources.contains(where: { $0.official && $0.enabled }) { InfoNote(text: "请先在来源中添加并登录官方账号。", color: .orange) }
                SettingsCard("官方套餐生图", symbol: "photo", accessory: {
                    StatusPill(text: status["running"].bool == true ? "运行中" : status["enabled"].bool == false ? "已关闭" : "未启动", color: status["running"].bool == true ? .green : .secondary)
                }) {
                    SettingRow(title: "启用接入") {
                        Toggle("启用图片生成", isOn: Binding(get: {status["enabled"].bool ?? true}, set: { update(.object(["enabled":.bool($0)])) })).toggleStyle(.switch).labelsHidden().controlSize(.small).disabled(!canEdit)
                    }
                    Divider()
                    SettingRow(title: "监听端口") { TextField("17866", text: Binding(get: { port }, set: { port = $0; error = nil })).settingsField().frame(width: 100).multilineTextAlignment(.trailing).disabled(!canEdit) }
                    SettingRow(title: "官方账号") {
                        Picker("官方账号", selection: Binding(get: { sourceID }, set: { sourceID = $0; error = nil })) {
                            Text("按来源优先级").tag("")
                            ForEach(model.sources.filter(\.official)) { Text($0.name + ($0.enabled ? "" : "（已停用）")).tag($0.id) }
                            if !sourceID.isEmpty && !model.sources.contains(where: {$0.id == sourceID}) { Text("原账号已移除").tag(sourceID) }
                        }.labelsHidden().frame(maxWidth: 240).disabled(!canEdit)
                    }
                    SettingRow(title: "新 API Key", subtitle: "留空保留现有密钥") { SecureField("16–256 位字符", text: Binding(get: { newKey }, set: { newKey = $0; error = nil })).settingsField().frame(maxWidth: 270).disabled(!canEdit) }
                    Text("使用所选官方账号及其代理，仅供本机调用。生图可用额度以官方生图接口返回为准。").font(.system(size: 11)).foregroundStyle(.secondary)
                    HStack(spacing: 10) {
                        Button("复制当前 Key") { copyKey() }.disabled(!canEdit)
                        Button("重新生成…") { confirmReset = true }.disabled(!canEdit)
                        Spacer()
                        if working { ProgressView().controlSize(.small) }
                        Button("应用设置") { save() }.buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                    }.controlSize(.small)
                    if let error { InfoNote(text: error, color: .orange) }
                    if status["enabled"].bool == true && status["running"].bool == false { InfoNote(text: "图片入口未启动，请检查端口后重新应用设置。", color: .orange) }
                }
                SettingsCard("接入 Codex 或其他 Agent", symbol: "doc.on.clipboard") {
                    Text("复制提示词发给 Agent，即可使用应用内置的 skill 和调用脚本。").font(.system(size: 12))
                    HStack {
                        Text("gpt-image-2 · 生成／编辑 · PNG").font(.system(size: 11)).foregroundStyle(.secondary)
                        Spacer()
                        Button("复制接入提示词") { copyPrompt() }.buttonStyle(.borderedProminent).disabled(!canEdit)
                    }
                    Text("接入提示词不含密钥；密钥保存在本机私有配置中。").font(.system(size: 11)).foregroundStyle(.secondary)
                }
                SettingsCard("其他 Images API 客户端", symbol: "link") {
                    HStack {
                        Text("API 基址").foregroundStyle(.secondary); Spacer()
                        Text(address).font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                        Button { copy(address, message: "图片 API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }.buttonStyle(.plain).disabled(status == .null).accessibilityLabel("复制图片 API 地址")
                    }
                    Text("在支持自定义图片接口的 Skill 或客户端中填写此地址与独立 Key，选择 gpt-image-2 和 PNG 输出。").font(.system(size: 11)).foregroundStyle(.secondary)
                }
            }.settingsPage()
        }.task(id: model.running) { await load() }
            .alert("重新生成图片 API Key？", isPresented: $confirmReset) {
                Button("取消", role: .cancel) {}
                Button("重新生成") { update(.object(["regenerate_key":.bool(true)])) }
            } message: { Text("旧 Key 将失效，本机接入配置会同步更新。其他客户端需要更换密钥。") }
    }
    private func load() async {
        guard model.running else {return}
        do { apply(try await model.remoteCall("images.status")); error = nil } catch {self.error = error.localizedDescription}
    }
    private func apply(_ value: JSONValue) { status = value; port = value["port"].number.map {String(Int($0))} ?? ""; sourceID = value["source_id"].string ?? "" }
    private func export() async throws -> URL {
        let key = try await model.remoteCall("images.key")
        guard let key = key["key"].string else {throw AppError.message("无法读取图片接入密钥。")}
        return try ImageClientAccess.export(baseURL: address, key: key)
    }
    private func save() {
        guard let p = Int(port), (1...65535).contains(p) else {error = "端口须为 1–65535 的整数。"; return}
        var payload: [String:JSONValue] = ["port":.number(Double(p)),"source_id":sourceID.isEmpty ? .null : .string(sourceID)]
        if !newKey.isEmpty {payload["key"] = .string(newKey)}
        update(.object(payload))
    }
    private func update(_ payload: JSONValue) {
        guard canEdit else {return}; working = true; error = nil
        Task {
            defer {working = false}
            do {
                apply(try await model.remoteCall("images.configure", payload)); newKey = ""
                if FileManager.default.fileExists(atPath: ImageClientAccess.file.path) { _ = try await export() }
                model.notice = "图片接入设置已应用。"
            } catch {self.error = error.localizedDescription}
        }
    }
    private func copyKey() {
        guard canEdit else {return}; working = true; error = nil
        Task {defer {working = false}; do {let data = try await model.remoteCall("images.key");guard let key = data["key"].string else {throw AppError.message("无法读取图片接入密钥。")};copy(key,message:"图片 API Key 已复制。")}catch{self.error = error.localizedDescription}}
    }
    private func copyPrompt() {
        guard canEdit else {return}; working = true; error = nil
        Task {
            defer {working = false}
            do {
                let skill = Bundle.main.bundleURL.appendingPathComponent("Contents/Resources/skills/codexpress-image/SKILL.md")
                guard FileManager.default.fileExists(atPath: skill.path) else {throw AppError.message("应用内的生图 Skill 不完整，请重新安装 Codexpress。")}
                let config = try await export()
                copy(ImageClientAccess.prompt(bundle: Bundle.main.bundleURL, config: config), message: "接入提示词已复制，可发送给本机 Agent。")
            } catch {self.error = error.localizedDescription}
        }
    }
    private func copy(_ text: String, message: String) { NSPasteboard.general.clearContents();NSPasteboard.general.setString(text,forType:.string);model.notice = message }
}
