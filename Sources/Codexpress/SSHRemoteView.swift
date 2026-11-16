import SwiftUI

struct SSHRemoteView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var remote = SSHRemoteModel()
    @State private var confirmation: String?
    @State private var revokeID: String?
    var body: some View {
        let pendingProfiles = pendingModelProfiles
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("远端接入") {
                    if remote.busy { ProgressView().controlSize(.small); Text(remote.phase).font(.system(size: 12)).foregroundStyle(.secondary) }
                }
                if !pendingProfiles.isEmpty { modelSyncReminder(pendingProfiles) }
                Picker("远端客户端", selection: Binding(get: { remote.profile.kind }, set: { remote.selectKind($0) })) {
                    ForEach(SSHRemoteKind.allCases, id: \.self) { kind in Text(kind.title).tag(kind) }
                }.pickerStyle(.segmented)
                ingress
                connection
                destination
                if !remote.summary.isEmpty { preview }
                if remote.profile.kind == .codex && !remote.status["peers"].array.isEmpty { peers }
            }.settingsPage()
        }
        .task { await remote.loadSSHConfig() }
        .task(id: "\(model.running):\(remote.profile.kind)") { await remote.refresh(model) }
        .onChange(of: remote.error) { _, error in model.error = error }
        .onChange(of: remote.notice) { _, notice in model.notice = notice }
        .disabled(remote.busy || model.busy)
        .alert(confirmation == "restore" ? "还原接入前设置？" : "撤销此机器的 Key？", isPresented: Binding(get:{confirmation != nil},set:{if !$0 {confirmation=nil}})) {
            Button("取消", role:.cancel){confirmation=nil}
            Button("确认") {
                switch confirmation {
                case "restore":remote.restore(model)
                case "revoke":if let id=revokeID {remote.revoke(id,model:model)}
                default:break
                };confirmation=nil
            }
        } message: {
            Text(confirmationMessage)
        }
    }
    private var ingress: some View {
        SettingsCard("网关连接", symbol: "network", accessory: {
            StatusPill(text: remote.running ? "运行中" : "未连接", color: remote.running ? .green : .secondary)
        }) {
            LabeledContent(remote.profile.kind == .omp ? "与聊天客户端共用端口" : "与本机共用端口") {
                Text(remote.port.isEmpty ? "—" : remote.port)
                    .font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
            }
            if remote.profile.kind == .omp {
                InfoNote(text: "安装 Codexpress 扩展并绑定官方 OMP。远端通过此 Mac 的局域网或 VPN 地址访问来源池。")
            }
            if let old = remote.status["previous_port"].number {
                InfoNote(text: "原远端端口为 \(Int(old))，请重新应用连接以使用当前端口。", color: .orange)
            }
        }
    }

    private var connection: some View {
        SettingsCard("SSH 连接", symbol: "terminal") {
            VStack(alignment: .leading, spacing: 18) {
                HStack(spacing: 12) {
                    Menu {
                        ForEach(remote.configuredHosts, id: \.self) { host in
                            Button(host) { remote.selectConfiguredHost(host, saved: model.settings.sshRemoteProfiles ?? []) }
                        }
                    } label: {
                        Label("从 SSH 配置选择", systemImage: "list.bullet.rectangle")
                    }.disabled(remote.configuredHosts.isEmpty || remote.readingSSHConfig)
                    Spacer(minLength: 0)
                    Text("~/.ssh/config" + (remote.configuredHosts.isEmpty ? "" : " · \(remote.configuredHosts.count) 个主机"))
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                    if remote.readingSSHConfig { ProgressView().controlSize(.small) }
                    Button { Task { await remote.loadSSHConfig() } } label: { Image(systemName: "arrow.clockwise") }
                        .accessibilityLabel("刷新 SSH 配置").help("刷新 SSH 配置").disabled(remote.readingSSHConfig)
                }
                if let message = remote.sshConfigMessage {
                    Text(message).font(.system(size: 12)).foregroundStyle(.secondary)
                }
                if let profiles = model.settings.sshRemoteProfiles?.filter({ $0.kind == remote.profile.kind }), !profiles.isEmpty {
                    HStack {
                        Menu("已保存的机器") {
                            ForEach(profiles) { profile in
                                Button(profile.displayName + (profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision).map { " · " + $0.title } ?? "")) {
                                    remote.select(profile)
                                }
                            }
                        }
                        Spacer()
                        Button("新增机器") { var profile = SSHRemoteProfile(); profile.clientKind = remote.profile.kind; remote.select(profile) }
                    }
                }
                Divider()
                HStack(spacing: 20) {
                    field("服务器／SSH 别名", placeholder: "server 或 192.168.1.20", text: $remote.profile.destination)
                    field("用户名", placeholder: "使用 SSH 配置", text: $remote.profile.username)
                }
                HStack(spacing: 20) {
                    field("SSH 端口", placeholder: "使用 SSH 配置／22", text: $remote.profile.sshPort)
                        .frame(maxWidth: 220)
                    Spacer(minLength: 0)
                }
                field("私钥文件", placeholder: "使用 ssh-agent 或 SSH 配置", text: $remote.profile.identityFile)
                HStack(alignment: .firstTextBaseline, spacing: 20) {
                    Spacer()
                    Button("连接并探测") { remote.discover(model) }
                        .help("SSH 密钥登录 · Python 3")
                }
            }
        }
    }

    private var destination: some View {
        SettingsCard("远端 \(remote.profile.kind.title)", symbol: "desktopcomputer") {
            VStack(alignment: .leading, spacing: 18) {
                if remote.profile.kind == .omp {
                    field("OMP Agent 配置目录", placeholder: "/home/user/.omp/agent", text: Binding(get: { remote.profile.ompAgentDirectory ?? "" }, set: { remote.profile.ompAgentDirectory = $0 }))
                } else {
                    field("Codex 配置目录", placeholder: "/home/user/.codex", text: $remote.profile.codexHome)
                }
                if remote.candidates.count > 1 {
                    Menu("选择发现的目录") {
                        ForEach(remote.candidates, id: \.self) { path in
                            Button(path) {
                                if remote.profile.kind == .omp { remote.profile.ompAgentDirectory = path }
                                else { remote.profile.codexHome = path }
                            }
                        }
                    }
                }
                field("远端可访问的本机地址（应用连接时必填）", placeholder: "自动探测或填写局域网／VPN IP", text: $remote.profile.gatewayHost)
                Divider()
                if remote.profile.kind == .omp {
                    LabeledContent("OMP 版本", value: remote.ompStatus["omp_version"].string ?? "未读取")
                    LabeledContent("扩展", value: remote.hasOMPStatus ? (remote.ompInstalled ? (remote.ompStatus["extension_version"].string ?? "已安装") : "未安装") : "未读取")
                    if let base = remote.ompStatus["base_url"].string {
                        LabeledContent("已保存地址", value: base)
                            .font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                    }
                    if remote.ompAddressChanged { InfoNote(text: "地址或端口已修改，点击更新连接地址后生效。", color: .orange) }
                    if remote.ompStatus["environment_override"].bool == true {
                        InfoNote(text: "远端 CODEXPRESS_BASE_URL 环境变量仍覆盖配置地址，请在启动 OMP 前移除旧值。", color: .orange)
                    }
                    HStack {
                        Button("读取扩展状态") { remote.loadOMPStatus() }
                        Spacer()
                        if remote.ompInstalled {
                            Button("安装／更新扩展") { remote.installOMPExtension(model) }.disabled(!model.running)
                            Button("更新连接地址") { remote.updateOMPAddress(model) }.disabled(!model.running)
                                .buttonStyle(.borderedProminent)
                        } else {
                            Button("安装扩展并连接") { remote.installOMPExtension(model) }.disabled(!model.running)
                                .buttonStyle(.borderedProminent)
                        }
                    }
                    Text("安装后重启远端 OMP，在 /model 中选择 Codexpress 模型。IP 或端口变化后更新连接地址；模型和压缩规则在 OMP 中管理。")
                        .font(AppTypography.caption).foregroundStyle(.secondary)
                } else {
                    Toggle("同时接入已有自定义供应商 ID", isOn: $remote.profile.mapExistingProviders)
                    HStack {
                        Button("还原接入前设置…") { confirmation = "restore" }.disabled(!remote.canRestore)
                        Spacer()
                        Button("预览连接配置") { remote.preview(model) }.disabled(!model.running)
                        Button("应用到远端") { remote.apply(model) }.disabled(!model.running || !remote.canApply(model)).buttonStyle(.borderedProminent)
                    }
                    Text(model.running ? "先预览，再应用并备份。模型设置保存后须重新预览；应用后请重载远端窗口或启动新 Codex 会话，当前进程不会自动加载。" : "可先填写连接或探测目录；预览和应用连接前，请启动本地网关。")
                        .font(AppTypography.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var preview: some View {
        SettingsCard(remote.canApply(model) ? "连接预览" : "上次连接预览", symbol: "doc.text") {
            if !remote.canApply(model) {
                InfoNote(text: "此预览已失效，请按当前模型设置重新预览后再应用。", color: .orange)
            }
            Text(remote.summary).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                .lineSpacing(5).frame(maxWidth: .infinity, alignment: .leading)
            if !remote.aliases.isEmpty {
                Text("接入已有供应商：" + remote.aliases.joined(separator: "、"))
                    .font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
    }

    private var peers: some View {
        SettingsCard("远端访问凭据", symbol: "key") {
            ForEach(remote.status["peers"].array.enumerated().map { UsageItem($0.element, index: $0.offset, key: "id") }) { item in
                let peer = item.value
                HStack(spacing: 20) {
                    VStack(alignment: .leading, spacing: 5) {
                        Text(peer["name"].string ?? "远端机器").font(.system(size: 13, weight: .medium))
                        Text(peer["host"].string ?? "").font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("撤销 Key…") { revokeID = peer["id"].string; confirmation = "revoke" }
                }
            }
        }
    }

    private var confirmationMessage: String {
        if confirmation == "revoke" { return "此机器的旧 Key 将无法发起新请求，远端配置和对话记录保留。" }
        return "将恢复此 Codex 目录首次接入前的配置、文件认证与模型目录，接入后自行修改的这些设置也会还原。当前状态会另行备份。请先结束远端 Codex 任务，还原后重启。"
    }

    private var pendingModelProfiles: [SSHRemoteProfile] {
        (model.settings.sshRemoteProfiles ?? []).filter { $0.modelSettingsSyncStatus(revision: model.modelSettingsRevision) != nil }
    }

    private func modelSyncReminder(_ profiles: [SSHRemoteProfile]) -> some View {
        SettingsCard("远端 Codex 模型设置", symbol: "exclamationmark.arrow.triangle.2.circlepath") {
            InfoNote(text: "以下机器需手动重新预览并应用。仅根据本地接入记录提醒，不会自动 SSH；旧记录未保存模型版本时需核对。应用后请重载远端窗口或启动新会话。", color: .orange)
            ForEach(profiles) { profile in
                HStack {
                    Text(profile.displayName).font(AppTypography.body)
                    Spacer()
                    Text(profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision)?.title ?? "")
                        .font(AppTypography.caption).foregroundStyle(.orange)
                    Button("选择并更新") { remote.select(profile) }
                }
            }
        }
    }

    private func field(_ title: String, placeholder: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 13, weight: .medium))
            TextField(placeholder, text: text).font(.system(size: 13, design: .monospaced)).settingsField()
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
