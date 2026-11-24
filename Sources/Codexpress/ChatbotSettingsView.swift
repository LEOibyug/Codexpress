import SwiftUI
import AppKit

struct ChatbotSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = ""
    @State private var savedPort = ""
    @State private var newKey = ""
    @State private var working = false
    @State private var loading = false
    @State private var copyingKey = false
    @State private var confirmReset = false
    @State private var error: String?
    @State private var showsSettings = false

    private var canEdit: Bool { model.running && status != .null && !working && !loading }
    private var address: String? { model.running ? status["base_url"].string : nil }
    private var changed: Bool { !newKey.isEmpty || port != savedPort }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("聊天客户端", subtitle: "将 Codexpress 添加为 OpenAI 兼容供应商，使用 Chat Completions 或 Responses。")
                SettingsCard("接入地址与密钥", symbol: "bubble.left.and.bubble.right", accessory: {
                    ConnectionListenerStatus(gatewayRunning: model.running, loading: loading, status: status)
                }) {
                    SettingRow(title: "启用接入", subtitle: "与本机 OMP 共用此入口。") {
                        Toggle("启用聊天客户端接入", isOn: Binding(get: { status["enabled"].bool ?? true }, set: { enabled in update(.object(["enabled": .bool(enabled)])) }))
                            .toggleStyle(.switch).labelsHidden().disabled(!canEdit)
                    }
                    if let address {
                        SettingRow(title: "已应用的 API 基址") {
                            HStack(spacing: 8) {
                                Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                                Button { copy(address, message: "API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                                    .accessibilityLabel("复制 API 地址").help("复制 API 地址")
                            }
                        }
                        HStack(spacing: 12) {
                            Text("远端客户端请用此 Mac 的局域网或 VPN 地址替换 127.0.0.1。不要将 HTTP 端口直接暴露到公网。")
                                .font(AppTypography.caption).foregroundStyle(.secondary)
                            Spacer(minLength: 0)
                            Button("复制当前 Key") { copyKey() }.buttonStyle(.borderedProminent)
                                .disabled(!canEdit || copyingKey)
                        }
                    } else if !model.running {
                        InfoNote(text: "请先在设置中启动本机网关。")
                    }
                    if model.running && status["enabled"].bool == true && status["running"].bool == false {
                        SettingRow(title: "聊天入口未启动") {
                            Button("重试启动") { update(.object([:])) }.disabled(!canEdit)
                        }
                    }
                    Divider()
                    advancedSettings
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    if model.running && (error != nil || (status == .null && !loading)) {
                        Button("重新读取") { Task { await load() } }.disabled(loading || working)
                    }
                }
                environmentCard
            }.settingsPage()
        }.task(id: model.running) {
            await load()
            model.refreshEnvironmentSyncInspection()
        }
            .onChange(of: port) { _, _ in error = nil }
            .onChange(of: newKey) { _, _ in error = nil }
            .alert("重新生成聊天客户端 API Key？", isPresented: $confirmReset) {
                Button("取消", role: .cancel) {}
                Button("重新生成") { update(.object(["regenerate_key": .bool(true)])) }
            } message: {
                Text(model.settings.autoExportChatbotEnvironmentEnabled
                     ? "旧 Key 将无法发起新请求，环境变量会自动同步更新；使用其它方式接入的客户端需要更新密钥。"
                     : "旧 Key 将无法发起新请求，使用此入口的客户端需要更新密钥。")
            }
    }

    private var environmentCard: some View {
        let enabled = model.settings.autoExportChatbotEnvironmentEnabled
        return SettingsCard("环境变量", symbol: "terminal", accessory: {
            if model.environmentSyncWorking {
                ProgressView().controlSize(.small).accessibilityLabel("正在同步环境变量")
            } else if !enabled {
                StatusPill(text: "已关闭")
            } else if model.environmentSynced {
                StatusPill(text: "已同步", color: .green)
            } else {
                StatusPill(text: "待同步", color: .orange)
            }
        }) {
            SettingRow(title: "自动写入环境变量", subtitle: "在启动、修改端口或更换 Key 时同步更新，关闭后自动清理。") {
                Toggle("自动写入环境变量", isOn: Binding(
                    get: { model.settings.autoExportChatbotEnvironmentEnabled },
                    set: { model.setAutoExportChatbotEnvironment($0) }
                ))
                .toggleStyle(.switch).labelsHidden()
                .disabled(model.metadataLoadFailed || model.environmentSyncWorking)
            }
            LabeledContent("接口地址变量") {
                HStack(spacing: 8) {
                    Text(ChatbotEnvironmentAccess.baseURLVariable)
                        .font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                    Text(address ?? "等待读取")
                        .font(AppTypography.small).foregroundStyle(.secondary).textSelection(.enabled)
                }
            }
            LabeledContent("密钥变量") {
                HStack(spacing: 8) {
                    Text(ChatbotEnvironmentAccess.apiKeyVariable)
                        .font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                    Text("使用当前聊天客户端 Key")
                        .font(AppTypography.small).foregroundStyle(.secondary)
                }
            }
            LabeledContent("写入位置") {
                Text("~/.codexpress/env.sh、~/.zshrc 与 macOS 图形环境")
                    .font(AppTypography.small).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if enabled && status["enabled"].bool == false {
                InfoNote(text: "聊天客户端入口当前已关闭，启用后接口才可访问。", symbol: "exclamationmark.triangle", color: .orange)
            }
            if let syncError = model.environmentSyncError {
                InfoNote(text: syncError, symbol: "exclamationmark.triangle", color: .orange)
            }
            HStack(spacing: 12) {
                Text("新打开的终端和图形应用会自动读取；已打开的终端可执行 `source ~/.zshrc`。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                if enabled {
                    Button("立即同步") { model.syncChatbotEnvironmentNow() }
                        .disabled(!model.running || working || model.environmentSyncWorking)
                } else if model.environmentSyncError != nil || model.environmentSynced {
                    Button("重试清理") { model.retryChatbotEnvironmentCleanup() }
                        .disabled(model.environmentSyncWorking)
                }
            }
        }
    }

    private var advancedSettings: some View {
        DisclosureGroup(isExpanded: $showsSettings) {
            VStack(alignment: .leading, spacing: 12) {
                SettingRow(title: "监听端口") {
                    TextField("17865", text: $port).font(.system(size: 13, design: .monospaced))
                        .accessibilityLabel("监听端口")
                        .multilineTextAlignment(.trailing).settingsField().frame(width: 110).disabled(!canEdit)
                }
                SettingRow(title: "新 API Key") {
                    SecureField("留空保留原 Key", text: $newKey).settingsField().frame(maxWidth: 300).disabled(!canEdit)
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
            let actual = try await model.remoteCall("chatbot.status")
            guard !Task.isCancelled, model.running else { return }
            if port == savedPort { port = actual["port"].number.map { String(Int($0)) } ?? "" }
            savedPort = actual["port"].number.map { String(Int($0)) } ?? ""
            status = actual; error = nil
        }
        catch { self.error = error.localizedDescription }
    }
    private func save() {
        guard let value = Int(port), (1...65535).contains(value) else { error = "端口须为 1–65535 的整数。"; return }
        var payload: [String: JSONValue] = ["port": .number(Double(value))]
        if !newKey.isEmpty { payload["key"] = .string(newKey) }
        update(.object(payload))
    }
    private func update(_ payload: JSONValue) {
        guard canEdit else { return }; working = true; error = nil
        Task {
            defer { working = false }
            do {
                status = try await model.remoteCall("chatbot.configure", payload)
                savedPort = status["port"].number.map { String(Int($0)) } ?? ""
                if payload["port"] != .null { port = status["port"].number.map { String(Int($0)) } ?? "" }
                if payload["key"] != .null { newKey = "" }
                let rotated = payload["regenerate_key"].bool == true || payload["key"] != .null
                let changedEndpoint = rotated || payload["port"] != .null
                if changedEndpoint && model.settings.autoExportChatbotEnvironmentEnabled {
                    let synced = await model.syncChatbotEnvironmentIfEnabled(showSuccessNotice: false)
                    if synced {
                        model.notice = rotated
                            ? "API Key 已更换，环境变量已同步更新；使用其它方式接入的客户端需手动更新密钥。"
                            : "聊天客户端设置已应用，环境变量已同步更新。"
                    } else {
                        model.notice = "聊天客户端设置已应用，但环境变量同步失败。"
                    }
                } else {
                    model.notice = rotated ? "API Key 已更换，请更新客户端密钥。" : "聊天客户端设置已应用。"
                }
            } catch {
                let message = error.localizedDescription
                if let actual = try? await model.remoteCall("chatbot.status") {
                    status = actual
                    savedPort = actual["port"].number.map { String(Int($0)) } ?? ""
                }
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
