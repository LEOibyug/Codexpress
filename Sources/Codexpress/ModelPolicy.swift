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
    static func applicableClients(for modelID: String) -> [ModelClient] {
        ModelClient.allCases.filter { $0.isApplicable(to: modelID) }
    }
    static func supports(model modelID: String, source: Source) -> Bool {
        if modelID == "gpt-image-2" { return source.official }
        return source.models.isEmpty || source.models.contains(modelID)
    }
    static func supportingSourceIDs(for modelID: String, in settings: AppSettings) -> [String] {
        settings.sources
            .sorted { $0.priority < $1.priority }
            .filter { supports(model: modelID, source: $0) }
            .map(\.id)
    }
    static func isEnabled(_ modelID: String, in settings: AppSettings) -> Bool {
        let policy = settings.modelPolicies?.first { $0.model == modelID }
        let clients = (policy?.clients ?? ModelClient.allCases).filter { $0.isApplicable(to: modelID) }
        return !clients.isEmpty && policy?.sourceIDs?.isEmpty != true
    }
    static func isEnabled(_ modelID: String, on source: Source, in settings: AppSettings) -> Bool {
        guard supports(model: modelID, source: source) else { return false }
        let policy = settings.modelPolicies?.first { $0.model == modelID }
        let clients = (policy?.clients ?? ModelClient.allCases).filter { $0.isApplicable(to: modelID) }
        guard !clients.isEmpty else { return false }
        if let sourceIDs = policy?.sourceIDs {
            return sourceIDs.contains(source.id)
        }
        return true
    }
    static func setModelsEnabled(_ ids: [String], enabled: Bool, in settings: inout AppSettings) throws {
        var seen = Set<String>()
        let uniqueIDs = try ids.compactMap { raw -> String? in
            let id = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            try validateModelID(id)
            return seen.insert(id).inserted ? id : nil
        }
        guard !uniqueIDs.isEmpty else { return }
        var policies = settings.modelPolicies ?? []
        for id in uniqueIDs {
            let applicable = applicableClients(for: id)
            if let index = policies.firstIndex(where: { $0.model == id }) {
                var policy = policies[index]
                if enabled {
                    if policy.sourceIDs?.isEmpty == true { policy.sourceIDs = nil }
                    policy.clients = applicable
                } else {
                    policy.clients = []
                }
                policies[index] = policy
            } else {
                policies.append(ModelPolicy(model: id, sourceIDs: nil, clients: enabled ? applicable : []))
            }
        }
        try validate(policies)
        settings.modelPolicies = policies
    }
    static func setProviderModelsEnabled(sourceID: String, modelIDs: [String], enabled: Bool, in settings: inout AppSettings) throws {
        var seen = Set<String>()
        let uniqueIDs = try modelIDs.compactMap { raw -> String? in
            let id = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            try validateModelID(id)
            return seen.insert(id).inserted ? id : nil
        }
        guard !uniqueIDs.isEmpty else { return }
        var policies = settings.modelPolicies ?? []
        for id in uniqueIDs {
            let applicable = applicableClients(for: id)
            let existing = policies.first { $0.model == id }
            let effectiveClients = (existing?.clients ?? applicable).filter { $0.isApplicable(to: id) }
            var nextIDs: [String]
            if let explicit = existing?.sourceIDs {
                nextIDs = explicit
            } else if effectiveClients.isEmpty {
                nextIDs = []
            } else {
                nextIDs = supportingSourceIDs(for: id, in: settings)
            }
            let nextClients: [ModelClient]
            if enabled {
                if !nextIDs.contains(sourceID) { nextIDs.append(sourceID) }
                nextClients = effectiveClients.isEmpty ? applicable : effectiveClients
            } else {
                nextIDs.removeAll { $0 == sourceID }
                nextClients = effectiveClients
            }
            let updated = ModelPolicy(model: id, sourceIDs: nextIDs, clients: nextClients)
            if let index = policies.firstIndex(where: { $0.model == id }) {
                policies[index] = updated
            } else {
                policies.append(updated)
            }
        }
        try validate(policies)
        settings.modelPolicies = policies
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
