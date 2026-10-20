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
                if let error = remote.error { InfoNote(text: error, color: .orange) }
                if let notice = remote.notice { InfoNote(text: notice, color: .accentColor) }
                ingress
                connection
                destination
                if !remote.summary.isEmpty { preview }
                if !remote.status["peers"].array.isEmpty { peers }
            }.settingsPage()
        }
        .task { await remote.loadSSHConfig(); await remote.refresh(model) }
        .disabled(remote.busy || model.busy || !model.running)
        .alert(confirmation == "apply" ? "应用远端连接？" : confirmation == "restore" ? "还原接入前设置？" : confirmation == "start" ? "开启远端入口？" : "撤销此机器的 Key？", isPresented: Binding(get:{confirmation != nil},set:{if !$0 {confirmation=nil}})) {
            Button("取消", role:.cancel){confirmation=nil}
            Button("确认") {
                switch confirmation {
                case "apply":remote.apply(model)
                case "restore":remote.restore(model)
                case "start":remote.start(model)
                case "revoke":if let id=revokeID {remote.revoke(id,model:model)}
                default:break
                };confirmation=nil
            }
        } message: {
            Text(confirmation == "start" ? "将监听本机所有 IPv4 网络接口的所选端口。远端使用独立 Key；HTTP 连接仅适合可信局域网或 VPN，请勿直接暴露到公网。" : confirmation == "apply" ? "将先从远端验证网关连通性，再备份 config.toml、auth.json 等文件并更新配置。请先结束远端 Codex 的运行任务，应用后重启远端 Codex。" : confirmation == "restore" ? "将恢复此 Codex 目录首次接入前的配置与文件认证，接入后自行修改的这些设置也会还原。当前状态会另行备份。请先结束远端 Codex 任务，还原后重启。" : "此机器的旧 Key 将无法发起新请求，远端配置和对话记录保留。")
        }
    }
    private var ingress: some View {
        SettingsCard("本机远端入口", symbol:"network", accessory:{ StatusPill(text:remote.running ? "运行中" : "已关闭",color:remote.running ? .green : .secondary) }) {
            HStack(spacing:10) {
                Text("监听端口").font(.system(size:12))
                TextField("17864",text:$remote.port).settingsField().frame(width:95)
                Spacer()
                if remote.running { Button("关闭入口"){remote.stop(model)} }
                Button(remote.running ? "更新端口…" : "开启入口…"){confirmation="start"}
            }
            Text("远端请求使用当前来源池；仅用于可信局域网或 VPN。更改端口后需重新应用各远端配置。").font(.system(size:11)).foregroundStyle(.secondary)
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
                Text(remote.canApply ? "配置与文件认证将在远端备份。" : "设置已变化，请重新预览。").font(.system(size:11)).foregroundStyle(.secondary)
                Spacer();Button("备份并应用…"){confirmation="apply"}.disabled(!remote.canApply || !remote.running).buttonStyle(.borderedProminent)
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
