import SwiftUI

struct HistoryCacheView: View {
    @EnvironmentObject var model: AppModel
    @State private var enabled = true
    @State private var capacity = "5"
    @State private var confirmingClear = false
    @State private var validationError: String?
    private var preferences: HistoryCacheSettings? { try? .parse(enabled: enabled, gigabytes: capacity) }
    private var changed: Bool { preferences != model.settings.historyCacheSettings }
    private func bytes(_ value: JSONValue) -> String {
        value.number.map { ByteCountFormatter.string(fromByteCount: Int64($0), countStyle: .decimal) } ?? "—"
    }
    var body: some View {
        SettingsCard("本地原文缓存", symbol: "externaldrive") {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: "保存原文") {
                    Toggle("保存原文", isOn: $enabled).toggleStyle(.switch).labelsHidden()
                }
                Divider().overlay(AppPalette.border)
                SettingRow(title: "最大容量") {
                    HStack(spacing: 6) {
                        TextField("容量", text: $capacity).multilineTextAlignment(.trailing).settingsField().frame(width: 110)
                        Text("GB").foregroundStyle(.secondary)
                    }
                }
                if let error = model.historyStatus["error"].string { InfoNote(text: error, color: .orange) }
                if let skipped = model.historyStatus["skipped_captures"].number, skipped > 0 {
                    Text("本次运行有 \(usageCount(skipped)) 次响应未完整缓存。")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                }
                if let validationError { InfoNote(text: validationError, color: .orange) }
                HStack(spacing: 16) {
                    VStack(alignment: .leading, spacing: 5) {
                        Text("已用 \(bytes(model.historyStatus["disk_bytes"])) / \(bytes(model.historyStatus["max_bytes"]))")
                        Text("\(usageCount(model.historyStatus["blocks"].number ?? 0)) 个块")
                    }.font(.system(size: 12)).foregroundStyle(.secondary)
                    Spacer(minLength: 12)
                    Button("清空缓存…") { confirmingClear = true }.disabled(!model.running)
                    Button("保存") {
                        do { let value = try HistoryCacheSettings.parse(enabled: enabled, gigabytes: capacity); validationError = nil; model.saveHistoryCache(value) }
                        catch { validationError = error.localizedDescription }
                    }.disabled(!changed)
                }
            }.disabled(model.busy || model.metadataLoadFailed)
        }
        .onAppear { let value = model.settings.historyCacheSettings; enabled = value.enabled; capacity = String(format: "%g", Double(value.maxBytes) / 1_000_000_000) }
        .task { while !Task.isCancelled { await model.refreshHistoryStatus(); do { try await Task.sleep(for: .seconds(5)) } catch { break } } }
        .alert("清空本地原文缓存？", isPresented: $confirmingClear) {
            Button("取消", role: .cancel) {}
            Button("清空", role: .destructive) { model.clearHistoryCache() }
        } message: { Text("将删除所有缓存的增量块。依赖这些块的上下文将无法从网关重建，Codex 中的对话记录保留。") }
    }
}
