import SwiftUI

struct OverviewView: View {
    @EnvironmentObject var model: AppModel
    let openSources: () -> Void
    let openUsage: () -> Void
    let openConnection: (ConnectionSection) -> Void
    private var usageSnapshot: JSONValue { model.cachedUsageAnalytics(UsageQuery.last(days: 1))?.value ?? .null }
    @State private var usageError: String?
    private var totals: UsageTotals { UsageTotals(rows: usageSnapshot["groups"].array) }
    private var available: Int {
        model.sources.filter { $0.enabled && model.state($0.id)["admission"]["accepting_new_conversations"].bool == true }.count
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: AppMetrics.sectionSpacing) {
                PageHeading("概览", subtitle: Date().formatted(.dateTime.month(.wide).day().weekday(.wide))) {
                    Button(action: openUsage) { Label("查看用量", systemImage: "chart.bar.xaxis") }
                }
                connection
                HStack(spacing: 14) {
                    metric("今日费用", symbol: "dollarsign.circle", value: usageMoney(totals.cost, estimated: totals.costEstimated), detail: totals.cost == nil ? "暂无已计价请求" : totals.costEstimated ? "含参考价估算" : "上游实际扣费")
                    metric("今日 Token", symbol: "number", value: usageSnapshot == .null ? "—" : usageCount(totals.total, estimated: totals.estimated), detail: usageSnapshot == .null ? "等待用量数据" : "\(usageCount(totals.sum("requests"))) 次请求 · 入/出 \(totals.inputOutputRatio)")
                    metric("进行中的请求", symbol: "waveform.path", value: model.running ? usageCount(model.status["inflight"].number ?? 0) : "—", detail: model.running ? "\(available) / \(model.sources.count) 个来源可用" : "网关未连接")
                }
                if let usageError { InfoNote(text: usageError, symbol: "clock", color: .orange) }
                SettingsCard("来源状态", symbol: "server.rack", accessory: {
                    if !model.sources.isEmpty { Button("管理来源池", action: openSources) }
                }) {
                    if model.sources.isEmpty {
                        SettingRow(title: "还没有来源", subtitle: "添加官方账号、OpenAI API Platform、Google AI Studio 或 Responses API 后即可接收请求。") {
                            Button("添加来源", action: openSources).buttonStyle(.borderedProminent)
                        }
                    } else {
                        VStack(spacing: 0) {
                            ForEach(Array(model.sources.prefix(4).enumerated()), id: \.element.id) { index, source in
                                if index > 0 { Divider() }
                                accountRow(source, index: index).padding(.vertical, 14)
                            }
                            if model.sources.count > 4 {
                                Divider()
                                Button("查看全部 \(model.sources.count) 个来源", action: openSources)
                                    .buttonStyle(.link).frame(maxWidth: .infinity, alignment: .leading).padding(.top, 12)
                            }
                        }
                    }
                }
                SettingsCard("客户端与服务", symbol: "point.3.connected.trianglepath.dotted") {
                    LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 10), count: 3), spacing: 10) {
                        entry(.omp, symbol: "terminal")
                        entry(.chatbot, symbol: "bubble.left.and.bubble.right")
                        entry(.remote, symbol: "desktopcomputer")
                        entry(.images, symbol: "photo")
                        entry(.control, symbol: "curlybraces")
                    }
                }
            }.settingsPage()
        }
        .task(id: "\(model.running):\(model.settingsWindowActive)") {
            guard model.running, model.settingsWindowActive else { return }
            await model.refreshAccountInfo()
            var refresh = false
            while !Task.isCancelled {
                let query = UsageQuery.last(days: 1)
                do {
                    _ = try await model.usageAnalytics(query, refresh: refresh)
                    guard !Task.isCancelled else { break }
                    usageError = nil
                } catch {
                    guard !Task.isCancelled else { break }
                    usageError = usageSnapshot == .null ? "今日用量暂时无法读取。" : "用量更新失败，显示上次统计。"
                }
                refresh = true
                do { try await Task.sleep(for: .seconds(15)) } catch { break }
            }
        }
    }

    private var connection: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top, spacing: 14) {
                Image(systemName: model.connectionMode == "official" ? "person.crop.circle" : "arrow.triangle.branch")
                    .font(.system(size: 21, weight: .medium)).foregroundStyle(Color.accentColor)
                    .frame(width: 42, height: 42)
                    .background(Color.accentColor.opacity(0.12), in: RoundedRectangle(cornerRadius: 11))
                VStack(alignment: .leading, spacing: 5) {
                    Text(model.connectionMode == "official" ? "Codex 直接官方登录" : model.connectionMode == "gateway" ? (model.running ? "Codex 已连接来源池" : "Codex 配置为来源池") : "尚未接入 Codex")
                        .font(.system(size: 17, weight: .semibold))
                    if model.connectionMode == "gateway" {
                        Text(model.routingMode == "auto" ? "自动切换" : "固定来源：\(model.fixedName)")
                            .font(AppTypography.caption).foregroundStyle(.secondary)
                    } else if model.connectionMode == "official" {
                        Text("Codex 请求不经过网关，其它客户端仍可使用来源池。")
                            .font(AppTypography.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
                StatusPill(text: model.running ? "网关运行中" : "网关未连接", color: model.running ? .green : .orange, active: model.gatewayHasActivity)
            }
            HStack(spacing: 12) {
                if !model.running {
                    Button("启动网关") { model.retry() }.buttonStyle(.borderedProminent).disabled(model.busy)
                    Button("连接设置") { openConnection(.codex) }
                } else {
                    Button("连接设置") { openConnection(.codex) }.buttonStyle(.borderedProminent)
                }
                Spacer()
                if model.connectionMode == "official" {
                    Button("使用来源池") { model.switchCodexConnection(.gateway) }
                        .disabled(model.busy || !model.running || model.metadataLoadFailed)
                } else { DirectOfficialLoginButton() }
            }
        }.padding(20).cardSurface()
    }

    private func metric(_ title: String, symbol: String, value: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(title, systemImage: symbol).font(AppTypography.caption).foregroundStyle(.secondary)
            Text(value).font(.system(size: 27, weight: .semibold)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.72)
            Text(detail).font(AppTypography.small).foregroundStyle(.secondary)
        }.padding(17).frame(maxWidth: .infinity, alignment: .leading).cardSurface()
    }

    private func accountRow(_ source: Source, index: Int) -> some View {
        HStack(spacing: 12) {
            Text("\(index + 1)").font(.system(size: 12, weight: .medium)).foregroundStyle(.secondary).frame(width: 18)
            ActivityDot(color: source.enabled ? (model.state(source.id)["health"].string == "ready" ? .green : .orange) : .secondary, active: model.sourceHasActivity(source.id))
            VStack(alignment: .leading, spacing: 4) {
                Text(source.name).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                Text(source.enabled ? healthLabel(model.state(source.id)["health"].string) : "已停用")
                    .font(AppTypography.small).foregroundStyle(.secondary)
            }
            Spacer(minLength: 12)
            if source.official {
                let overview = QuotaOverview(model.state(source.id)["quota"])
                HStack(alignment: .top, spacing: 18) {
                    ForEach(overview.mainWindows) { window in
                        VStack(alignment: .trailing, spacing: 3) {
                            Text(window.title).font(AppTypography.small).foregroundStyle(.secondary)
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text(window.remainingLabel).font(.system(size: 18, weight: .semibold)).monospacedDigit()
                                    .foregroundStyle(AccountLevel.quota(window.remaining).color).help(window.resetLabel)
                                if !overview.hasShortWindow { QuotaReferenceLabel(window: window) }
                            }
                            if overview.hasShortWindow { QuotaReferenceLabel(window: window) }
                        }
                    }
                    OfficialResetControl(source: source)
                }
            } else if source.supportsBalanceQuery {
                APIBalanceView(sourceID: source.id).fixedSize(horizontal: true, vertical: false)
            } else {
                Text(source.models.isEmpty ? (source.discoversModels ? "尚未获取模型" : "未限定模型") : "\(source.models.count) 个模型")
                    .font(AppTypography.small).foregroundStyle(.secondary)
            }
        }
    }

    private func entry(_ section: ConnectionSection, symbol: String) -> some View {
        Button { openConnection(section) } label: {
            HStack(spacing: 11) {
                Image(systemName: symbol).font(.system(size: 15)).foregroundStyle(Color.accentColor).frame(width: 22)
                Text(section.rawValue).font(.system(size: 13, weight: .medium))
                Spacer(minLength: 0)
                Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).foregroundStyle(.tertiary)
            }.padding(12).frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 8)).contentShape(Rectangle())
        }.buttonStyle(.plain)
    }
}
