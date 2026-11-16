import SwiftUI

enum ConnectionSection: String, CaseIterable, Identifiable {
    case codex = "Codex", omp = "OMP", chatbot = "聊天客户端", remote = "远端 SSH", images = "图片生成", control = "管理 API"
    var id: String { rawValue }
}

struct ConnectionSettingsView: View {
    @EnvironmentObject var model: AppModel
    @Binding var section: ConnectionSection

    var body: some View {
        let pendingProfiles = pendingRemoteProfiles
        let showLocalNotice = model.localModelSettingsNeedsSync && section != .codex
        VStack(spacing: 0) {
            HStack {
                Text("接入方式").font(AppTypography.caption).foregroundStyle(.secondary)
                Spacer(minLength: 16)
                Picker("接入方式", selection: $section) {
                    ForEach(ConnectionSection.allCases) { Text($0.rawValue).tag($0) }
                }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 590)
            }.padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 16)
            Divider()
            if showLocalNotice || !pendingProfiles.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    if showLocalNotice {
                        HStack {
                            InfoNote(text: model.localModelSettingsStatus, color: .orange)
                            Spacer(minLength: 12)
                            Button("查看本地 Codex") { section = .codex }
                        }
                    }
                    if !pendingProfiles.isEmpty {
                        HStack(alignment: .top) {
                            InfoNote(text: "远端 Codex 模型设置需手动更新：" + pendingProfiles.map { profile in
                                profile.displayName + "（" + (profile.modelSettingsSyncStatus(revision: model.modelSettingsRevision)?.title ?? "") + "）"
                            }.joined(separator: "、") + "。请到远端 SSH 重新预览并应用模型设置。", color: .orange)
                            Spacer(minLength: 12)
                            Button("查看远端机器") { section = .remote }
                        }
                    }
                }.padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 12)
                Divider()
            }
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
        .frame(minWidth: 700, maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }

    private var pendingRemoteProfiles: [SSHRemoteProfile] {
        (model.settings.sshRemoteProfiles ?? []).filter { $0.modelSettingsSyncStatus(revision: model.modelSettingsRevision) != nil }
    }
}
