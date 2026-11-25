import SwiftUI

struct RemoteSourceSettingsForm: View {
    @Environment(\.dismiss) private var dismiss
    @State var source: Source
    @State var key: String
    let apply: (Source, String) -> Void
    @State private var failure: String?

    init(source: Source, key: String, apply: @escaping (Source, String) -> Void) {
        _source = State(initialValue: source); _key = State(initialValue: key); self.apply = apply
    }

    var body: some View {
        VStack {
            Form {
                Section(source.kindLabel) {
                    TextField("名称", text: $source.name)
                    if !source.official {
                        TextField("API 基址", text: $source.base_url)
                        SecureField("API 密钥", text: $key, prompt: Text("留空不更换远端密钥"))
                    }
                    Toggle("启用", isOn: $source.enabled)
                    Toggle("在 cmux 中展示", isOn: Binding(get: { source.showInCmux }, set: { source.show_in_cmux = $0 }))
                }
                Section("服务器出站网络") {
                    Picker("代理策略", selection: $source.proxy.mode) {
                        if !source.official { Text("直连").tag("direct") }
                        Text("服务器系统代理").tag("system")
                        Text("指定服务器代理").tag("explicit")
                    }
                    if source.proxy.mode == "explicit" {
                        TextField("代理 URL", text: Binding(get: { source.proxy.url ?? "" }, set: { source.proxy.url = $0 }), prompt: Text("http://127.0.0.1:7890"))
                        Text("localhost 指服务器，不是这台 Mac。不要在 URL 中填写凭据。").foregroundStyle(.secondary)
                    }
                    if source.official { Text("更换代理后需要在服务器重新登录，并重新确认自动重置授权。").foregroundStyle(.secondary) }
                }
                Section("模型") {
                    Toggle("服务器自动发现模型", isOn: Binding(get: { source.discoversModels }, set: { source.automaticModelDiscovery = $0 }))
                    if !source.discoversModels {
                        RemoteManualModelField(models: $source.models)
                    }
                }
                Section("并发对话") {
                    Toggle("限制活跃对话数", isOn: Binding(get: { source.max_active_conversations != nil }, set: { source.max_active_conversations = $0 ? 3 : nil }))
                    if source.max_active_conversations != nil {
                        Stepper("上限：\(source.max_active_conversations ?? 3)", value: Binding(get: { source.max_active_conversations ?? 3 }, set: { source.max_active_conversations = $0 }), in: 1...1024)
                        Stepper("空闲释放：\(source.activeConversationWindowSeconds) 秒", value: Binding(get: { source.activeConversationWindowSeconds }, set: { source.active_conversation_window_seconds = $0 }), in: 1...300)
                    }
                }
                if source.supportsBalanceQuery {
                    Section("余额与订阅") {
                        Toggle("订阅来源", isOn: Binding(get: { source.is_subscription ?? false }, set: { source.is_subscription = $0 }))
                        Toggle("设置余额预警", isOn: Binding(get: { source.balanceWarningThreshold != nil }, set: { source.balanceWarningThreshold = $0 ? 10 : nil }))
                        if source.balanceWarningThreshold != nil {
                            TextField("预警金额", value: Binding(get: { source.balanceWarningThreshold ?? 0 }, set: { source.balanceWarningThreshold = $0 }), format: .number)
                        }
                    }
                }
            }.formStyle(.grouped)
            if let failure { Text(failure).foregroundStyle(.red) }
            HStack {
                Spacer(); Button("取消") { dismiss() }
                Button("应用到草稿") {
                    do {
                        if source.proxy.mode != "explicit" { source.proxy.url = nil }
                        try source.validate(); apply(source, key); dismiss()
                    } catch { failure = error.localizedDescription }
                }.keyboardShortcut(.defaultAction)
            }.padding()
        }.frame(width: 570, height: 570)
    }
}

private struct RemoteManualModelField: View {
    @Binding var models: [String]
    @State private var text: String
    init(models: Binding<[String]>) {
        _models = models; _text = State(initialValue: models.wrappedValue.joined(separator: ", "))
    }
    var body: some View {
        TextField("模型 ID（用逗号分隔）", text: $text).onChange(of: text) { _, value in
            models = value.split { $0 == "," || $0 == "，" || $0.isWhitespace }.map(String.init)
        }
    }
}
