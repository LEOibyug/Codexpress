import SwiftUI

struct SSHRemoteView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var remote = SSHRemoteModel()
    @State private var confirmation: String?
    @State private var revokeID: String?
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                PageHeading("远端接入") {
                    if remote.busy { ProgressView().controlSize(.small); Text(remote.phase).font(.system(size: 11)).foregroundStyle(.secondary) }
                }
                ingress
                connection
                destination
                if !remote.summary.isEmpty { preview }
                if !remote.status["peers"].array.isEmpty { peers }
            }.settingsPage()
        }
        .task { await remote.loadSSHConfig(); await remote.refresh(model) }
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
            Text(confirmation == "apply" ? "将先从远端验证网关连通性，再备份配置、文件认证与模型目录，并应用本机已保存的模型压缩规则。请先结束远端 Codex 的运行任务，应用后重启远端 Codex。" : confirmation == "restore" ? "将恢复此 Codex 目录首次接入前的配置、文件认证与模型目录，接入后自行修改的这些设置也会还原。当前状态会另行备份。请先结束远端 Codex 任务，还原后重启。" : "此机器的旧 Key 将无法发起新请求，远端配置和对话记录保留。")
        }
    }
    private var ingress: some View {
        SettingsCard("网关连接", symbol:"network", accessory:{ StatusPill(text:model.running ? "运行中" : "未连接",color:model.running ? .green : .secondary) }) {
            HStack(spacing:10) {
                Text("与本机共用端口").font(.system(size:12))
                Spacer()
                Text(remote.port.isEmpty ? "—" : remote.port).font(.system(size:12, design:.monospaced)).textSelection(.enabled)
            }
            Text("随本地网关启动。远端使用独立机器 Key；仅用于可信局域网或 VPN。更改端口后需重新应用远端配置。").font(.system(size:11)).foregroundStyle(.secondary)
            if let old = remote.status["previous_port"].number { InfoNote(text:"原远端端口为 \(Int(old))，请重新预览并应用连接以使用当前端口。", color:.orange) }
        }
    }
    private var connection: some View {
        SettingsCard("SSH 连接",symbol:"terminal") {
            VStack(alignment:.leading,spacing:12) {
                HStack(spacing: 10) {
                    Menu {
                        ForEach(remote.configuredHosts, id: \.self) { host in
                            Button(host) { remote.selectConfiguredHost(host, saved: model.settings.sshRemoteProfiles ?? []) }
                        }
                    } label: { Label("从 SSH 配置选择", systemImage: "list.bullet.rectangle") }
                        .disabled(remote.configuredHosts.isEmpty || remote.readingSSHConfig)
                    Text("~/.ssh/config" + (remote.configuredHosts.isEmpty ? "" : " · \(remote.configuredHosts.count) 个主机"))
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                    Spacer(minLength: 0)
                    if remote.readingSSHConfig { ProgressView().controlSize(.small) }
                    Button { Task { await remote.loadSSHConfig() } } label: { Image(systemName: "arrow.clockwise") }
                        .accessibilityLabel("刷新 SSH 配置").help("刷新 SSH 配置").disabled(remote.readingSSHConfig)
                }
                if let message = remote.sshConfigMessage { Text(message).font(.system(size: 11)).foregroundStyle(.secondary) }
                if let profiles=model.settings.sshRemoteProfiles, !profiles.isEmpty {
                    HStack {
                        Menu("已保存的机器") { ForEach(profiles){p in Button(p.username.isEmpty ? p.destination : "\(p.username)@\(p.destination)"){remote.select(p)} } }
                        Spacer();Button("新增机器"){remote.select(SSHRemoteProfile())}
                    }
                }
                HStack(spacing:12) {
                    field("服务器／SSH 别名",placeholder:"server 或 192.168.1.20",text:$remote.profile.destination)
                    field("用户名",placeholder:"使用 SSH 配置",text:$remote.profile.username)
                }
                HStack(spacing:12) {
                    field("SSH 端口",placeholder:"使用 SSH 配置／22",text:$remote.profile.sshPort)
                    field("私钥文件",placeholder:"使用 ssh-agent 或 SSH 配置",text:$remote.profile.identityFile)
                }
                HStack {
                    Text("需预先确认主机指纹并完成密钥登录，远端需 Python 3。").font(.system(size:11)).foregroundStyle(.secondary)
                    Spacer();Button("连接并探测"){remote.discover(model)}
                }
            }
        }
    }
    private var destination: some View {
        SettingsCard("远端 Codex",symbol:"desktopcomputer") {
            VStack(alignment:.leading,spacing:12) {
                field("Codex 配置目录",placeholder:"/home/user/.codex",text:$remote.profile.codexHome)
                if remote.candidates.count>1 { Menu("选择发现的目录"){ForEach(remote.candidates,id:\.self){path in Button(path){remote.profile.codexHome=path}}} }
                field("远端可访问的本机地址",placeholder:"自动探测或填写局域网／VPN IP",text:$remote.profile.gatewayHost)
                Text("SSH 探测地址仅供参考；应用前会从远端实际验证，不通时不会写入配置。").font(.system(size:11)).foregroundStyle(.secondary)
                Toggle("同时接入已有自定义供应商 ID",isOn:$remote.profile.mapExistingProviders).font(.system(size:12))
                if remote.profile.mapExistingProviders { Text("旧对话可能带有原来源的加密状态，接入后可能触发上下文降级。").font(.system(size:11)).foregroundStyle(.secondary) }
                HStack {
                    Button("还原接入前设置…"){confirmation="restore"}.disabled(!remote.canRestore)
                    Spacer();Button("预览连接配置"){remote.preview(model)}.buttonStyle(.borderedProminent)
                }
            }
        }
    }
    private var preview: some View {
        SettingsCard("连接预览",symbol:"doc.text") {
            Text(remote.summary).font(.system(size:11,design:.monospaced)).textSelection(.enabled)
            if !remote.aliases.isEmpty { Text("接入已有供应商："+remote.aliases.joined(separator:"、")).font(.system(size:11)).foregroundStyle(.secondary) }
            HStack {
                Text(!remote.canApply(model) ? "设置已变化，请重新预览。" : !remote.running ? "请先启动本地网关。" : "配置、文件认证与模型目录将在远端备份。").font(.system(size:11)).foregroundStyle(.secondary)
                Spacer();Button("备份并应用…"){confirmation="apply"}.disabled(!remote.canApply(model) || !remote.running).buttonStyle(.borderedProminent)
            }
        }
    }
    private var peers: some View {
        SettingsCard("远端访问凭据",symbol:"key") {
            ForEach(remote.status["peers"].array.enumerated().map { UsageItem($0.element,index:$0.offset,key:"id") }){item in
                let peer = item.value
                HStack {
                    VStack(alignment:.leading,spacing:3){Text(peer["name"].string ?? "远端机器");Text(peer["host"].string ?? "").font(.system(size:11)).foregroundStyle(.secondary)}
                    Spacer();Button("撤销 Key…"){revokeID=peer["id"].string;confirmation="revoke"}
                }
            }
        }
    }
    private func field(_ title:String,placeholder:String,text:Binding<String>)->some View {
        VStack(alignment:.leading,spacing:6){Text(title).font(.system(size:11)).foregroundStyle(.secondary);TextField(placeholder,text:text).font(.system(size:12,design:.monospaced)).settingsField()}.frame(maxWidth:.infinity,alignment:.leading)
    }
}
