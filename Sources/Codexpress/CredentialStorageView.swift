import AppKit
import SwiftUI

struct CredentialStorageView: View {
    @EnvironmentObject var model: AppModel
    @State private var pendingMode: String?
    var body: some View {
        SettingsCard("凭据存储", symbol: "lock.shield") {
            VStack(alignment: .leading, spacing: 12) {
                SettingRow(title: "保存位置") {
                    Picker("保存位置", selection: Binding(get: { model.credentialStorageMode }, set: { if $0 != model.credentialStorageMode { pendingMode = $0 } })) {
                        Text("钥匙串").tag("keychain")
                        Text("本地文件").tag("file")
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 200)
                        .disabled(model.busy || !model.canManageCredentials || model.credentialStorage == .null)
                }
                if model.credentialStorageMode == "file" {
                    Text(model.credentialStorage["file_path"].string ?? model.localCredentialFile.path)
                        .font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                    HStack {
                        Text("本地文件不加密，请勿分享或同步此目录。")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                        Spacer(minLength: 0)
                        Button("打开目录") {
                            let file = model.credentialStorage["file_path"].string.map { URL(fileURLWithPath: $0) } ?? model.localCredentialFile
                            NSWorkspace.shared.open(file.deletingLastPathComponent())
                        }
                    }
                } else {
                    Text("由 macOS 钥匙串保护；系统可能要求授权访问。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                Text("切换后原位置保留副本，后续更新只写入当前存储。")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
            }
        }
        .task { await model.refreshCredentialStorage() }
        .alert("切换凭据存储？", isPresented: Binding(get: { pendingMode != nil }, set: { if !$0 { pendingMode = nil } })) {
            Button("取消", role: .cancel) { pendingMode = nil }
            Button("迁移并切换") { if let mode = pendingMode { model.changeCredentialStorage(mode) }; pendingMode = nil }
        } message: {
            Text(pendingMode == "file" ? "将 API Key 和官方登录令牌迁移到 \(model.localCredentialFile.path)。文件不加密，请勿分享或同步该目录。首次迁移可能需要授权读取已有钥匙串凭据，旧副本会保留。迁移期间新请求可能短暂等待。" : "将凭据迁移回 macOS 钥匙串，系统可能要求授权。原本地文件仍会保留明文副本，可在确认新存储可用后自行清理。迁移期间新请求可能短暂等待。")
        }
    }
}
