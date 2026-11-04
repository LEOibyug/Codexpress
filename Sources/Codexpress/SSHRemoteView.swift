import SwiftUI

struct SSHRemoteView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var remote = SSHRemoteModel()
    @State private var confirmation: String?
    @State private var revokeID: String?
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                PageHeading("远端接入") {
                    if remote.busy { ProgressView().controlSize(.small); Text(remote.phase).font(.system(size: 12)).foregroundStyle(.secondary) }
                }
                Picker("远端客户端", selection: Binding(get: { remote.profile.kind }, set: { remote.selectKind($0) })) {
                    ForEach(SSHRemoteKind.allCases, id: \.self) { kind in Text(kind.title).tag(kind) }
                }.pickerStyle(.segmented)
                ingress
                connection
                destination
                if remote.profile.kind == .omp { ompConfiguration }
                if !remote.summary.isEmpty { preview }
                if remote.profile.kind == .codex && !remote.status["peers"].array.isEmpty { peers }
            }.settingsPage()
        }
        .task { await remote.loadSSHConfig(); await remote.detectLocalOMPDirectory() }
        .task(id: "\(model.running):\(remote.profile.kind)") { await remote.refresh(model) }
        .onChange(of: remote.error) { _, error in model.error = error }
        .onChange(of: remote.notice) { _, notice in model.notice = notice }
        .disabled(remote.busy || model.busy || !model.running)
        .alert(confirmation == "apply" ? "应用远端连接？" : confirmation == "restore" ? "还原接入前设置？" : "撤销此机器的 Key？", isPresented: Binding(get:{confirmation != nil},set:{if !$0 {confirmation=nil}})) {
            Button("取消", role:.cancel){confirmation=nil}
            Button("确认") {
                switch confirmation {
                case "apply":remote.apply(model)
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
                InfoNote(text: "OMP 使用聊天客户端共享 Key，不是每台机器独立的 Key。请先启用聊天客户端接入；更换共享 Key 会影响所有已接入的 OMP。此 HTTP 入口不开放管理 API。")
            }
            if let old = remote.status["previous_port"].number {
                InfoNote(text: "原远端端口为 \(Int(old))，请重新预览并应用连接以使用当前端口。", color: .orange)
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
                                Button(profile.username.isEmpty ? profile.destination : "\(profile.username)@\(profile.destination)") {
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
                    field("默认模型（预览连接时必填）", placeholder: "模型 ID", text: Binding(get: { remote.profile.ompDefaultModel ?? "" }, set: { remote.profile.ompDefaultModel = $0 }))
                    Text("新安装尚无配置文件时，会在应用后自动创建。OpenAI 推荐设置仅适用于与官方 Responses 一致的上游。")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
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
                field(remote.profile.kind == .omp ? "远端可访问的本机地址（预览连接时必填）" : "远端可访问的本机地址", placeholder: "自动探测或填写局域网／VPN IP", text: $remote.profile.gatewayHost)
                Divider()
                if remote.profile.kind == .codex { Toggle("同时接入已有自定义供应商 ID", isOn: $remote.profile.mapExistingProviders) }
                HStack {
                    Button("还原接入前设置…") { confirmation = "restore" }.disabled(!remote.canRestore)
                    Spacer()
                    Button(remote.profile.kind == .omp && !remote.hasOMPConfiguration ? "读取远端 OMP 配置" : "预览连接配置") { remote.preview(model) }.buttonStyle(.borderedProminent)
                }
            }
        }
    }

    private var ompConfiguration: some View {
        VStack(alignment: .leading, spacing: 20) {
            SettingsCard("远端 OMP 配置", symbol: "slider.horizontal.3") {
                HStack {
                    Text(remote.hasOMPConfiguration ? "编辑当前远端配置" : "填写连接信息后，先读取远端配置。")
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("重新读取远端配置") { remote.loadOMPConfiguration(model) }
                }
                field("导入用的本地 OMP 配置目录", placeholder: "~/.omp/agent", text: $remote.localOMPDirectory)
                HStack(spacing: 12) {
                    Button("应用本地配置") { remote.importLocalOMPConfiguration(model) }
                    Button("应用 OpenAI 推荐配置") { remote.useOpenAIOMPConfiguration() }
                }.disabled(!remote.hasOMPConfiguration)
                Text("仅填入压缩与模型覆盖，不复制本机网关地址、Key、工具或扩展。自定义压缩端点将由远端访问。修改后预览，再备份并应用；不会改动本地配置。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
            if remote.hasOMPConfiguration {
                OMPCompactionSettingsView(
                    draft: $remote.ompDraft,
                    configuration: remote.ompConfiguration,
                    configurationRevision: remote.ompConfigurationRevision,
                    agentDirectory: remote.profile.directory,
                    canEdit: !remote.busy && model.running && !model.busy,
                    draftRevisionRequest: remote.ompDraftRefresh
                )
                HStack {
                    Spacer()
                    Button("预览连接配置") { remote.preview(model) }.buttonStyle(.borderedProminent)
                }
            }
        }
    }

    private var preview: some View {
        SettingsCard("连接预览", symbol: "doc.text") {
            Text(remote.summary).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                .lineSpacing(5).frame(maxWidth: .infinity, alignment: .leading)
            if !remote.aliases.isEmpty {
                Text("接入已有供应商：" + remote.aliases.joined(separator: "、"))
                    .font(.system(size: 12)).foregroundStyle(.secondary)
            }
            Divider()
            HStack(alignment: .firstTextBaseline, spacing: 20) {
                if !remote.canApply(model) {
                    Text("设置已变化，请重新预览。").font(.system(size: 12)).foregroundStyle(.secondary)
                } else if !remote.running {
                    Text(remote.profile.kind == .omp ? "请先启用聊天客户端接入。" : "请先启动本地网关。").font(.system(size: 12)).foregroundStyle(.secondary)
                }
                Spacer()
                Button("备份并应用…") { confirmation = "apply" }
                    .disabled(!remote.canApply(model) || !remote.running).buttonStyle(.borderedProminent)
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
        if remote.profile.kind == .omp {
            return confirmation == "apply"
                ? "将从远端验证聊天客户端入口和共享 Key，备份 OMP 配置后应用连接。不会读取认证、数据库或历史；请先结束远端 OMP 任务，应用后重启 OMP。"
                : "将撤回此 OMP 目录中的 Codexpress 供应商、默认模型和已应用的 OMP 配置，保留无关修改。当前配置会另行备份；请先结束远端 OMP 任务，还原后重启。"
        }
        return confirmation == "apply" ? "将先从远端验证网关连通性，再备份配置、文件认证与模型目录，并应用本机已保存的模型压缩规则。请先结束远端 Codex 的运行任务，应用后重启远端 Codex。" : confirmation == "restore" ? "将恢复此 Codex 目录首次接入前的配置、文件认证与模型目录，接入后自行修改的这些设置也会还原。当前状态会另行备份。请先结束远端 Codex 任务，还原后重启。" : "此机器的旧 Key 将无法发起新请求，远端配置和对话记录保留。"
    }

    private func field(_ title: String, placeholder: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 13, weight: .medium))
            TextField(placeholder, text: text).font(.system(size: 13, design: .monospaced)).settingsField()
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
