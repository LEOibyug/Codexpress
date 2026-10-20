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
            Text(rules.isEmpty ? "模型默认" : "\(rules.count) 条规则").font(.system(size: 10, weight: .medium))
                .foregroundStyle(.secondary).padding(.horizontal, 8).padding(.vertical, 5)
                .background(AppPalette.well, in: Capsule())
        }) {
            VStack(alignment: .leading, spacing: 14) {
                if rules.isEmpty {
                    HStack(spacing: 12) {
                        Image(systemName: "text.badge.plus").font(.system(size: 23, weight: .light)).foregroundStyle(.tertiary)
                        Text("未设置自定义阈值").font(.system(size: 12)).foregroundStyle(.secondary)
                        Spacer()
                        Button { addRule() } label: { Label("添加模型", systemImage: "plus") }.controlSize(.small)
                    }.padding(14).frame(maxWidth: .infinity, alignment: .leading)
                        .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
                } else {
                    VStack(spacing: 9) {
                        HStack(spacing: 8) {
                            Text("模型 ID").frame(maxWidth: .infinity, alignment: .leading)
                            Text("压缩阈值 · Token").frame(width: 117, alignment: .leading)
                            Text("上下文窗口 · 可选").frame(width: 128, alignment: .leading)
                            Color.clear.frame(width: 25, height: 1)
                        }.font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
                        ForEach($rules) { $rule in
                            HStack(spacing: 8) {
                                TextField("例如 gpt-6.1-sol", text: $rule.model).accessibilityLabel("模型 ID")
                                    .settingsField().frame(maxWidth: .infinity)
                                TextField("180000", text: $rule.threshold).accessibilityLabel("压缩阈值 Token")
                                    .settingsField().frame(width: 117)
                                TextField("模型默认", text: $rule.window).accessibilityLabel("上下文窗口 Token，可选")
                                    .settingsField().frame(width: 128)
                                Button { rules.removeAll { $0.id == rule.id } } label: {
                                    Image(systemName: "xmark").font(.system(size: 10, weight: .medium)).frame(width: 25, height: 28)
                                }.buttonStyle(.plain).foregroundStyle(.secondary).help("删除此规则").accessibilityLabel("删除模型规则")
                            }.font(.system(size: 11, design: .monospaced))
                        }
                    }
                }
                if let validationError { InfoNote(text: validationError, symbol: "exclamationmark.triangle", color: .orange) }
                HStack(alignment: .center, spacing: 12) {
                    if !rules.isEmpty {
                        Button { addRule() } label: { Label("添加模型", systemImage: "plus") }
                            .buttonStyle(.plain).foregroundStyle(Color.accentColor).font(.system(size: 11)).disabled(rules.count >= 200)
                    }
                    Spacer(minLength: 0)
                    Button("保存规则") { save() }.controlSize(.small).disabled(!hasChanges)
                }
                DisclosureGroup("填写说明", isExpanded: $detailsExpanded) {
                    VStack(alignment: .leading, spacing: 7) {
                        Text("单位为 Token，压缩阈值不能超过上下文窗口的 90%。窗口留空使用模型默认值；未知模型默认为 272000。")
                        Text("保存规则后，应用连接设置并重启 Codex。同一模型的所有来源使用相同阈值。")
                    }.font(.system(size: 11)).foregroundStyle(.secondary).lineSpacing(3).padding(.top, 6)
                }.font(.system(size: 11)).foregroundStyle(.secondary)
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
