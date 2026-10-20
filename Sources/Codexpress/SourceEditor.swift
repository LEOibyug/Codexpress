import SwiftUI

struct SourceEditor: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State var source: Source
    let isNew: Bool
    @State private var apiKey = ""
    @State private var modelsText = ""
    @State private var localError: String?
    @State private var saving = false
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(isNew ? "添加来源" : "编辑来源").font(.title2.bold())
            Form {
                TextField("名称", text: $source.name)
                LabeledContent("类型", value: source.official ? "官方 OAuth 账号" : "Responses API")
                TextField("API 基址", text: $source.base_url).textContentType(.URL).disabled(source.official)
                if !source.official {
                    SecureField(isNew ? "API 密钥" : "新 API 密钥（留空保留现有密钥）", text: $apiKey)
                    Text("密钥存储于 macOS 钥匙串，不会写入来源文件或 Codex 配置。").font(.caption).foregroundStyle(.secondary)
                }
                Toggle("启用此来源", isOn: $source.enabled)
                Picker("网络策略", selection: $source.proxy.mode) {
                    if !source.official { Text("强制直连").tag("direct") }
                    Text("指定代理").tag("explicit")
                }
                if source.proxy.mode == "explicit" {
                    TextField("代理地址", text: Binding(get: { source.proxy.url ?? "" }, set: { source.proxy.url = $0 }), prompt: Text("http://127.0.0.1:端口"))
                    Text("代理不可达时停止此来源，不会自动切到直连。TUN／增强模式应保持关闭。").font(.caption).foregroundStyle(.secondary)
                }
                TextField("支持模型（逗号分隔，可留空）", text: $modelsText)
                Text("留空表示允许请求中的原始模型名称；不会自动映射模型。").font(.caption).foregroundStyle(.secondary)
                if source.official { Text("保存后可发起官方登录。应用请求使用此来源的指定代理；授权网页使用浏览器自己的网络设置。").font(.callout).foregroundStyle(.secondary) }
            }.formStyle(.grouped)
            if let localError { Label(localError, systemImage: "exclamationmark.triangle").foregroundStyle(.red).font(.callout) }
            HStack {
                Spacer(); Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(saving)
                Button(saving ? "保存中…" : "保存") { save() }.keyboardShortcut(.defaultAction).disabled(saving || !model.running || model.busy)
            }
        }.padding(24).frame(width: 570, height: source.official ? 580 : 600)
            .onAppear { modelsText = source.models.joined(separator: ", ") }
    }
    private func save() {
        saving = true; localError = nil
        source.models = modelsText.split(whereSeparator: { $0 == "," || $0 == "，" }).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
        if source.proxy.mode == "direct" { source.proxy.url = nil }
        Task {
            do { try await model.saveSource(source, apiKey: apiKey); apiKey = ""; dismiss() }
            catch { localError = error.localizedDescription }
            saving = false
        }
    }
}
