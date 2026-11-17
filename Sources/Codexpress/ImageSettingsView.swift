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
    @State private var settingsError: String?
    @State private var error: String?
    @State private var confirmReset = false
    @State private var showsSettings = false
    private var address: String? { model.running ? status["base_url"].string : nil }
    private var canEdit: Bool { model.running && !working && !loading && !exporting && status != .null }
    private var changed: Bool { !newKey.isEmpty || port != savedPort }
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("图片生成", subtitle: "使用启用的官方账号生成或编辑图片。")
                SettingsCard("本机 Agent 接入", symbol: "photo", accessory: {
                    ConnectionListenerStatus(gatewayRunning: model.running, loading: loading, status: status)
                }) {
                    SettingRow(title: "启用图片 API") {
                        Toggle("启用图片生成", isOn: Binding(get: { status["enabled"].bool ?? true }, set: { update(.object(["enabled": .bool($0)])) }))
                            .toggleStyle(.switch).labelsHidden().disabled(!canEdit)
                    }
                    SettingRow(title: "gpt-image-2", subtitle: "生成／编辑 · PNG。将接入提示词发送给本机 Agent。") {
                        if exporting { ProgressView().controlSize(.small) }
                        Button("复制接入提示词") { copyPrompt() }.buttonStyle(.borderedProminent)
                            .disabled(!canEdit || address == nil)
                    }
                    if !model.running {
                        InfoNote(text: "请先在设置中启动本机网关。")
                    } else if !model.sources.contains(where: { $0.official && $0.enabled }) {
                        InfoNote(text: "请先在来源池添加并启用官方账号。", color: .orange)
                    }
                    if model.running && status["enabled"].bool == true && status["running"].bool == false {
                        SettingRow(title: "图片入口未启动") {
                            Button("重试启动") { update(.object([:])) }.disabled(!canEdit)
                        }
                    }
                    if configNeedsSync {
                        Button("重试更新本机接入配置") { retryExport() }.disabled(!canEdit || address == nil)
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    if model.running && (error != nil || (status == .null && !loading)) {
                        Button("重新读取") { Task { await load() } }.disabled(loading || working || exporting)
                    }
                }
                apiSettings
            }.settingsPage()
        }.task(id: model.running) { await load() }
            .onChange(of: port) { _, _ in settingsError = nil }
            .onChange(of: newKey) { _, _ in settingsError = nil }
            .alert("重新生成图片 API Key？", isPresented: $confirmReset) {
                Button("取消", role: .cancel) {}
                Button("重新生成") { update(.object(["regenerate_key": .bool(true)])) }
            } message: { Text("旧 Key 将无法发起新请求，本机接入配置会同步更新。其他客户端需要更换密钥。") }
    }

    private var apiSettings: some View {
        SettingsCard("账号与 API 设置", symbol: "slider.horizontal.3") {
            SettingRow(title: "官方账号", subtitle: "选择后立即生效。") {
                Picker("官方账号", selection: Binding(get: { sourceID }, set: { update(.object(["source_id": $0.isEmpty ? .null : .string($0)]), fromAPISettings: true) })) {
                    Text("按来源优先级").tag("")
                    ForEach(model.sources.filter(\.official)) { Text($0.name + ($0.enabled ? "" : "（已停用）")).tag($0.id) }
                    if !sourceID.isEmpty && !model.sources.contains(where: { $0.id == sourceID }) { Text("原账号已移除").tag(sourceID) }
                }.labelsHidden().frame(maxWidth: 320, alignment: .trailing).disabled(!canEdit)
            }
            if let address {
                SettingRow(title: "已应用的 Images API 基址") {
                    HStack(spacing: 8) {
                        Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                        Button { copy(address, message: "图片 API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                            .accessibilityLabel("复制图片 API 地址").help("复制图片 API 地址")
                        Button("复制当前 Key") { copyKey() }.disabled(!canEdit || copyingKey)
                    }
                }
            }
            Divider()
            advancedSettings
            if let settingsError { InfoNote(text: settingsError, symbol: "exclamationmark.triangle", color: .orange) }
        }
    }

    private var advancedSettings: some View {
        DisclosureGroup(isExpanded: $showsSettings) {
            VStack(alignment: .leading, spacing: 12) {
                SettingRow(title: "监听端口") {
                    TextField("17866", text: $port)
                        .font(.system(size: 13, design: .monospaced)).settingsField().frame(width: 110)
                        .accessibilityLabel("监听端口").multilineTextAlignment(.trailing).disabled(!canEdit)
                }
                SettingRow(title: "新 API Key") {
                    SecureField("留空保留原 Key", text: $newKey)
                        .settingsField().frame(maxWidth: 300).disabled(!canEdit)
                }
                if !newKey.isEmpty { InfoNote(text: "应用后旧 Key 将失效，请同步更新客户端。", color: .orange) }
                HStack(spacing: 12) {
                    Button("重新生成 Key…") { confirmReset = true }.disabled(!canEdit)
                    Spacer(minLength: 0)
                    if working { ProgressView().controlSize(.small) }
                    Button("应用更改", action: save).buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                }
            }.padding(.top, 12)
        } label: {
            Text(changed ? "端口与密钥 · 有未应用的更改" : "端口与密钥")
        }
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
        guard let address else { throw AppError.message("无法读取已应用的图片 API 地址。") }
        let key = try await model.remoteCall("images.key")
        guard let key = key["key"].string else {throw AppError.message("无法读取图片接入密钥。")}
        let file = try ImageClientAccess.export(baseURL: address, key: key)
        configNeedsSync = false
        return file
    }
    private func save() {
        guard let p = Int(port), (1...65535).contains(p) else { settingsError = "端口须为 1–65535 的整数。"; return }
        var payload: [String: JSONValue] = ["port": .number(Double(p))]
        if !newKey.isEmpty { payload["key"] = .string(newKey) }
        update(.object(payload))
    }
    private func update(_ payload: JSONValue, fromAPISettings: Bool = false) {
        guard canEdit else {return}; working = true; error = nil
        settingsError = nil
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
                if fromAPISettings || payload["port"] != .null || payload["regenerate_key"].bool == true { settingsError = message }
                else { self.error = message }
            }
        }
    }
    private func copyKey() {
        guard model.running, status != .null, !working, !copyingKey else { return }
        copyingKey = true; settingsError = nil
        Task {
            defer { copyingKey = false }
            do {
                let data = try await model.remoteCall("images.key")
                guard let key = data["key"].string else { throw AppError.message("无法读取图片接入密钥。") }
                copy(key, message: "图片 API Key 已复制。")
            } catch { settingsError = error.localizedDescription }
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
