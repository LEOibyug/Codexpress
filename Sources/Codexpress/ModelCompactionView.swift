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
}

struct ModelCompactionView: View {
    @EnvironmentObject var model: AppModel
    @State private var rules: [CompactionDraft] = []
    @State private var validationError: String?

    var body: some View {
        GroupBox("按模型自动压缩上下文") {
            VStack(alignment: .leading, spacing: 12) {
                Text("每个模型可设置不同的压缩阈值，单位为 Token。未配置的模型保留原有默认值；同一模型在不同来源间使用相同规则。").font(.callout).foregroundStyle(.secondary)
                if !rules.isEmpty {
                    HStack {
                        Text("模型 ID").frame(maxWidth: .infinity, alignment: .leading)
                        Text("压缩阈值").frame(width: 120, alignment: .leading)
                        Text("上下文窗口（可选）").frame(width: 145, alignment: .leading)
                        Color.clear.frame(width: 24, height: 1)
                    }.font(.caption).foregroundStyle(.secondary)
                }
                ForEach($rules) { $rule in
                    HStack {
                        TextField("例如 gpt-6.1-sol", text: $rule.model)
                        TextField("例如 180000", text: $rule.threshold).frame(width: 120)
                        TextField("沿用模型默认", text: $rule.window).frame(width: 145)
                        Button { rules.removeAll { $0.id == rule.id } } label: { Image(systemName: "minus.circle") }.buttonStyle(.borderless).frame(width: 24).help("删除此规则")
                    }
                }
                HStack {
                    Button { rules.append(CompactionDraft()) } label: { Label("添加模型", systemImage: "plus") }.disabled(rules.count >= 200)
                    Spacer()
                    Button("保存模型规则") { save() }
                }
                if let validationError { Text(validationError).font(.callout).foregroundStyle(.orange) }
                Text("压缩阈值最多为上下文窗口的 90%。窗口留空时使用已有模型目录或内置元数据；未知模型默认窗口为 272000。保存后需在下方预览并应用，重启 Codex 后生效。").font(.caption).foregroundStyle(.secondary)
            }.padding(8).disabled(model.busy || model.metadataLoadFailed)
        }
        .onAppear { rules = (model.settings.modelCompaction ?? []).map(CompactionDraft.init) }
    }

    private func save() {
        do {
            let values = try rules.map { try ModelCompactionRule.parse(model: $0.model, threshold: $0.threshold, window: $0.window) }
            try ModelCompactionRule.validate(values)
            validationError = nil; model.saveModelCompaction(values)
        } catch { validationError = error.localizedDescription }
    }
}
