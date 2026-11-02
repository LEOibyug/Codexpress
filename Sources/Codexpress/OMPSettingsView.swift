import SwiftUI
import AppKit

struct OMPSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var installation: OMPInstallation?
    @State private var directory = ""
    @State private var selectedModel = ""
    @State private var status: JSONValue = .null
    @State private var chatbot: JSONValue = .null
    @State private var working = false
    @State private var detecting = false
    @State private var error: String?
    @State private var confirmRestore = false

    private var models: [String] {
        Array(Set(model.sources.filter(\.enabled).flatMap(\.models) + (model.settings.modelCompaction ?? []).map(\.model)
                  + [status["model_id"].string, selectedModel].compactMap { $0 }.filter { !$0.isEmpty })).sorted()
    }
    private var attached: Bool { status["attached"].bool == true }
    private var resolvedDirectory: String { (directory as NSString).expandingTildeInPath }
    private var canEdit: Bool { model.running && !model.busy && !working && !detecting && resolvedDirectory.hasPrefix("/") }
    private var connectionLabel: String { attached ? (status["needs_update"].bool == true ? "待更新" : "已接入") : "未接入" }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                PageHeading("OMP") {
                    if detecting { ProgressView().controlSize(.small) }
                    else {
                        Button { Task { await detect() } } label: { Image(systemName: "arrow.clockwise") }
                            .help("重新检测 OMP").accessibilityLabel("重新检测 OMP").disabled(working)
                    }
                }
                SettingsCard("oh-my-pi", symbol: "terminal", accessory: {
                    StatusPill(text: connectionLabel, color: attached ? (status["needs_update"].bool == true ? .orange : .green) : .secondary)
                }) {
                    SettingRow(title: "安装版本") {
                        Text(installation?.version ?? (installation?.executable == nil ? "未检测到" : "已安装"))
                            .foregroundStyle(.secondary)
                    }
                    if let executable = installation?.executable {
                        LabeledContent("程序") {
                            Text(executable).font(AppTypography.small).foregroundStyle(.secondary)
                                .lineLimit(1).truncationMode(.middle).textSelection(.enabled).help(executable)
                        }
                    }
                    Divider()
                    SettingRow(title: "配置目录") {
                        HStack(spacing: 8) {
                            TextField("~/.omp/agent", text: $directory).settingsField().frame(maxWidth: 420)
                                .disabled(working || detecting)
                            Button { chooseDirectory() } label: { Image(systemName: "folder") }
                                .help("选择 OMP 配置目录").accessibilityLabel("选择 OMP 配置目录").disabled(working || detecting)
                        }
                    }
                    SettingRow(title: "默认模型") {
                        HStack(spacing: 8) {
                            TextField("模型 ID", text: $selectedModel).settingsField().frame(maxWidth: 360).disabled(!canEdit)
                            Menu {
                                ForEach(models, id: \.self) { name in Button(name) { selectedModel = name } }
                            } label: { Image(systemName: "list.bullet") }
                                .menuStyle(.borderlessButton).frame(width: 22).disabled(!canEdit || models.isEmpty)
                                .help("选择模型").accessibilityLabel("选择模型")
                        }
                    }
                    SettingRow(title: "状态栏扩展") {
                        Text(status["status_plugin_installed"].bool == true ? "已安装" : "随一键设置安装")
                            .foregroundStyle(.secondary)
                    }
                    Divider()
                    LabeledContent("网关", value: chatbot["base_url"].string ?? status["base_url"].string ?? "—")
                        .font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                    LabeledContent("协议", value: "OpenAI Responses")
                    if chatbot["enabled"].bool == false || (chatbot != .null && chatbot["running"].bool == false) {
                        InfoNote(text: "请先启用聊天客户端接入。", color: .orange)
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    HStack(spacing: 12) {
                        if attached {
                            Button("还原原配置…") { confirmRestore = true }.disabled(!canEdit)
                        }
                        Spacer()
                        if working { ProgressView().controlSize(.small) }
                        Button("一键设置") { apply() }.buttonStyle(.borderedProminent)
                            .disabled(!canEdit || selectedModel.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
                OMPCompactionSettingsView(
                    configuration: status["configuration"],
                    configurationRevision: status["configuration_revision"].string ?? "",
                    agentDirectory: resolvedDirectory,
                    canEdit: canEdit && attached,
                    onSavingChange: { working = $0 },
                    onSaved: { status = $0 }
                )
            }.settingsPage()
        }
        .task { await detect() }
        .task(id: "\(model.running):\(directory)") {
            status = .null
            guard model.running, resolvedDirectory.hasPrefix("/") else { return }
            do { try await Task.sleep(for: .milliseconds(300)) } catch { return }
            await refresh()
        }
        .alert("还原 OMP 原配置？", isPresented: $confirmRestore) {
            Button("取消", role: .cancel) {}
            Button("还原") { restore() }
        } message: { Text("撤回 Codexpress 的供应商、默认模型、压缩规则和状态栏扩展。") }
    }

    private func detect() async {
        guard !working, !detecting else { return }
        detecting = true
        let found = await OMPInstallation.detect()
        guard !Task.isCancelled else { detecting = false; return }
        installation = found
        let needsDirectory = directory.isEmpty
        if needsDirectory { directory = found.agentDirectory }
        detecting = false
        if !needsDirectory { await refresh() }
    }

    private func refresh() async {
        guard model.running, !working, resolvedDirectory.hasPrefix("/") else { return }
        let requested = directory
        do {
            let connection = try await model.remoteCall("chatbot.status")
            let next = try await model.remoteCall("omp.status", .object(["agent_dir": .string(resolvedDirectory)]))
            guard !Task.isCancelled, directory == requested else { return }
            chatbot = connection; status = next; error = nil
            if selectedModel.isEmpty {
                selectedModel = next["model_id"].string ?? (models.contains("gpt-6.1-sol") ? "gpt-6.1-sol" : models.first ?? "gpt-6.1-sol")
            }
        } catch {
            guard !Task.isCancelled, directory == requested else { return }
            self.error = error.localizedDescription
        }
    }

    private func apply() {
        let name = selectedModel.trimmingCharacters(in: .whitespacesAndNewlines)
        perform("omp.attach", additional: ["model_id": .string(name)], message: "OMP 接入与状态栏扩展已设置，重新启动 OMP 后生效。")
    }

    private func restore() { perform("omp.restore", message: "OMP 原配置已还原。") }

    private func perform(_ operation: String, additional: [String: JSONValue] = [:], message: String) {
        guard canEdit else { return }
        working = true; error = nil
        var payload = additional; payload["agent_dir"] = .string(resolvedDirectory)
        Task {
            defer { working = false }
            do {
                status = try await model.remoteCall(operation, .object(payload))
                model.notice = message
            } catch { self.error = error.localizedDescription }
        }
    }

    private func chooseDirectory() {
        let picker = NSOpenPanel()
        picker.canChooseDirectories = true; picker.canChooseFiles = false
        picker.allowsMultipleSelection = false; picker.showsHiddenFiles = true
        picker.directoryURL = URL(fileURLWithPath: directory.isEmpty ? NSHomeDirectory() : resolvedDirectory)
        if picker.runModal() == .OK, let selected = picker.url { directory = selected.path }
    }
}
