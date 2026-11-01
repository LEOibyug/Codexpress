import SwiftUI

enum SettingsPage: String, CaseIterable, Identifiable {
    case overview = "概览", sources = "来源池", usage = "用量", connections = "接入", general = "设置"
    var id: String { rawValue }
    var symbol: String {
        switch self {
        case .overview: return "square.grid.2x2"
        case .sources: return "server.rack"
        case .usage: return "chart.bar.xaxis"
        case .connections: return "point.3.connected.trianglepath.dotted"
        case .general: return "slider.horizontal.3"
        }
    }
}

extension Notification.Name {
    static let codexpressNavigate = Notification.Name("local.codexpress.navigate")
}

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var page: SettingsPage? = .overview
    @State private var connectionSection: ConnectionSection = .codex

    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                List(selection: $page) {
                    Section {
                        ForEach([SettingsPage.overview, .sources, .usage]) { navigationItem($0) }
                    }
                    Section {
                        navigationItem(.connections)
                        navigationItem(.general)
                    }
                }.listStyle(.sidebar).scrollContentBackground(.hidden)
                    .padding(.top, 10)

                VStack(alignment: .leading, spacing: 10) {
                    Divider()
                    HStack(spacing: 8) {
                        ActivityDot(color: model.running ? .green : .orange, active: model.gatewayHasActivity)
                        Text(model.running ? "网关运行中" : "网关未连接").font(.system(size: 12))
                        Spacer(minLength: 0)
                    }
                    Text("端口 \(model.settings.port)").font(.system(size: 11)).monospacedDigit().foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading).padding(16)
            }.navigationSplitViewColumnWidth(min: 190, ideal: 206, max: 230)
        } detail: {
            VStack(spacing: 0) {
                notices
                Group {
                    switch page ?? .overview {
                    case .overview: OverviewView(openSources: { page = .sources }, openUsage: { page = .usage }, openConnection: { connectionSection = $0; page = .connections })
                    case .sources: SourcesView()
                    case .usage: UsageView()
                    case .connections: ConnectionSettingsView(section: $connectionSection)
                    case .general: GeneralSettingsView()
                    }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }.background(AppPalette.canvas)
        }.navigationSplitViewStyle(.balanced)
            .toolbarBackground(AppPalette.canvas, for: .windowToolbar)
            .toolbarBackground(.visible, for: .windowToolbar)
            .font(AppTypography.body).frame(minWidth: 960, minHeight: 700)
            .environment(\.windowIsActive, model.settingsWindowActive)
            .overlay(alignment: .bottomLeading) { AppFeedbackOverlay() }
            .sheet(item: $model.creditResetRequest) { request in
                ResetCreditsSheet(request: request).environmentObject(model)
            }
            .onReceive(NotificationCenter.default.publisher(for: .codexpressNavigate)) { notification in
                if let destination = notification.object as? SettingsPage { page = destination }
            }
    }

    private func navigationItem(_ item: SettingsPage) -> some View {
        Label {
            Text(item.rawValue).font(.system(size: 13, weight: page == item ? .medium : .regular))
        } icon: {
            Image(systemName: item.symbol).font(.system(size: 14)).frame(width: 20)
        }.padding(.vertical, 4).tag(item)
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
            }.padding(.horizontal, AppMetrics.pageInset).padding(.top, 18).frame(maxWidth: 1120)
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
