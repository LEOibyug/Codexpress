import SwiftUI

enum ConnectionSection: String, CaseIterable, Identifiable {
    case codex = "Codex", omp = "OMP", chatbot = "聊天客户端", remote = "远端 SSH", images = "图片生成", control = "管理 API"
    var id: String { rawValue }
}

struct ConnectionSettingsView: View {
    @EnvironmentObject var model: AppModel
    @Binding var section: ConnectionSection

    var body: some View {
        VStack(spacing: 0) {
            tabBar
            Divider()
            Group {
                switch section {
                case .codex: CodexSettingsView()
                case .omp: OMPSettingsView()
                case .chatbot: ChatbotSettingsView()
                case .remote: SSHRemoteView()
                case .images: ImageSettingsView()
                case .control: ControlSettingsView()
                }
            }.frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }

    private var tabBar: some View {
        let showLocal = section != .codex && model.showsLocalModelSettingsWarning
        let remoteCount = section == .remote ? 0 : (model.settings.sshRemoteProfiles ?? []).lazy.filter { profile in
            profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision) != nil
        }.count
        return HStack(spacing: 16) {
            Picker("接入方式", selection: $section) {
                ForEach(ConnectionSection.allCases) { Text($0.rawValue).tag($0) }
            }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 490)
            Spacer(minLength: 0)
            if showLocal || remoteCount > 0 {
                Menu {
                    if showLocal { Button("本机 Codex · 待同步") { section = .codex } }
                    if remoteCount > 0 { Button("远端 Codex · \(remoteCount) 台待处理") { section = .remote } }
                } label: {
                    Label("模型同步", systemImage: "exclamationmark.arrow.triangle.2.circlepath")
                }
                .fixedSize()
                .help("查看需要手动同步的 Codex 连接")
            }
        }
        .padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 12)
        .frame(maxWidth: 1120).frame(maxWidth: .infinity)
        .background(AppPalette.canvas)
    }
}

struct ConnectionListenerStatus: View {
    let gatewayRunning: Bool
    let loading: Bool
    let status: JSONValue

    var body: some View {
        if !gatewayRunning {
            StatusPill(text: "网关未运行", color: .orange)
        } else if loading {
            ProgressView().controlSize(.small).accessibilityLabel("正在读取入口状态")
        } else if status["running"].bool == true {
            StatusPill(text: "监听中", color: .green)
        } else if status["enabled"].bool == true {
            StatusPill(text: "未能启动", color: .orange)
        } else if status["enabled"].bool == false {
            StatusPill(text: "已关闭")
        } else {
            StatusPill(text: "状态未读取")
        }
    }
}
