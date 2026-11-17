import SwiftUI

struct OfficialAccountView: View {
    @EnvironmentObject var model: AppModel
    let source: Source

    var body: some View {
        let live = model.state(source.id)
        let quota = live["quota"]
        let overview = QuotaOverview(quota)
        let extraWindows = overview.windows.filter { window in !overview.mainWindows.contains { $0.id == window.id } }
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Text(quota["plan_type"].string ?? "套餐未知").font(.system(size: 12, weight: .medium))
                    .foregroundStyle(.secondary).fixedSize()
                if let account = live["account_id"].string {
                    Text(account).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                        .truncationMode(.middle).textSelection(.enabled).help("账号／工作区：\(account)")
                }
                Spacer(minLength: 0)
                Button(live["account_id"].string == nil ? "登录…" : "重新登录…") { model.login(source) }
                    .disabled(!model.running || model.busy || model.metadataLoadFailed || model.loginFlow != nil)
                Button { model.refresh(source.id) } label: {
                    Label(model.refreshingQuotas.contains(source.id) ? "刷新中…" : "刷新额度", systemImage: "arrow.clockwise")
                }.disabled(!model.running || source.account_id == nil || model.refreshingQuotas.contains(source.id))
            }.controlSize(.small)
            SettingRow(title: "自动使用重置卡", subtitle: source.account_id == nil ? "登录此账号后可启用。" : "此账号的周额度耗尽时，自动消耗最早到期的可用卡片。更换账号或网络设置后需重新启用。") {
                Toggle("自动使用重置卡", isOn: Binding(get: { source.matchesOfficialStatus(live) && model.settings.weeklyResetEnabled(for: source) }, set: { model.changeAutoResetWeeklyQuota($0, for: source) }))
                    .labelsHidden().toggleStyle(.switch).controlSize(.small)
                    .disabled(!model.canChangeAutoResetWeeklyQuota(for: source))
            }
            if live["account_id"].string == nil && quota["windows"].array.isEmpty {
                Text("登录后查看额度").font(.system(size: 11)).foregroundStyle(.secondary)
            } else if !extraWindows.isEmpty {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 220), spacing: 24)], spacing: 14) {
                    ForEach(extraWindows) { window in quotaWindow(window, referenceBelow: overview.hasShortWindow) }
                }
            }
            if quota != .null {
                Label("更新于 \(timestampLabel(quota["fetched_at"]))" + (quota["stale"].bool == true ? " · 更新失败" : ""), systemImage: "clock")
                    .font(.system(size: 11)).foregroundStyle(quota["stale"].bool == true ? Color.orange : Color.secondary)
            }
            if let error = quota["error"].string { InfoNote(text: error, color: .orange) }
            if model.loginSource == source.id {
                VStack(alignment: .leading, spacing: 10) {
                    Divider()
                    Text(model.loginMessage ?? "等待浏览器授权…").font(.system(size: 13, weight: .medium)).textSelection(.enabled)
                    SecureField("完整回调地址", text: $model.callbackURL).settingsField().disabled(model.busy || model.loginFlow == nil)
                    HStack {
                        Button("完成登录") { model.completeLogin() }.buttonStyle(.borderedProminent)
                            .disabled(model.callbackURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.busy || model.loginFlow == nil || !model.running)
                        Button("取消登录") { model.cancelLogin() }.disabled(model.busy)
                    }.controlSize(.small)
                }.padding(.top, 2)
            }
        }
    }

    private func quotaWindow(_ window: QuotaWindow, referenceBelow: Bool) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(window.title).font(.system(size: 12)).foregroundStyle(.secondary)
            HStack(alignment: .firstTextBaseline, spacing: 5) {
                Text(window.remainingLabel).font(AccountTypography.value).monospacedDigit()
                    .foregroundStyle(AccountLevel.quota(window.remaining).color)
                Text("剩余").font(.system(size: 11)).foregroundStyle(.secondary)
                if !referenceBelow { QuotaReferenceLabel(window: window) }
                Spacer(minLength: 0)
            }
            if referenceBelow { QuotaReferenceLabel(window: window) }
            if let remaining = window.remaining {
                ProgressView(value: remaining, total: 100).tint(AccountLevel.quota(remaining).color).controlSize(.small)
            }
            Text(window.resetLabel).font(.system(size: 11)).foregroundStyle(.secondary)
                .lineLimit(1).help(window.resetLabel)
        }.padding(.vertical, 2)
    }
}

struct OfficialResetControl: View {
    @EnvironmentObject var model: AppModel
    let source: Source
    var compact = false

    var body: some View {
        let live = model.state(source.id)
        let inventory = CreditInventory(live["credits"])
        let exhausted = QuotaOverview(live["quota"]).isExhausted || live["health"].string == "exhausted"
        let quick = exhausted && inventory.hasAvailableCredits
        Group {
            if compact {
                resetButton(quick: true).buttonStyle(.borderedProminent)
                    .accessibilityLabel("重置 \(source.name) 的额度")
            } else {
                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 4) {
                        Image(systemName: "ticket")
                        Text("重置卡")
                        Text(inventory.countLabel).font(.system(size: 18, weight: .semibold)).monospacedDigit()
                            .foregroundStyle(quick ? Color.accentColor : Color.primary)
                        if inventory.snapshot["stale"].bool == true {
                            Image(systemName: "clock").help("卡片信息待更新")
                        }
                    }.font(AppTypography.small).foregroundStyle(.secondary)
                    Group {
                        if quick { resetButton(quick: true).buttonStyle(.borderedProminent) }
                        else { resetButton(quick: false).buttonStyle(.bordered) }
                    }.accessibilityLabel("\(source.name)，重置卡 \(inventory.countLabel)，\(quick ? "立即重置" : "查看卡片")")
                }
            }
        }.controlSize(.small).fixedSize(horizontal: true, vertical: false)
            .disabled(!model.running || source.account_id == nil)
    }

    private func resetButton(quick: Bool) -> some View {
        Button {
            if compact { AppDelegate.current?.showSettings(destination: .sources) }
            model.creditResetRequest = CreditResetRequest(source: source, quickReset: quick)
        } label: {
            Text(compact ? "重置" : quick ? "立即重置…" : "查看卡片…")
                .font(.system(size: compact ? 10 : 12, weight: quick ? .semibold : .regular))
        }
    }
}
