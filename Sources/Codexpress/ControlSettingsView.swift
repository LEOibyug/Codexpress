import AppKit
import SwiftUI

struct ControlSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var status: JSONValue = .null
    @State private var port = "17867"
    @State private var working = false
    @State private var confirmRotation = false
    @State private var error: String?

    private var canEdit: Bool { model.running && !model.busy && !working && status != .null }
    private var address: String { status["base_url"].string ?? "http://127.0.0.1:17867" }
    private var changed: Bool { port != status["port"].number.map { String(Int($0)) } }
    private var dashboardURL: URL? {
        guard let base = URL(string: address) else { return nil }
        return base.deletingLastPathComponent().appendingPathComponent("dashboard", isDirectory: true)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("管理 API")
                SettingsCard("本机管理", symbol: "switch.2", accessory: {
                    StatusPill(text: status["running"].bool == true ? "运行中" : status["enabled"].bool == true ? "未启动" : "已关闭", color: status["running"].bool == true ? .green : .secondary)
                }) {
                    SettingRow(title: "启用管理 API") {
                        Toggle("启用管理 API", isOn: Binding(get: { status["enabled"].bool ?? false }, set: { enabled in
                            update(.object(["enabled": .bool(enabled)]))
                        })).toggleStyle(.switch).labelsHidden().disabled(!canEdit)
                    }
                    Divider()
                    SettingRow(title: "监听端口") {
                        TextField("17867", text: $port).font(.system(size: 13, design: .monospaced))
                            .multilineTextAlignment(.trailing).settingsField().frame(width: 130).disabled(!canEdit)
                    }
                    HStack {
                        Button("复制独立 Key") { copyKey() }.disabled(!canEdit)
                        Button("重新生成 Key…") { confirmRotation = true }.disabled(!canEdit)
                        Spacer()
                        if working { ProgressView().controlSize(.small) }
                        Button("应用设置") { save() }.buttonStyle(.borderedProminent).disabled(!canEdit || !changed)
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    if status["enabled"].bool == true && status["running"].bool == false {
                        InfoNote(text: "管理入口未启动，请更换端口后应用设置。", color: .orange)
                    }
                }
                SettingsCard("数据面板", symbol: "chart.bar.xaxis") {
                    HStack {
                        Text("按时间、来源和模型筛选请求日志，查看性能与费用，阅读和导出缓存原文。")
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button("打开数据面板") {
                            guard let url = dashboardURL else { return }
                            if !NSWorkspace.shared.open(url) { error = "无法打开浏览器，请使用面板地址手动访问。" }
                        }.buttonStyle(.borderedProminent)
                            .disabled(status["running"].bool != true || dashboardURL == nil)
                    }
                }
                SettingsCard("访问权限", symbol: "lock.shield") {
                    LabeledContent("地址") {
                        HStack(spacing: 10) {
                            Text(address).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                            Button { copy(address, message: "管理 API 地址已复制。") } label: { Image(systemName: "doc.on.doc") }
                                .accessibilityLabel("复制管理 API 地址").disabled(status == .null)
                        }
                    }
                    LabeledContent("认证", value: "Authorization: Bearer <独立 Key>")
                    Divider()
                    Text("持有 Key 的程序可查看状态、余额和统计，进行来源抢占、停止抢占及主动刷新。")
                        .foregroundStyle(.secondary)
                    Text("通过 API 请求使用重置卡时，仍需在应用弹窗中确认；确认后将消耗卡片。")
                        .foregroundStyle(.secondary)
                }
            }.settingsPage()
        }
        .task(id: model.running) { await load() }
        .onChange(of: port) { _, _ in error = nil }
        .alert("重新生成管理 API Key？", isPresented: $confirmRotation) {
            Button("取消", role: .cancel) {}
            Button("重新生成") { update(.object(["regenerate_key": .bool(true)]), rotated: true) }
        } message: { Text("旧 Key 将立即失效，已接入的本机程序需要更新密钥。") }
    }

    private func apply(_ value: JSONValue, updatingPort: Bool = true) {
        status = value
        if updatingPort { port = value["port"].number.map { String(Int($0)) } ?? "17867" }
    }

    private func load() async {
        guard model.running else { status = .null; return }
        do { apply(try await model.remoteCall("control.status")); error = nil }
        catch { self.error = error.localizedDescription }
    }

    private func save() {
        guard let number = Int(port), (1...65535).contains(number) else { error = "端口须为 1–65535 的整数。"; return }
        update(.object(["port": .number(Double(number))]))
    }

    private func update(_ payload: JSONValue, rotated: Bool = false) {
        guard canEdit else { return }
        working = true; error = nil
        Task {
            defer { working = false }
            do {
                apply(try await model.remoteCall("control.configure", payload), updatingPort: payload["port"] != .null)
                model.notice = rotated ? "管理 API Key 已重新生成，请更新接入程序。" : "管理 API 设置已应用。"
            } catch {
                let message = error.localizedDescription
                if let actual = try? await model.remoteCall("control.status") { apply(actual, updatingPort: false) }
                self.error = message
            }
        }
    }

    private func copyKey() {
        guard canEdit else { return }
        working = true; error = nil
        Task {
            defer { working = false }
            do {
                let result = try await model.remoteCall("control.key")
                guard let key = result["key"].string, !key.isEmpty else { throw AppError.message("无法读取管理 API Key。") }
                copy(key, message: "管理 API Key 已复制。")
            } catch { self.error = error.localizedDescription }
        }
    }

    private func copy(_ value: String, message: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
        model.notice = message
    }
}
