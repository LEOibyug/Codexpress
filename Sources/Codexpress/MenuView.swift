import SwiftUI
import AppKit

struct MenuView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "arrow.triangle.branch").font(.system(size: 16, weight: .medium))
                    .foregroundStyle(.white).frame(width: 34, height: 34)
                    .background(Color.accentColor.gradient, in: RoundedRectangle(cornerRadius: 10))
                Text("Codexpress").font(.system(size: 13, weight: .semibold))
                Spacer()
                StatusPill(text: model.running ? "运行中" : "未连接", color: model.running ? .green : .orange)
            }
            Divider()
            if model.sources.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    Text("暂无来源").font(.system(size: 13, weight: .medium))
                    Button { showSettings() } label: { Label("添加来源", systemImage: "plus") }
                        .buttonStyle(.borderedProminent).controlSize(.small).padding(.top, 3)
                }.padding(.vertical, 4)
            } else {
                HStack {
                    Text("当前路由").foregroundStyle(.secondary)
                    Spacer()
                    Text(model.routingMode == "auto" ? "自动切换" : "固定 · \(model.fixedName)").fontWeight(.medium)
                }.font(.system(size: 11))
                VStack(spacing: 13) {
                    ForEach(Array(model.sources.prefix(4))) { source in
                        HStack(alignment: .top, spacing: 9) {
                            Circle().fill(source.enabled && model.state(source.id)["health"].string == "ready" ? .green : .secondary)
                                .frame(width: 5, height: 5).padding(.top, 5)
                            VStack(alignment: .leading, spacing: 5) {
                                Text(source.name).font(.system(size: 12, weight: .medium)).lineLimit(1)
                                Text(source.policyLabel + (source.official ? " · \(quotaSummary(model.state(source.id)["quota"]))" : ""))
                                    .font(.system(size: 10)).foregroundStyle(.secondary)
                            }
                            Spacer(minLength: 0)
                            Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用")
                                .font(.system(size: 10)).foregroundStyle(.secondary)
                        }
                    }
                }
            }
            if model.pendingSync { InfoNote(text: "配置尚未应用，当前路由使用上次设置。", symbol: "clock", color: .orange) }
            if let error = model.error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange).lineLimit(3) }
            Divider()
            HStack {
                Button { showSettings() } label: { Label("设置", systemImage: "slider.horizontal.3") }
                if !model.running { Button("重试启动") { model.retry() } }
                Spacer()
                Button("退出") { NSApp.terminate(nil) }.keyboardShortcut("q").foregroundStyle(.secondary)
            }.buttonStyle(.borderless).font(.system(size: 11))
        }.padding(18).frame(width: 340)
    }

    private func showSettings() { AppDelegate.current?.showSettings() }
}

func quotaSummary(_ quota: JSONValue) -> String {
    guard let window = quota["windows"].array.first, let used = window["used_percent"].number else { return "额度未知" }
    return "剩余 \(Int(max(0, 100 - used)))%" + (quota["stale"].bool == true ? " · 陈旧" : "")
}
