import SwiftUI

struct SourceRoutingActions: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    var compact = false

    private var cooling: Bool { ["cooldown", "proxy_error"].contains(model.state(source.id)["health"].string ?? "") }
    private var takingOver: Bool { model.status["manual_route"]["source_id"].string == source.id }
    private var exhausted: Bool { source.official && model.state(source.id)["health"].string == "exhausted" }
    private var needsLogin: Bool { source.official && source.account_id == nil }
    private var takeoverHelp: String {
        if !source.enabled { return "请先启用此来源" }
        if needsLogin { return "请先登录此账号" }
        if exhausted { return "额度已耗尽，请先刷新额度或使用重置卡" }
        return takingOver ? "正在接管后续请求" : "从后续请求开始尝试此来源"
    }

    @ViewBuilder var body: some View {
        if compact {
            HStack(spacing: 4) { actions }.controlSize(.small)
        } else {
            actions
        }
    }

    private var unavailable: Bool { model.busy || !model.running }

    @ViewBuilder private var actions: some View {
        if cooling {
            action("立即恢复", symbol: "arrow.clockwise") { model.recoverSource(source.id) }
                .help(source.enabled ? "解除临时冷却，重新参与分配" : "请先启用此来源")
                .disabled(unavailable || !source.enabled)
        }
        action("立即抢占", symbol: "bolt.fill") { model.startSourceTakeover(source.id) }
            .help(takeoverHelp)
            .disabled(unavailable || takingOver || exhausted || needsLogin || !source.enabled)
        if takingOver {
            action("停止抢占", symbol: "xmark.circle") { model.cancelSourceTakeover() }
                .help("停止抢占").disabled(unavailable)
        }
    }

    @ViewBuilder private func action(_ title: String, symbol: String, perform: @escaping () -> Void) -> some View {
        if compact {
            Button(action: perform) {
                Image(systemName: symbol).font(.system(size: 11, weight: .medium))
                    .frame(width: 22, height: 22).contentShape(Rectangle())
            }.buttonStyle(.plain).accessibilityLabel(title)
        } else {
            Button(action: perform) { Label(title, systemImage: symbol) }
        }
    }
}
