import Foundation

/// Memory mirror of helper-owned durable storage. The app never opens the Keychain.
struct CredentialSnapshot {
    private var values: [String: JSONValue] = [:]
    func load(_ id: String) throws -> JSONValue? { values[id] }
    mutating func apply(_ response: JSONValue, requested ids: [String]) throws {
        let items = response["items"].array
        guard items.count == ids.count, Set(ids).count == ids.count else { throw AppError.message("凭据快照不完整，未更新来源。") }
        var updates: [String: JSONValue] = [:]
        for item in items {
            guard case .object(let fields) = item, fields["secret"] != nil else { throw AppError.message("凭据快照无效，未更新来源。") }
            guard let id = item["source_id"].string, ids.contains(id), updates[id] == nil else { throw AppError.message("凭据快照无效，未更新来源。") }
            switch item["secret"] { case .null, .object: break; default: throw AppError.message("凭据快照无效，未更新来源。") }
            updates[id] = item["secret"]
        }
        for (id, value) in updates { values[id] = value == .null ? nil : value }
    }
}
