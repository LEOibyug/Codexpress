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
                    Text(source.official ? "连接官方 ChatGPT 账号" : "连接原生 Responses API")
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
                        Text(isNew ? "密钥安全存放于 macOS 钥匙串。" : "密钥留空保留现有值；新密钥存放于 macOS 钥匙串。")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
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
                        Text("代理不可达时停止此来源。Clash TUN／增强模式应保持关闭。")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                    } else {
                        Text("忽略系统和环境代理，直接连接此来源。")
                            .font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                }
                Section("模型") {
                    TextField("支持模型", text: $modelsText, prompt: Text("模型 ID，以逗号分隔"))
                    Text("留空表示允许请求中的原始模型名称，不进行模型映射。")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                }
                if source.official {
                    Section {
                        InfoNote(text: "保存后可发起登录。应用请求使用指定代理；授权网页遵循浏览器网络设置。")
                    }
                }
            }.formStyle(.grouped).scrollContentBackground(.hidden)

            VStack(spacing: 12) {
                if let localError { InfoNote(text: localError, symbol: "exclamationmark.triangle", color: .orange).frame(maxWidth: .infinity, alignment: .leading) }
                HStack {
                    Label("钥匙串保护", systemImage: "lock.shield").font(.system(size: 11)).foregroundStyle(.tertiary)
                    Spacer()
                    Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(saving)
                    Button(saving ? "保存中…" : "保存来源") { save() }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(saving || !model.running || model.busy)
                }
            }.padding(.horizontal, 26).padding(.bottom, 22).padding(.top, 14)
        }.font(.system(size: 13)).background(AppPalette.canvas).frame(width: 580, height: source.official ? 700 : 660)
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
