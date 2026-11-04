import SwiftUI

struct SourceRoutingActions: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    var compact = false

    private var cooling: Bool { ["cooldown", "proxy_error"].contains(model.state(source.id)["health"].string ?? "") }
    private var takingOver: Bool { model.status["manual_route"]["source_id"].string == source.id }
    private var exhausted: Bool { source.official && model.state(source.id)["health"].string == "exhausted" }

    var body: some View {
        HStack(spacing: compact ? 4 : 8) {
            if cooling {
                action("立即恢复", symbol: "arrow.clockwise") { model.recoverSource(source.id) }
                    .help("解除临时冷却，重新参与分配")
                    .disabled(!source.enabled)
            }
            action("立即抢占", symbol: "bolt.fill") { model.startSourceTakeover(source.id) }
                .help(exhausted ? "额度已耗尽" : takingOver ? "正在抢占" : "立即抢占")
                .disabled(takingOver || exhausted || !source.enabled)
            if takingOver {
                if !compact {
                    Label("正在抢占", systemImage: "clock")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                action("停止抢占", symbol: "xmark.circle") { model.cancelSourceTakeover() }
                    .help("停止抢占")
            }
        }.controlSize(.small)
            .disabled(model.busy || !model.running)
    }

    @ViewBuilder private func action(_ title: String, symbol: String, perform: @escaping () -> Void) -> some View {
        if compact {
            Button(action: perform) {
                Image(systemName: symbol).font(.system(size: 11, weight: .medium))
                    .frame(width: 22, height: 22).contentShape(Rectangle())
            }.buttonStyle(.plain).accessibilityLabel(title)
        } else {
            Button(title, action: perform)
        }
    }
}
