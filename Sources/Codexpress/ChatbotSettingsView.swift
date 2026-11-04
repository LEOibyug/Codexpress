import SwiftUI
import AppKit

struct ChatbotSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = ""
    @State private var newKey = ""
    @State private var working = false
    @State private var confirmReset = false
    @State private var error: String?

    private var canEdit: Bool { model.running && !model.busy && !working }
    private var address: String { status["base_url"].string ?? "—" }
    private var changed: Bool { !newKey.isEmpty || port != status["port"].number.map({ String(Int($0)) }) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("聊天客户端")
                SettingsCard("客户端 API", symbol: "bubble.left.and.bubble.right", accessory: {
                    StatusPill(text: status["running"].bool == true ? "运行中" : status["enabled"].bool == false ? "已关闭" : "未启动", color: status["running"].bool == true ? .green : .secondary)
                }) {
                    SettingRow(title: "启用接入") {
                        Toggle("启用聊天客户端接入", isOn: Binding(get: { status["enabled"].bool ?? true }, set: { enabled in update(.object(["enabled": .bool(enabled)])) }))
                            .toggleStyle(.switch).labelsHidden().disabled(!canEdit || status == .null)
                    }
                    Divider()
                    SettingRow(title: "监听端口") {
                        TextField("17865", text: $port).font(.system(size: 13, design: .monospaced))
                            .multilineTextAlignment(.trailing).settingsField().frame(width: 130).disabled(!canEdit || status == .null)
                    }
                    SettingRow(title: "新 API Key") {
                        SecureField("留空保留原 Key", text: $newKey).settingsField().frame(maxWidth: 360).disabled(!canEdit || status == .null)
                    }
                    Divider()
                    HStack(spacing: 12) {
                        Button("复制当前 Key") { copyKey() }.disabled(!canEdit || status == .null)
                        Button("重新生成…") { confirmReset = true }.disabled(!canEdit || status == .null)
                        Spacer()
                        if working { ProgressView().controlSize(.small) }
                        Button("应用设置") { save() }.buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    if status["enabled"].bool == true && status["running"].bool == false {
                        InfoNote(text: "入口未启动，请更换端口后应用设置。", color: .orange)
                    }
                }
                SettingsCard("供应商填写", symbol: "link") {
                    LabeledContent("类型", value: "OpenAI 兼容")
                    LabeledContent("API 基址") {
                        HStack(spacing: 10) {
                            Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                            Button { copy(address, message: "API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                                .accessibilityLabel("复制 API 地址").help("复制 API 地址").disabled(status == .null)
                        }
                    }
                    Divider()
                    LabeledContent("接口", value: "Chat Completions / Responses")
                    Text("OMP 与聊天客户端共用此端口，允许局域网访问。远端请将 127.0.0.1 替换为这台 Mac 的局域网或 VPN 地址，并使用独立 Key。不要将 HTTP 端口直接暴露到公网。")
                        .foregroundStyle(.secondary)
                }
            }.settingsPage()
        }.task(id: model.running) { await load() }
            .onChange(of: port) { _, _ in error = nil }
            .onChange(of: newKey) { _, _ in error = nil }
            .alert("重新生成 API Key？", isPresented: $confirmReset) {
                Button("取消", role: .cancel) {}
                Button("重新生成") { update(.object(["regenerate_key": .bool(true)]), reset: true) }
            } message: { Text("旧 Key 将无法发起新请求，使用此入口的客户端需要更新密钥。") }
    }
    private func load() async {
        guard model.running else { return }
        do { status = try await model.remoteCall("chatbot.status"); port = status["port"].number.map { String(Int($0)) } ?? ""; error = nil }
        catch { self.error = error.localizedDescription }
    }
    private func save() {
        guard let value = Int(port), (1...65535).contains(value) else { error = "端口须为 1–65535 的整数。"; return }
        var payload: [String: JSONValue] = ["port": .number(Double(value))]
        if !newKey.isEmpty { payload["key"] = .string(newKey) }
        update(.object(payload))
    }
    private func update(_ payload: JSONValue, reset: Bool = false) {
        guard canEdit else { return }; working = true; error = nil
        Task {
            defer { working = false }
            do {
                status = try await model.remoteCall("chatbot.configure", payload)
                port = status["port"].number.map { String(Int($0)) } ?? ""; newKey = ""
                model.notice = reset ? "API Key 已重新生成，请更新客户端密钥。" : "聊天客户端设置已应用。"
            } catch { self.error = error.localizedDescription }
        }
    }
    private func copyKey() {
        guard canEdit else { return }; working = true; error = nil
        Task {
            defer { working = false }
            do {
                let result = try await model.remoteCall("chatbot.key")
                guard let key = result["key"].string else { throw AppError.message("无法读取聊天客户端 Key。") }
                copy(key, message: "API Key 已复制。")
            } catch { self.error = error.localizedDescription }
        }
    }
    private func copy(_ text: String, message: String) {
        NSPasteboard.general.clearContents(); NSPasteboard.general.setString(text, forType: .string)
        model.notice = message
    }
}
