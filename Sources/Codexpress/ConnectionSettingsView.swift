import SwiftUI

enum ConnectionSection: String, CaseIterable, Identifiable {
    case codex = "Codex", chatbot = "聊天客户端", remote = "远端 SSH", images = "图片生成"
    var id: String { rawValue }
}

struct ConnectionSettingsView: View {
    @Binding var section: ConnectionSection

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("接入方式").font(AppTypography.caption).foregroundStyle(.secondary)
                Spacer(minLength: 16)
                Picker("接入方式", selection: $section) {
                    ForEach(ConnectionSection.allCases) { Text($0.rawValue).tag($0) }
                }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 480)
            }.padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 16)
            Divider()
            Group {
                switch section {
                case .codex: CodexSettingsView()
                case .chatbot: ChatbotSettingsView()
                case .remote: SSHRemoteView()
                case .images: ImageSettingsView()
                }
            }.frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}
