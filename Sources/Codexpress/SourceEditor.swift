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
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                Image(systemName: source.official ? "person.crop.circle" : "terminal")
                    .font(.system(size: 22)).foregroundStyle(Color.accentColor).frame(width: 46, height: 46)
                    .background(Color.accentColor.opacity(0.09), in: RoundedRectangle(cornerRadius: 13))
                VStack(alignment: .leading, spacing: 5) {
                    Text(isNew ? "添加来源" : "编辑来源").font(.system(size: 21, weight: .semibold))
                    Text(source.official ? "ChatGPT 账号" : "Responses API")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                }
                Spacer()
            }.padding(.horizontal, 26).padding(.top, 26).padding(.bottom, 8)

            Form {
                Section("基本信息") {
                    TextField("名称", text: $source.name)
                    TextField("API 基址", text: $source.base_url).textContentType(.URL).disabled(source.official)
                    if !source.official {
                        SecureField(isNew ? "API 密钥" : "新 API 密钥", text: $apiKey)
                        if !isNew {
                            Text("留空保留现有密钥。").font(.system(size: 11)).foregroundStyle(.secondary)
                        }
                    }
                    Toggle("启用此来源", isOn: $source.enabled).toggleStyle(.switch).controlSize(.small)
                }
                Section("网络") {
                    Picker("出站策略", selection: $source.proxy.mode) {
                        if !source.official { Text("强制直连").tag("direct") }
                        Text("指定代理").tag("explicit")
                    }
                    if source.proxy.mode == "explicit" {
                        TextField("代理地址", text: Binding(get: { source.proxy.url ?? "" }, set: { source.proxy.url = $0 }), prompt: Text("http://127.0.0.1:端口"))
                    }
                }
                Section("模型") {
                    TextField("支持模型", text: $modelsText, prompt: Text("模型 ID，以逗号分隔"))
                    Text("留空表示不限模型，多个模型 ID 用逗号分隔。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
            }.formStyle(.grouped).scrollContentBackground(.hidden)

            VStack(spacing: 12) {
                if let localError { InfoNote(text: localError, symbol: "exclamationmark.triangle", color: .orange).frame(maxWidth: .infinity, alignment: .leading) }
                HStack {
                    Spacer()
                    Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(saving)
                    Button(saving ? "保存中…" : "保存来源") { save() }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(saving || !model.running || model.busy)
                }
            }.padding(.horizontal, 26).padding(.bottom, 22).padding(.top, 14)
        }.font(.system(size: 13)).background(AppPalette.canvas).frame(width: 580, height: 600)
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
