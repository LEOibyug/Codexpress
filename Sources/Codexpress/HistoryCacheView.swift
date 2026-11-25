import SwiftUI

struct HistoryCacheView: View {
    @EnvironmentObject var model: AppModel
    @State private var capacity = "5"
    @State private var confirmingClear = false
    @State private var validationError: String?
    private var preferences: HistoryCacheSettings? { try? .parse(enabled: model.settings.historyCacheSettings.enabled, gigabytes: capacity) }
    private var changed: Bool { preferences != model.settings.historyCacheSettings }
    var body: some View {
        HistoryCacheSettingsCard(title: "本地原文缓存", enabled: Binding(get: { model.settings.historyCacheSettings.enabled }, set: { enabled in
            var next = model.settings.historyCacheSettings
            next.enabled = enabled
            model.saveHistoryCache(next)
        }), capacity: $capacity, status: model.historyStatus, validationError: validationError,
        canClear: model.running, clear: { confirmingClear = true }, canApply: changed, applyCapacity: saveCapacity)
        .disabled(model.busy || model.metadataLoadFailed)
        .onAppear { capacity = String(format: "%g", Double(model.settings.historyCacheSettings.maxBytes) / 1_000_000_000) }
        .task { while !Task.isCancelled { await model.refreshHistoryStatus(); do { try await Task.sleep(for: .seconds(5)) } catch { break } } }
        .alert("清空本地原文缓存？", isPresented: $confirmingClear) {
            Button("取消", role: .cancel) {}
            Button("清空", role: .destructive) { model.clearHistoryCache() }
        } message: { Text("将删除所有缓存的增量块。依赖这些块的上下文将无法从网关重建，Codex 中的对话记录保留。") }
    }

    private func saveCapacity() {
        do {
            let value = try HistoryCacheSettings.parse(enabled: model.settings.historyCacheSettings.enabled, gigabytes: capacity)
            validationError = nil
            model.saveHistoryCache(value)
        } catch { validationError = error.localizedDescription }
    }
}

struct HistoryCacheSettingsCard: View {
    let title: String
    var scope: String?
    @Binding var enabled: Bool
    @Binding var capacity: String
    let status: JSONValue
    let validationError: String?
    let canClear: Bool
    let clear: () -> Void
    var canApply = false
    var applyCapacity: (() -> Void)?

    private func bytes(_ value: JSONValue) -> String {
        value.number.flatMap { Int64(exactly: $0) }.map { ByteCountFormatter.string(fromByteCount: $0, countStyle: .decimal) } ?? "—"
    }

    var body: some View {
        SettingsCard(title, symbol: "externaldrive") {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: "保存原文", subtitle: scope) {
                    Toggle("保存原文", isOn: $enabled).toggleStyle(.switch).labelsHidden()
                }
                Divider().overlay(AppPalette.border)
                SettingRow(title: "最大容量") {
                    HStack(spacing: 6) {
                        TextField("容量", text: $capacity).multilineTextAlignment(.trailing).settingsField().frame(width: 110)
                            .onSubmit { applyCapacity?() }
                        Text("GB").foregroundStyle(.secondary)
                    }
                }
                if let error = status["error"].string { InfoNote(text: error, color: .orange) }
                if let skipped = status["skipped_captures"].number, skipped > 0 {
                    Text("本次运行有 \(usageCount(skipped)) 次响应未完整缓存。")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                }
                if let validationError { InfoNote(text: validationError, color: .orange) }
                HStack(spacing: 16) {
                    VStack(alignment: .leading, spacing: 5) {
                        Text("已用 \(bytes(status["disk_bytes"])) / \(bytes(status["max_bytes"]))")
                        Text("\(usageCount(status["blocks"].number ?? 0)) 个块")
                    }.font(.system(size: 12)).foregroundStyle(.secondary)
                    Spacer(minLength: 12)
                    Button("清空缓存…", action: clear).disabled(!canClear)
                    if let applyCapacity {
                        Button("应用容量", action: applyCapacity).disabled(!canApply)
                    }
                }
            }
        }
    }
}
