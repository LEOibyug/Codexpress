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
            VStack(alignment: .leading, spacing: 7) {
                DirectOfficialLoginButton().buttonStyle(.bordered).controlSize(.small)
                if model.connectionMode == "official" {
                    HStack {
                        Text("官方连接 · 系统代理").foregroundStyle(.secondary)
                        Spacer()
                        Button("使用来源池") { model.switchCodexConnection(.gateway) }.disabled(model.busy || !model.running)
                    }.font(.system(size: 11))
                } else {
                    Text("重启 Codex，使用系统代理").font(.system(size: 10)).foregroundStyle(.secondary)
                }
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
                    Text(model.connectionMode == "official" ? "来源池设置" : "当前路由").foregroundStyle(.secondary)
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
                                Text(source.policyLabel)
                                    .font(.system(size: 10)).foregroundStyle(.secondary)
                                if source.official { compactQuota(model.state(source.id)["quota"]) }
                            }.frame(maxWidth: .infinity, alignment: .leading)
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
            .task { await model.refreshOfficialIfDue(foreground: true) }
    }

    private func showSettings() { AppDelegate.current?.showSettings() }
    private func compactQuota(_ quota: JSONValue) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .top, spacing: 12) {
                ForEach(QuotaOverview(quota).mainWindows) { window in
                    VStack(alignment: .leading, spacing: 3) {
                        HStack(spacing: 5) {
                            Text(window.title).foregroundStyle(.secondary)
                            Text("剩余 \(window.remainingLabel)").fontWeight(.medium).monospacedDigit()
                        }.font(.system(size: 10))
                        Text(window.resetLabel).font(.system(size: 9)).foregroundStyle(.secondary)
                            .lineLimit(1).help(window.resetLabel)
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            if quota["stale"].bool == true {
                Label("更新失败", systemImage: "clock").font(.system(size: 9)).foregroundStyle(.orange)
                    .help("显示上次获取的额度：\(timestampLabel(quota["fetched_at"]))")
            }
        }.padding(.top, 2)
    }
}
