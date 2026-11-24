import SwiftUI

struct LegacyProviderOriginsView: View {
    @EnvironmentObject var model: AppModel
    @State private var selections: [String:String] = [:]
    @State private var confirmed = false
    @State private var loadedSelections: [String: String] = [:]

    private var changed: Bool { selections.filter { !$0.value.isEmpty } != loadedSelections }
    private var originError: Bool { model.preview["origin_error"].string != nil }
    private var needsConfirmation: Bool { selections.values.contains(where: { !$0.isEmpty }) && (changed || originError) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Divider()
            Text("已有对话的原来源").font(.system(size: 14, weight: .semibold))
            if let error = model.preview["origin_error"].string {
                Text(error).font(.system(size: 12)).foregroundStyle(.orange)
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
            if needsConfirmation {
                Toggle("所选来源与原供应商使用同一账号和 API Key", isOn: $confirmed)
                    .font(.system(size: 13))
            }
            HStack(alignment: .firstTextBaseline, spacing: 20) {
                Spacer()
                Button("保存来源对应") { model.saveLegacyOrigins(selections) }
                    .disabled(model.busy || (!changed && !originError) || (needsConfirmation && !confirmed))
            }
        }
            .onAppear {
                loadedSelections = Dictionary(uniqueKeysWithValues: (model.settings.legacyProviderOrigins ?? []).map { ($0.provider_id, $0.source_id) })
                selections = loadedSelections
            }
            .onChange(of: model.settings.legacyProviderOrigins) { _, values in
                let next = Dictionary(uniqueKeysWithValues: (values ?? []).map { ($0.provider_id, $0.source_id) })
                if selections == loadedSelections { selections = next }
                loadedSelections = next
                confirmed = false
            }
    }

    private func compatibleSources(_ base: String?) -> [Source] {
        func normalize(_ value: String) -> String {
            var result = value
            while result.hasSuffix("/") { result.removeLast() }
            if result.hasSuffix("/responses") { result.removeLast("/responses".count) }
            return result
        }
        return model.sources.filter { source in !source.official && !source.isGoogle && base.map { normalize($0) == normalize(source.base_url) } == true }
    }
}
