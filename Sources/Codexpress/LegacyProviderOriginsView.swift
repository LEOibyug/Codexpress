import SwiftUI

struct LegacyProviderOriginsView: View {
    @EnvironmentObject var model: AppModel
    @State private var selections: [String:String] = [:]
    @State private var confirmed = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("已有对话的原来源").font(.system(size: 12, weight: .medium))
            if let error = model.preview["origin_error"].string {
                Text(error).font(.system(size: 11)).foregroundStyle(.orange)
                Button("清除已保存的对应关系") { model.saveLegacyOrigins([:]); selections = [:] }.disabled(model.busy)
            }
            ForEach(Array(model.preview["legacy_providers"].array.enumerated()), id: \.offset) { _, provider in
                if let id = provider["provider_id"].string {
                    Picker(id, selection: Binding(get: { selections[id] ?? "" }, set: { selections[id] = $0; confirmed = false })) {
                        Text("保持原连接").tag("")
                        ForEach(compatibleSources(provider["base_url"].string)) { source in
                            Text(source.name).tag(source.id)
                        }
                    }
                }
            }
            Toggle("所选来源与原供应商使用同一账号和 API Key", isOn: $confirmed)
                .font(.system(size: 11))
            HStack {
                Text("已有对话固定回原来源，新对话按优先级分配。")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
                Spacer()
                Button("确认来源并更新预览") { model.saveLegacyOrigins(selections) }
                    .disabled(model.busy || (!confirmed && selections.values.contains(where: { !$0.isEmpty })))
            }
            Text("原来源无法续接时，网关会尝试移除请求中的加密块并通知；隐藏上下文可能缺失，本地记录保留。")
                .font(.system(size: 11)).foregroundStyle(.secondary)
        }.padding(12).background(AppPalette.well, in: RoundedRectangle(cornerRadius: 8))
            .onAppear { selections = Dictionary(uniqueKeysWithValues: (model.settings.legacyProviderOrigins ?? []).map { ($0.provider_id, $0.source_id) }) }
    }

    private func compatibleSources(_ base: String?) -> [Source] {
        func normalize(_ value: String) -> String {
            var result = value
            while result.hasSuffix("/") { result.removeLast() }
            if result.hasSuffix("/responses") { result.removeLast("/responses".count) }
            return result
        }
        return model.sources.filter { source in !source.official && base.map { normalize($0) == normalize(source.base_url) } == true }
    }
}
