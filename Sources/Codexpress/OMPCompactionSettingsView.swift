import SwiftUI

struct OMPCompactionSettingsView: View {
    @EnvironmentObject var model: AppModel
    let configuration: JSONValue
    let configurationRevision: String
    let agentDirectory: String
    let canEdit: Bool
    let onSavingChange: (Bool) -> Void
    let onSaved: (JSONValue) -> Void

    @State private var draft = OMPConfiguration()
    @State private var saving = false
    @State private var saveError: String?
    @State private var importMessage: String?
    @State private var selector = ""
    @State private var selectorError: String?
    @State private var selectedModel = ""
    @State private var modelRemoteExpanded = false
    @State private var thresholdMode = "default"
    @State private var selectorModes: [String: String] = [:]

    private typealias Field = OMPConfiguration.Field
    private let methods = ["remote", "snapcompact", "handoff", "shake", "soft"]
    private var editable: Bool { canEdit && draft.available && !configurationRevision.isEmpty && !saving }
    private var pending: Bool { draft.hasChanges }
    private var initialThresholdMode: String {
        if (draft.value(.config("thresholdTokens", "固定阈值", .integer))?.number ?? 0) > 0 { return "fixed" }
        if (draft.value(.config("thresholdPercent", "阈值百分比", .number))?.number ?? 0) > 0 { return "percent" }
        return "default"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            SettingsCard("OMP 压缩", subtitle: "全局 config.yml / models.yml", symbol: "text.alignleft") {
                automaticControls
                Divider()
                thresholdControls
                numberField(.config("reserveTokens", "预留预算", .number), help: "留空使用 OMP 原生预留策略。")
                numberField(.config("keepRecentTokens", "保留近期预算", .number))
                Divider()
                methodControls
            }
            .help("显示当前目录存储的全局原生配置；项目、命令行或会话设置仍可能覆盖这些值。")

            SettingsCard("模型阈值", symbol: "slider.horizontal.3") {
                booleanField(.config("modelThresholdsEnabled", "模型阈值规则", .boolean))
                selectorControls
                HStack {
                    Button("导入 Codex 规则") {
                        let skipped = draft.importCodex(model.settings.modelCompaction ?? [])
                        selectorModes = [:]
                        if !skipped.isEmpty { importMessage = "规则已填入；未导入：" + skipped.joined(separator: "、") }
                        else { importMessage = draft.hasChanges ? "规则已填入，保存压缩设置后生效。" : "规则与当前配置一致。" }
                    }
                    .disabled((model.settings.modelCompaction ?? []).isEmpty)
                    .help("显式导入固定触发阈值和可匹配模型的上下文窗口；不会自动保存。")
                    if let importMessage { Text(importMessage).font(AppTypography.caption).foregroundStyle(.secondary) }
                }
            }

            SettingsCard("模型覆盖", symbol: "square.stack.3d.up") {
                modelControls
            }

            SettingsCard("原生远程压缩", symbol: "network") {
                remoteControls(modelID: nil)
            }

            SettingsCard("高级压缩设置", symbol: "gearshape.2") {
                advancedControls
            }

            if let saveError { InfoNote(text: saveError, symbol: "exclamationmark.triangle", color: .orange) }
            HStack {
                Button("放弃更改") { load() }.disabled(!pending || saving)
                Spacer()
                if saving { ProgressView().controlSize(.small) }
                Button("保存压缩设置") { save() }.buttonStyle(.borderedProminent).disabled(!editable || !pending)
            }
        }
        .disabled(!canEdit || saving || !draft.available)
        .onAppear { load() }
        .onChange(of: configuration) { _, _ in load() }
        .onChange(of: agentDirectory) { _, _ in load() }
    }

    private var automaticControls: some View {
        VStack(spacing: 10) {
            booleanField(.config("enabled", "自动压缩", .boolean))
            booleanField(.config("midTurnEnabled", "轮次中压缩", .boolean))
            booleanField(.config("autoContinue", "压缩后自动继续", .boolean))
        }
    }

    private var thresholdControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            SettingRow(title: "全局触发阈值") {
                Picker("阈值模式", selection: Binding(get: { thresholdMode }, set: changeThresholdMode)) {
                    Text("默认 / 继承").tag("default")
                    Text("固定 Token").tag("fixed")
                    Text("百分比").tag("percent")
                }.labelsHidden().frame(width: 190)
            }
            if thresholdMode == "fixed" {
                numberField(.config("thresholdTokens", "固定阈值", .integer), help: "原生固定触发阈值；允许非正哨兵值。留空移除此值。")
            } else if thresholdMode == "percent" {
                numberField(.config("thresholdPercent", "阈值百分比", .number), help: "正值由 OMP 在运行时限制为 1–99%；不应用 Codex 的 90% 限制。允许原生非正哨兵值。留空恢复继承。")
            }
        }
    }

    private var methodControls: some View {
        let field = Field.config("methodOrder", "压缩方法顺序", .methods)
        let order = draft.text(field).split(separator: ",").map(String.init)
        return VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("压缩方法顺序").font(AppTypography.body)
                Spacer()
                Button("恢复继承") { draft.set(field, "") }.disabled(draft.value(field) == nil && draft.text(field).isEmpty)
                Menu("添加方法") {
                    ForEach(methods.filter { !order.contains($0) }, id: \.self) { method in
                        Button(method) { draft.set(field, (order + [method]).joined(separator: ",")) }
                    }
                }.disabled(order.count == methods.count)
            }
            if order.isEmpty {
                Text(draft.value(field) != nil && draft.changes[field] == nil ? "未启用压缩方法" : "继承 OMP 方法顺序")
                    .foregroundStyle(.secondary).font(AppTypography.caption)
            }
            ForEach(Array(order.enumerated()), id: \.element) { index, method in
                HStack {
                    Text("\(index + 1). \(method)").font(.system(size: 12, design: .monospaced))
                    Spacer()
                    Button { moveMethod(index, by: -1, order: order, field: field) } label: { Image(systemName: "arrow.up") }
                        .disabled(index == 0).help("向前移动").accessibilityLabel("向前移动 \(method)")
                    Button { moveMethod(index, by: 1, order: order, field: field) } label: { Image(systemName: "arrow.down") }
                        .disabled(index == order.count - 1).help("向后移动").accessibilityLabel("向后移动 \(method)")
                    Button { draft.set(field, order.filter { $0 != method }.joined(separator: ",")) } label: { Image(systemName: "minus") }
                        .help("移除此方法").accessibilityLabel("移除 \(method)")
                }
            }
            fieldError(field)
        }
    }

    private var selectorControls: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(draft.selectors, id: \.self) { name in
                selectorRow(name)
            }
            HStack {
                TextField("provider/model 或 provider/前缀*", text: $selector).settingsField()
                    .onSubmit { addSelector() }
                Button("添加选择器") { addSelector() }
            }
            if let selectorError { Text(selectorError).font(AppTypography.caption).foregroundStyle(.orange) }
        }
    }

    private func selectorRow(_ name: String) -> some View {
        let field = Field.selector(name)
        let mode = selectorModes[name] ?? ruleMode(draft.text(field))
        return VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(name).font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                Spacer()
                Picker("\(name) 的阈值模式", selection: Binding(get: { selectorModes[name] ?? ruleMode(draft.text(field)) }, set: { changeRuleMode(name, to: $0) })) {
                    Text("继承").tag("inherit")
                    Text("基础窗口").tag("base")
                    Text("固定触发").tag("fixed")
                    Text("百分比").tag("percent")
                    Text("清除下层规则").tag("clear")
                }.labelsHidden().frame(width: 160)
                    .help("基础窗口是计算压缩阈值的窗口基数，不是固定触发点；固定触发使用原生 fN。清除写入 null 屏蔽下层匹配项，继承则移除此项。")
                if ["base", "fixed", "percent"].contains(mode) {
                    TextField(mode == "percent" ? "0 < 百分比 ≤ 100" : "Token", text: Binding(get: { ruleNumber(draft.text(field)) }, set: { text in
                        draft.set(field, encodeRule(text, mode: mode))
                    })).settingsField().frame(width: 160)
                }
            }
            fieldError(field)
        }
    }

    @ViewBuilder private var modelControls: some View {
        if draft.modelIDs.isEmpty {
            Text("当前供应商尚无可编辑模型。").font(AppTypography.caption).foregroundStyle(.secondary)
        } else {
            SettingRow(title: "模型") {
                Picker("模型覆盖", selection: $selectedModel) {
                    ForEach(draft.modelIDs, id: \.self) { Text($0).tag($0) }
                }.labelsHidden().frame(maxWidth: 340)
            }
            if draft.modelIDs.contains(selectedModel) {
                numberField(.model(selectedModel, "contextWindow", "上下文窗口"), placeholder: baseLabel("contextWindow"))
                numberField(.model(selectedModel, "maxContextWindow", "扩展窗口"), placeholder: baseLabel("maxContextWindow"))
                numberField(.model(selectedModel, "maxTokens", "输出上限"), placeholder: baseLabel("maxTokens"), help: "maxTokens 只控制输出上限，不是输入或上下文窗口。")
                booleanField(.model(selectedModel, "omitMaxOutputTokens", "省略输出上限参数", .boolean), inherited: baseLabel("omitMaxOutputTokens"))
                textField(.model(selectedModel, "compactionModel", "压缩模型", .text), placeholder: baseLabel("compactionModel"))
                DisclosureGroup("模型原生远程覆盖", isExpanded: $modelRemoteExpanded) {
                    VStack(spacing: 12) { remoteControls(modelID: selectedModel) }.padding(.top, 10)
                }
            }
        }
    }

    private func remoteControls(modelID: String?) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            booleanField(.remote("enabled", "原生远程压缩", model: modelID, .boolean))
            remoteAPIField(modelID: modelID)
            textField(.remote("model", "远程压缩模型", model: modelID))
            textField(.remote("endpoint", "V1 压缩端点", model: modelID))
                .help("V1 需要当前来源真正支持 Responses /compact；并非所有官方或代理来源均支持。端点覆盖会沿用当前来源的认证。")
            textField(.remote("v2Endpoint", "V2 压缩端点", model: modelID))
                .help("V2 通过普通 Responses 流的 compaction_trigger 工作，需要来源支持，且须启用流式 V2 与全局许可。")
            textField(.remote("streamingEndpoint", "流式端点", model: modelID))
            booleanField(.remote("v2StreamingEnabled", "流式 V2", model: modelID, .boolean))
            if ["endpoint", "v2Endpoint", "streamingEndpoint"].contains(where: { !draft.text(.remote($0, "端点", model: modelID)).isEmpty }) {
                endpointWarning
            }
        }
    }

    private func remoteAPIField(modelID: String?) -> some View {
        let field = Field.remote("api", "API", model: modelID)
        let current = draft.text(field)
        return VStack(alignment: .leading, spacing: 4) {
            SettingRow(title: "API") {
                Picker("原生远程 API", selection: binding(field)) {
                    Text("继承").tag("")
                    Text("OpenAI Responses").tag("openai-responses")
                    if !current.isEmpty && current != "openai-responses" {
                        Text("当前存储值：\(current)").tag(current)
                    }
                }.labelsHidden().frame(width: 220)
            }
            fieldError(field)
        }
    }

    private var endpointWarning: some View {
        InfoNote(text: "自定义端点可能接收当前来源的凭据与会话内容，并产生上游费用。仅使用可信服务。", symbol: "exclamationmark.triangle", color: .orange)
    }

    private var advancedControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            booleanField(.config("asyncEnabled", "异步压缩", .boolean))
            booleanField(.config("idleEnabled", "空闲压缩", .boolean))
            numberField(.config("idleThresholdTokens", "空闲触发预算", .number))
            numberField(.config("idleTimeoutSeconds", "空闲超时（秒）", .number))
            Divider()
            booleanField(.config("remoteStreamingV2Enabled", "允许远程流式 V2", .boolean))
                .help("这里只授予全局许可，不会自动启用供应商或模型的 V2 远程压缩。")
            numberField(.config("v2RetainedMessageBudget", "V2 保留消息预算", .number))
            Divider()
            booleanField(.config("handoffSaveToDisk", "Handoff 写入磁盘", .boolean))
            booleanField(.config("supersedeReads", "替代重复读取", .boolean))
            booleanField(.config("dropUseless", "移除无用内容", .boolean))
            booleanField(.config("experimentalContextManagement", "实验性上下文管理", .boolean))
        }
    }

    private func booleanField(_ field: Field, inherited: String = "OMP 默认") -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SettingRow(title: field.title) {
                Picker(field.title, selection: binding(field)) {
                    Text("继承（\(inherited)）").tag("")
                    Text("启用").tag("true")
                    Text("禁用").tag("false")
                }.labelsHidden().frame(width: 220)
            }
            fieldError(field)
        }
    }

    private func numberField(_ field: Field, placeholder: String = "留空继承", help: String = "留空移除此覆盖，恢复 OMP 原生继承。") -> some View {
        textField(field, placeholder: placeholder).help(help)
    }

    private func textField(_ field: Field, placeholder: String = "留空继承") -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SettingRow(title: field.title) {
                TextField(placeholder, text: binding(field)).settingsField().frame(maxWidth: 340)
            }
            fieldError(field)
        }
    }

    @ViewBuilder private func fieldError(_ field: Field) -> some View {
        if let error = draft.errors[field] { Text("\(field.title)：\(error)").font(AppTypography.caption).foregroundStyle(.orange) }
    }

    private func binding(_ field: Field) -> Binding<String> {
        Binding(get: { draft.text(field) }, set: { draft.set(field, $0) })
    }

    private func baseLabel(_ key: String) -> String {
        let text = OMPConfiguration.text(draft.base(selectedModel, key))
        return text.isEmpty ? "模型默认" : "模型默认：\(text)"
    }

    private func load(_ current: JSONValue? = nil) {
        draft = OMPConfiguration(stored: current ?? configuration)
        thresholdMode = initialThresholdMode
        selectorModes = [:]
        selector = ""; selectorError = nil; saveError = nil; importMessage = nil
        if !draft.modelIDs.contains(selectedModel) { selectedModel = draft.modelIDs.first ?? "" }
    }

    private func changeThresholdMode(_ mode: String) {
        thresholdMode = mode
        if mode != "fixed" { draft.set(.config("thresholdTokens", "固定阈值", .integer), "") }
        if mode != "percent" { draft.set(.config("thresholdPercent", "阈值百分比", .number), "") }
    }

    private func ruleMode(_ text: String) -> String {
        if text.isEmpty { return "inherit" }
        if text == "null" { return "clear" }
        if text.hasPrefix("f") || text.hasPrefix("F") { return "fixed" }
        if text.hasSuffix("%") { return "percent" }
        return "base"
    }

    private func ruleNumber(_ text: String) -> String {
        if text == "null" { return "" }
        if text.hasPrefix("f") || text.hasPrefix("F") { return String(text.dropFirst()) }
        if text.hasSuffix("%") { return String(text.dropLast()) }
        return text
    }

    private func encodeRule(_ number: String, mode: String) -> String {
        if mode == "clear" { return "null" }
        if mode == "inherit" { return "" }
        let text = number.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty { return "" }
        if mode == "fixed" { return "f" + text }
        if mode == "percent" { return text + "%" }
        return text
    }

    private func changeRuleMode(_ name: String, to mode: String) {
        let field = Field.selector(name)
        selectorModes[name] = mode
        draft.set(field, encodeRule(ruleNumber(draft.text(field)), mode: mode))
    }

    private func addSelector() {
        do { try draft.addSelector(selector); selector = ""; selectorError = nil }
        catch { selectorError = error.localizedDescription }
    }

    private func moveMethod(_ index: Int, by offset: Int, order: [String], field: Field) {
        var next = order
        next.swapAt(index, index + offset)
        draft.set(field, next.joined(separator: ","))
    }

    private func revealFirstError() {
        if let field = draft.errors.keys.sorted(by: { $0.path.joined(separator: "/") < $1.path.joined(separator: "/") }).first(where: {
            $0.file == "models" && $0.path.count > 4 && $0.path[2] == "modelOverrides"
        }) {
            selectedModel = field.path[3]
            if field.path[4] == "remoteCompaction" { modelRemoteExpanded = true }
        }
    }

    private func save() {
        guard editable else { return }
        saveError = nil
        let edits = draft.edits()
        guard let edits, draft.errors.isEmpty else {
            revealFirstError()
            saveError = "请修正标出的字段后保存。"
            return
        }
        guard !edits.isEmpty else { load(); return }
        let directory = agentDirectory
        saving = true
        onSavingChange(true)
        Task {
            defer { saving = false; onSavingChange(false) }
            do {
                let status = try await model.remoteCall("omp.configure", .object([
                    "agent_dir": .string(directory),
                    "expected_revision": .string(configurationRevision),
                    "edits": .array(edits)
                ]))
                guard directory == agentDirectory else { return }
                onSaved(status)
                load(status["configuration"])
                model.notice = "OMP 压缩设置已保存。"
            } catch {
                guard directory == agentDirectory else { return }
                let message = error.localizedDescription
                if let field = draft.changes.keys.first(where: { message.contains($0.path.joined(separator: ".")) || message.contains($0.path.joined(separator: "/")) }) {
                    draft.errors[field] = message
                    revealFirstError()
                }
                saveError = message
            }
        }
    }
}
