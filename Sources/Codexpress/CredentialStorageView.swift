import AppKit
import SwiftUI

struct CredentialStorageView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        SettingsCard("凭据存储", symbol: "lock.shield") {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: model.running ? "本机凭据文件" : "默认凭据文件", subtitle: "API Key 和官方账号的登录凭据以明文保存，仅当前用户可访问。") {
                    Button("打开目录") {
                        NSWorkspace.shared.open(model.localCredentialFile.deletingLastPathComponent())
                    }
                }
                Text(model.localCredentialFile.path)
                    .font(.system(size: 13, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                if model.settings.sources.contains(where: {
                    (!$0.official || $0.account_id != nil) && model.isMissingCredential($0)
                }) {
                    InfoNote(text: "文件中缺少部分来源的凭据，Codexpress 不会读取旧钥匙串。请在「来源」中为这些 API 来源填写 API Key，或重新登录对应的官方账号。", symbol: "exclamationmark.triangle", color: .orange)
                }
            }
        }
    }
}
