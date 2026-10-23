import SwiftUI

struct SourceRoutingActions: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    var compact = false

    private var cooling: Bool { ["cooldown", "proxy_error"].contains(model.state(source.id)["health"].string ?? "") }
    private var queued: Bool { model.status["manual_route"]["source_id"].string == source.id }

    var body: some View {
        HStack(spacing: compact ? 4 : 8) {
            if cooling {
                action("立即恢复", symbol: "arrow.clockwise") { model.recoverSource(source.id) }
                    .help("解除临时冷却，重新参与分配")
                    .disabled(!source.enabled)
            }
            action("立即插入", symbol: "bolt.fill") { model.trySourceOnce(source.id) }
                .help(queued ? "已等待下一条请求" : "下一条对话请求先尝试此来源，成功后沿用；失败可回退。活跃对话上限仍有效。")
                .disabled(queued || !source.enabled || model.connectionMode == "official")
            if queued {
                if !compact {
                    Label("等待下一条请求", systemImage: "clock")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                action("取消插入", symbol: "xmark.circle") { model.cancelSourceTrial() }
                    .help("取消等待中的来源插入")
            }
        }.controlSize(.small)
            .disabled(model.busy || !model.running)
    }

    @ViewBuilder private func action(_ title: String, symbol: String, perform: @escaping () -> Void) -> some View {
        if compact {
            Button(action: perform) {
                Image(systemName: symbol).font(.system(size: 11, weight: .medium))
                    .frame(width: 18, height: 16).contentShape(Rectangle())
            }.buttonStyle(.plain).accessibilityLabel(title)
        } else {
            Button(title, action: perform)
        }
    }
}
