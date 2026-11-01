import SwiftUI

private struct CompactionDraft: Identifiable {
    let id = UUID()
    var model = ""
    var threshold = ""
    var window = ""
    init() {}
    init(_ rule: ModelCompactionRule) {
        model = rule.model; threshold = String(rule.auto_compact_token_limit)
        window = rule.context_window.map(String.init) ?? ""
    }
    var values: [String] { [model, threshold, window] }
}

struct ModelCompactionView: View {
    @EnvironmentObject var model: AppModel
    @State private var rules: [CompactionDraft] = []
    @State private var validationError: String?
    @State private var detailsExpanded = false

    private var hasChanges: Bool { rules.map(\.values) != (model.settings.modelCompaction ?? []).map { CompactionDraft($0).values } }

    var body: some View {
        SettingsCard("上下文压缩", symbol: "text.alignleft", accessory: {
            Text(rules.isEmpty ? "模型默认" : "\(rules.count) 条规则").font(.system(size: 12, weight: .medium))
                .foregroundStyle(.secondary)
        }) {
            VStack(alignment: .leading, spacing: 18) {
                if rules.isEmpty {
                    HStack(spacing: 12) {
                        Text("未设置自定义阈值").font(.system(size: 13)).foregroundStyle(.secondary)
                        Spacer()
                        Button { addRule() } label: { Label("添加模型", systemImage: "plus") }
                    }.padding(.vertical, 6).frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    VStack(spacing: 12) {
                        HStack(spacing: 12) {
                            Text("模型 ID").frame(maxWidth: .infinity, alignment: .leading)
                            Text("压缩阈值 · Token").frame(width: 150, alignment: .leading)
                            Text("上下文窗口 · 可选").frame(width: 160, alignment: .leading)
                            Color.clear.frame(width: 28, height: 1)
                        }.font(.system(size: 12, weight: .medium)).foregroundStyle(.secondary)
                        Divider()
                        ForEach($rules) { $rule in
                            HStack(spacing: 12) {
                                TextField("例如 gpt-6.1-sol", text: $rule.model).accessibilityLabel("模型 ID")
                                    .settingsField().frame(maxWidth: .infinity)
                                TextField("180000", text: $rule.threshold).accessibilityLabel("压缩阈值 Token")
                                    .settingsField().frame(width: 150)
                                TextField("模型默认", text: $rule.window).accessibilityLabel("上下文窗口 Token，可选")
                                    .settingsField().frame(width: 160)
                                Button { rules.removeAll { $0.id == rule.id } } label: {
                                    Image(systemName: "minus.circle").font(.system(size: 15)).frame(width: 28, height: 28)
                                }.buttonStyle(.plain).foregroundStyle(.secondary).help("删除此规则").accessibilityLabel("删除模型规则")
                            }.font(.system(size: 13, design: .monospaced))
                        }
                    }
                }
                if let validationError { InfoNote(text: validationError, symbol: "exclamationmark.triangle", color: .orange) }
                HStack(alignment: .center, spacing: 12) {
                    if !rules.isEmpty {
                        Button { addRule() } label: { Label("添加模型", systemImage: "plus") }
                            .disabled(rules.count >= 200)
                    }
                    Spacer(minLength: 0)
                    Button("保存规则") { save() }.disabled(!hasChanges)
                }
                DisclosureGroup("填写说明", isExpanded: $detailsExpanded) {
                    VStack(alignment: .leading, spacing: 7) {
                        Text("单位为 Token，压缩阈值不能超过上下文窗口的 90%。窗口留空使用模型默认值；未知模型默认为 272000。")
                        Text("保存后预览并应用连接配置，再完整退出并重开 Codex。远端使用 VS Code 时请重载对应窗口。同一模型的所有来源使用相同阈值。")
                    }.font(.system(size: 12)).foregroundStyle(.secondary).lineSpacing(3).padding(.top, 6)
                }.font(.system(size: 12)).foregroundStyle(.secondary)
            }.disabled(model.busy || model.metadataLoadFailed)
        }
        .onAppear { rules = (model.settings.modelCompaction ?? []).map(CompactionDraft.init) }
        .onChange(of: model.settings.modelCompaction) { _, value in
            rules = (value ?? []).map(CompactionDraft.init)
        }
    }

    private func addRule() { rules.append(CompactionDraft()) }
    private func save() {
        do {
            let values = try rules.map { try ModelCompactionRule.parse(model: $0.model, threshold: $0.threshold, window: $0.window) }
            try ModelCompactionRule.validate(values)
            validationError = nil; model.saveModelCompaction(values)
        } catch { validationError = error.localizedDescription }
    }
}
