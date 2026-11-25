import SwiftUI

struct RemoteLoginLink {
    let url: URL
    let state: String
    private static let callback = "http://localhost:1455/auth/callback"

    init?(_ value: String) {
        guard let c = URLComponents(string: value), c.scheme == "https", c.host == "auth.openai.com", c.port == nil,
              c.user == nil, c.password == nil, c.fragment == nil, c.percentEncodedPath == "/oauth/authorize", let url = c.url,
              let items = c.queryItems, Set(items.map(\.name)).count == items.count else { return nil }
        let fields = Dictionary(uniqueKeysWithValues: items.map { ($0.name, $0.value ?? "") })
        guard fields["redirect_uri"] == Self.callback, fields["client_id"] == "app_EMoamEEZ73f0CkXaXp7hrann",
              fields["response_type"] == "code", fields["code_challenge_method"] == "S256", fields["code_challenge"]?.isEmpty == false,
              let state = fields["state"], !state.isEmpty else { return nil }
        self.url = url; self.state = state
    }
    func accepts(_ callback: String) -> Bool {
        guard var parts = URLComponents(string: callback), let items = parts.queryItems else { return false }
        parts.query = nil
        return parts.string == Self.callback && items.filter { $0.name == "state" }.map(\.value) == [state]
            && items.filter { $0.name == "code" }.count == 1 && items.first { $0.name == "code" }?.value?.isEmpty == false
    }
}

struct RemoteSourceAccountActions: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    @Environment(\.openURL) private var openURL
    @Environment(\.dismiss) private var dismiss
    let source: RemotePoolSource
    let snapshot: RemotePoolSnapshot
    let generation: UUID
    let canQueryUpstream: Bool
    @State private var flow: String?
    @State private var login: RemoteLoginLink?
    @State private var callback = ""
    @State private var busy = false
    @State private var message: String?
    @State private var quota: JSONValue?
    @State private var models: [String]?

    private func call(_ op: String, _ fields: [String: JSONValue] = [:]) async throws -> JSONValue {
        guard generation == management.targetGeneration, management.capabilities.contains(op) else { throw CancellationError() }
        var payload = fields; payload["source_id"] = .string(source.id)
        let result = try await management.operation(op, payload: .object(payload), on: snapshot)
        guard !Task.isCancelled, generation == management.targetGeneration else { throw CancellationError() }
        return result
    }
    private func run(_ action: @escaping @MainActor () async throws -> Void) {
        guard !busy else { return }; busy = true; message = nil
        Task { @MainActor in
            defer { busy = false }
            do { try await action() } catch { message = "远端操作未获确认。请刷新来源池后重试。" }
        }
    }
    private func finish() {
        flow = nil; login = nil; callback = ""; message = "服务器已确认登录。"
        Task { await management.refresh() }
    }
    var body: some View {
        VStack(alignment: .leading) {
            PageHeading(source.name, subtitle: management.endpoint)
            Form {
                if source.kind == "official" {
                    Section("服务器官方账号") {
                        if let flow, let login {
                            Button("在浏览器中打开 OpenAI 登录页") { openURL(login.url) }
                            Text("登录后，浏览器可能无法连接 localhost。复制完整的 localhost 回调地址，粘贴到下方交给当前服务器确认。")
                            SecureField("localhost 回调 URL", text: $callback)
                            HStack {
                                Button("提交到此服务器") { run {
                                    let result = try await call("official.login.complete", ["flow_id": .string(flow), "callback_url": .string(callback)])
                                    guard result["state"].string == "complete" else { throw CancellationError() }; finish()
                                } }.disabled(!login.accepts(callback))
                                Button("取消登录") { run {
                                    let result = try await call("official.login.cancel", ["flow_id": .string(flow)])
                                    guard result["cancelled"].bool == true else { throw CancellationError() }
                                    self.flow = nil; self.login = nil; callback = ""
                                } }
                            }
                        } else if management.capabilities.isSuperset(of: ["official.login.start", "official.login.complete", "official.login.cancel", "official.login.status"]) {
                            Button("开始远端登录…") { run {
                                let result = try await call("official.login.start")
                                guard let id = result["flow_id"].string, !id.isEmpty, let link = RemoteLoginLink(result["auth_url"].string ?? ""), result["manual_callback_required"].bool == true else { throw CancellationError() }
                                flow = id; login = link
                            } }
                        }
                        if management.capabilities.contains("official.refresh") {
                            Button("刷新远端额度") { run { quota = try await call("official.refresh")["quota"] } }.disabled(!canQueryUpstream)
                        }
                        if let quota {
                            ForEach(QuotaOverview(quota).mainWindows) { window in LabeledContent(window.title, value: "\(window.remainingLabel) · \(window.resetLabel)") }
                        }
                    }
                }
                if management.capabilities.contains("models.list") {
                    Section("服务器模型发现") {
                        Button("查询此来源模型") { run {
                            models = try await call("models.list")["models"].array.compactMap(\.string)
                        } }.disabled(!canQueryUpstream)
                        if let models { Text(models.isEmpty ? "未发现模型" : models.joined(separator: ", ")).textSelection(.enabled) }
                    }
                }
            }.formStyle(.grouped).disabled(busy || generation != management.targetGeneration)
            if let message { Text(message) }
            HStack { Spacer(); Button("关闭") { dismiss() }.disabled(busy || flow != nil) }
        }.padding().frame(width: 560, height: 450).interactiveDismissDisabled(busy || flow != nil)
        .task(id: flow) {
            guard let flow else { return }
            while !Task.isCancelled {
                do {
                    try await Task.sleep(for: .seconds(3)); guard !busy else { continue }
                    let result = try await call("official.login.status", ["flow_id": .string(flow)])
                    guard self.flow == flow, !busy else { continue }
                    switch result["state"].string {
                    case "pending": continue
                    case "complete": finish()
                    default: self.flow = nil; login = nil; callback = ""; message = "登录会话已失效，请重新开始。"
                    }
                } catch {
                    guard !Task.isCancelled, !busy else { continue }
                    self.flow = nil; login = nil; callback = ""; message = "无法确认远端登录状态。请刷新后重试。"
                }
                return
            }
        }
    }
}
