import SwiftUI
import AppKit

struct MenuView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            HStack {
                Image(systemName: "arrow.triangle.branch").font(.title2).foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 2) { Text("Codexpress").font(.headline); Text(model.running ? "本地网关运行中" : "网关未连接").font(.caption).foregroundStyle(.secondary) }
                Spacer()
                Circle().fill(model.running ? .green : .orange).frame(width: 8, height: 8)
            }
            Divider()
            if model.sources.isEmpty {
                VStack(alignment: .leading, spacing: 7) {
                    Text("添加第一个来源").font(.headline)
                    Text("连接官方账号或 Responses API，开始使用本地优先级网关。").font(.callout).foregroundStyle(.secondary)
                    Button("添加来源…") { showSettings() }.buttonStyle(.borderedProminent)
                }.padding(.vertical, 6)
            } else {
                HStack { Text("路由").foregroundStyle(.secondary); Spacer(); Text(model.routingMode == "auto" ? "自动优先级池" : "固定 · \(model.fixedName)") }
                ForEach(Array(model.sources.prefix(4))) { source in
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Circle().fill(source.enabled && model.state(source.id)["health"].string == "ready" ? .green : .secondary).frame(width: 6, height: 6)
                            Text(source.name).lineLimit(1)
                            Spacer()
                            Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用").font(.caption).foregroundStyle(.secondary)
                        }
                        HStack { Text(source.policyLabel); if source.official { Text("·"); Text(quotaSummary(model.state(source.id)["quota"])) } }.font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            if model.pendingSync { Label("已保存的配置尚未应用，当前路由以网关状态为准。", systemImage: "clock").font(.caption).foregroundStyle(.orange) }
            if let error = model.error { Label(error, systemImage: "exclamationmark.triangle").font(.caption).foregroundStyle(.orange).lineLimit(3) }
            Divider()
            HStack {
                Button("设置…") { showSettings() }
                if !model.running { Button("重试启动") { model.retry() } }
                Spacer()
                Button("退出") { NSApp.terminate(nil) }.keyboardShortcut("q")
            }.buttonStyle(.borderless)
            Text("退出应用后本地网关停止；恢复原连接请在设置中操作。").font(.caption2).foregroundStyle(.tertiary)
        }.padding(18).frame(width: 340)
    }
    private func showSettings() { AppDelegate.current?.showSettings() }
}
func quotaSummary(_ quota: JSONValue) -> String {
    guard let window = quota["windows"].array.first, let used = window["used_percent"].number else { return "额度未知" }
    return "剩余 \(Int(max(0, 100 - used)))%" + (quota["stale"].bool == true ? " · 陈旧" : "")
}
