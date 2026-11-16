import Foundation
import CryptoKit

enum ModelClient: String, Codable, CaseIterable, Sendable {
    case codex, omp, chatbot, images
    var label: String {
        switch self {
        case .codex: return "Codex"
        case .omp: return "OMP"
        case .chatbot: return "聊天客户端"
        case .images: return "图片生成"
        }
    }
    func isApplicable(to modelID: String) -> Bool {
        modelID == "gpt-image-2" ? self == .omp || self == .images : self != .images
    }
}

struct ModelPolicy: Codable, Equatable, Sendable {
    var model: String
    var sourceIDs: [String]?
    var clients: [ModelClient] = ModelClient.allCases

    enum CodingKeys: String, CodingKey { case model, sourceIDs = "source_ids", clients }

    init(model: String, sourceIDs: [String]? = nil, clients: [ModelClient] = ModelClient.allCases) {
        self.model = model; self.sourceIDs = sourceIDs; self.clients = clients
    }
    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        model = try values.decode(String.self, forKey: .model)
        sourceIDs = try values.decodeIfPresent([String].self, forKey: .sourceIDs)
        clients = try values.decodeIfPresent([ModelClient].self, forKey: .clients) ?? ModelClient.allCases
    }
    static func validate(_ policies: [Self]) throws {
        guard Set(policies.map(\.model)).count == policies.count else { throw AppError.message("模型 ID 不能重复。") }
        for policy in policies {
            try validateModelID(policy.model)
            if let ids = policy.sourceIDs {
                guard Set(ids).count == ids.count, ids.allSatisfy({ !$0.isEmpty && !$0.contains(where: \.isWhitespace) }) else {
                    throw AppError.message("模型来源 ID 不能重复或包含空白。")
                }
            }
            guard Set(policy.clients).count == policy.clients.count else { throw AppError.message("模型接入端不能重复。") }
        }
    }
    static func validateModelID(_ id: String) throws {
        guard !id.isEmpty, id.utf8.count <= 256, !id.contains(where: \.isWhitespace) else {
            throw AppError.message("请填写不含空白的模型 ID，最多 256 字节。")
        }
    }
}

enum ModelSettingsRevision {
    private struct Snapshot: Encodable {
        var compaction: [ModelCompactionRule]
        var policies: [ModelPolicy]
    }
    static let empty = digest(Data(#"{"compaction":[],"policies":[]}"#.utf8))

    static func value(for settings: AppSettings) throws -> String {
        let policies = (settings.modelPolicies ?? []).map { policy in
            var canonical = policy
            canonical.clients.sort { $0.rawValue < $1.rawValue }
            return canonical
        }.sorted { $0.model < $1.model }
        let snapshot = Snapshot(compaction: (settings.modelCompaction ?? []).sorted { $0.model < $1.model }, policies: policies)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return digest(try encoder.encode(snapshot))
    }

    private static func digest(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}
