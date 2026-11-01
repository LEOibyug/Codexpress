import SwiftUI
import AppKit

struct MenuView: View {
    @EnvironmentObject var model: AppModel
    @State private var windowActive = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            connection
            if model.sources.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    Text("暂无来源").font(.system(size: 12, weight: .medium))
                    Button { showSettings(.sources) } label: { Label("添加来源", systemImage: "plus") }
                        .buttonStyle(.borderedProminent).controlSize(.small)
                }.frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 6)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(model.sources.prefix(4).enumerated()), id: \.element.id) { index, source in
                        if index > 0 { Divider().padding(.leading, 13) }
                        sourceRow(source)
                    }
                }.padding(.horizontal, 10)
                    .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            }
            if model.pendingSync {
                Label("配置尚未应用，当前使用上次设置。", systemImage: "clock")
                    .font(.system(size: 10)).foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            footer
        }.padding(14).frame(width: 340)
            .background(.regularMaterial)
            .environment(\.windowIsActive, windowActive)
            .background(AccountWindowOpenRefresh(refresh: {
                Task { await model.refreshAccountInfo(sourceIDs: Array(model.sources.prefix(4)).map(\.id)) }
            }, activityChanged: { windowActive = $0 }))
            .task(id: model.running) { await model.refreshAccountInfo(sourceIDs: Array(model.sources.prefix(4)).map(\.id)) }
            .overlay(alignment: .bottomLeading) { AppFeedbackOverlay(compact: true) }
    }

    private var header: some View {
        HStack(spacing: 9) {
            Image(systemName: "arrow.triangle.branch")
                .font(.system(size: 15, weight: .medium)).foregroundStyle(.secondary)
                .frame(width: 28, height: 28)
                .background(.quaternary.opacity(0.45), in: RoundedRectangle(cornerRadius: 7))
            VStack(alignment: .leading, spacing: 3) {
                Text("Codexpress").font(.system(size: 13, weight: .semibold))
                Text(model.routingMode == "auto" ? "来源池 · 自动切换" : "固定来源 · \(model.fixedName)")
                    .font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 8)
            StatusPill(text: model.running ? "运行中" : "未连接", color: model.running ? .green : .orange, compact: true)
        }
    }

    private var connection: some View {
        HStack(alignment: .center, spacing: 8) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Codex 连接").font(.system(size: 10)).foregroundStyle(.secondary)
                Text(model.connectionMode == "official" ? "官方连接" : model.connectionMode == "gateway" ? "来源池连接" : "尚未接入")
                    .font(.system(size: 11, weight: .medium))
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 4) {
                if model.connectionMode == "official" {
                    Button { model.switchCodexConnection(.gateway) } label: {
                        Label(model.codexConnectionPhase ?? "使用 API 模式", systemImage: "arrow.triangle.branch")
                    }.disabled(model.busy || !model.running || model.metadataLoadFailed)
                        .help("重启 Codex，使用已配置的来源池")
                } else {
                    DirectOfficialLoginButton()
                }
            }.buttonStyle(.bordered).controlSize(.small)
        }.padding(.horizontal, 2).padding(.vertical, 1)
    }

    private func sourceRow(_ source: Source) -> some View {
        HStack(alignment: .top, spacing: 8) {
            ActivityDot(color: source.enabled && model.state(source.id)["health"].string == "ready" ? .green : .secondary,
                        active: model.sourceHasActivity(source.id)).padding(.top, 5)
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 5) {
                    Button { showSettings(.sources) } label: {
                        Text(source.name).font(.system(size: 12, weight: .semibold)).lineLimit(1)
                    }.buttonStyle(.plain).help("在来源池中管理 \(source.name)")
                    if source.official && model.refreshingQuotas.contains(source.id) {
                        ProgressView().controlSize(.mini).help("正在更新额度")
                    } else if source.official && model.state(source.id)["quota"]["stale"].bool == true {
                        Image(systemName: "clock").font(.system(size: 10)).foregroundStyle(.orange)
                            .help("更新失败，显示上次获取的额度：\(timestampLabel(model.state(source.id)["quota"]["fetched_at"]))")
                    }
                    Spacer(minLength: 3)
                    SourceRoutingActions(source: source, compact: true).fixedSize()
                    Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用")
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                        .fixedSize().frame(minWidth: 24, alignment: .trailing)
                }
                if source.official { compactQuota(model.state(source.id)["quota"]) }
                else { APIBalanceView(sourceID: source.id, compact: true) }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }.padding(.vertical, 9)
    }

    private func compactQuota(_ quota: JSONValue) -> some View {
        HStack(alignment: .top, spacing: 14) {
            ForEach(QuotaOverview(quota).mainWindows) { window in
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        Text(window.title).font(.system(size: 10)).foregroundStyle(.secondary)
                        Text(window.remainingLabel).font(AccountTypography.compactValue)
                            .monospacedDigit().foregroundStyle(AccountLevel.quota(window.remaining).color)
                            .accessibilityLabel("\(window.title)剩余 \(window.remainingLabel)")
                    }
                    Text(resetLabel(window)).font(.system(size: 10)).foregroundStyle(.secondary)
                        .lineLimit(1).help(window.resetLabel)
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private func resetLabel(_ window: QuotaWindow) -> String {
        guard let resetAt = window.resetAt else { return "重置时间未知" }
        return Date(timeIntervalSince1970: resetAt)
            .formatted(.dateTime.month().day().hour().minute().locale(Locale(identifier: "zh_CN"))) + " 重置"
    }

    private var footer: some View {
        VStack(spacing: 9) {
            Divider()
            HStack(spacing: 10) {
                Button { showSettings() } label: { Label("打开主窗口", systemImage: "macwindow") }
                if !model.running { Button("重试启动") { model.retry() } }
                Spacer(minLength: 4)
                Button { showSettings(.usage) } label: { Image(systemName: "chart.bar.xaxis") }
                    .help("查看用量与连接日志").accessibilityLabel("查看用量")
                Button { showSettings(.general) } label: { Image(systemName: "gearshape") }
                    .help("应用设置").accessibilityLabel("应用设置")
                Button("退出") { NSApp.terminate(nil) }.keyboardShortcut("q").foregroundStyle(.secondary)
            }.buttonStyle(.borderless).font(.system(size: 11))
        }
    }

    private func showSettings(_ destination: SettingsPage? = nil) { AppDelegate.current?.showSettings(destination: destination) }
}

// MenuBarExtra can retain its hosting view while hidden, so a SwiftUI task
// alone does not reliably run on every reopening of the existing panel.
private struct AccountWindowOpenRefresh: NSViewRepresentable {
    var refresh: () -> Void
    var activityChanged: (Bool) -> Void = { _ in }
    func makeNSView(context: Context) -> ObserverView { let view = ObserverView(); view.refresh = refresh; view.activityChanged = activityChanged; return view }
    func updateNSView(_ view: ObserverView, context: Context) { view.refresh = refresh; view.activityChanged = activityChanged }

    final class ObserverView: NSView {
        var refresh: () -> Void = {}
        var activityChanged: (Bool) -> Void = { _ in }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            NotificationCenter.default.removeObserver(self)
            if let window {
                NotificationCenter.default.addObserver(self, selector: #selector(didOpen(_:)), name: NSWindow.didBecomeKeyNotification, object: window)
                for name in [NSWindow.didResignKeyNotification, NSWindow.didChangeOcclusionStateNotification] {
                    NotificationCenter.default.addObserver(self, selector: #selector(activity(_:)), name: name, object: window)
                }
            }
            DispatchQueue.main.async { [weak self] in self?.reportActivity() }
        }
        @objc private func didOpen(_ notification: Notification) { refresh(); activity(notification) }
        @objc private func activity(_ notification: Notification) { reportActivity() }
        private func reportActivity() {
            let active = window.map { $0.isKeyWindow && $0.isVisible && $0.occlusionState.contains(.visible) } ?? false
            activityChanged(active)
        }
        deinit { NotificationCenter.default.removeObserver(self) }
    }
}
