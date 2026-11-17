import SwiftUI

struct SSHRemoteView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var remote = SSHRemoteModel()
    @State private var confirmation: String?
    @State private var revokeID: String?
    @State private var showsSSHOptions = false
    @State private var showsCodexOptions = false
    private var hasCurrentPreview: Bool { model.running && remote.canApply(model) }

    var body: some View {
        ScrollViewReader { scroll in
            ScrollView {
                VStack(alignment: .leading, spacing: 24) {
                    PageHeading("远端 SSH", subtitle: "选择目标机器，通过 SSH 配置远端 Codex 或 OMP。") {
                        if remote.busy {
                            ProgressView().controlSize(.small)
                            Text(remote.phase).font(AppTypography.caption).foregroundStyle(.secondary)
                        }
                    }
                    connection
                    destination
                    if !remote.summary.isEmpty { preview.id("remote-preview") }
                    if remote.profile.kind == .codex && !remote.status["peers"].array.isEmpty { peers }
                }.settingsPage()
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if let error = remote.error {
                    Divider()
                    HStack(spacing: 12) {
                        InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange).textSelection(.enabled)
                        Spacer(minLength: 0)
                        Button { remote.error = nil } label: { Image(systemName: "xmark") }
                            .buttonStyle(.plain).accessibilityLabel("关闭错误提示")
                    }.padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 10)
                        .frame(maxWidth: 1120).frame(maxWidth: .infinity).background(AppPalette.canvas)
                }
            }
            .onChange(of: hasCurrentPreview) { _, ready in
                if ready { withAnimation(.easeInOut(duration: 0.25)) { scroll.scrollTo("remote-preview", anchor: .center) } }
            }
        }
        .task { await remote.loadSSHConfig() }
        .task(id: "\(model.running):\(remote.profile.kind)") { await remote.refresh(model) }
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
    private var ingressStatus: some View {
        StatusPill(text: !model.running ? "本机网关未运行" : remote.running ? "入口可用" : remote.status == .null ? "状态未读取" : "入口未启动",
                   color: remote.running && model.running ? .green : .secondary)
    }

    private var ingressDetails: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.running && !remote.port.isEmpty {
                LabeledContent(remote.profile.kind == .omp ? "聊天客户端入口端口" : "网关端口") {
                    Text(remote.port).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                }.foregroundStyle(.secondary)
            }
            if remote.profile.kind == .omp {
                Text("远端 OMP 通过此 Mac 的局域网或 VPN 地址访问来源池。")
                    .font(AppTypography.caption).foregroundStyle(.secondary)
            }
            if model.running, let old = remote.status["previous_port"].number {
                InfoNote(text: "原远端端口为 \(Int(old))，请重新应用连接以使用当前端口。", color: .orange)
            }
        }
    }

    private var connection: some View {
        let profiles = (model.settings.sshRemoteProfiles ?? []).filter { $0.kind == remote.profile.kind }
        let pending = pendingModelProfiles
        return SettingsCard("目标机器", symbol: "terminal") {
            VStack(alignment: .leading, spacing: 14) {
                SettingRow(title: "远端客户端") {
                    Picker("远端客户端", selection: Binding(get: { remote.profile.kind }, set: { remote.selectKind($0) })) {
                        ForEach(SSHRemoteKind.allCases, id: \.self) { kind in Text(kind.title).tag(kind) }
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 180)
                }
                Divider()
                HStack(spacing: 12) {
                    if !profiles.isEmpty {
                        Menu("已保存的机器") {
                            ForEach(profiles) { profile in
                                Button(profile.displayName + (profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision).map { " · " + $0.title } ?? "")) {
                                    remote.select(profile)
                                }
                            }
                        }
                    }
                    if !remote.configuredHosts.isEmpty {
                        Menu("SSH 别名") {
                            ForEach(remote.configuredHosts, id: \.self) { host in
                                Button(host) { remote.selectConfiguredHost(host, saved: model.settings.sshRemoteProfiles ?? []) }
                            }
                        }.disabled(remote.readingSSHConfig)
                    }
                    Spacer(minLength: 0)
                    if !pending.isEmpty { modelSyncMenu(pending) }
                    if !remote.profile.destination.isEmpty || !profiles.isEmpty {
                        Button("新建目标") { var profile = SSHRemoteProfile(); profile.clientKind = remote.profile.kind; remote.select(profile) }
                    }
                    if remote.readingSSHConfig { ProgressView().controlSize(.small) }
                }
                field("服务器或 SSH 别名", placeholder: "server 或 192.168.1.20", text: $remote.profile.destination)
                DisclosureGroup("SSH 选项", isExpanded: $showsSSHOptions) {
                    VStack(alignment: .leading, spacing: 12) {
                        HStack(spacing: 20) {
                            field("用户名", placeholder: "使用 SSH 配置", text: $remote.profile.username)
                            field("SSH 端口", placeholder: "使用 SSH 配置或 22", text: $remote.profile.sshPort)
                        }
                        field("私钥文件", placeholder: "使用 ssh-agent 或 SSH 配置", text: $remote.profile.identityFile)
                        if let message = remote.sshConfigMessage {
                            Text(message).font(AppTypography.caption).foregroundStyle(.secondary)
                        }
                        Button("刷新 ~/.ssh/config") { Task { await remote.loadSSHConfig() } }.disabled(remote.readingSSHConfig)
                    }.padding(.top, 12)
                }
                SettingRow(title: "探测远端目录", subtitle: "需可用的 SSH 密钥登录和 Python 3。") {
                    Button("连接并探测") { remote.discover(model) }.buttonStyle(.borderedProminent)
                        .disabled(remote.profile.destination.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
        }
    }

    private var destination: some View {
        SettingsCard("配置远端 \(remote.profile.kind.title)", symbol: "desktopcomputer", accessory: { ingressStatus }) {
            VStack(alignment: .leading, spacing: 14) {
                if !model.running {
                    InfoNote(text: "可先探测目录；预览或应用连接前，请在设置中启动本机网关。")
                }
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
                field("远端可访问的本机地址", placeholder: "应用时必填；探测或填写局域网／VPN IP", text: $remote.profile.gatewayHost)
                ingressDetails
                Divider()
                if remote.profile.kind == .omp {
                    LabeledContent("OMP 版本", value: remote.ompStatus["omp_version"].string ?? "未读取")
                    LabeledContent("扩展", value: remote.hasOMPStatus ? (remote.ompInstalled ? (remote.ompStatus["extension_version"].string ?? "已安装") : "未安装") : "未读取")
                    if let base = remote.ompStatus["base_url"].string {
                        LabeledContent("已应用的地址", value: base)
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
                    Text("安装后重启远端 OMP，在 /model 中选择 Codexpress 模型。模型和压缩规则在 OMP 中管理。")
                        .font(AppTypography.caption).foregroundStyle(.secondary)
                } else {
                    if let sync = remote.profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision) {
                        InfoNote(text: "\(sync.title)。请重新预览并应用，不会自动同步。", symbol: "arrow.triangle.2.circlepath", color: .orange)
                    }
                    SettingRow(title: "预览来源池配置", subtitle: "预览不会写入远端；应用时会先备份。") {
                        Button("预览连接配置") { remote.preview(model) }.buttonStyle(.borderedProminent)
                            .disabled(!model.running)
                    }
                    DisclosureGroup("连接选项与还原", isExpanded: $showsCodexOptions) {
                        VStack(alignment: .leading, spacing: 12) {
                            Toggle("同时接入已有自定义供应商 ID", isOn: $remote.profile.mapExistingProviders)
                            Button("还原接入前设置…") { confirmation = "restore" }.disabled(!remote.canRestore)
                        }.padding(.top, 12)
                    }
                }
            }
        }
    }

    private var preview: some View {
        SettingsCard(hasCurrentPreview ? "连接预览" : "上次连接预览", symbol: "doc.text") {
            if !hasCurrentPreview {
                InfoNote(text: "此预览仅供参考。如需应用，请按当前设置重新预览。")
            }
            Text(remote.summary).font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                .lineSpacing(5).frame(maxWidth: .infinity, alignment: .leading)
            if !remote.aliases.isEmpty {
                Text("接入已有供应商：" + remote.aliases.joined(separator: "、"))
                    .font(.system(size: 12)).foregroundStyle(.secondary)
            }
            if hasCurrentPreview {
                SettingRow(title: "写入远端配置", subtitle: "应用后重载远端窗口或启动新的 Codex 会话。") {
                    Button("应用到远端") { remote.apply(model) }.buttonStyle(.borderedProminent)
                }
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

    private func modelSyncMenu(_ profiles: [SSHRemoteProfile]) -> some View {
        Menu {
            ForEach(profiles) { profile in
                Button(profile.displayName + (profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision).map { " · " + $0.title } ?? "")) {
                    remote.selectKind(profile.kind)
                    remote.select(profile)
                }
            }
        } label: {
            Label("\(profiles.count) 台待处理", systemImage: "arrow.triangle.2.circlepath")
        }
        .help("选择需重新预览并应用的远端 Codex 机器；不会自动发起 SSH")
    }

    private func field(_ title: String, placeholder: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 13, weight: .medium))
            TextField(placeholder, text: text).font(.system(size: 13, design: .monospaced)).settingsField()
                .accessibilityLabel(title)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
