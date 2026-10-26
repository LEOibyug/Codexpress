import SwiftUI

private enum SettingsPage: String, CaseIterable, Identifiable {
    case sources = "来源", routing = "路由", usage = "用量", remote = "远端", chatbot = "聊天客户端", images = "图片生成", general = "设置"
    var id: String { rawValue }
    var symbol: String {
        switch self {
        case .sources: return "server.rack"
        case .routing: return "arrow.triangle.branch"
        case .usage: return "chart.bar.xaxis"
        case .remote: return "desktopcomputer.and.arrow.down"
        case .chatbot: return "bubble.left.and.bubble.right"
        case .images: return "photo"
        case .general: return "slider.horizontal.3"
        }
    }
}

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var page: SettingsPage? = .sources

    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    Image(systemName: "arrow.triangle.branch")
                        .font(.system(size: 17, weight: .semibold)).foregroundStyle(.white)
                        .frame(width: 34, height: 34)
                        .background(Color.accentColor.gradient, in: RoundedRectangle(cornerRadius: 10))
                    Text("Codexpress").font(.system(size: 14, weight: .semibold))
                    Spacer(minLength: 0)
                }.padding(.horizontal, 18).padding(.top, 18).padding(.bottom, 23)

                List(SettingsPage.allCases, selection: $page) { item in
                    Label {
                        Text(item.rawValue).font(.system(size: 13, weight: page == item ? .medium : .regular))
                    } icon: {
                        Image(systemName: item.symbol).font(.system(size: 14, weight: .medium)).frame(width: 20)
                    }.listItemTint(.monochrome).padding(.vertical, 5).tag(item)
                }.listStyle(.sidebar).scrollContentBackground(.hidden)

                VStack(alignment: .leading, spacing: 9) {
                    Divider().padding(.bottom, 5)
                    HStack(spacing: 7) {
                        Circle().fill(model.running ? .green : .orange).frame(width: 6, height: 6)
                        Text(model.running ? "网关运行中" : "网关未连接").font(.system(size: 11, weight: .medium))
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(18)
            }.navigationSplitViewColumnWidth(min: 178, ideal: 190, max: 210)
        } detail: {
            VStack(spacing: 0) {
                notices
                Group {
                    switch page ?? .sources {
                    case .sources: SourcesView()
                    case .routing: RoutingView()
                    case .usage: UsageView()
                    case .remote: SSHRemoteView()
                    case .chatbot: ChatbotSettingsView()
                    case .images: ImageSettingsView()
                    case .general: GeneralSettingsView()
                    }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }.background(AppPalette.canvas)
        }.font(.system(size: 13)).frame(minWidth: 860, minHeight: 620)
            .environment(\.windowIsActive, model.settingsWindowActive)
            .overlay(alignment: .bottomLeading) { AppFeedbackOverlay() }
    }

    @ViewBuilder private var notices: some View {
        if model.metadataLoadFailed || model.pendingSync {
            VStack(spacing: 8) {
                if model.metadataLoadFailed {
                    banner("来源文件无法读取，已进入只读保护。请从备份修复 sources.json 后重启。", symbol: "lock.shield", color: .orange)
                }
                if model.pendingSync {
                    banner("配置尚未应用到网关，当前路由仍使用上次设置。请重试启动。", symbol: "arrow.triangle.2.circlepath", color: .orange)
                }
            }.padding(.horizontal, 28).padding(.top, 18).frame(maxWidth: 820)
        }
    }

    private func banner(_ text: String, symbol: String, color: Color, dismiss: (() -> Void)? = nil) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(color)
            Text(text).font(.system(size: 12)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            if let dismiss {
                Button(action: dismiss) { Image(systemName: "xmark").font(.system(size: 10, weight: .medium)) }
                    .buttonStyle(.plain).foregroundStyle(.secondary).accessibilityLabel("关闭提示")
            }
        }.padding(12).background(color.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
    }
}
