import AppKit
import SwiftUI

struct ControlSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = ""
    @State private var savedPort = ""
    @State private var working = false
    @State private var loading = false
    @State private var copyingKey = false
    @State private var confirmRotation = false
    @State private var error: String?
    @State private var settingsError: String?
    @State private var showsSettings = false

    private var canEdit: Bool { model.running && !working && !loading && status != .null }
    private var address: String? { model.running ? status["base_url"].string : nil }
    private var changed: Bool { port != savedPort }
    private var dashboardURL: URL? {
        guard let address, let base = URL(string: address) else { return nil }
        return base.deletingLastPathComponent().appendingPathComponent("dashboard", isDirectory: true)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("管理 API", subtitle: "仅限本机：打开数据面板，或使用独立 Key 连接自动化工具。")
                SettingsCard("本机数据面板", symbol: "chart.bar.xaxis", accessory: {
                    ConnectionListenerStatus(gatewayRunning: model.running, loading: loading, status: status)
                }) {
                    SettingRow(title: "启用管理 API", subtitle: "同时启用本机数据面板。") {
                        Toggle("启用管理 API", isOn: Binding(get: { status["enabled"].bool ?? false }, set: { enabled in
                            update(.object(["enabled": .bool(enabled)]))
                        })).toggleStyle(.switch).labelsHidden().disabled(!canEdit)
                    }
                    SettingRow(title: "请求记录", subtitle: "在浏览器中筛选日志、查看性能与费用，并导出缓存原文。") {
                        Button("打开数据面板") {
                            guard let url = dashboardURL else { return }
                            if !NSWorkspace.shared.open(url) { error = "无法打开浏览器，请使用面板地址手动访问。" }
                        }.buttonStyle(.borderedProminent)
                            .disabled(!model.running || status["running"].bool != true || dashboardURL == nil)
                    }
                    if !model.running {
                        InfoNote(text: "请先在设置中启动本机网关。")
                    } else if status["enabled"].bool == false {
                        InfoNote(text: "启用管理 API 后可打开面板。")
                    } else if status["enabled"].bool == true && status["running"].bool == false {
                        SettingRow(title: "管理入口未启动") {
                            Button("重试启动") { update(.object([:])) }.disabled(!canEdit)
                        }
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    if model.running && (error != nil || (status == .null && !loading)) {
                        Button("重新读取") { Task { await load() } }.disabled(loading || working)
                    }
                    if model.running, status["running"].bool == true, let dashboardURL {
                        LabeledContent("面板地址") {
                            Text(dashboardURL.absoluteString).font(AppTypography.small).textSelection(.enabled)
                        }
                    }
                }
                apiSettings
            }.settingsPage()
        }
        .task(id: model.running) { await load() }
        .onChange(of: port) { _, _ in settingsError = nil }
        .alert("重新生成管理 API Key？", isPresented: $confirmRotation) {
            Button("取消", role: .cancel) {}
            Button("重新生成") { update(.object(["regenerate_key": .bool(true)]), rotated: true) }
        } message: { Text("旧 Key 将立即失效，已接入的本机程序需要更新密钥。") }
    }

    private var apiSettings: some View {
        SettingsCard("自动化接入", symbol: "lock.shield") {
            if let address {
                SettingRow(title: "已应用的 API 地址") {
                    HStack(spacing: 8) {
                        Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                        Button { copy(address, message: "管理 API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                            .accessibilityLabel("复制管理 API 地址").help("复制管理 API 地址")
                    }
                }
            }
            SettingRow(title: "独立 Key", subtitle: "认证方式：Authorization: Bearer <独立 Key>") {
                Button("复制当前 Key") { copyKey() }.disabled(!canEdit || copyingKey)
            }
            Divider()
            DisclosureGroup(isExpanded: $showsSettings) {
                VStack(alignment: .leading, spacing: 12) {
                    SettingRow(title: "监听端口") {
                        HStack(spacing: 8) {
                            TextField("17867", text: $port).font(.system(size: 13, design: .monospaced))
                                .accessibilityLabel("监听端口").multilineTextAlignment(.trailing).settingsField().frame(width: 110)
                                .disabled(!canEdit)
                            Button("应用端口", action: save).buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                        }
                    }
                    if working { ProgressView().controlSize(.small) }
                    InfoNote(text: "持有 Key 的程序可以查询状态、余额和统计，启动或停止抢占，以及主动刷新。通过 API 请求使用重置卡时，仍需在应用中确认后才会消耗卡片。")
                    Button("重新生成 Key…") { confirmRotation = true }.disabled(!canEdit)
                }.padding(.top, 12)
            } label: {
                Text(changed ? "端口与权限 · 有未应用的更改" : "端口与权限")
            }
            if let settingsError { InfoNote(text: settingsError, symbol: "exclamationmark.triangle", color: .orange) }
        }
    }

    private func apply(_ value: JSONValue, updatingPort: Bool = true) {
        status = value
        savedPort = value["port"].number.map { String(Int($0)) } ?? ""
        if updatingPort { port = savedPort }
    }

    private func load() async {
        guard model.running else { status = .null; error = nil; return }
        loading = true
        defer { loading = false }
        do {
            let actual = try await model.remoteCall("control.status")
            guard !Task.isCancelled, model.running else { return }
            apply(actual, updatingPort: port == savedPort)
            error = nil
        } catch { self.error = error.localizedDescription }
    }

    private func save() {
        guard let number = Int(port), (1...65535).contains(number) else { settingsError = "端口须为 1–65535 的整数。"; return }
        update(.object(["port": .number(Double(number))]))
    }

    private func update(_ payload: JSONValue, rotated: Bool = false) {
        guard canEdit else { return }
        working = true; error = nil
        settingsError = nil
        Task {
            defer { working = false }
            do {
                apply(try await model.remoteCall("control.configure", payload), updatingPort: payload["port"] != .null)
                model.notice = rotated ? "管理 API Key 已重新生成，请更新接入程序。" : "管理 API 设置已应用。"
            } catch {
                let message = error.localizedDescription
                if let actual = try? await model.remoteCall("control.status") { apply(actual, updatingPort: false) }
                if rotated || payload["port"] != .null { settingsError = message }
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
                let result = try await model.remoteCall("control.key")
                guard let key = result["key"].string, !key.isEmpty else { throw AppError.message("无法读取管理 API Key。") }
                copy(key, message: "管理 API Key 已复制。")
            } catch { settingsError = error.localizedDescription }
        }
    }

    private func copy(_ value: String, message: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
        model.notice = message
    }
}
