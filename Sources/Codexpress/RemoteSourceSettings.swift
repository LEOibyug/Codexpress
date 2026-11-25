import SwiftUI

struct RemoteSourceSettings: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    @EnvironmentObject private var local: AppModel
    @Environment(\.dismiss) private var dismiss
    let snapshot: RemotePoolSnapshot
    let generation: UUID
    @State private var draft: RemoteSourceConfiguration?
    @State private var editing: Source?
    @State private var importing = false
    @State private var busy = false
    @State private var message: String?
    private var current: Bool { generation == management.targetGeneration && management.snapshot?.revision == snapshot.revision && management.snapshot?.instanceID == snapshot.instanceID }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            PageHeading("编辑远端来源池", subtitle: management.endpoint)
            if let draft {
                HStack {
                    Picker("分配策略", selection: Binding(get: { self.draft?.mode ?? "auto" }, set: { self.draft?.mode = $0 })) {
                        Text("按优先级自动切换").tag("auto"); Text("固定来源").tag("fixed")
                    }
                    if draft.mode == "fixed" {
                        Picker("来源", selection: Binding(get: { self.draft?.fixed ?? "" }, set: { self.draft?.fixed = $0 })) {
                            Text("请选择").tag(""); ForEach(draft.sources.filter(\.enabled)) { Text($0.name).tag($0.id) }
                        }
                    }
                }
                List {
                    ForEach(Array(draft.sources.enumerated()), id: \.element.id) { idx, source in
                        HStack {
                            Toggle("启用 \(source.name)", isOn: Binding(get: { source.enabled }, set: { value in
                                var changed = source; changed.enabled = value; self.draft?.apply(changed, key: draft.keys[source.id] ?? "")
                            })).labelsHidden()
                            VStack(alignment: .leading) { Text(source.name); Text(source.kindLabel).font(.caption).foregroundStyle(.secondary) }
                            Spacer()
                            Button { self.draft?.sources.swapAt(idx, idx - 1) } label: { Image(systemName: "arrow.up") }.accessibilityLabel("上移").disabled(idx == 0)
                            Button { self.draft?.sources.swapAt(idx, idx + 1) } label: { Image(systemName: "arrow.down") }.accessibilityLabel("下移").disabled(idx + 1 == draft.sources.count)
                            Button("编辑") { editing = source }
                            Button("移除", role: .destructive) { self.draft?.remove(source.id) }
                        }.padding(.vertical, 4)
                    }
                }
                HStack {
                    Menu("添加来源") {
                        Button("官方账号") { var s = Source.newOfficial(); s.proxy = ProxyPolicy(mode: "system"); editing = s }
                        Button("Responses API") { editing = Source() }
                        Button("OpenAI API Platform") { editing = Source.newOpenAI() }
                        Button("Google AI Studio") { editing = Source.newGoogle() }
                    }.fixedSize()
                    Button("从本机导入…") { importing = true }.disabled(local.sources.isEmpty || local.metadataLoadFailed)
                }
            } else if busy { ProgressView("读取远端配置…") } else { Button("重新读取") { Task { await load() } }.disabled(!current) }
            Spacer(minLength: 0)
            if !current { Text("目标或配置版本已更改。请关闭草稿并重新打开。").foregroundStyle(.orange) }
            if let message { Text(message).foregroundStyle(.orange) }
            HStack {
                Text("保存后，来源及优先级才会在服务器生效。").font(.caption).foregroundStyle(.secondary)
                Spacer(); Button("取消") { dismiss() }.disabled(busy)
                Button("保存到服务器") { Task { await save() } }.disabled(!current || draft == nil || busy).keyboardShortcut(.defaultAction)
            }
        }.padding(22).frame(width: 670, height: 530).interactiveDismissDisabled(busy)
        .disabled(busy)
        .sheet(item: $editing) { source in
            RemoteSourceSettingsForm(source: source, key: "") { changed, newKey in
                let sameDestination = source.proxy == changed.proxy && source.base_url == changed.base_url
                let key = newKey.isEmpty && sameDestination ? (draft?.keys[source.id] ?? "") : newKey
                draft?.apply(changed, key: key)
            }
        }
        .sheet(isPresented: $importing) {
            RemoteSourceImport(sources: local.sources, endpoint: management.endpoint, current: current, hasCredentials: local.canManageCredentials) { source, includeKey in
                guard current else { throw CancellationError() }
                try draft?.importSource(source, includeKey: includeKey, credential: local.credentialSnapshot.load(source.id))
            }
        }
        .task { await load() }
    }
    private func load() async {
        guard current, !busy else { return }; busy = true; defer { busy = false }
        do {
            let data = try await management.operation("config.get", on: snapshot)
            guard current, !Task.isCancelled else { return }; draft = try RemoteSourceConfiguration(data); message = nil
        } catch { message = "无法读取远端配置。请刷新连接后重试。" }
    }
    private func save() async {
        guard current, let draft, !busy else { return }; busy = true; defer { busy = false }
        let payload: JSONValue
        do { payload = try draft.payload() } catch { message = error.localizedDescription; return }
        do {
            let reply = try await management.operation("config.save", payload: payload, on: snapshot)
            guard reply["revision"].string?.isEmpty == false, generation == management.targetGeneration else { throw CancellationError() }
            self.draft = nil; dismiss(); await management.refresh()
        } catch { message = "保存未获确认。请重新打开并核对远端配置；新启用或更换地址、代理的 API 来源需要提供密钥。" }
    }
}

private struct RemoteSourceImport: View {
    @Environment(\.dismiss) private var dismiss
    let sources: [Source]
    let endpoint: String
    let current: Bool
    let hasCredentials: Bool
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
