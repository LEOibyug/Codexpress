import Foundation

struct RemoteConsoleMetadataFile: Sendable {
    let json: String
    let models: [JSONValue]
    var count: Int { models.count }

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
        self.models = models
        json = String(decoding: try JSONEncoder().encode(JSONValue.object(root)), as: UTF8.self)
    }
}

/// Local and remote imports read at most one byte beyond the allowed size, even for large files.
func readModelMetadata(_ url: URL) throws -> Data {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    guard try url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else {
        throw AppError.message("请选择普通 JSON 文件。")
    }
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    let limit = 256 * 1024
    let data = try handle.read(upToCount: limit + 1) ?? Data()
    guard data.count <= limit else { throw AppError.message("模型能力 JSON 不能超过 256 KiB。") }
    return data
}

