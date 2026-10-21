import Foundation
import CryptoKit

// Persist alert episodes separately from editable source metadata. A failed
// delivery keeps its event ID, while acknowledged low balances stay quiet.
struct BalanceWarnings {
    private struct Episode: Codable, Equatable {
        let fingerprint: String
        let eventID: String
        var delivered: Bool
    }
    private let directory: URL
    private var file: URL { directory.appendingPathComponent("balance-warnings.json") }
    private var episodes: [String: Episode] = [:]

    init(directory: URL) throws {
        self.directory = directory
        if FileManager.default.fileExists(atPath: file.path) {
            let attributes = try FileManager.default.attributesOfItem(atPath: file.path)
            guard attributes[.type] as? FileAttributeType == .typeRegular,
                  ((attributes[.size] as? NSNumber)?.intValue ?? Int.max) <= 1_048_576 else {
                throw AppError.message("余额预警记录无效。")
            }
            episodes = try JSONDecoder().decode([String: Episode].self, from: Data(contentsOf: file))
        }
    }

    mutating func retain(sources: [Source]) throws {
        let ids = Set(sources.filter { !$0.official && $0.enabled && $0.balanceWarningThreshold != nil }.map(\.id))
        try save(episodes.filter { ids.contains($0.key) })
    }

    mutating func evaluate(source: Source, balance: JSONValue, apiKey: String) throws -> JSONValue? {
        guard !source.official, source.enabled, let threshold = source.balanceWarningThreshold,
              threshold.isFinite, threshold >= 0 else { return nil }
        guard balance["status"].string == "available", balance["stale"].bool == false,
              let amount = balance["amount"].number, amount.isFinite,
              let currency = balance["currency"].string, !currency.isEmpty else { return nil }
        var next = episodes
        guard amount <= threshold else {
            next.removeValue(forKey: source.id)
            try save(next)
            return nil
        }
        // Only a digest is persisted; endpoint/key changes start a new episode.
        let identity = try JSONEncoder().encode([source.base_url, apiKey, String(threshold), currency])
        let fingerprint = SHA256.hash(data: identity).map { String(format: "%02x", $0) }.joined()
        let episode: Episode
        if let existing = episodes[source.id], existing.fingerprint == fingerprint {
            guard !existing.delivered else { return nil }
            episode = existing
        } else {
            episode = Episode(fingerprint: fingerprint, eventID: UUID().uuidString, delivered: false)
            next[source.id] = episode
            try save(next)
        }
        return .object([
            "id": .string(episode.eventID), "kind": .string("balance_low"),
            "title": .string("余额预警"),
            "body": .string("\(source.name) 余额不足，当前余额 \(balanceMoney(amount, currency: currency))。")
        ])
    }

    mutating func acknowledge(_ ids: [String]) throws {
        let ids = Set(ids)
        var next = episodes
        for id in next.keys where ids.contains(next[id]!.eventID) { next[id]?.delivered = true }
        try save(next)
    }

    private mutating func save(_ next: [String: Episode]) throws {
        guard next != episodes else { return }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let temp = directory.appendingPathComponent(".balance-warnings-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: temp) }
        guard FileManager.default.createFile(atPath: temp.path, contents: try JSONEncoder().encode(next), attributes: [.posixPermissions: 0o600]),
              rename(temp.path, file.path) == 0 else {
            throw AppError.message("无法保存余额预警记录。")
        }
        episodes = next
    }
}
