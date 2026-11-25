import SwiftUI
import UniformTypeIdentifiers

struct RemoteConsoleMetadataFile {
    let json: String
    let count: Int

    init(data: Data) throws {
        guard data.count <= 256 * 1024,
              case let .object(root) = try JSONDecoder().decode(JSONValue.self, from: data), root.count == 1,
              case let .array(models) = root["models"], models.count <= 200 else { throw RemoteConsoleError.invalid }
        var names = Set<String>()
        for row in models {
            guard case let .object(fields) = row, fields.keys.allSatisfy({ ["slug", "max_context_window", "auto_compact_token_limit"].contains($0) }),
                  let slug = row["slug"].string, !slug.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains), slug != "gpt-image-2", names.insert(slug).inserted,
                  let window = row["max_context_window"].number.flatMap({ Int(exactly: $0) }) else { throw RemoteConsoleError.invalid }
            let limit = row["auto_compact_token_limit"].number.flatMap { Int(exactly: $0) }
            guard row["auto_compact_token_limit"] == .null || limit != nil else { throw RemoteConsoleError.invalid }
            try ModelPolicy.validateModelID(slug)
            _ = try ModelCompactionRule.parse(model: slug, threshold: limit.map(String.init) ?? "", window: String(window))
        }
        // Serialize the validated tree, not the original bytes (which can contain duplicate keys).
        json = String(decoding: try JSONEncoder().encode(JSONValue.object(root)), as: UTF8.self); count = models.count
    }
}

private struct RemoteMetadataChange {
    let target: RemoteConsoleTarget
    let revision: String
    var file: RemoteConsoleMetadataFile?
    var filename: String?
}

struct RemoteConsoleMetadata: View {
    let target: RemoteConsoleTarget
    let revision: String
    @State private var importing = false
    @State private var busy = false
    @State private var picking: RemoteMetadataChange?
    @State private var confirming: RemoteMetadataChange?
    @State private var notice: String?

    var body: some View {
        SettingsCard("默认模型能力", symbol: "doc.text") {
            Text("用 JSON 替换服务器导入的模型窗口与压缩阈值；不会更改各模型单独设置的策略。最多 200 个模型、256 KiB。")
                .font(.caption).foregroundStyle(.secondary)
            HStack {
                Button("导入 JSON…") { picking = .init(target: target, revision: revision); importing = true }
                Button("恢复默认…", role: .destructive) { confirming = .init(target: target, revision: revision) }
            }.disabled(busy)
            if let notice { Text(notice).font(.caption) }
        }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.json]) { result in
            guard let capture = picking else { return }; picking = nil
            guard case .success(let url) = result else { return }
            busy = true
            Task {
                defer { busy = false }
                do {
                    let file = try await Task.detached(priority: .utility) {
                        let scoped = url.startAccessingSecurityScopedResource()
                        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                        guard try url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else { throw RemoteConsoleError.invalid }
                        let handle = try FileHandle(forReadingFrom: url); defer { try? handle.close() }
                        return try RemoteConsoleMetadataFile(data: handle.read(upToCount: 256 * 1024 + 1) ?? Data())
                    }.value
                    guard capture.target.current else { throw RemoteConsoleError.invalid }
                    confirming = .init(target: capture.target, revision: capture.revision, file: file, filename: url.lastPathComponent)
                } catch { notice = "文件无效或网关已改变。仅接受模型能力 JSON。" }
            }
        }
        .alert("替换远端模型能力？", isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }), presenting: confirming) { change in
            Button("取消", role: .cancel) {}; Button(change.file == nil ? "恢复默认" : "确认上传并替换", role: .destructive) { save(change) }
        } message: { change in
            if let file = change.file { Text("将「\(change.filename ?? "JSON")」中 \(file.count) 个模型的能力信息上传至 \(change.target.endpoint)，并替换之前的导入。") }
            else { Text("将清除 \(change.target.endpoint) 上所有导入的默认模型能力。单独配置的规则保留。") }
        }
    }
    private func save(_ change: RemoteMetadataChange) {
        guard !busy else { return }; busy = true
        Task { defer { busy = false }; do {
            var fields: [String: JSONValue] = ["metadata_revision": .string(change.revision)]
            if let file = change.file { fields["json"] = .string(file.json) } else { fields["reset"] = .bool(true) }
            _ = try await change.target.call("models.metadata.replace", fields)
            notice = change.file == nil ? "已恢复默认模型能力。" : "已替换导入的模型能力。"
            await change.target.management.refresh()
        } catch { notice = "未确认替换。请刷新服务器能力后重新核对。" } }
    }
}
