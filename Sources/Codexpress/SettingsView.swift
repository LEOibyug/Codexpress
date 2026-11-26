import SwiftUI

enum SettingsPage: String, CaseIterable, Identifiable {
    case overview = "概览", sources = "来源池", models = "模型", usage = "用量", connections = "接入", general = "设置"
    var id: String { rawValue }
    var symbol: String {
        switch self {
        case .overview: return "square.grid.2x2"
        case .sources: return "server.rack"
        case .models: return "cube"
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
    @EnvironmentObject var management: RemoteGatewayManagement
    @State private var choosingGateway = false
    @State private var page: SettingsPage? = .overview
    @State private var connectionSection: ConnectionSection = .codex
    private var sourcesOnly: Bool { management.remoteSelected && !management.capabilities.contains("config.edit") }

    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                Button { choosingGateway = true } label: {
                    Label(management.remoteSelected ? "远端网关" : "本地网关", systemImage: "arrow.left.arrow.right")
                        .frame(maxWidth: .infinity, alignment: .leading)
                }.controlSize(.large).padding(.horizontal, 12).padding(.top, 12)
                    .help("切换正在管理的网关")
                List(selection: $page) {
                    Section("工作台") {
                        ForEach(sourcesOnly ? [SettingsPage.sources] : [.overview, .sources, .models, .usage]) { navigationItem($0) }
                    }
                    if !sourcesOnly {
                        Section("配置") {
                            navigationItem(.connections)
                            navigationItem(.general)
                        }
                    }
                }.listStyle(.sidebar).scrollContentBackground(.hidden)
                    .padding(.top, 6)

                if management.remoteSelected {
                    VStack(alignment: .leading, spacing: 5) {
                        Text(management.busy ? "正在连接…" : management.snapshot != nil ? "已连接远端" : "远端未连接")
                        Text(management.endpoint).font(AppTypography.small).foregroundStyle(.secondary).lineLimit(2)
                    }.padding(12).frame(maxWidth: .infinity, alignment: .leading)
                } else { gatewayFooter }
            }.navigationSplitViewColumnWidth(min: 180, ideal: 194, max: 218)
        } detail: {
            VStack(spacing: 0) {
                if !management.remoteSelected { notices }
                Group {
                    if management.remoteSelected {
                        RemoteSettingsWorkspace(page: sourcesOnly ? .sources : (page ?? .overview))
                            .id(management.targetGeneration)
                    } else {
                        switch page ?? .overview {
                        case .overview: OverviewView(openSources: { page = .sources }, openUsage: { page = .usage }, openConnection: { connectionSection = $0; page = .connections })
                        case .sources: SourcesView()
                        case .models: ModelSettingsView()
                        case .usage: UsageView()
                        case .connections: ConnectionSettingsView(section: $connectionSection)
                        case .general: GeneralSettingsView()
                        }
                    }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }.background(AppPalette.canvas)
                .overlay(alignment: .bottomTrailing) { if !management.remoteSelected { AppFeedbackOverlay() } }
        }.navigationSplitViewStyle(.balanced)
            .background(AppPalette.canvas)
            .font(AppTypography.body).frame(minWidth: 960, minHeight: 700)
            .environment(\.windowIsActive, model.settingsWindowActive)
            .sheet(isPresented: $choosingGateway) { GatewayTargetSheet() }
            .onChange(of: management.remoteSelected) { _, remote in
                page = remote ? .sources : .overview
            }
            .onAppear { if management.remoteSelected { page = .sources } }
            .sheet(item: $model.creditResetRequest, onDismiss: { model.controlResetDismissed() }) { request in
                ResetCreditsSheet(request: request).environmentObject(model)
            }
            .onReceive(NotificationCenter.default.publisher(for: .codexpressNavigate)) { notification in
                if let destination = notification.object as? SettingsPage { page = sourcesOnly ? .sources : destination }
            }
    }

    private var gatewayFooter: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 7) {
                ActivityDot(color: model.running ? .green : .orange, active: model.gatewayHasActivity)
                Text(model.running ? "网关运行中" : "网关未连接").font(AppTypography.caption.weight(.medium))
                Spacer(minLength: 0)
            }
            if model.running || model.syncing {
                HStack(spacing: 6) {
                    if let port = model.status["port"].number, model.running {
                        Text("端口 \(Int(port).formatted(.number.grouping(.never)))").monospacedDigit().foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                    if model.syncing {
                        HStack(spacing: 4) {
                            ProgressView().controlSize(.mini)
                            Text("同步中…").foregroundStyle(.secondary)
                        }
                    }
                }
            }
            if model.installedGatewayUpdate != nil && page != .general {
                Button { page = .general } label: {
                    Label("有可用更新", systemImage: "arrow.down.circle")
                }.buttonStyle(.plain).foregroundStyle(Color.accentColor)
            }
        }.font(AppTypography.small)
            .padding(12).frame(maxWidth: .infinity, alignment: .leading)
            .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9))
            .padding(.horizontal, 12).padding(.vertical, 14)
    }

    private func navigationItem(_ item: SettingsPage) -> some View {
        Label {
            Text(item.rawValue).font(.system(size: 13, weight: page == item ? .medium : .regular))
        } icon: {
            Image(systemName: item.symbol).font(.system(size: 14)).frame(width: 20)
        }.padding(.vertical, 5).tag(item)
    }

    @ViewBuilder private var notices: some View {
        if model.metadataLoadFailed || model.showsPendingSyncWarning || management.message != nil {
            VStack(spacing: 8) {
                if let error = management.message { banner(error, symbol: "exclamationmark.triangle", color: .orange) }
                if model.metadataLoadFailed {
                    banner("来源文件无法读取，已进入只读保护。请从备份修复 sources.json 后重启。", symbol: "lock.shield", color: .orange)
                }
                if model.showsPendingSyncWarning {
                    HStack(spacing: 8) {
                        banner("配置尚未应用到网关，当前仍使用上次设置。", symbol: "arrow.triangle.2.circlepath", color: .orange)
                        Button("重试同步") { model.retry() }.disabled(model.busy)
                    }
                }
            }.padding(.horizontal, AppMetrics.pageInset).padding(.top, 18).frame(minWidth: 700, maxWidth: 1120)
        }
    }

    private func banner(_ text: String, symbol: String, color: Color) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(color)
            Text(text).font(.system(size: 12)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
        }.padding(11).background(color.opacity(0.09), in: RoundedRectangle(cornerRadius: 9))
    }
}
