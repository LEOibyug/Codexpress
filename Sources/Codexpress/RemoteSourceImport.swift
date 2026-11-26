import SwiftUI

struct RemoteSourceImport: View {
    @Environment(\.dismiss) private var dismiss
    let sources: [Source]
    let endpoint: String
    let current: Bool
    let hasCredentials: Bool
    let supportsSourceCompactionV2: Bool
    let apply: (Source, Bool) throws -> Void
    @State private var selected = ""
    @State private var includeKey = false
    @State private var confirm = false
    @State private var message: String?
    private var source: Source? { sources.first { $0.id == selected } }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PageHeading("导入本机来源", subtitle: "目标服务器：\(endpoint)")
            Picker("本机来源", selection: $selected) {
                Text("请选择").tag(""); ForEach(sources) { Text($0.name).tag($0.id) }
            }.onChange(of: selected) { _, _ in includeKey = false; message = nil }
            Text("仅复制所选来源的设置。新来源默认停用；本机代理、官方账号身份和登录凭据均不导入。")
            if !supportsSourceCompactionV2, let source, !source.isGoogle, source.remote_compaction_v2 != nil {
                Text("此服务器不支持设置流式上下文压缩（V2），不会导入这一项。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let source, !source.official {
                Toggle("同时导入此来源的 API 密钥", isOn: $includeKey).disabled(!hasCredentials)
                if !hasCredentials { Text("本机凭据当前不可用。可先导入设置，再手动输入远端密钥。").font(.caption) }
            }
            if let message { Text(message).foregroundStyle(.orange) }
            HStack { Spacer(); Button("取消") { dismiss() }; Button("核对目标…") { confirm = true }.disabled(source == nil || !current) }
        }.padding(22).frame(width: 480)
        .alert("将此来源添加至远端草稿？", isPresented: $confirm) {
            Button("取消", role: .cancel) {}
            Button("确认添加") {
                guard current, let source else { return }
                do { try apply(source, includeKey); dismiss() } catch { message = "无法导入所选设置或密钥。" }
            }
        } message: { Text("来源：\(source?.name ?? "")\nAPI 基址：\(source?.base_url ?? "")\n目标：\(endpoint)\n\(includeKey ? "保存时也将发送此来源的 API 密钥。" : "不会发送任何本机凭据。")") }
    }
}
