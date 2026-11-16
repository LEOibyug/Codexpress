import AppKit
import SwiftUI

struct CredentialStorageView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        SettingsCard("凭据存储", symbol: "lock.shield") {
            VStack(alignment: .leading, spacing: 16) {
                SettingRow(title: "保存位置") {
                    Picker("保存位置", selection: Binding(get: { model.credentialStorageMode }, set: { if $0 != model.credentialStorageMode { model.changeCredentialStorage($0) } })) {
                        Text("钥匙串").tag("keychain")
                        Text("本地文件").tag("file")
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 240)
                        .disabled(model.busy || !model.canManageCredentials || model.credentialStorage == .null)
                }
                Text("切换时自动迁移已有凭据，原副本保留。")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
                if model.credentialStorageMode == "file" {
                    Divider()
                    Text(model.credentialStorage["file_path"].string ?? model.localCredentialFile.path)
                        .font(.system(size: 13, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                    HStack(alignment: .firstTextBaseline, spacing: 20) {
                        Text("明文文件")
                            .font(.system(size: 12)).foregroundStyle(.secondary)
                        Spacer(minLength: 0)
                        Button("打开目录") {
                            let file = model.credentialStorage["file_path"].string.map { URL(fileURLWithPath: $0) } ?? model.localCredentialFile
                            NSWorkspace.shared.open(file.deletingLastPathComponent())
                        }
                    }
                }
            }
        }
        .task { await model.refreshCredentialStorage() }
    }
}
